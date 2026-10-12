//! Câmera pelo Media Foundation (SPEC-04 §3): `IMFSourceReader` com o nosso
//! dispositivo D3D11, para os quadros saírem como textura NV12 na GPU. O
//! leitor converte o formato da câmera (MJPEG, YUY2…) sozinho.
//!
//! Depois disso o caminho é o da tela: redimensiona e converte na GPU, e o
//! mesmo encoder de hardware codifica ([`CameraSender`]).

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use lerri_core::media::Codec;
use windows::Win32::Graphics::Direct3D11::{D3D11_BIND_RENDER_TARGET, D3D11_TEXTURE2D_DESC, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::{GUID, Interface};

use super::d3d::{Colors, Converter, D3D, fit};
use super::encode::{self, Encoder, OnFrame, attr_string, init_mf};
use super::render::Screen;

/// Resolução e taxa da câmera (SPEC-04 §3: 720p30).
pub const CAMERA_SIZE: (u32, u32) = (1280, 720);
pub const CAMERA_FPS: u32 = 30;
const STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
const RING: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraDevice {
    /// Link simbólico do dispositivo (estável entre execuções).
    pub id: String,
    pub name: String,
}

fn enum_activates() -> anyhow::Result<Vec<IMFActivate>> {
    init_mf();
    let mut attrs = None;
    unsafe { MFCreateAttributes(&mut attrs, 1)? };
    let attrs = attrs.context("MFCreateAttributes")?;
    unsafe { attrs.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID)? };
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe { MFEnumDeviceSources(&attrs, &mut list, &mut count)? };
    if list.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: o MF devolveu `count` ativadores em `list`, liberados aqui.
    let items = unsafe { std::slice::from_raw_parts_mut(list, count as usize) };
    let out = items.iter_mut().filter_map(Option::take).collect();
    unsafe { CoTaskMemFree(Some(list as *const _)) };
    Ok(out)
}

/// Câmeras ligadas.
pub fn cameras() -> Vec<CameraDevice> {
    enum_activates()
        .unwrap_or_default()
        .iter()
        .filter_map(|a| {
            Some(CameraDevice {
                id: attr_string(a, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK)?,
                name: attr_string(a, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME).unwrap_or_else(|| "câmera".into()),
            })
        })
        .collect()
}

fn split_u64(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, v as u32)
}

/// Um modo nativo da câmera.
#[derive(Debug, Clone, Copy)]
struct Mode {
    index: u32,
    size: (u32, u32),
    fps: f64,
    subtype: GUID,
}

/// Quanto pior, maior: menos de 30 fps, tamanho longe de 1280×720, formato
/// sem compressão. Vídeo cru em 720p30 (~40 MB/s) passa da banda real do
/// USB 2.0, e a câmera cai de taxa sozinha; MJPEG não.
fn mode_cost(m: &Mode) -> (u32, u32, u32, u32) {
    let height = m.size.1.abs_diff(CAMERA_SIZE.1);
    let width = m.size.0.abs_diff(CAMERA_SIZE.0);
    let fps = if m.fps >= f64::from(CAMERA_FPS) - 1.0 { 0 } else { (f64::from(CAMERA_FPS) - m.fps) as u32 + 1 };
    let format = match m.subtype {
        s if s == MFVideoFormat_MJPG => 0,
        s if s == MFVideoFormat_NV12 => 1,
        s if s == MFVideoFormat_YUY2 => 2,
        _ => 3,
    };
    (fps, height, width, format)
}

/// Câmera aberta, lendo quadros NV12 na GPU.
pub struct Camera {
    reader: IMFSourceReader,
    source: IMFMediaSource,
    _manager: IMFDXGIDeviceManager,
    /// Tamanho dos quadros que a câmera entrega.
    pub size: (u32, u32),
    pub fps: f64,
    pub name: String,
}

// SAFETY: o leitor é usado por uma thread de cada vez (a da câmera).
unsafe impl Send for Camera {}

impl Camera {
    /// Abre a câmera `id` (ou a primeira).
    pub fn open(d3d: &D3D, id: Option<&str>) -> anyhow::Result<Self> {
        let activates = enum_activates()?;
        let act = match id {
            Some(id) => activates
                .iter()
                .find(|a| attr_string(a, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK).as_deref() == Some(id)),
            None => activates.first(),
        }
        .context("câmera não encontrada")?;
        let name = attr_string(act, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME).unwrap_or_else(|| "câmera".into());
        let source: IMFMediaSource = unsafe { act.ActivateObject()? };
        let manager = encode::dxgi_manager(d3d)?;
        let mut attrs = None;
        unsafe { MFCreateAttributes(&mut attrs, 3)? };
        let attrs = attrs.context("MFCreateAttributes")?;
        unsafe {
            attrs.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &manager)?;
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
            attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
        }
        let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source, &attrs)? };

        // Escolhe o modo nativo mais perto de 720p30.
        let mut modes = Vec::new();
        for index in 0.. {
            let Ok(t) = (unsafe { reader.GetNativeMediaType(STREAM, index) }) else { break };
            let attrs = unsafe { (t.GetUINT64(&MF_MT_FRAME_SIZE), t.GetUINT64(&MF_MT_FRAME_RATE), t.GetGUID(&MF_MT_SUBTYPE)) };
            let (Ok(size), Ok(rate), Ok(subtype)) = attrs else { continue };
            let (num, den) = split_u64(rate);
            modes.push(Mode { index, size: split_u64(size), fps: f64::from(num) / f64::from(den.max(1)), subtype });
        }
        let mode = *modes.iter().min_by_key(|m| mode_cost(m)).context("a câmera não informou nenhum modo de vídeo")?;
        unsafe {
            let native = reader.GetNativeMediaType(STREAM, mode.index)?;
            reader.SetCurrentMediaType(STREAM, None, &native)?;
            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, (u64::from(mode.size.0) << 32) | u64::from(mode.size.1))?;
            reader.SetCurrentMediaType(STREAM, None, &out).context("a câmera não converte para NV12")?;
        }
        tracing::info!("câmera {name}: {}x{} {:.0} fps", mode.size.0, mode.size.1, mode.fps);
        Ok(Self { reader, source, _manager: manager, size: mode.size, fps: mode.fps, name })
    }

    /// Próximo quadro (bloqueia até a câmera entregar): textura NV12 e índice
    /// da fatia. `None` se o leitor só avisou algo (sem quadro).
    pub fn read(&mut self) -> anyhow::Result<Option<(ID3D11Texture2D, u32)>> {
        let mut flags = 0u32;
        let mut sample = None;
        unsafe { self.reader.ReadSample(STREAM, 0, None, Some(&mut flags), None, Some(&mut sample))? };
        if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
            bail!("a câmera parou");
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            bail!("a câmera foi desconectada");
        }
        let Some(sample) = sample else { return Ok(None) };
        let buffer = unsafe { sample.GetBufferByIndex(0)? };
        let dxgi: IMFDXGIBuffer = buffer.cast().context("a câmera entregou o quadro fora da GPU")?;
        let mut raw = std::ptr::null_mut();
        unsafe { dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)? };
        // SAFETY: GetResource devolveu uma referência nossa a uma textura.
        let texture = unsafe { ID3D11Texture2D::from_raw(raw) };
        let index = unsafe { dxgi.GetSubresourceIndex()? };
        Ok(Some((texture, index)))
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        unsafe {
            let _ = self.source.Shutdown();
        }
    }
}

/// Tamanho do vídeo da câmera: cabe em [`CAMERA_SIZE`], lados pares.
pub fn target_size(src: (u32, u32)) -> (u32, u32) {
    let (w, h) = if src.0 <= CAMERA_SIZE.0 && src.1 <= CAMERA_SIZE.1 {
        src
    } else {
        let (_, _, w, h) = fit(src, CAMERA_SIZE);
        (w, h)
    };
    ((w & !1).max(2), (h & !1).max(2))
}

/// Prévia da própria câmera: a janela-filha e a swapchain dela.
struct Preview {
    window: isize,
    screen: Screen,
}

/// Câmera → NV12 no tamanho do vídeo → encoder de hardware. Com prévia, cada
/// quadro lido também vai para a janela dela, direto da GPU, mesmo sem
/// ninguém assistindo.
pub struct CameraSender {
    d3d: D3D,
    camera: Camera,
    preview: Option<Preview>,
    conv: Option<Converter>,
    ring: Vec<ID3D11Texture2D>,
    next: usize,
    encoder: Encoder,
    on_frame: OnFrame,
    pub size: (u32, u32),
    bitrate: u32,
    last_encode: Option<Instant>,
}

impl CameraSender {
    pub fn start(d3d: &D3D, device: Option<&str>, codec: Codec, bitrate: u32, on_frame: OnFrame) -> anyhow::Result<Self> {
        let camera = Camera::open(d3d, device)?;
        let size = target_size(camera.size);
        let encoder = Encoder::new(d3d, codec, size, CAMERA_FPS, bitrate, on_frame.clone())?;
        let ring = (0..RING).map(|_| d3d.texture(size.0, size.1, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET)).collect::<Result<_, _>>()?;
        Ok(Self { d3d: d3d.clone(), camera, preview: None, conv: None, ring, next: 0, encoder, on_frame, size, bitrate, last_encode: None })
    }

    /// Mostra a câmera na janela-filha dada (tamanho em pixels físicos), ou
    /// para de mostrar (`None`). A mesma janela com outro tamanho só
    /// redimensiona. Chamar na thread da câmera.
    pub fn set_preview(&mut self, want: Option<(isize, (u32, u32))>) -> anyhow::Result<()> {
        match want {
            None => self.preview = None,
            Some((window, size)) => match &mut self.preview {
                Some(p) if p.window == window => p.screen.resize(size)?,
                p => {
                    // A anterior sai antes: a janela dela pode sumir logo.
                    *p = None;
                    let mut screen = Screen::new(&self.d3d, window, size)?;
                    screen.set_mirror(true);
                    *p = Some(Preview { window, screen });
                }
            },
        }
        Ok(())
    }

    /// Janela da prévia em uso agora.
    pub fn preview_window(&self) -> Option<isize> {
        self.preview.as_ref().map(|p| p.window)
    }

    pub fn codec(&self) -> Codec {
        self.encoder.codec
    }

    pub fn description(&self) -> String {
        format!("{} {}×{} · {} · {}", self.encoder.codec.name(), self.size.0, self.size.1, self.encoder.name, self.camera.name)
    }

    /// Codificando na CPU (a placa não tem encoder).
    pub fn software(&self) -> bool {
        self.encoder.software
    }

    pub fn set_bitrate(&mut self, bitrate: u32) {
        if bitrate != self.bitrate {
            self.bitrate = bitrate;
            self.encoder.set_bitrate(bitrate);
        }
    }

    pub fn set_codec(&mut self, codec: Codec) -> anyhow::Result<()> {
        if codec != self.encoder.codec {
            self.encoder = Encoder::new(&self.d3d, codec, self.size, CAMERA_FPS, self.bitrate, self.on_frame.clone())?;
        }
        Ok(())
    }

    /// Lê o próximo quadro da câmera (bloqueia ~1/fps) e codifica, se
    /// `encode` (com ninguém assistindo, só lê para a câmera não acumular).
    pub fn step(&mut self, encode: bool, keyframe: bool) -> anyhow::Result<bool> {
        let Some((texture, index)) = self.camera.read()? else { return Ok(false) };
        if let Some(p) = &mut self.preview
            && let Err(e) = p.screen.present_nv12(&texture, index, self.camera.size)
        {
            // Sem prévia a câmera continua no ar para quem assiste.
            tracing::warn!("câmera: prévia desligada: {e:#}");
            self.preview = None;
        }
        // Câmeras que entregam acima de 30 fps: pula o excedente.
        let interval = Duration::from_secs(1) / CAMERA_FPS;
        let early = self.last_encode.is_some_and(|t| t.elapsed() + Duration::from_millis(3) < interval);
        if !encode || (early && !keyframe) {
            return Ok(false);
        }
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        let pool = (desc.Width, desc.Height);
        if self.conv.as_ref().is_none_or(|c| c.input != pool) {
            let mut conv = self.d3d.converter(pool, self.size, Colors::YuvToYuv)?;
            conv.set_rects(self.camera.size, fit(self.camera.size, self.size), self.size);
            self.conv = Some(conv);
        }
        let dst = self.ring[self.next].clone();
        self.next = (self.next + 1) % RING;
        self.conv.as_mut().context("conversor")?.convert(&texture, index, &dst)?;
        self.encoder.encode(&dst, keyframe)?;
        self.last_encode = Some(Instant::now());
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_sizes() {
        assert_eq!(target_size((1280, 720)), (1280, 720));
        assert_eq!(target_size((1920, 1080)), (1280, 720));
        assert_eq!(target_size((640, 480)), (640, 480));
        assert_eq!(target_size((1440, 1080)), (960, 720));
    }

    #[test]
    fn prefers_720p30() {
        let m = |w, h, fps, subtype| Mode { index: 0, size: (w, h), fps, subtype };
        let modes = [
            m(1920, 1080, 30.0, MFVideoFormat_MJPG),
            m(1280, 720, 10.0, MFVideoFormat_YUY2),
            m(1280, 720, 30.0, MFVideoFormat_MJPG),
            m(640, 480, 30.0, MFVideoFormat_YUY2),
            m(960, 720, 30.0, MFVideoFormat_YUY2),
            m(1280, 720, 30.0, MFVideoFormat_YUY2),
        ];
        let best = modes.iter().min_by_key(|m| mode_cost(m)).unwrap();
        assert_eq!((best.size, best.fps as u32, best.subtype), ((1280, 720), 30, MFVideoFormat_MJPG));
    }
}

/// Webcam real → encoder → decoder (`cargo test -p lerri-media --
/// --ignored --nocapture camera_roundtrip`, FFmpeg no PATH).
#[cfg(test)]
mod hw_tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::video::encode::{EncodedFrame, encode_caps};
    use crate::video::ffmpeg::Decoder;

    #[test]
    #[ignore = "usa a câmera, a GPU e o FFmpeg desta máquina"]
    fn camera_roundtrip() {
        let d3d = D3D::new().unwrap();
        let list = cameras();
        println!("câmeras: {list:?}");
        let codec = encode_caps().into_iter().find(|c| *c != Codec::Av1).expect("encoder");
        let frames: Arc<Mutex<Vec<EncodedFrame>>> = Arc::default();
        let f = frames.clone();
        let mut s = CameraSender::start(&d3d, None, codec, 1_200_000, Arc::new(move |e| f.lock().unwrap().push(e))).unwrap();
        println!("{}", s.description());
        // A câmera leva um tempo para ligar e acertar a exposição.
        let warm = Instant::now();
        while warm.elapsed() < Duration::from_secs(2) {
            s.step(false, false).unwrap();
        }
        let start = Instant::now();
        let mut encoded = 0;
        while start.elapsed() < Duration::from_secs(3) {
            encoded += usize::from(s.step(true, encoded == 0).unwrap());
        }
        std::thread::sleep(Duration::from_millis(200));
        let frames = std::mem::take(&mut *frames.lock().unwrap());
        let bytes: usize = frames.iter().map(|f| f.data.len()).sum();
        println!(
            "{} quadros em 3 s ({encoded} codificados), {:.1} fps, {:.0} kbps",
            frames.len(),
            frames.len() as f64 / 3.0,
            bytes as f64 * 8.0 / 3.0 / 1000.0
        );
        // Em ambiente escuro a C920 alonga a exposição e cai para ~15 fps.
        assert!(frames.len() >= 30, "câmera lenta demais: {} quadros em 3 s", frames.len());
        assert!(frames[0].keyframe);
        let mut decoder = Decoder::new(codec, &d3d.device).unwrap();
        let mut decoded = 0;
        for f in &frames {
            if let Some(d) = decoder.decode(&f.data).unwrap() {
                assert_eq!((d.width, d.height), s.size);
                decoded += 1;
            }
        }
        println!("{decoded} quadros decodificados");
        assert_eq!(decoded, frames.len());
    }

    /// Prévia: a câmera desenha numa janela-filha de verdade, sem ninguém
    /// assistindo, troca de tamanho e de janela, e fecha.
    #[test]
    #[ignore = "usa a câmera e a GPU desta máquina"]
    fn camera_preview() {
        use crate::video::render::{self, Rect};
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::*;
        use windows::core::w;
        let d3d = D3D::new().unwrap();
        let codec = encode_caps().into_iter().find(|c| *c != Codec::Av1).expect("encoder");
        let mut s = CameraSender::start(&d3d, None, codec, 1_200_000, Arc::new(|_| {})).unwrap();
        let top = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("prévia"),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                900,
                600,
                None,
                None,
                Some(GetModuleHandleW(None).unwrap().into()),
                None,
            )
            .unwrap()
        };
        let a = render::create_child(top.0 as isize, Rect { x: 0, y: 0, w: 640, h: 360 }).unwrap();
        let b = render::create_child(top.0 as isize, Rect { x: 0, y: 0, w: 320, h: 180 }).unwrap();
        let run = |s: &mut CameraSender, frames: usize| {
            let mut n = 0;
            while n < frames {
                s.step(false, false).unwrap();
                n += 1;
            }
        };
        s.set_preview(Some((a, (640, 360)))).unwrap();
        run(&mut s, 30);
        assert_eq!(s.preview_window(), Some(a), "a prévia caiu ao desenhar");
        s.set_preview(Some((a, (480, 270)))).unwrap();
        run(&mut s, 10);
        s.set_preview(Some((b, (320, 180)))).unwrap();
        run(&mut s, 10);
        assert_eq!(s.preview_window(), Some(b));
        s.set_preview(None).unwrap();
        assert_eq!(s.preview_window(), None);
        render::destroy_child(a);
        render::destroy_child(b);
        unsafe { DestroyWindow(top).unwrap() };
        println!("prévia da câmera: ok ({})", s.description());
    }
}
