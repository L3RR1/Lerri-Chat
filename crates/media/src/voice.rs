//! Canal de voz (SPEC-04 §1, SPEC-02 §4.3), sem depender de dispositivo:
//!
//! - [`Uplink`]: microfone → tratamento → VAD → Opus (quadros de 20 ms);
//! - [`Mixer`]: pacotes recebidos → buffer de jitter e decoder por remetente
//!   → mistura;
//! - [`Voice`] e [`VoiceIo`]: ligam os dois às chaves SFrame e ao transporte.
//!   Quem tem os dispositivos (ou um teste) só chama [`VoiceIo::capture`] com
//!   o microfone e [`Voice::render`] para encher a saída.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use lerri_core::MemberId;
use lerri_core::media::{self, Header, Kind};
use crate::screen::Keys;

use crate::dsp::{Dsp, TEN_MS};
use crate::jitter::{Jitter, Pop};
use crate::opus::{Decoder, Encoder, FRAME, OpusError};
use crate::vad::Vad;

/// Quanto tempo depois do último pacote alguém ainda conta como falando.
const SPEAKING_HOLD: Duration = Duration::from_millis(300);
pub const DEFAULT_BITRATE: i32 = 32_000;

pub struct Uplink {
    dsp: Dsp,
    vad: Vad,
    enc: Encoder,
    pcm: Vec<f32>,
    voice_prob: Option<f32>,
    seq: u16,
    talking: bool,
    packet: Vec<u8>,
}

impl Uplink {
    pub fn new(strong_ns: bool) -> Result<Self, OpusError> {
        Ok(Self {
            dsp: Dsp::new(strong_ns),
            vad: Vad::new(),
            enc: Encoder::new(DEFAULT_BITRATE)?,
            pcm: Vec::with_capacity(FRAME),
            voice_prob: None,
            seq: 0,
            talking: false,
            packet: Vec::new(),
        })
    }

    pub fn set_strong_ns(&mut self, on: bool) {
        self.dsp.set_strong_ns(on);
    }

    /// 10 ms do que saiu nas caixas (referência do AEC).
    pub fn far_end(&mut self, frame: &[f32]) {
        self.dsp.far_end(frame);
    }

    /// 10 ms do microfone. A cada 20 ms com fala, devolve o cabeçalho do
    /// pacote Opus pronto em [`Uplink::packet`].
    pub fn capture(&mut self, frame: &[f32], muted: bool) -> Option<Header> {
        debug_assert_eq!(frame.len(), TEN_MS);
        let at = self.pcm.len();
        self.pcm.extend_from_slice(frame);
        let prob = self.dsp.capture(&mut self.pcm[at..]);
        self.voice_prob = match (self.voice_prob, prob) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        if self.pcm.len() < FRAME {
            return None;
        }
        let speech = self.vad.update(&self.pcm, self.voice_prob.take());
        let send = speech && !muted;
        let start = send && !self.talking;
        self.talking = send;
        let result = if send {
            match self.enc.encode(&self.pcm, &mut self.packet) {
                Ok(()) => {
                    self.seq = self.seq.wrapping_add(1);
                    Some(Header::voice(self.seq, start))
                }
                Err(e) => {
                    tracing::warn!("{e}");
                    None
                }
            }
        } else {
            None
        };
        self.pcm.clear();
        result
    }

    /// Pacote Opus do último [`Uplink::capture`] que devolveu cabeçalho.
    pub fn packet(&self) -> &[u8] {
        &self.packet
    }

    pub fn talking(&self) -> bool {
        self.talking
    }

    /// Ajusta o encoder à perda medida na rede (SPEC-03 §6.1): mais
    /// redundância e menos bitrate quando perde.
    pub fn adapt(&mut self, loss_percent: f32) {
        let bitrate = match loss_percent {
            l if l >= 15.0 => 20_000,
            l if l >= 5.0 => 24_000,
            _ => DEFAULT_BITRATE,
        };
        let expected = (loss_percent * 1.5).clamp(5.0, 30.0) as i32;
        if let Err(e) = self.enc.set_bitrate(bitrate).and_then(|()| self.enc.set_expected_loss(expected)) {
            tracing::debug!("{e}");
        }
    }
}

struct Remote {
    jitter: Jitter,
    dec: Decoder,
    buf: [f32; FRAME],
    pos: usize,
    last_packet: Instant,
}

/// Mistura quem está falando. A saída de áudio puxa o ritmo.
#[derive(Default)]
pub struct Mixer {
    remotes: HashMap<MemberId, Remote>,
}

impl Mixer {
    pub fn push(&mut self, from: MemberId, header: Header, opus: Vec<u8>, now: Instant) {
        let remote = match self.remotes.entry(from) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => match Decoder::new() {
                Ok(dec) => e.insert(Remote { jitter: Jitter::new(), dec, buf: [0.0; FRAME], pos: FRAME, last_packet: now }),
                Err(err) => return tracing::warn!("{err}"),
            },
        };
        remote.last_packet = now;
        remote.jitter.push(header.seq, header.start, opus, now);
    }

    /// Enche `out` (48 kHz mono) com a mistura.
    pub fn render(&mut self, out: &mut [f32], now: Instant) {
        out.fill(0.0);
        for r in self.remotes.values_mut() {
            let mut i = 0;
            while i < out.len() {
                if r.pos == FRAME {
                    let res = match r.jitter.pop(now) {
                        Pop::Packet(p) => r.dec.decode(Some(&p), false, &mut r.buf),
                        Pop::Fec(p) => r.dec.decode(Some(&p), true, &mut r.buf),
                        Pop::Lost => r.dec.decode(None, false, &mut r.buf),
                        Pop::Silence => {
                            r.buf.fill(0.0);
                            Ok(())
                        }
                    };
                    if let Err(e) = res {
                        tracing::debug!("{e}");
                        r.buf.fill(0.0);
                    }
                    r.pos = 0;
                }
                let n = (FRAME - r.pos).min(out.len() - i);
                for (o, s) in out[i..i + n].iter_mut().zip(&r.buf[r.pos..r.pos + n]) {
                    *o += s;
                }
                r.pos += n;
                i += n;
            }
        }
        for s in out.iter_mut() {
            *s = s.clamp(-1.0, 1.0);
        }
    }

    pub fn speaking(&self, now: Instant) -> Vec<MemberId> {
        self.remotes.iter().filter(|(_, r)| now.duration_since(r.last_packet) < SPEAKING_HOLD).map(|(m, _)| *m).collect()
    }

    /// Esquece quem saiu da voz.
    pub fn retain(&mut self, keep: impl Fn(&MemberId) -> bool) {
        self.remotes.retain(|m, _| keep(m));
    }
}

/// Contadores para a interface e para diagnóstico.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct VoiceStats {
    pub sent: u64,
    pub received: u64,
    /// Datagramas que não decifraram (outro epoch, lixo, replay).
    pub rejected: u64,
}

type SendFn = Box<dyn Fn(Bytes) + Send + Sync>;

/// O que a saída de áudio, a rede e a interface compartilham.
pub struct Voice {
    keys: Keys,
    mixer: Mutex<Mixer>,
    muted: AtomicBool,
    deafened: AtomicBool,
    talking: AtomicBool,
    send: SendFn,
    sent: AtomicU64,
    received: AtomicU64,
    rejected: AtomicU64,
}

impl Voice {
    /// `keys`: chaves SFrame da sala (as mesmas da tela). `send` manda um
    /// datagrama ao relay.
    pub fn new(keys: Keys, send: impl Fn(Bytes) + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            keys,
            mixer: Mutex::default(),
            muted: AtomicBool::new(false),
            deafened: AtomicBool::new(false),
            talking: AtomicBool::new(false),
            send: Box::new(send),
            sent: AtomicU64::new(0),
            received: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
        })
    }

    /// Ponta do microfone, para a thread de captura.
    pub fn io(self: &Arc<Self>, strong_ns: bool) -> Result<VoiceIo, OpusError> {
        Ok(VoiceIo { uplink: Uplink::new(strong_ns)?, voice: self.clone(), datagram: Vec::new() })
    }

    pub fn set_muted(&self, on: bool) {
        self.muted.store(on, Ordering::Relaxed);
    }

    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    /// Ensurdecer: não toca nada e também não manda.
    pub fn set_deafened(&self, on: bool) {
        self.deafened.store(on, Ordering::Relaxed);
    }

    pub fn deafened(&self) -> bool {
        self.deafened.load(Ordering::Relaxed)
    }

    /// Estamos transmitindo agora (VAD).
    pub fn talking(&self) -> bool {
        self.talking.load(Ordering::Relaxed)
    }

    pub fn speaking(&self) -> Vec<MemberId> {
        self.mixer.lock().unwrap().speaking(Instant::now())
    }

    pub fn retain(&self, keep: impl Fn(&MemberId) -> bool) {
        self.mixer.lock().unwrap().retain(keep);
    }

    pub fn stats(&self) -> VoiceStats {
        VoiceStats {
            sent: self.sent.load(Ordering::Relaxed),
            received: self.received.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
        }
    }

    /// Datagrama que chegou do relay.
    pub fn receive(&self, datagram: &[u8]) {
        let Some((header, frame)) = Header::parse(datagram) else { return };
        if header.kind != Kind::Voice {
            return;
        }
        let meta = &datagram[..media::HEADER_LEN];
        match self.keys.lock().unwrap().open(meta, frame) {
            Ok((from, opus)) => {
                self.received.fetch_add(1, Ordering::Relaxed);
                if !self.deafened() {
                    self.mixer.lock().unwrap().push(from, header, opus, Instant::now());
                }
            }
            Err(e) => {
                self.rejected.fetch_add(1, Ordering::Relaxed);
                tracing::trace!("datagrama de voz descartado: {e}");
            }
        }
    }

    /// Enche a saída (48 kHz mono).
    pub fn render(&self, out: &mut [f32]) {
        if self.deafened() {
            out.fill(0.0);
            return;
        }
        self.mixer.lock().unwrap().render(out, Instant::now());
    }
}

/// Ponta do microfone (uma thread só).
pub struct VoiceIo {
    uplink: Uplink,
    voice: Arc<Voice>,
    datagram: Vec<u8>,
}

impl VoiceIo {
    pub fn voice(&self) -> &Arc<Voice> {
        &self.voice
    }

    pub fn far_end(&mut self, frame: &[f32]) {
        self.uplink.far_end(frame);
    }

    /// 10 ms do microfone: trata, decide se é fala e manda.
    pub fn capture(&mut self, frame: &[f32]) {
        let v = &self.voice;
        let silent = v.muted() || v.deafened();
        let out = self.uplink.capture(frame, silent);
        v.talking.store(self.uplink.talking(), Ordering::Relaxed);
        let Some(header) = out else { return };
        let opus = self.uplink.packet();
        self.datagram.clear();
        let meta = header.encode();
        self.datagram.extend_from_slice(&meta);
        if let Err(e) = v.keys.lock().unwrap().seal(&meta, opus, &mut self.datagram) {
            // Sem chave ainda (aguardando admissão): não manda nada em claro.
            return tracing::trace!("voz não enviada: {e}");
        }
        v.sent.fetch_add(1, Ordering::Relaxed);
        (v.send)(Bytes::copy_from_slice(&self.datagram));
    }

    pub fn set_strong_ns(&mut self, on: bool) {
        self.uplink.set_strong_ns(on);
    }

    pub fn adapt(&mut self, loss_percent: f32) {
        self.uplink.adapt(loss_percent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    use lerri_crypto::sframe::{Keyring, MediaEpoch};
    use zeroize::Zeroizing;

    pub fn keys(me: u32) -> Keys {
        let mut k = Keyring::new();
        k.set_epoch(MediaEpoch {
            epoch: 1,
            secret: Zeroizing::new([3; 32]),
            me,
            members: vec![(0, MemberId([1; 32])), (1, MemberId([2; 32]))],
        });
        Arc::new(Mutex::new(k))
    }

    /// Fala sintética: tom com envelope, alto o bastante para o VAD.
    fn speech(i: usize) -> Vec<f32> {
        (i * TEN_MS..(i + 1) * TEN_MS)
            .map(|k| {
                let t = k as f32 / 48_000.0;
                0.3 * (t * 220.0 * std::f32::consts::TAU).sin() * (1.0 + (t * 3.0 * std::f32::consts::TAU).sin()) / 2.0
            })
            .collect()
    }

    #[test]
    fn speech_goes_through_and_silence_does_not() {
        let (tx, rx) = mpsc::channel();
        let a = Voice::new(keys(0), move |d| tx.send(d).unwrap());
        let b = Voice::new(keys(1), |_| {});
        let mut io = a.io(false).unwrap();

        // Silêncio: nada sai.
        for _ in 0..50 {
            io.capture(&[0.0; TEN_MS]);
        }
        assert!(rx.try_recv().is_err());

        // Fala: um datagrama a cada 20 ms, cifrado, pequeno.
        for i in 0..100 {
            io.capture(&speech(i));
        }
        let datagrams: Vec<Bytes> = rx.try_iter().collect();
        assert!(datagrams.len() >= 40, "{} datagramas para 1 s de fala", datagrams.len());
        let avg = datagrams.iter().map(Bytes::len).sum::<usize>() / datagrams.len();
        assert!(avg < 200, "datagrama médio de {avg} bytes");
        assert!(a.talking());

        for d in &datagrams {
            b.receive(d);
        }
        assert_eq!(b.stats().received, datagrams.len() as u64);
        assert_eq!(b.speaking(), vec![MemberId([1; 32])]);
        let mut out = vec![0.0; 48_000];
        b.render(&mut out);
        let loud = crate::vad::level_db(&out);
        assert!(loud > -30.0, "saída em {loud:.1} dBFS");

        // Mudo: para de mandar.
        a.set_muted(true);
        for i in 0..50 {
            io.capture(&speech(i));
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn foreign_datagrams_are_rejected() {
        let b = Voice::new(keys(1), |_| {});
        b.receive(&[0x10, 0, 0, 1, 0, 1, 2, 3]);
        b.receive(b"lixo");
        let mut out = vec![0.0; 960];
        b.render(&mut out);
        assert!(out.iter().all(|s| *s == 0.0));
        assert_eq!(b.stats().received, 0);
        assert_eq!(b.stats().rejected, 1);
    }
}

/// Custo de CPU do caminho de voz (`cargo test --release -p lerri-media --
/// --ignored --nocapture cpu_cost`): microfone com AEC/NS/AGC + VAD + Opus +
/// SFrame de um lado, decifrar + jitter + decode + mistura do outro.
#[cfg(test)]
mod bench {
    use super::*;
    use crate::dsp::TEN_MS;

    #[test]
    #[ignore = "medição; rode em release"]
    fn cpu_cost() {
        let (tx, rx) = std::sync::mpsc::channel();
        let a = Voice::new(super::tests::keys(0), move |d| tx.send(d).unwrap());
        let b = Voice::new(super::tests::keys(1), |_| {});
        let mut io = a.io(false).unwrap();
        let mut out = vec![0.0; TEN_MS];
        let seconds = 20;
        let start = Instant::now();
        for i in 0..seconds * 100 {
            let t0 = i * TEN_MS;
            let frame: Vec<f32> = (t0..t0 + TEN_MS).map(|k| 0.3 * (k as f32 * 0.0287).sin()).collect();
            b.render(&mut out);
            io.far_end(&out);
            io.capture(&frame);
            for d in rx.try_iter() {
                b.receive(&d);
            }
        }
        let busy = start.elapsed();
        let per_core = busy.as_secs_f64() / seconds as f64 * 100.0;
        println!(
            "{seconds} s de voz (falando o tempo todo) em {busy:?}: {per_core:.2}% de um núcleo, {:.2}% de 12 threads",
            per_core / 12.0
        );
    }
}
