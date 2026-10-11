//! Transmissor de tela (SPEC-04 §2): captura → redimensiona e converte para
//! NV12 na GPU → encoder de hardware.
//!
//! O ritmo é o da tela: sem mudança, nada é codificado (tela parada custa
//! quase nada); com mudança, no máximo `fps` quadros por segundo.

use std::time::{Duration, Instant};

use anyhow::Context;
use lerri_core::media::Codec;
use windows::Win32::Graphics::Direct3D11::{D3D11_BIND_RENDER_TARGET, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;

use super::capture::{Capture, Target};
use super::d3d::{Colors, Converter, D3D, fit};
use super::encode::{Encoder, OnFrame};

/// Maior resolução transmitida (SPEC-04 §2: 1080p).
pub const MAX_SIZE: (u32, u32) = (1920, 1080);
/// Com o encoder por software (sem encoder na placa): 720p30, que a CPU
/// aguenta sem atrapalhar o resto da máquina.
pub const SOFTWARE_SIZE: (u32, u32) = (1280, 720);
pub const SOFTWARE_FPS: u32 = 30;
/// Camada baixa do simulcast: para quem está com a rede ruim.
pub const LOW_SIZE: (u32, u32) = (640, 360);
const LOW_BITRATE: u32 = 400_000;
const LOW_FPS: u32 = 30;
/// Texturas NV12 em rodízio: o encoder pode ainda estar lendo a anterior.
const RING: usize = 4;

/// Tamanho do vídeo para uma tela `src`: cabe em [`MAX_SIZE`] (deitado ou
/// em pé, como a tela), mantém a proporção e tem lados pares (exigência do
/// NV12).
pub fn target_size(src: (u32, u32)) -> (u32, u32) {
    fit_even(src, MAX_SIZE)
}

/// Tamanho da camada baixa para uma tela `src` (cabe em [`LOW_SIZE`]).
pub fn low_size(src: (u32, u32)) -> (u32, u32) {
    fit_even(src, LOW_SIZE)
}

fn fit_even(src: (u32, u32), max: (u32, u32)) -> (u32, u32) {
    let max = if src.1 > src.0 { (max.1, max.0) } else { max };
    let (w, h) = if src.0 <= max.0 && src.1 <= max.1 {
        src
    } else {
        let (_, _, w, h) = fit(src, max);
        (w, h)
    };
    ((w & !1).max(2), (h & !1).max(2))
}

/// Camada baixa do simulcast: codificada só enquanto o relay diz que alguém
/// precisa dela.
struct Low {
    on_frame: OnFrame,
    enabled: bool,
    encoder: Option<Encoder>,
    conv: Option<Converter>,
    ring: Vec<ID3D11Texture2D>,
    next: usize,
    size: (u32, u32),
    last_encode: Option<Instant>,
}

pub struct Sender {
    d3d: D3D,
    low: Option<Low>,
    capture: Capture,
    conv: Option<Converter>,
    ring: Vec<ID3D11Texture2D>,
    next: usize,
    encoder: Encoder,
    on_frame: OnFrame,
    pub size: (u32, u32),
    fps: u32,
    bitrate: u32,
    last_encode: Option<Instant>,
}

impl Sender {
    /// `on_low`: para onde vão os quadros da camada baixa do simulcast
    /// (`None`: sem simulcast). Sem encoder na placa, vai por software em
    /// [`SOFTWARE_SIZE`] e [`SOFTWARE_FPS`], sem camada baixa.
    pub fn start(
        d3d: &D3D,
        target: Target,
        codec: Codec,
        fps: u32,
        bitrate: u32,
        on_frame: OnFrame,
        on_low: Option<OnFrame>,
    ) -> anyhow::Result<Self> {
        let capture = Capture::start(d3d, target)?;
        let software = !super::encode::has_hardware(codec);
        let (size, fps, on_low) =
            if software { (fit_even(capture.size, SOFTWARE_SIZE), fps.min(SOFTWARE_FPS), None) } else { (target_size(capture.size), fps, on_low) };
        let encoder = Encoder::new(d3d, codec, size, fps, bitrate, on_frame.clone())?;
        let ring = (0..RING).map(|_| d3d.texture(size.0, size.1, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET)).collect::<Result<_, _>>()?;
        let low = match on_low {
            Some(on_frame) => {
                let size = low_size(capture.size);
                let ring =
                    (0..RING).map(|_| d3d.texture(size.0, size.1, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET)).collect::<Result<_, _>>()?;
                Some(Low { on_frame, enabled: false, encoder: None, conv: None, ring, next: 0, size, last_encode: None })
            }
            None => None,
        };
        Ok(Self { d3d: d3d.clone(), low, capture, conv: None, ring, next: 0, encoder, on_frame, size, fps, bitrate, last_encode: None })
    }

    /// Liga ou desliga a camada baixa (o relay diz se alguém precisa dela).
    /// Desligada, o encoder dela é liberado.
    pub fn set_low_layer(&mut self, on: bool) {
        if let Some(low) = &mut self.low
            && low.enabled != on
        {
            tracing::info!("vídeo: camada baixa {}", if on { "ligada" } else { "desligada" });
            low.enabled = on;
            if !on {
                low.encoder = None;
            }
        }
    }

    /// Camada baixa: no máximo [`LOW_FPS`], e sempre que pedem keyframe. Um
    /// erro nela desliga só ela; a alta continua.
    fn step_low(&mut self, texture: &ID3D11Texture2D, src: (u32, u32), keyframe: bool) {
        let codec = self.encoder.codec;
        let Some(low) = self.low.as_mut().filter(|l| l.enabled) else { return };
        let fresh = low.encoder.is_none();
        let due = low.last_encode.is_none_or(|t| t.elapsed() >= Duration::from_secs(1) / LOW_FPS);
        if !(keyframe || fresh || due) {
            return;
        }
        let result = (|| {
            if low.encoder.is_none() {
                low.encoder = Some(Encoder::new(&self.d3d, codec, low.size, LOW_FPS, LOW_BITRATE, low.on_frame.clone())?);
            }
            if low.conv.as_ref().is_none_or(|c| c.input != src) {
                let mut conv = self.d3d.converter(src, low.size, Colors::RgbToYuv)?;
                conv.set_rects(src, fit(src, low.size), low.size);
                low.conv = Some(conv);
            }
            let dst = low.ring[low.next].clone();
            low.next = (low.next + 1) % RING;
            low.conv.as_mut().context("conversor")?.convert(texture, 0, &dst)?;
            low.encoder.as_mut().context("encoder")?.encode(&dst, keyframe || fresh)
        })();
        match result {
            Ok(()) => low.last_encode = Some(Instant::now()),
            Err(e) => {
                tracing::warn!("vídeo: camada baixa desligada: {e:#}");
                low.enabled = false;
                low.encoder = None;
            }
        }
    }

    pub fn codec(&self) -> Codec {
        self.encoder.codec
    }

    pub fn encoder_name(&self) -> &str {
        &self.encoder.name
    }

    /// Codificando na CPU (a placa não tem encoder).
    pub fn software(&self) -> bool {
        self.encoder.software
    }

    /// Duas camadas temporais (SVC).
    pub fn svc(&self) -> bool {
        self.encoder.svc
    }

    pub fn set_bitrate(&mut self, bitrate: u32) {
        if bitrate != self.bitrate {
            self.bitrate = bitrate;
            self.encoder.set_bitrate(bitrate);
        }
    }

    /// Troca de codec (alguém que entrou não decodifica o atual): recria o
    /// encoder e o próximo quadro sai como keyframe.
    pub fn set_codec(&mut self, codec: Codec) -> anyhow::Result<()> {
        if codec != self.encoder.codec {
            // O tamanho e o fps foram escolhidos para o encoder de agora.
            anyhow::ensure!(
                self.encoder.software || super::encode::has_hardware(codec),
                "sem encoder de hardware para {}",
                codec.name()
            );
            self.encoder = Encoder::new(&self.d3d, codec, self.size, self.fps, self.bitrate, self.on_frame.clone())?;
            if let Some(low) = &mut self.low {
                // Recriado no próximo quadro, já no codec novo e com keyframe.
                low.encoder = None;
            }
        }
        Ok(())
    }

    /// Espera a tela mudar (até `max_wait`) e codifica o quadro mais recente.
    /// `keyframe` codifica mesmo sem mudança (alguém precisa de um quadro
    /// completo). Devolve se codificou.
    pub fn step(&mut self, keyframe: bool, max_wait: Duration) -> anyhow::Result<bool> {
        // Respeita o fps máximo.
        let interval = Duration::from_secs(1) / self.fps;
        if let Some(last) = self.last_encode {
            let since = last.elapsed();
            if since < interval {
                std::thread::sleep(interval - since);
            }
        }
        anyhow::ensure!(!self.capture.shared.closed(), "a janela compartilhada foi fechada");
        if !keyframe && !self.capture.shared.wait_fresh(max_wait) {
            return Ok(false);
        }
        let (texture, src) = {
            let mut latest = self.capture.shared.latest.lock().unwrap();
            let Some(l) = latest.as_mut() else { return Ok(false) };
            l.fresh = false;
            (l.texture.clone(), l.size)
        };
        if self.conv.as_ref().is_none_or(|c| c.input != src) {
            // Tela mudou de resolução: o vídeo continua no tamanho negociado,
            // com barras se a proporção mudou.
            let mut conv = self.d3d.converter(src, self.size, Colors::RgbToYuv)?;
            conv.set_rects(src, fit(src, self.size), self.size);
            self.conv = Some(conv);
        }
        let dst = self.ring[self.next].clone();
        self.next = (self.next + 1) % RING;
        self.conv.as_mut().context("conversor")?.convert(&texture, 0, &dst)?;
        self.encoder.encode(&dst, keyframe)?;
        self.last_encode = Some(Instant::now());
        self.step_low(&texture, src, keyframe);
        Ok(true)
    }
}

/// Tela real → encoder de hardware → decoder D3D11VA, nesta máquina
/// (`cargo test -p lerri-media -- --ignored --nocapture screen_roundtrip`,
/// com as DLLs do FFmpeg no PATH).
#[cfg(test)]
mod hw_tests {
    use std::sync::{Arc, Mutex};

    use lerri_core::media::choose_codec;

    use super::*;
    use crate::video::encode::{EncodedFrame, encode_caps};
    use crate::video::ffmpeg::{Decoder, decode_caps};
    use crate::video::{capture, d3d::D3D};

    /// Tipos das unidades NAL (Annex B) de um quadro H.264/HEVC.
    fn nal_types(data: &[u8], codec: Codec) -> Vec<u8> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 3 < data.len() {
            if data[i..i + 3] == [0, 0, 1] {
                let h = data[i + 3];
                out.push(if codec == Codec::Hevc { (h >> 1) & 0x3f } else { h & 0x1f });
                i += 3;
            } else {
                i += 1;
            }
        }
        out
    }

    #[test]
    #[ignore = "usa a GPU, a tela e o FFmpeg desta máquina"]
    fn screen_roundtrip() {
        let d3d = D3D::new().unwrap();
        let (enc, dec) = (encode_caps(), decode_caps(&d3d.device));
        println!("GPU: {} | codifica {enc:?} | decodifica {dec:?}", d3d.adapter);
        assert!(choose_codec(&enc, [dec.as_slice()]).is_some(), "nenhum codec em comum");
        for codec in enc.iter().copied().filter(|c| dec.contains(c)) {
            roundtrip(&d3d, codec);
        }
    }

    fn roundtrip(d3d: &D3D, codec: Codec) {
        let monitor = capture::monitors().into_iter().next().expect("um monitor");
        let frames: Arc<Mutex<Vec<(Instant, EncodedFrame)>>> = Arc::default();
        let f = frames.clone();
        let low_frames: Arc<Mutex<Vec<EncodedFrame>>> = Arc::default();
        let lf = low_frames.clone();
        let on_low: OnFrame = Arc::new(move |e| lf.lock().unwrap().push(e));
        let mut s = Sender::start(d3d, Target::Monitor(monitor.handle), codec, 60, 4_000_000, Arc::new(move |e| {
            f.lock().unwrap().push((Instant::now(), e));
        }), Some(on_low))
        .unwrap();
        println!("monitor {} {}x{} → vídeo {}x{} {} ({})", monitor.name, monitor.width, monitor.height, s.size.0, s.size.1, codec.name(), s.encoder_name());

        let start = Instant::now();
        let mut encoded = 0;
        let mut first = true;
        while start.elapsed() < Duration::from_secs(3) {
            // Na metade, o relay pede a camada baixa.
            if start.elapsed() > Duration::from_millis(1500) {
                s.set_low_layer(true);
            }
            encoded += usize::from(s.step(first, Duration::from_millis(100)).unwrap());
            first = false;
        }
        // Mais um keyframe sob pedido, com a tela possivelmente parada.
        assert!(s.step(true, Duration::ZERO).unwrap());
        std::thread::sleep(Duration::from_millis(300));
        let fed = s.ring[(s.next + RING - 1) % RING].clone();

        let frames = std::mem::take(&mut *frames.lock().unwrap());
        for (_, f) in frames.iter().filter(|(_, f)| f.keyframe) {
            println!("keyframe: NALs {:?}", nal_types(&f.data, codec));
        }
        let bytes: usize = frames.iter().map(|(_, f)| f.data.len()).sum();
        let keys = frames.iter().filter(|(_, f)| f.keyframe).count();
        println!(
            "{} quadros em 3 s ({} entregues ao encoder), {keys} keyframes, keyframe inicial {} KB, média {:.0} kbps",
            frames.len(),
            encoded + 1,
            frames[0].1.data.len() / 1024,
            bytes as f64 * 8.0 / 3.0 / 1000.0
        );
        assert!(frames[0].1.keyframe, "primeiro quadro não é keyframe");
        assert!(frames.last().unwrap().1.keyframe, "keyframe sob pedido não veio");
        assert!(keys >= 2);

        let mut decoder = Decoder::new(codec, &d3d.device).unwrap();
        let t = Instant::now();
        let mut last = None;
        for (_, f) in &frames {
            if let Some(d) = decoder.decode(&f.data).unwrap() {
                last = Some(d);
            }
        }
        println!("decode de {} quadros: {:?}", frames.len(), t.elapsed());
        let last = last.expect("nenhum quadro decodificado");
        assert_eq!((last.width, last.height), s.size);

        // Plano Y do que entrou no encoder × do que saiu do decoder.
        let (din, yin) = d3d.read_back(&fed, 0).unwrap();
        let (dout, yout) = d3d.read_back(&last.texture, last.index).unwrap();
        let (w, h) = (s.size.0 as usize, s.size.1 as usize);
        let mut se = 0f64;
        for y in 0..h {
            for x in 0..w {
                let a = f64::from(yin[y * din.Width as usize + x]);
                let b = f64::from(yout[y * dout.Width as usize + x]);
                se += (a - b) * (a - b);
            }
        }
        let mse = se / (w * h) as f64;
        let psnr = if mse == 0.0 { 99.0 } else { 10.0 * (255.0 * 255.0 / mse).log10() };
        println!("PSNR Y do último quadro: {psnr:.1} dB");
        assert!(psnr > 30.0, "imagem decodificada ruim: {psnr:.1} dB");

        // Camada baixa: começa num keyframe, pequena, e decodifica no tamanho dela.
        let low = std::mem::take(&mut *low_frames.lock().unwrap());
        assert!(!low.is_empty() && low[0].keyframe, "camada baixa não começou num keyframe");
        let low_bytes: usize = low.iter().map(|f| f.data.len()).sum();
        let mut decoder = Decoder::new(codec, &d3d.device).unwrap();
        let mut last_low = None;
        for f in &low {
            if let Some(d) = decoder.decode(&f.data).unwrap() {
                last_low = Some((d.width, d.height));
            }
        }
        println!("camada baixa: {} quadros, {:?}, {:.0} kbps", low.len(), last_low, low_bytes as f64 * 8.0 / 1.5 / 1000.0);
        assert_eq!(last_low, Some(low_size((monitor.width, monitor.height))));

        // Exibição: janela de verdade (escondida) + janela-filha + swapchain.
        show_in_window(d3d, &last);
    }

    fn show_in_window(d3d: &D3D, frame: &crate::video::ffmpeg::Decoded) {
        use crate::video::render::{self, Rect, Screen};
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::*;
        use windows::core::w;
        let top = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("teste"),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                1000,
                700,
                None,
                None,
                Some(GetModuleHandleW(None).unwrap().into()),
                None,
            )
            .unwrap()
        };
        let child = render::create_child(top.0 as isize, Rect { x: 10, y: 40, w: 640, h: 360 }).unwrap();
        let mut screen = Screen::new(d3d, child, (640, 360)).unwrap();
        screen.present(frame).unwrap();
        render::move_child(child, Rect { x: 0, y: 0, w: 900, h: 300 });
        screen.resize((900, 300)).unwrap();
        screen.present(frame).unwrap();
        drop(screen);
        render::destroy_child(child);
        unsafe { DestroyWindow(top).unwrap() };
        println!("exibição na janela-filha: ok");
    }
}

#[cfg(test)]
mod tests {
    use super::target_size;

    #[test]
    fn sizes() {
        assert_eq!(target_size((1920, 1080)), (1920, 1080));
        assert_eq!(target_size((2560, 1440)), (1920, 1080));
        assert_eq!(target_size((3440, 1440)), (1920, 804));
        assert_eq!(target_size((1366, 768)), (1366, 768));
        assert_eq!(target_size((1001, 701)), (1000, 700));
        // Monitor em pé.
        assert_eq!(target_size((1080, 1920)), (1080, 1920));
        assert_eq!(target_size((1440, 2560)), (1080, 1920));
    }
}
