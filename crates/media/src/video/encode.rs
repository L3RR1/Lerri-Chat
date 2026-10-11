//! Encode de vídeo pelo Media Foundation, SPEC-04 §2: os MFTs de hardware
//! (AMF, NVENC, Quick Sync) e, sem nenhum, o H.264 por software do Windows.
//!
//! Os MFTs de hardware são assíncronos: avisam por eventos quando querem
//! entrada e quando têm saída. Uma thread fica no `GetEvent`; "quer entrada"
//! vira crédito para [`Encoder::encode`], e "tem saída" vira um
//! [`EncodedFrame`] entregue ao callback. A entrada é uma textura NV12 do
//! nosso dispositivo D3D11: nada passa pela CPU até o bitstream.
//!
//! O encoder por software (o "H264 Encoder MFT" que vem no Windows) é o
//! último recurso, para placa sem encoder: síncrono, recebe o quadro na
//! memória (uma cópia da GPU por quadro) e pesa na CPU. Vem com o Windows e
//! com a licença das patentes do H.264, sem DLL nem download a mais.
//!
//! SVC temporal (SPEC-04 §5): onde o encoder aceita duas camadas temporais
//! (AV1, por padrão), metade dos quadros não é referência para ninguém. Eles
//! saem marcados como descartáveis, e o relay corta esses primeiro de quem
//! está com a rede ruim: metade do fps, sem keyframe e sem outro encoder.

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Once};
use std::time::Duration;

use anyhow::{Context, bail};
use lerri_core::media::Codec;
use windows::Win32::Foundation::VARIANT_TRUE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};
use windows::core::{GUID, Interface};

use super::d3d::D3D;

/// Tempo máximo esperando o encoder pedir entrada.
const INPUT_WAIT: Duration = Duration::from_millis(500);
/// Camadas temporais do SVC: a de base e uma descartável.
const TEMPORAL_LAYERS: u32 = 2;

fn subtype(codec: Codec) -> GUID {
    match codec {
        Codec::H264 => MFVideoFormat_H264,
        Codec::Hevc => MFVideoFormat_HEVC,
        Codec::Av1 => MFVideoFormat_AV1,
    }
}

/// Media Foundation e COM (MTA) nesta thread.
pub fn init_mf() {
    static MF: Once = Once::new();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    MF.call_once(|| unsafe {
        if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_LITE) {
            tracing::warn!("MFStartup: {e}");
        }
    });
}

fn enum_encoders(codec: Codec) -> Vec<IMFActivate> {
    init_mf();
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: subtype(codec) };
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    let r = unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0),
            Some(&input),
            Some(&output),
            &mut list,
            &mut count,
        )
    };
    if r.is_err() || list.is_null() {
        return Vec::new();
    }
    // SAFETY: o MF devolveu `count` ativadores em `list`, liberados aqui.
    let items = unsafe { std::slice::from_raw_parts_mut(list, count as usize) };
    let out = items.iter_mut().filter_map(Option::take).collect();
    unsafe { CoTaskMemFree(Some(list as *const _)) };
    out
}

/// O encoder H.264 por software do Windows (falta nas edições "N" sem o
/// Media Feature Pack).
fn software_h264() -> Option<IMFTransform> {
    init_mf();
    unsafe { CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER) }.ok()
}

/// Gerenciador que dá ao Media Foundation o nosso dispositivo D3D11.
pub(crate) fn dxgi_manager(d3d: &D3D) -> anyhow::Result<IMFDXGIDeviceManager> {
    let mut token = 0u32;
    let mut manager = None;
    unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager)? };
    let manager = manager.context("MFCreateDXGIDeviceManager")?;
    unsafe { manager.ResetDevice(&d3d.device, token)? };
    Ok(manager)
}

/// Esta máquina codifica `codec` em hardware. Com `LERRICHAT_SOFTWARE_ENCODE`
/// no ambiente, finge que não (diagnóstico do caminho por software).
pub fn has_hardware(codec: Codec) -> bool {
    !force_software() && !enum_encoders(codec).is_empty()
}

fn force_software() -> bool {
    std::env::var_os("LERRICHAT_SOFTWARE_ENCODE").is_some()
}

/// Codecs que esta máquina codifica, do melhor para o pior: os de hardware
/// e, sem H.264 em hardware, o H.264 por software.
pub fn encode_caps() -> Vec<Codec> {
    let mut caps: Vec<Codec> = Codec::ALL.into_iter().filter(|c| has_hardware(*c)).collect();
    if !caps.contains(&Codec::H264) && software_h264().is_some() {
        caps.push(Codec::H264);
    }
    caps
}

/// SVC temporal ligado por padrão: no AV1 (SPEC-04 §5). Nos outros o
/// simulcast já cobre, e camadas temporais custam alguns % de compressão.
pub fn svc_by_default(codec: Codec) -> bool {
    codec == Codec::Av1
}

pub(crate) fn attr_string(act: &IMFActivate, key: &GUID) -> Option<String> {
    let mut ptr = windows::core::PWSTR::null();
    let mut len = 0u32;
    unsafe { act.GetAllocatedString(key, &mut ptr, &mut len).ok()? };
    let s = unsafe { ptr.to_string().ok() };
    unsafe { CoTaskMemFree(Some(ptr.0 as *const _)) };
    s
}

#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// Camada temporal de cima do SVC: nenhum quadro depende deste.
    pub droppable: bool,
    pub codec: Codec,
}

pub type OnFrame = Arc<dyn Fn(EncodedFrame) + Send + Sync>;

/// Como abrir um encoder.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub codec: Codec,
    pub size: (u32, u32),
    pub fps: u32,
    pub bitrate: u32,
    /// Tenta duas camadas temporais (SVC).
    pub svc: bool,
    /// Só o encoder por software (testes).
    pub software_only: bool,
}

impl Config {
    pub fn new(codec: Codec, size: (u32, u32), fps: u32, bitrate: u32) -> Self {
        Self { codec, size, fps, bitrate, svc: svc_by_default(codec), software_only: false }
    }
}

#[derive(Default)]
struct Credits {
    available: u32,
    stopped: bool,
}

/// Encoder de hardware (assíncrono).
struct Inner {
    transform: IMFTransform,
    codec_api: ICodecAPI,
    credits: Mutex<Credits>,
    signal: Condvar,
    on_frame: OnFrame,
    codec: Codec,
    /// Cabeçalho de sequência (SPS/PPS/VPS), posto na frente de keyframe que
    /// vier sem ele: quem começa a assistir no meio precisa dele.
    sequence_header: Mutex<Option<Vec<u8>>>,
    failed: AtomicBool,
}

// SAFETY: MFTs assíncronos aceitam chamadas de threads diferentes (é o modelo
// de uso do próprio Media Foundation).
unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

/// Encoder por software (síncrono): roda na thread de quem chama.
struct Soft {
    transform: IMFTransform,
    codec_api: ICodecAPI,
    d3d: D3D,
    /// Cópia do quadro NV12 que a CPU consegue ler.
    staging: ID3D11Texture2D,
    on_frame: OnFrame,
    sequence_header: Option<Vec<u8>>,
    /// O MFT aloca as amostras de saída; senão, alocamos com `out_size`.
    provides_samples: bool,
    out_size: u32,
}

enum Backend {
    Hardware { inner: Arc<Inner>, events: Option<std::thread::JoinHandle<()>>, _manager: IMFDXGIDeviceManager },
    Software(Box<Soft>),
}

pub struct Encoder {
    backend: Backend,
    pub codec: Codec,
    pub name: String,
    pub size: (u32, u32),
    /// Codifica na CPU (sem encoder na placa).
    pub software: bool,
    /// Duas camadas temporais ligadas.
    pub svc: bool,
    frame_index: i64,
    fps: u32,
}

// SAFETY: ver `Inner`; o encoder por software é usado por uma thread por vez.
unsafe impl Send for Encoder {}

fn set_u32(api: &ICodecAPI, key: &GUID, value: u32) -> windows::core::Result<()> {
    let mut v = VARIANT::default();
    // SAFETY: VARIANT zerado; preenche o tipo e o campo correspondente.
    unsafe {
        let inner = &mut *v.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = value;
        api.SetValue(key, &v)
    }
}

fn set_bool(api: &ICodecAPI, key: &GUID) -> windows::core::Result<()> {
    let mut v = VARIANT::default();
    // SAFETY: como em `set_u32`.
    unsafe {
        let inner = &mut *v.Anonymous.Anonymous;
        inner.vt = VT_BOOL;
        inner.Anonymous.boolVal = VARIANT_TRUE;
        api.SetValue(key, &v)
    }
}

/// Baixa latência, sem B-frames (cada quadro sai na hora) e GOP longo:
/// keyframe só sob pedido (SPEC-04 §2). Nem todo encoder aceita tudo; o que
/// ele recusar fica no padrão dele. Devolve se o SVC ficou ligado.
fn configure(api: &ICodecAPI, cfg: &Config) -> bool {
    set_bool(api, &CODECAPI_AVLowLatencyMode).ok();
    set_u32(api, &CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_LowDelayVBR.0 as u32)
        .or_else(|_| set_u32(api, &CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_CBR.0 as u32))
        .ok();
    set_u32(api, &CODECAPI_AVEncCommonMeanBitRate, cfg.bitrate).ok();
    set_u32(api, &CODECAPI_AVEncMPVDefaultBPictureCount, 0).ok();
    set_u32(api, &CODECAPI_AVEncMPVGOPSize, 0xffff).ok();
    // O máximo de camadas vai antes dos tipos de mídia; a contagem, também.
    cfg.svc
        && set_u32(api, &CODECAPI_AVEncVideoMaxTemporalLayers, TEMPORAL_LAYERS).is_ok()
        && set_u32(api, &CODECAPI_AVEncVideoTemporalLayerCount, TEMPORAL_LAYERS).is_ok()
}

/// Tipos de saída (antes) e de entrada (NV12), e começa o stream.
fn set_types(transform: &IMFTransform, cfg: &Config) -> anyhow::Result<()> {
    let frame_size = (u64::from(cfg.size.0) << 32) | u64::from(cfg.size.1);
    let rate = (u64::from(cfg.fps) << 32) | 1;
    unsafe {
        let out = MFCreateMediaType()?;
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out.SetGUID(&MF_MT_SUBTYPE, &subtype(cfg.codec))?;
        out.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate)?;
        out.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
        out.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
        out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)?;
        out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        match cfg.codec {
            Codec::H264 => out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?,
            Codec::Hevc => out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH265VProfile_Main_420_8.0 as u32)?,
            Codec::Av1 => {}
        }
        transform.SetOutputType(0, &out, 0)?;
        let inp = MFCreateMediaType()?;
        inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        inp.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
        inp.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
        inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)?;
        inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        transform.SetInputType(0, &inp, 0)?;
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
    }
    Ok(())
}

impl Encoder {
    /// Primeiro encoder de hardware de `codec` que aceitar a configuração;
    /// sem nenhum, o H.264 por software.
    pub fn new(d3d: &D3D, codec: Codec, size: (u32, u32), fps: u32, bitrate: u32, on_frame: OnFrame) -> anyhow::Result<Self> {
        Self::open(d3d, Config::new(codec, size, fps, bitrate), on_frame)
    }

    pub fn open(d3d: &D3D, cfg: Config, on_frame: OnFrame) -> anyhow::Result<Self> {
        init_mf();
        let mut last = anyhow::anyhow!("nenhum encoder de hardware para {}", cfg.codec.name());
        if !cfg.software_only && !force_software() {
            for act in enum_encoders(cfg.codec) {
                let name = attr_string(&act, &MFT_FRIENDLY_NAME_Attribute).unwrap_or_else(|| "?".into());
                match Self::open_hardware(d3d, &act, &cfg, on_frame.clone()) {
                    Ok(mut e) => {
                        e.name = name;
                        e.log();
                        return Ok(e);
                    }
                    Err(e) => {
                        tracing::debug!("encoder {name} recusou: {e:#}");
                        last = e;
                    }
                }
            }
        }
        if cfg.codec == Codec::H264 {
            match Self::open_software(d3d, &cfg, on_frame) {
                Ok(e) => {
                    e.log();
                    return Ok(e);
                }
                Err(e) => last = e.context("H.264 por software"),
            }
        }
        Err(last)
    }

    fn log(&self) {
        tracing::info!(
            "encoder {} ({}{}{}) {}x{} {} fps",
            self.name,
            self.codec.name(),
            if self.software { ", software" } else { "" },
            if self.svc { ", SVC L1T2" } else { "" },
            self.size.0,
            self.size.1,
            self.fps
        );
    }

    fn open_hardware(d3d: &D3D, act: &IMFActivate, cfg: &Config, on_frame: OnFrame) -> anyhow::Result<Self> {
        let transform: IMFTransform = unsafe { act.ActivateObject()? };
        let attrs = unsafe { transform.GetAttributes()? };
        unsafe {
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
        }
        let manager = dxgi_manager(d3d)?;
        unsafe {
            transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;
        }
        let codec_api: ICodecAPI = transform.cast()?;
        let svc = configure(&codec_api, cfg);
        set_types(&transform, cfg)?;
        let inner = Arc::new(Inner {
            transform,
            codec_api,
            credits: Mutex::default(),
            signal: Condvar::new(),
            on_frame,
            codec: cfg.codec,
            sequence_header: Mutex::new(None),
            failed: AtomicBool::new(false),
        });
        let i = inner.clone();
        let events = std::thread::Builder::new().name("encoder".into()).spawn(move || event_loop(&i))?;
        Ok(Self {
            backend: Backend::Hardware { inner, events: Some(events), _manager: manager },
            codec: cfg.codec,
            name: String::new(),
            size: cfg.size,
            software: false,
            svc,
            frame_index: 0,
            fps: cfg.fps,
        })
    }

    fn open_software(d3d: &D3D, cfg: &Config, on_frame: OnFrame) -> anyhow::Result<Self> {
        let transform = software_h264().context("o Windows não tem o encoder H.264 (edição N sem o Media Feature Pack?)")?;
        let codec_api: ICodecAPI = transform.cast()?;
        let svc = configure(&codec_api, cfg);
        // O mais rápido: a CPU é o gargalo aqui, não a banda.
        set_u32(&codec_api, &CODECAPI_AVEncCommonQualityVsSpeed, 0).ok();
        set_types(&transform, cfg)?;
        let info = unsafe { transform.GetOutputStreamInfo(0)? };
        let provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        // Um keyframe nunca passa de um quadro cru.
        let out_size = info.cbSize.max(cfg.size.0 * cfg.size.1 * 3 / 2);
        let desc = D3D11_TEXTURE2D_DESC {
            Width: cfg.size.0,
            Height: cfg.size.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        unsafe { d3d.device.CreateTexture2D(&desc, None, Some(&mut staging))? };
        let soft = Soft {
            transform,
            codec_api,
            d3d: d3d.clone(),
            staging: staging.context("textura de leitura")?,
            on_frame,
            sequence_header: None,
            provides_samples,
            out_size,
        };
        Ok(Self {
            backend: Backend::Software(Box::new(soft)),
            codec: cfg.codec,
            name: "H.264 por software (Windows)".into(),
            size: cfg.size,
            software: true,
            svc,
            frame_index: 0,
            fps: cfg.fps,
        })
    }

    fn codec_api(&self) -> &ICodecAPI {
        match &self.backend {
            Backend::Hardware { inner, .. } => &inner.codec_api,
            Backend::Software(s) => &s.codec_api,
        }
    }

    /// Codifica a textura NV12 (tamanho do encoder). `keyframe` força um
    /// quadro independente (alguém começou a assistir ou perdeu dados).
    /// A textura não pode ser reescrita até o quadro sair.
    pub fn encode(&mut self, nv12: &ID3D11Texture2D, keyframe: bool) -> anyhow::Result<()> {
        // Tempo em unidades de 100 ns.
        let duration = 10_000_000 / i64::from(self.fps);
        let time = self.frame_index * duration;
        self.frame_index += 1;
        if keyframe {
            set_u32(self.codec_api(), &CODECAPI_AVEncVideoForceKeyFrame, 1).ok();
        }
        match &mut self.backend {
            Backend::Hardware { inner, .. } => {
                if inner.failed.load(Ordering::Relaxed) {
                    bail!("o encoder parou");
                }
                {
                    let credits = inner.credits.lock().unwrap();
                    let (mut credits, _) =
                        inner.signal.wait_timeout_while(credits, INPUT_WAIT, |c| c.available == 0 && !c.stopped).unwrap();
                    if credits.stopped {
                        bail!("o encoder parou");
                    }
                    if credits.available == 0 {
                        bail!("o encoder não pediu entrada a tempo");
                    }
                    credits.available -= 1;
                }
                unsafe {
                    let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, nv12, 0, false)?;
                    if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
                        buffer.SetCurrentLength(b2.GetContiguousLength()?)?;
                    }
                    let sample = sample_with(&buffer, time, duration)?;
                    inner.transform.ProcessInput(0, &sample, 0)?;
                }
            }
            Backend::Software(s) => s.encode(nv12, self.size, time, duration)?,
        }
        Ok(())
    }

    /// Bitrate novo (bits/s), sem reiniciar.
    pub fn set_bitrate(&self, bitrate: u32) {
        if let Err(e) = set_u32(self.codec_api(), &CODECAPI_AVEncCommonMeanBitRate, bitrate) {
            tracing::debug!("encoder não aceitou bitrate novo: {e}");
        }
    }
}

fn sample_with(buffer: &IMFMediaBuffer, time: i64, duration: i64) -> windows::core::Result<IMFSample> {
    unsafe {
        let sample = MFCreateSample()?;
        sample.AddBuffer(buffer)?;
        sample.SetSampleTime(time)?;
        sample.SetSampleDuration(duration)?;
        Ok(sample)
    }
}

impl Soft {
    fn encode(&mut self, nv12: &ID3D11Texture2D, size: (u32, u32), time: i64, duration: i64) -> anyhow::Result<()> {
        let (w, h) = (size.0 as usize, size.1 as usize);
        let len = w * h * 3 / 2;
        let buffer = unsafe { MFCreateMemoryBuffer(len as u32)? };
        unsafe {
            let ctx = &self.d3d.ctx;
            ctx.CopyResource(&self.staging, nv12);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let mut dst = std::ptr::null_mut();
            let copied = buffer.Lock(&mut dst, None, None).map(|()| {
                // Plano Y (h linhas) e, logo depois na textura, o UV (h/2).
                let pitch = mapped.RowPitch as usize;
                for y in 0..h + h / 2 {
                    // SAFETY: a textura mapeada tem h·3/2 linhas de `pitch`
                    // bytes; o buffer, w·h·3/2 bytes.
                    std::ptr::copy_nonoverlapping((mapped.pData as *const u8).add(y * pitch), dst.add(y * w), w);
                }
                let _ = buffer.Unlock();
            });
            ctx.Unmap(&self.staging, 0);
            copied?;
            buffer.SetCurrentLength(len as u32)?;
        }
        let sample = sample_with(&buffer, time, duration)?;
        match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
            Err(e) if e.code() == MF_E_NOTACCEPTING => {
                self.drain()?;
                unsafe { self.transform.ProcessInput(0, &sample, 0)? };
            }
            r => r?,
        }
        self.drain()
    }

    /// Tira tudo o que o encoder tem pronto.
    fn drain(&mut self) -> anyhow::Result<()> {
        loop {
            let given = if self.provides_samples {
                None
            } else {
                let buffer = unsafe { MFCreateMemoryBuffer(self.out_size)? };
                Some(sample_with(&buffer, 0, 0)?)
            };
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(given),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let r = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };
            let [mut out] = buffers;
            // SAFETY: devolve as referências do buffer (a nossa amostra ou a do MFT).
            let sample = unsafe { ManuallyDrop::take(&mut out.pSample) };
            unsafe { ManuallyDrop::drop(&mut out.pEvents) };
            match r {
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => unsafe {
                    let t = self.transform.GetOutputAvailableType(0, 0)?;
                    self.transform.SetOutputType(0, &t, 0)?;
                },
                Err(e) => return Err(e.into()),
                Ok(()) => {
                    let sample = sample.context("encoder sem amostra de saída")?;
                    let frame = to_frame(&sample, Codec::H264, &self.transform, &mut self.sequence_header)?;
                    (self.on_frame)(frame);
                }
            }
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let Backend::Hardware { inner, events, .. } = &mut self.backend else { return };
        inner.credits.lock().unwrap().stopped = true;
        inner.signal.notify_all();
        // Desliga o MFT: o `GetEvent` da thread de eventos volta com erro.
        if let Ok(s) = inner.transform.cast::<IMFShutdown>() {
            unsafe {
                let _ = s.Shutdown();
            }
        }
        if let Some(t) = events.take() {
            let _ = t.join();
        }
    }
}

fn event_loop(inner: &Inner) {
    init_mf();
    let Ok(generator) = inner.transform.cast::<IMFMediaEventGenerator>() else {
        inner.failed.store(true, Ordering::Relaxed);
        return;
    };
    loop {
        let event = match unsafe { generator.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)) } {
            Ok(e) => e,
            Err(_) => break, // desligado
        };
        let kind = unsafe { event.GetType() }.unwrap_or(0);
        if kind == METransformNeedInput.0 as u32 {
            inner.credits.lock().unwrap().available += 1;
            inner.signal.notify_all();
        } else if kind == METransformHaveOutput.0 as u32 {
            if let Err(e) = take_output(inner) {
                tracing::warn!("encoder: {e:#}");
            }
        } else if kind == METransformDrainComplete.0 as u32 {
            break;
        }
    }
    let mut c = inner.credits.lock().unwrap();
    c.stopped = true;
    inner.signal.notify_all();
}

fn take_output(inner: &Inner) -> anyhow::Result<()> {
    let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
        dwStreamID: 0,
        pSample: ManuallyDrop::new(None),
        dwStatus: 0,
        pEvents: ManuallyDrop::new(None),
    }];
    let mut status = 0u32;
    let r = unsafe { inner.transform.ProcessOutput(0, &mut buffers, &mut status) };
    let [mut out] = buffers;
    // SAFETY: devolve as referências que o MFT pôs no buffer.
    let sample = unsafe { ManuallyDrop::take(&mut out.pSample) };
    unsafe { ManuallyDrop::drop(&mut out.pEvents) };
    if let Err(e) = r {
        if e.code() == MF_E_TRANSFORM_STREAM_CHANGE {
            // O encoder mudou o tipo de saída (ex.: cabeçalho novo): aceita.
            unsafe {
                let t = inner.transform.GetOutputAvailableType(0, 0)?;
                inner.transform.SetOutputType(0, &t, 0)?;
            }
            return Ok(());
        }
        return Err(e.into());
    }
    let sample = sample.context("encoder sem amostra de saída")?;
    let frame = to_frame(&sample, inner.codec, &inner.transform, &mut inner.sequence_header.lock().unwrap())?;
    (inner.on_frame)(frame);
    Ok(())
}

/// Bitstream da amostra de saída, com o cabeçalho de sequência na frente do
/// keyframe que vier sem ele e a camada temporal lida do próprio quadro.
fn to_frame(sample: &IMFSample, codec: Codec, transform: &IMFTransform, header: &mut Option<Vec<u8>>) -> anyhow::Result<EncodedFrame> {
    let keyframe = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.unwrap_or(0) != 0;
    let buffer = unsafe { sample.ConvertToContiguousBuffer()? };
    let mut data = Vec::new();
    unsafe {
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut ptr, None, Some(&mut len))?;
        data.extend_from_slice(std::slice::from_raw_parts(ptr, len as usize));
        buffer.Unlock()?;
    }
    if keyframe && !has_parameter_sets(&data, codec) {
        if header.is_none() {
            *header = sequence_header(transform);
        }
        if let Some(h) = header.as_ref() {
            data.splice(0..0, h.iter().copied());
        }
    }
    let droppable = !keyframe && temporal_id(&data, codec).is_some_and(|t| t > 0);
    Ok(EncodedFrame { data, keyframe, droppable, codec })
}

/// O keyframe já traz os conjuntos de parâmetros (SPS no H.264, VPS no
/// HEVC)? O AV1 sempre traz o cabeçalho de sequência no keyframe.
fn has_parameter_sets(data: &[u8], codec: Codec) -> bool {
    let wanted = match codec {
        Codec::H264 => |h: u8| h & 0x1f == 7,
        Codec::Hevc => |h: u8| (h >> 1) & 0x3f == 32,
        Codec::Av1 => return true,
    };
    data.windows(4).any(|w| w[..3] == [0, 0, 1] && wanted(w[3]))
}

fn sequence_header(transform: &IMFTransform) -> Option<Vec<u8>> {
    unsafe {
        let t = transform.GetOutputCurrentType(0).ok()?;
        let len = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok()?;
        let mut blob = vec![0u8; len as usize];
        t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None).ok()?;
        (!blob.is_empty()).then_some(blob)
    }
}

/// Camada temporal de um quadro, lida do bitstream: no HEVC, do cabeçalho da
/// primeira NAL de imagem; no AV1, da extensão do cabeçalho da OBU de quadro.
/// `None` se o bitstream não diz (H.264 sem SVC, quadro sem imagem).
pub fn temporal_id(data: &[u8], codec: Codec) -> Option<u8> {
    match codec {
        Codec::Hevc => {
            let mut i = 0;
            while i + 4 < data.len() {
                if data[i..i + 3] == [0, 0, 1] {
                    // NALs de 0 a 31 são de imagem.
                    if (data[i + 3] >> 1) & 0x3f < 32 {
                        return Some((data[i + 4] & 0x07).saturating_sub(1));
                    }
                    i += 3;
                } else {
                    i += 1;
                }
            }
            None
        }
        Codec::Av1 => {
            let mut i = 0;
            while i < data.len() {
                let h = data[i];
                let kind = (h >> 3) & 0x0f;
                let mut j = i + 1;
                let tid = if h & 0x04 != 0 {
                    let ext = *data.get(j)?;
                    j += 1;
                    ext >> 5
                } else {
                    0
                };
                let size = if h & 0x02 != 0 {
                    let (v, n) = leb128(data.get(j..)?)?;
                    j += n;
                    v
                } else {
                    data.len() - j
                };
                // OBU_FRAME_HEADER e OBU_FRAME.
                if kind == 3 || kind == 6 {
                    return Some(tid);
                }
                i = j.checked_add(size)?;
            }
            None
        }
        Codec::H264 => None,
    }
}

fn leb128(data: &[u8]) -> Option<(usize, usize)> {
    let mut v = 0usize;
    for (i, b) in data.iter().take(8).enumerate() {
        v |= usize::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporal_id_from_the_bitstream() {
        // HEVC: VPS (32) sem importância, depois uma fatia TRAIL_R (1) com
        // nuh_temporal_id_plus1 = 2.
        let hevc = [0, 0, 0, 1, 0x40, 0x01, 0xaa, 0, 0, 1, 0x02, 0x02, 0xbb];
        assert_eq!(temporal_id(&hevc, Codec::Hevc), Some(1));
        assert_eq!(temporal_id(&[0, 0, 1, 0x26, 0x01, 0xcc], Codec::Hevc), Some(0), "IDR na base");
        // AV1: delimitador temporal (2), depois OBU_FRAME (6) com extensão
        // temporal_id = 1, tamanho 2.
        let av1 = [0x12, 0x00, 0x36, 0x20, 0x02, 0xde, 0xad];
        assert_eq!(temporal_id(&av1, Codec::Av1), Some(1));
        // Sem extensão: camada 0.
        assert_eq!(temporal_id(&[0x12, 0x00, 0x32, 0x01, 0xff], Codec::Av1), Some(0));
        // Tamanho que passa do fim não entra em pânico.
        assert_eq!(temporal_id(&[0x12, 0x7f], Codec::Av1), None);
        assert_eq!(temporal_id(&[0, 0, 1, 0x65, 0x88], Codec::H264), None);
    }
}

/// Encoder por software e SVC temporal nesta máquina
/// (`cargo test -p lerri-media -- --ignored --nocapture encoder_modes`, com as
/// DLLs do FFmpeg no PATH).
#[cfg(test)]
mod hw_tests {
    use super::*;
    use crate::video::d3d::Colors;
    use crate::video::ffmpeg::Decoder;
    use windows::Win32::Graphics::Direct3D11::D3D11_BIND_RENDER_TARGET;

    /// Quadros sintéticos que mudam a cada passo, já em NV12 na GPU.
    fn frames(d3d: &D3D, size: (u32, u32), n: usize) -> Vec<ID3D11Texture2D> {
        let bgra = d3d.texture(size.0, size.1, windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_RENDER_TARGET).unwrap();
        let mut conv = d3d.converter(size, size, Colors::RgbToYuv).unwrap();
        (0..n)
            .map(|i| {
                let px: Vec<u8> = (0..size.0 * size.1)
                    .flat_map(|p| {
                        let (x, y) = (p % size.0, p / size.0);
                        [(x + i as u32 * 7) as u8, (y * 2) as u8, ((x ^ y) + i as u32 * 3) as u8, 255]
                    })
                    .collect();
                unsafe { d3d.ctx.UpdateSubresource(&bgra, 0, None, px.as_ptr().cast(), size.0 * 4, 0) };
                let nv12 = d3d.texture(size.0, size.1, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET).unwrap();
                conv.convert(&bgra, 0, &nv12).unwrap();
                nv12
            })
            .collect()
    }

    fn run(d3d: &D3D, cfg: Config, input: &[ID3D11Texture2D]) -> (Encoder, Vec<EncodedFrame>) {
        let out: Arc<Mutex<Vec<EncodedFrame>>> = Arc::default();
        let o = out.clone();
        let mut e = Encoder::open(d3d, cfg, Arc::new(move |f| o.lock().unwrap().push(f))).unwrap();
        for (i, t) in input.iter().enumerate() {
            e.encode(t, i == 0 || i == input.len() / 2).unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(300));
        let frames = std::mem::take(&mut *out.lock().unwrap());
        (e, frames)
    }

    fn decode_all(d3d: &D3D, codec: Codec, frames: &[&EncodedFrame]) -> usize {
        let mut dec = Decoder::new(codec, &d3d.device).unwrap();
        frames.iter().filter(|f| dec.decode(&f.data).expect("decodificar").is_some()).count()
    }

    #[test]
    #[ignore = "usa a GPU, o Media Foundation e o FFmpeg desta máquina"]
    fn encoder_modes() {
        let d3d = D3D::new().unwrap();
        let size = (1280, 720);
        let input = frames(&d3d, size, 40);

        // Software: o H.264 do Windows, quadro a quadro na CPU.
        let mut cfg = Config::new(Codec::H264, size, 30, 2_000_000);
        cfg.software_only = true;
        let t = std::time::Instant::now();
        let (e, out) = run(&d3d, cfg, &input);
        println!("software: {} | {} quadros em {:?}, {} keyframes", e.name, out.len(), t.elapsed(), out.iter().filter(|f| f.keyframe).count());
        assert!(e.software);
        assert_eq!(out.len(), input.len(), "um quadro de saída por entrada");
        assert!(out[0].keyframe && out[input.len() / 2].keyframe, "keyframe no começo e sob pedido");
        assert_eq!(decode_all(&d3d, Codec::H264, &out.iter().collect::<Vec<_>>()), input.len());

        // SVC temporal em cada codec de hardware: se o encoder aceitar, a
        // metade descartável sai marcada e só a base ainda decodifica inteira.
        for codec in Codec::ALL.into_iter().filter(|c| has_hardware(*c)) {
            let mut cfg = Config::new(codec, size, 60, 4_000_000);
            cfg.svc = true;
            let (e, out) = run(&d3d, cfg, &input);
            let drop = out.iter().filter(|f| f.droppable).count();
            let tids: Vec<Option<u8>> = out.iter().take(8).map(|f| temporal_id(&f.data, codec)).collect();
            println!("{} ({}): SVC {} | {} quadros, {drop} descartáveis, camadas {tids:?}", codec.name(), e.name, e.svc, out.len());
            if drop > 0 {
                let base: Vec<&EncodedFrame> = out.iter().filter(|f| !f.droppable).collect();
                let decoded = decode_all(&d3d, codec, &base);
                println!("  só a base: {decoded} de {} decodificados", base.len());
                assert_eq!(decoded, base.len());
            }
        }
    }
}
