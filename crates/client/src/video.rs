//! Vídeo com GPU (Windows, Fases 4–5): publicar a tela ou a câmera
//! ([`GpuShare`]) e assistir ([`GpuViewer`]). Cada um tem sua thread, porque
//! D3D11, Media Foundation e a swapchain não combinam com o runtime
//! assíncrono.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lerri_core::media::Codec;
use lerri_media::screen::{Frame, Gap, RateControl, Reorder};
use lerri_media::video::camera::{CameraDevice, CameraSender};
use lerri_media::video::capture::{Monitor, Target};
use lerri_media::video::d3d::D3D;
use lerri_media::video::encode::{self, EncodedFrame};
use lerri_media::video::ffmpeg::{self, Decoder};
use lerri_media::video::render::Screen;
use lerri_media::video::send::Sender;

use crate::screen::{Outgoing, Sink};

/// Teto de quadros por segundo da tela (SPEC-04 §2).
const FPS: u32 = 60;

/// O que esta máquina faz com vídeo.
#[derive(Debug, Clone, Default)]
pub struct Caps {
    pub encode: Vec<Codec>,
    pub decode: Vec<Codec>,
    pub monitors: Vec<Monitor>,
    pub cameras: Vec<CameraDevice>,
    /// Por que não dá para assistir, se não dá.
    pub decode_problem: Option<String>,
}

/// Consulta GPU, encoders, câmeras e FFmpeg (leva alguns milissegundos).
pub fn probe() -> Caps {
    encode::init_mf();
    let monitors = lerri_media::video::capture::monitors();
    let cameras = lerri_media::video::camera::cameras();
    let encode = encode::encode_caps();
    let (decode, decode_problem) = match (D3D::new(), ffmpeg::ffmpeg()) {
        (Ok(d3d), Ok(_)) => (ffmpeg::decode_caps(&d3d.device), None),
        (Err(e), _) => (Vec::new(), Some(format!("GPU indisponível: {e:#}"))),
        // O detalhe técnico vai para o log; a sala mostra o que fazer.
        (_, Err(e)) if e.starts_with("FFmpeg incompatível") => (Vec::new(), Some(format!("{e}; use a pasta ffmpeg que vem no zip do Lerri Chat"))),
        (_, Err(e)) => {
            tracing::info!("FFmpeg: {e}");
            (Vec::new(), Some("falta a pasta ffmpeg ao lado do lerrichat.exe (ela vem no zip do Lerri Chat)".into()))
        }
    };
    Caps { encode, decode, monitors, cameras, decode_problem }
}

/// O que publicar.
#[derive(Debug, Clone)]
pub enum Source {
    Screen { monitor: isize },
    /// Uma janela só (vai como "tela" para quem assiste).
    Window { hwnd: isize },
    /// `None`: a primeira câmera.
    Camera { device: Option<String> },
}

/// Captura + encoder em uso, tela ou câmera.
enum Active {
    Screen(Box<Sender>),
    Camera(Box<CameraSender>),
}

impl Active {
    fn start(source: &Source, codec: Codec, bitrate: u32, on_frame: encode::OnFrame, on_low: encode::OnFrame) -> anyhow::Result<Self> {
        let d3d = D3D::new()?;
        Ok(match source {
            Source::Screen { monitor } => {
                Active::Screen(Box::new(Sender::start(&d3d, Target::Monitor(*monitor), codec, FPS, bitrate, on_frame, Some(on_low))?))
            }
            Source::Window { hwnd } => {
                Active::Screen(Box::new(Sender::start(&d3d, Target::Window(*hwnd), codec, FPS, bitrate, on_frame, Some(on_low))?))
            }
            Source::Camera { device } => {
                Active::Camera(Box::new(CameraSender::start(&d3d, device.as_deref(), codec, bitrate, on_frame)?))
            }
        })
    }

    fn description(&self) -> String {
        match self {
            Active::Screen(s) => {
                format!("{} {}×{}{} · {}", s.codec().name(), s.size.0, s.size.1, if s.svc() { " SVC" } else { "" }, s.encoder_name())
            }
            Active::Camera(c) => c.description(),
        }
    }

    fn software(&self) -> bool {
        match self {
            Active::Screen(s) => s.software(),
            Active::Camera(c) => c.software(),
        }
    }

    fn codec(&self) -> Codec {
        match self {
            Active::Screen(s) => s.codec(),
            Active::Camera(c) => c.codec(),
        }
    }

    fn set_codec(&mut self, codec: Codec) -> anyhow::Result<()> {
        match self {
            Active::Screen(s) => s.set_codec(codec),
            Active::Camera(c) => c.set_codec(codec),
        }
    }

    fn set_bitrate(&mut self, bitrate: u32) {
        match self {
            Active::Screen(s) => s.set_bitrate(bitrate),
            Active::Camera(c) => c.set_bitrate(bitrate),
        }
    }

    /// Simulcast: só a tela tem camada baixa.
    fn set_low_layer(&mut self, on: bool) {
        if let Active::Screen(s) = self {
            s.set_low_layer(on);
        }
    }

    /// Um passo do laço. Sem ninguém assistindo (`encode: false`), a tela só
    /// espera; a câmera continua lendo para não acumular quadros velhos.
    fn step(&mut self, encode: bool, keyframe: bool) -> anyhow::Result<()> {
        match self {
            Active::Screen(_) if !encode => std::thread::sleep(Duration::from_millis(100)),
            Active::Screen(s) => {
                s.step(keyframe, Duration::from_millis(50))?;
            }
            Active::Camera(c) => {
                c.step(encode, keyframe)?;
            }
        }
        Ok(())
    }
}

struct ShareControl {
    stop: AtomicBool,
    keyframe: AtomicBool,
    watchers: AtomicU32,
    /// Alguém recebe a camada baixa do simulcast.
    low: AtomicBool,
    codec: Mutex<Codec>,
    error: Mutex<Option<String>>,
}

/// Nossa tela ou câmera sendo transmitida.
pub struct GpuShare {
    ctl: Arc<ShareControl>,
    thread: Option<JoinHandle<()>>,
    /// "HEVC 1920×1080 · AMDh265Encoder"
    pub description: String,
    /// Codificando na CPU: a placa não tem encoder.
    pub software: bool,
}

impl GpuShare {
    /// Abre captura e encoder (bloqueia até estarem prontos).
    pub fn start(out: Arc<Outgoing>, source: Source, codec: Codec) -> anyhow::Result<Self> {
        let ctl = Arc::new(ShareControl {
            stop: AtomicBool::new(false),
            keyframe: AtomicBool::new(true),
            watchers: AtomicU32::new(0),
            low: AtomicBool::new(false),
            codec: Mutex::new(codec),
            error: Mutex::new(None),
        });
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let c = ctl.clone();
        let name = match source {
            Source::Screen { .. } | Source::Window { .. } => "tela",
            Source::Camera { .. } => "câmera",
        };
        let thread =
            std::thread::Builder::new().name(name.into()).spawn(move || share_loop(out, &source, codec, &c, ready_tx))?;
        match ready_rx.recv() {
            Ok(Ok((description, software))) => Ok(Self { ctl, thread: Some(thread), description, software }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => anyhow::bail!("a captura terminou sem começar"),
        }
    }

    pub fn request_keyframe(&self) {
        self.ctl.keyframe.store(true, Ordering::Relaxed);
    }

    pub fn set_watchers(&self, count: u32) {
        self.ctl.watchers.store(count, Ordering::Relaxed);
    }

    /// O relay passou alguém para a camada baixa (ou ninguém precisa mais).
    pub fn set_low_layer(&self, on: bool) {
        self.ctl.low.store(on, Ordering::Relaxed);
    }

    /// Codec negociado mudou (alguém entrou sem decodificar o atual).
    pub fn set_codec(&self, codec: Codec) {
        *self.ctl.codec.lock().unwrap() = codec;
    }

    pub fn error(&self) -> Option<String> {
        self.ctl.error.lock().unwrap().clone()
    }
}

impl Drop for GpuShare {
    fn drop(&mut self) {
        self.ctl.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn share_loop(
    out: Arc<Outgoing>,
    source: &Source,
    codec: Codec,
    ctl: &ShareControl,
    ready: mpsc::SyncSender<anyhow::Result<(String, bool)>>,
) {
    encode::init_mf();
    let mut rc = match source {
        Source::Screen { .. } | Source::Window { .. } => RateControl::default(),
        Source::Camera { .. } => RateControl::camera(),
    };
    let o = out.clone();
    let on_frame: encode::OnFrame =
        Arc::new(move |f: EncodedFrame| o.send_frame(lerri_core::media::LAYER_HIGH, f.keyframe, f.droppable, f.codec, &f.data));
    let o = out.clone();
    let on_low: encode::OnFrame =
        Arc::new(move |f: EncodedFrame| o.send_layer(lerri_core::media::LAYER_LOW, f.keyframe, f.codec, &f.data));
    let mut active = match Active::start(source, codec, rc.bitrate, on_frame, on_low) {
        Ok(a) => {
            let _ = ready.send(Ok((a.description(), a.software())));
            a
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    while !ctl.stop.load(Ordering::Relaxed) {
        let desired = *ctl.codec.lock().unwrap();
        if desired != active.codec() {
            match active.set_codec(desired) {
                Ok(()) => {
                    tracing::info!("vídeo: codec trocado para {}", desired.name());
                    ctl.keyframe.store(true, Ordering::Relaxed);
                }
                Err(e) => tracing::warn!("vídeo: não deu para trocar para {}: {e:#}", desired.name()),
            }
        }
        active.set_low_layer(ctl.low.load(Ordering::Relaxed));
        // Ninguém assistindo: não gasta GPU nem rede.
        let watched = ctl.watchers.load(Ordering::Relaxed) > 0;
        let in_flight = out.in_flight();
        if watched && let Some(b) = rc.update(in_flight, Instant::now()) {
            tracing::debug!("vídeo: bitrate {} kbps ({in_flight} quadros em trânsito)", b / 1000);
            active.set_bitrate(b);
        }
        let encode = watched && !rc.should_skip(in_flight);
        let key = encode && ctl.keyframe.swap(false, Ordering::Relaxed);
        if let Err(e) = active.step(encode, key) {
            tracing::warn!("vídeo: {e:#}");
            *ctl.error.lock().unwrap() = Some(format!("{e:#}"));
            break;
        }
        if !encode && watched {
            // Congestionado: espera um pouco o que está em trânsito chegar.
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

enum ViewerMsg {
    Frame(Frame),
    Resize((u32, u32)),
    Stop,
}

/// Quadros e bytes decodificados (diagnóstico).
#[derive(Default)]
pub struct ViewStats {
    pub frames: AtomicU64,
    pub bytes: AtomicU64,
}

/// A tela de alguém, decodificada na GPU e desenhada numa janela-filha (ou
/// só decodificada, sem janela: `/assistir` no terminal mede o caminho todo).
pub struct GpuViewer {
    tx: mpsc::Sender<ViewerMsg>,
    thread: Option<JoinHandle<()>>,
    need_key: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    pub stats: Arc<ViewStats>,
    pub window: Option<isize>,
}

impl GpuViewer {
    /// `window`: janela-filha e tamanho; `None` decodifica sem mostrar.
    pub fn start(window: Option<(isize, (u32, u32))>) -> anyhow::Result<Self> {
        ffmpeg::ffmpeg().map_err(anyhow::Error::msg)?;
        let (tx, rx) = mpsc::channel();
        let need_key = Arc::new(AtomicBool::new(true));
        let error: Arc<Mutex<Option<String>>> = Arc::default();
        let stats: Arc<ViewStats> = Arc::default();
        let (k, e, s) = (need_key.clone(), error.clone(), stats.clone());
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new().name("vídeo".into()).spawn(move || {
            let ready = D3D::new().and_then(|d3d| {
                let screen = window.map(|(hwnd, size)| Screen::new(&d3d, hwnd, size)).transpose()?;
                Ok((screen, d3d))
            });
            match ready {
                Ok((screen, d3d)) => {
                    let _ = ready_tx.send(Ok(()));
                    if let Err(err) = view_loop(&d3d, screen, &rx, &k, &s) {
                        tracing::warn!("vídeo: {err:#}");
                        *e.lock().unwrap() = Some(format!("{err:#}"));
                    }
                }
                Err(err) => {
                    let _ = ready_tx.send(Err(err));
                }
            }
        })?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self { tx, thread: Some(thread), need_key, error, stats, window: window.map(|(h, _)| h) }),
            Ok(Err(e)) => Err(e),
            Err(_) => anyhow::bail!("a thread de vídeo terminou sem começar"),
        }
    }

    /// Para onde mandar os quadros recebidos.
    pub fn sink(&self) -> Sink {
        let tx = Mutex::new(self.tx.clone());
        Arc::new(move |f| {
            let _ = tx.lock().unwrap().send(ViewerMsg::Frame(f));
        })
    }

    pub fn resize(&self, size: (u32, u32)) {
        let _ = self.tx.send(ViewerMsg::Resize(size));
    }

    /// Precisa de um keyframe (começou, perdeu quadro, decoder falhou ou a
    /// janela mudou de tamanho e ficou sem imagem).
    pub fn take_keyframe_request(&self) -> bool {
        self.need_key.swap(false, Ordering::Relaxed)
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }
}

impl Drop for GpuViewer {
    fn drop(&mut self) {
        // A thread larga a swapchain antes de a janela sumir.
        let _ = self.tx.send(ViewerMsg::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn view_loop(
    d3d: &D3D,
    mut screen: Option<Screen>,
    rx: &mpsc::Receiver<ViewerMsg>,
    need_key: &AtomicBool,
    stats: &ViewStats,
) -> anyhow::Result<()> {
    let mut reorder = Reorder::default();
    let mut decoder: Option<Decoder> = None;
    // Camada do simulcast decodificada agora; outra resolução pede decoder novo.
    let mut layer = lerri_core::media::LAYER_HIGH;
    loop {
        let msg = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(m) => m,
            Err(RecvTimeoutError::Timeout) => {
                if reorder.poll(Instant::now()) == Gap::NeedKeyframe {
                    need_key.store(true, Ordering::Relaxed);
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        };
        match msg {
            ViewerMsg::Resize(size) => {
                if let Some(s) = &mut screen {
                    s.resize(size)?;
                    // A imagem some no redimensionamento; tela parada não mandaria outra.
                    need_key.store(true, Ordering::Relaxed);
                }
            }
            ViewerMsg::Stop => return Ok(()),
            ViewerMsg::Frame(f) => {
                for f in reorder.push(f, Instant::now()) {
                    if f.layer != layer {
                        layer = f.layer;
                        decoder = None;
                    }
                    if decoder.as_ref().is_none_or(|d| d.codec != f.codec) {
                        decoder = Some(Decoder::new(f.codec, &d3d.device)?);
                    }
                    let d = decoder.as_mut().expect("criado acima");
                    match d.decode(&f.data) {
                        Ok(Some(frame)) => {
                            stats.frames.fetch_add(1, Ordering::Relaxed);
                            stats.bytes.fetch_add(f.data.len() as u64, Ordering::Relaxed);
                            if let Some(s) = &mut screen {
                                s.present(&frame)?;
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            tracing::debug!("vídeo: {e:#}; pedindo keyframe");
                            reorder.reset();
                            decoder = None;
                            need_key.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
        }
    }
}
