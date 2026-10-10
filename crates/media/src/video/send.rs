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

use super::capture::Capture;
use super::d3d::{Colors, Converter, D3D, fit};
use super::encode::{Encoder, OnFrame};

/// Maior resolução transmitida (SPEC-04 §2: 1080p).
pub const MAX_SIZE: (u32, u32) = (1920, 1080);
/// Texturas NV12 em rodízio: o encoder pode ainda estar lendo a anterior.
const RING: usize = 4;

/// Tamanho do vídeo para uma tela `src`: cabe em [`MAX_SIZE`] (deitado ou
/// em pé, como a tela), mantém a proporção e tem lados pares (exigência do
/// NV12).
pub fn target_size(src: (u32, u32)) -> (u32, u32) {
    let max = if src.1 > src.0 { (MAX_SIZE.1, MAX_SIZE.0) } else { MAX_SIZE };
    let (w, h) = if src.0 <= max.0 && src.1 <= max.1 {
        src
    } else {
        let (_, _, w, h) = fit(src, max);
        (w, h)
    };
    ((w & !1).max(2), (h & !1).max(2))
}

pub struct Sender {
    d3d: D3D,
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
    pub fn start(d3d: &D3D, monitor: isize, codec: Codec, fps: u32, bitrate: u32, on_frame: OnFrame) -> anyhow::Result<Self> {
        let capture = Capture::start(d3d, monitor)?;
        let size = target_size(capture.size);
        let encoder = Encoder::new(d3d, codec, size, fps, bitrate, on_frame.clone())?;
        let ring = (0..RING).map(|_| d3d.texture(size.0, size.1, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET)).collect::<Result<_, _>>()?;
        Ok(Self { d3d: d3d.clone(), capture, conv: None, ring, next: 0, encoder, on_frame, size, fps, bitrate, last_encode: None })
    }

    pub fn codec(&self) -> Codec {
        self.encoder.codec
    }

    pub fn encoder_name(&self) -> &str {
        &self.encoder.name
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
            self.encoder = Encoder::new(&self.d3d, codec, self.size, self.fps, self.bitrate, self.on_frame.clone())?;
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
        let mut s = Sender::start(d3d, monitor.handle, codec, 60, 4_000_000, Arc::new(move |e| {
            f.lock().unwrap().push((Instant::now(), e));
        }))
        .unwrap();
        println!("monitor {} {}x{} → vídeo {}x{} {} ({})", monitor.name, monitor.width, monitor.height, s.size.0, s.size.1, codec.name(), s.encoder_name());

        let start = Instant::now();
        let mut encoded = 0;
        let mut first = true;
        while start.elapsed() < Duration::from_secs(3) {
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
