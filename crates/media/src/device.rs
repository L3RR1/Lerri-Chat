//! Microfone e caixas pelo WASAPI compartilhado (SPEC-04 §1), via `cpal`.
//!
//! Uma thread "voz" é dona dos streams e faz o trabalho pesado: converte o
//! microfone para 48 kHz mono, passa a referência do AEC e chama
//! [`VoiceIo::capture`] a cada 10 ms. O callback de saída só mistura (o que
//! já está decodificado ou chega pelo buffer de jitter) e devolve a mistura
//! como referência do AEC.

use std::sync::Arc;

use crate::voice::Voice;

#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("áudio ainda só no Windows")]
    Unsupported,
    #[error("nenhum {0} de áudio encontrado")]
    Missing(&'static str),
    #[error("{0}")]
    Audio(String),
}

/// Microfone e saída escolhidos pelo nome. `None`: o padrão do Windows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceChoice {
    pub input: Option<String>,
    pub output: Option<String>,
}

/// Nomes dos microfones e das saídas desta máquina (leva até ~1 s: rode fora
/// da thread da interface).
pub fn list() -> (Vec<String>, Vec<String>) {
    #[cfg(windows)]
    return imp::list();
    #[cfg(not(windows))]
    (Vec::new(), Vec::new())
}

/// Microfone e saída em uso. Para ao ser derrubado.
pub struct Devices {
    pub input: String,
    pub output: String,
    #[cfg(windows)]
    inner: imp::Running,
}

impl Devices {
    /// Abre o microfone e a saída escolhidos (ou os padrões) e começa a
    /// transmitir por `voice`. Um escolhido que sumiu cai no padrão.
    #[cfg(windows)]
    pub fn start(voice: Arc<Voice>, strong_ns: bool, choice: &DeviceChoice) -> Result<Self, DeviceError> {
        let (inner, input, output) = imp::start(voice, strong_ns, choice.clone())?;
        Ok(Self { input, output, inner })
    }

    #[cfg(not(windows))]
    pub fn start(voice: Arc<Voice>, strong_ns: bool, choice: &DeviceChoice) -> Result<Self, DeviceError> {
        let _ = (voice, strong_ns, choice);
        Err(DeviceError::Unsupported)
    }

    /// Erro do dispositivo (ex.: desconectado), se houve.
    pub fn error(&self) -> Option<String> {
        #[cfg(windows)]
        return self.inner.error();
        #[cfg(not(windows))]
        None
    }

    /// Liga ou desliga a supressão forte (RNNoise) sem reiniciar o áudio.
    pub fn set_strong_ns(&self, on: bool) {
        #[cfg(windows)]
        self.inner.set_strong_ns(on);
        #[cfg(not(windows))]
        let _ = on;
    }

    /// Perda medida na rede, em %, para o encoder se ajustar.
    pub fn set_loss(&self, percent: f32) {
        #[cfg(windows)]
        self.inner.set_loss(percent);
        #[cfg(not(windows))]
        let _ = percent;
    }
}

#[cfg(windows)]
mod imp {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{FromSample, SampleFormat, SizedSample, Stream, StreamConfig};

    use super::{DeviceChoice, DeviceError};
    use crate::dsp::TEN_MS;
    use crate::opus::RATE;
    use crate::resample::{Resampler, downmix, upmix};
    use crate::voice::Voice;

    /// Referência do AEC esperando a thread de voz (10 ms cada).
    const REF_QUEUE: usize = 32;
    const CAPTURE_QUEUE: usize = 64;

    #[derive(Default)]
    struct Control {
        stop: AtomicBool,
        strong_ns: AtomicBool,
        strong_ns_changed: AtomicBool,
        /// Perda em centésimos de %; `u32::MAX`: nada novo.
        loss: AtomicU32,
        error: Mutex<Option<String>>,
    }

    pub struct Running {
        ctl: Arc<Control>,
        thread: Option<JoinHandle<()>>,
    }

    impl Running {
        pub fn error(&self) -> Option<String> {
            self.ctl.error.lock().unwrap().clone()
        }

        pub fn set_strong_ns(&self, on: bool) {
            self.ctl.strong_ns.store(on, Ordering::Relaxed);
            self.ctl.strong_ns_changed.store(true, Ordering::Relaxed);
        }

        pub fn set_loss(&self, percent: f32) {
            self.ctl.loss.store((percent * 100.0) as u32, Ordering::Relaxed);
        }
    }

    impl Drop for Running {
        fn drop(&mut self) {
            self.ctl.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    /// Erros que encerram o stream (dispositivo sumiu). Os outros, como
    /// estouro de buffer ou troca de rota, são só um soluço: registra e segue.
    fn on_stream_error(ctl: &Control, what: &str, e: &cpal::Error) {
        use cpal::ErrorKind as K;
        if matches!(e.kind(), K::DeviceNotAvailable | K::HostUnavailable | K::StreamInvalidated | K::PermissionDenied) {
            tracing::warn!("{what}: {e}");
            *ctl.error.lock().unwrap() = Some(format!("{what}: {e}"));
        } else {
            tracing::debug!("{what}: {e}");
        }
    }

    fn audio(e: impl std::fmt::Display) -> DeviceError {
        DeviceError::Audio(e.to_string())
    }

    fn device_name(d: &cpal::Device) -> String {
        d.description().map_or_else(|_| "padrão".to_string(), |d| d.name().to_string())
    }

    pub fn list() -> (Vec<String>, Vec<String>) {
        let host = cpal::default_host();
        let names = |devs: Result<Vec<cpal::Device>, cpal::Error>| {
            let mut v: Vec<String> = devs.unwrap_or_default().iter().map(device_name).collect();
            v.dedup();
            v
        };
        (
            names(host.input_devices().map(Iterator::collect)),
            names(host.output_devices().map(Iterator::collect)),
        )
    }

    /// O dispositivo com esse nome, ou o padrão se não há escolha ou ele sumiu.
    fn pick(
        devices: Result<impl Iterator<Item = cpal::Device>, cpal::Error>,
        want: Option<&str>,
        default: Option<cpal::Device>,
    ) -> Option<cpal::Device> {
        let found = want.and_then(|w| devices.ok()?.find(|d| device_name(d) == w));
        if want.is_some() && found.is_none() {
            tracing::warn!("dispositivo {want:?} não encontrado; usando o padrão");
        }
        found.or(default)
    }

    pub fn start(voice: Arc<Voice>, strong_ns: bool, choice: DeviceChoice) -> Result<(Running, String, String), DeviceError> {
        let ctl = Arc::new(Control { loss: AtomicU32::new(u32::MAX), ..Control::default() });
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let c = ctl.clone();
        let thread = std::thread::Builder::new()
            .name("voz".into())
            .spawn(move || run(voice, strong_ns, choice, c, ready_tx))
            .map_err(audio)?;
        let running = Running { ctl, thread: Some(thread) };
        match ready_rx.recv() {
            Ok(Ok((input, output))) => Ok((running, input, output)),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(DeviceError::Audio("a thread de áudio terminou sem abrir os dispositivos".into())),
        }
    }

    struct Streams {
        _input: Stream,
        _output: Stream,
        in_rate: u32,
        in_channels: usize,
        /// Microfone, em blocos do tamanho que o dispositivo entrega.
        capture: Receiver<Vec<f32>>,
        /// Referência do AEC, 10 ms a 48 kHz.
        far_end: Receiver<Vec<f32>>,
        input_name: String,
        output_name: String,
    }

    fn open(voice: Arc<Voice>, choice: &DeviceChoice, ctl: &Arc<Control>) -> Result<Streams, DeviceError> {
        let host = cpal::default_host();
        let in_dev = pick(host.input_devices(), choice.input.as_deref(), host.default_input_device())
            .ok_or(DeviceError::Missing("microfone"))?;
        let out_dev = pick(host.output_devices(), choice.output.as_deref(), host.default_output_device())
            .ok_or(DeviceError::Missing("dispositivo de saída"))?;
        let (in_name, out_name) = (device_name(&in_dev), device_name(&out_dev));

        let in_cfg = in_dev.default_input_config().map_err(audio)?;
        let out_cfg = out_dev.default_output_config().map_err(audio)?;
        let (cap_tx, cap_rx) = mpsc::sync_channel(CAPTURE_QUEUE);
        let (ref_tx, ref_rx) = mpsc::sync_channel(REF_QUEUE);

        let in_format = in_cfg.sample_format();
        let in_config: StreamConfig = in_cfg.into();
        let input = match in_format {
            SampleFormat::F32 => build_input::<f32>(&in_dev, &in_config, cap_tx, ctl.clone()),
            SampleFormat::I16 => build_input::<i16>(&in_dev, &in_config, cap_tx, ctl.clone()),
            SampleFormat::I32 => build_input::<i32>(&in_dev, &in_config, cap_tx, ctl.clone()),
            SampleFormat::U16 => build_input::<u16>(&in_dev, &in_config, cap_tx, ctl.clone()),
            f => Err(DeviceError::Audio(format!("formato do microfone não suportado: {f}"))),
        }?;
        let out_format = out_cfg.sample_format();
        let out_config: StreamConfig = out_cfg.into();
        let output = match out_format {
            SampleFormat::F32 => build_output::<f32>(&out_dev, &out_config, voice, ref_tx, ctl.clone()),
            SampleFormat::I16 => build_output::<i16>(&out_dev, &out_config, voice, ref_tx, ctl.clone()),
            SampleFormat::I32 => build_output::<i32>(&out_dev, &out_config, voice, ref_tx, ctl.clone()),
            SampleFormat::U16 => build_output::<u16>(&out_dev, &out_config, voice, ref_tx, ctl.clone()),
            f => Err(DeviceError::Audio(format!("formato da saída não suportado: {f}"))),
        }?;
        input.play().map_err(audio)?;
        output.play().map_err(audio)?;
        tracing::info!(
            "voz: microfone \"{in_name}\" {} Hz/{} canais, saída \"{out_name}\" {} Hz/{} canais",
            in_config.sample_rate,
            in_config.channels,
            out_config.sample_rate,
            out_config.channels
        );
        Ok(Streams {
            _input: input,
            _output: output,
            in_rate: in_config.sample_rate,
            in_channels: usize::from(in_config.channels),
            capture: cap_rx,
            far_end: ref_rx,
            input_name: in_name,
            output_name: out_name,
        })
    }

    fn build_input<T>(dev: &cpal::Device, cfg: &StreamConfig, tx: SyncSender<Vec<f32>>, ctl: Arc<Control>) -> Result<Stream, DeviceError>
    where
        T: SizedSample + Send + 'static,
        f32: FromSample<T>,
    {
        let c = ctl;
        dev.build_input_stream::<T, _, _>(
            *cfg,
            move |data: &[T], _| {
                // Fila cheia: a thread de voz atrasou; perder 10 ms é melhor que acumular atraso.
                let _ = tx.try_send(data.iter().map(|s| s.to_sample::<f32>()).collect());
            },
            move |e| on_stream_error(&c, "microfone", &e),
            None,
        )
        .map_err(audio)
    }

    fn build_output<T>(
        dev: &cpal::Device,
        cfg: &StreamConfig,
        voice: Arc<Voice>,
        ref_tx: SyncSender<Vec<f32>>,
        ctl: Arc<Control>,
    ) -> Result<Stream, DeviceError>
    where
        T: SizedSample + FromSample<f32> + Send + 'static,
    {
        let channels = usize::from(cfg.channels);
        let mut rs = Resampler::new(RATE, cfg.sample_rate);
        let mut pending: VecDeque<f32> = VecDeque::new();
        let mut mix = vec![0.0f32; TEN_MS];
        let mut scratch = Vec::new();
        let mut mono = Vec::new();
        let mut interleaved = Vec::new();
        dev.build_output_stream::<T, _, _>(
            *cfg,
            move |data: &mut [T], _| {
                let frames = data.len() / channels;
                while pending.len() < frames {
                    voice.render(&mut mix);
                    let _ = ref_tx.try_send(mix.clone());
                    scratch.clear();
                    rs.process(&mix, &mut scratch);
                    pending.extend(scratch.iter().copied());
                }
                mono.clear();
                mono.extend(pending.drain(..frames));
                interleaved.resize(data.len(), 0.0);
                upmix(&mono, channels, &mut interleaved);
                for (d, &s) in data.iter_mut().zip(&interleaved) {
                    *d = T::from_sample(s);
                }
            },
            move |e| on_stream_error(&ctl, "saída de áudio", &e),
            None,
        )
        .map_err(audio)
    }

    type Ready = SyncSender<Result<(String, String), DeviceError>>;

    fn run(voice: Arc<Voice>, strong_ns: bool, choice: DeviceChoice, ctl: Arc<Control>, ready: Ready) {
        // O tratamento de áudio não pode mudar de thread: nasce aqui.
        let mut io = match voice.io(strong_ns) {
            Ok(io) => io,
            Err(e) => {
                let _ = ready.send(Err(audio(e)));
                return;
            }
        };
        let streams = match open(voice, &choice, &ctl) {
            Ok(s) => s,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        let _ = ready.send(Ok((streams.input_name.clone(), streams.output_name.clone())));
        let mut rs = Resampler::new(streams.in_rate, RATE);
        let mut mono = Vec::new();
        let mut acc: Vec<f32> = Vec::new();
        while !ctl.stop.load(Ordering::Relaxed) {
            if ctl.strong_ns_changed.swap(false, Ordering::Relaxed) {
                io.set_strong_ns(ctl.strong_ns.load(Ordering::Relaxed));
            }
            let loss = ctl.loss.swap(u32::MAX, Ordering::Relaxed);
            if loss != u32::MAX {
                io.adapt(loss as f32 / 100.0);
            }
            let chunk = match streams.capture.recv_timeout(Duration::from_millis(100)) {
                Ok(c) => c,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            mono.clear();
            downmix(&chunk, streams.in_channels, &mut mono);
            rs.process(&mono, &mut acc);
            let mut at = 0;
            while acc.len() - at >= TEN_MS {
                while let Ok(r) = streams.far_end.try_recv() {
                    io.far_end(&r);
                }
                io.capture(&acc[at..at + TEN_MS]);
                at += TEN_MS;
            }
            acc.drain(..at);
        }
        drop(streams);
    }
}

/// Abre o microfone e a saída padrão desta máquina por alguns segundos
/// (`cargo test -p lerri-media -- --ignored --nocapture real_devices`).
#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    #[ignore = "usa os dispositivos de áudio reais"]
    fn real_devices() {
        let voice = Voice::new(Default::default(), |_| {});
        let dev = Devices::start(voice.clone(), true, &DeviceChoice::default()).expect("abrir áudio");
        let (inputs, outputs) = list();
        println!("microfones: {inputs:?}\nsaídas: {outputs:?}");
        println!("microfone: {} | saída: {}", dev.input, dev.output);
        let mut talked = false;
        for _ in 0..30 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            talked |= voice.talking();
            assert_eq!(dev.error(), None);
        }
        println!("detectou fala: {talked}");
        drop(dev);

        // Escolhido pelo nome: abre esse, não o padrão.
        if let Some(other) = outputs.iter().find(|o| **o != outputs[0]) {
            let choice = DeviceChoice { input: None, output: Some(other.clone()) };
            let dev = Devices::start(voice.clone(), false, &choice).expect("abrir a saída escolhida");
            assert_eq!(&dev.output, other);
            println!("saída escolhida: {}", dev.output);
        }
        // Um que sumiu cai no padrão em vez de falhar.
        let gone = DeviceChoice { input: Some("microfone que não existe".into()), output: None };
        assert!(Devices::start(voice, false, &gone).is_ok());
    }
}
