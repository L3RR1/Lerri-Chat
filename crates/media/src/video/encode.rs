//! Encode de vídeo na GPU pelos MFTs de hardware do Media Foundation (AMF,
//! NVENC, Quick Sync), SPEC-04 §2.
//!
//! Os MFTs de hardware são assíncronos: avisam por eventos quando querem
//! entrada e quando têm saída. Uma thread fica no `GetEvent`; "quer entrada"
//! vira crédito para [`Encoder::encode`], e "tem saída" vira um
//! [`EncodedFrame`] entregue ao callback. A entrada é uma textura NV12 do
//! nosso dispositivo D3D11: nada passa pela CPU até o bitstream.

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Once};
use std::time::Duration;

use anyhow::{Context, bail};
use lerri_core::media::Codec;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::Foundation::VARIANT_TRUE;
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};
use windows::core::{GUID, Interface};

use super::d3d::D3D;

/// Tempo máximo esperando o encoder pedir entrada.
const INPUT_WAIT: Duration = Duration::from_millis(500);

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

/// Gerenciador que dá ao Media Foundation o nosso dispositivo D3D11.
pub(crate) fn dxgi_manager(d3d: &D3D) -> anyhow::Result<IMFDXGIDeviceManager> {
    let mut token = 0u32;
    let mut manager = None;
    unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager)? };
    let manager = manager.context("MFCreateDXGIDeviceManager")?;
    unsafe { manager.ResetDevice(&d3d.device, token)? };
    Ok(manager)
}

/// Codecs que esta máquina codifica em hardware, do melhor para o pior.
pub fn encode_caps() -> Vec<Codec> {
    Codec::ALL.into_iter().filter(|c| !enum_encoders(*c).is_empty()).collect()
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
    pub codec: Codec,
}

pub type OnFrame = Arc<dyn Fn(EncodedFrame) + Send + Sync>;

#[derive(Default)]
struct Credits {
    available: u32,
    stopped: bool,
}

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

pub struct Encoder {
    inner: Arc<Inner>,
    events: Option<std::thread::JoinHandle<()>>,
    _manager: IMFDXGIDeviceManager,
    pub codec: Codec,
    pub name: String,
    pub size: (u32, u32),
    frame_index: i64,
    fps: u32,
}

// SAFETY: ver `Inner`.
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

impl Encoder {
    /// Primeiro encoder de hardware de `codec` que aceitar a configuração.
    pub fn new(d3d: &D3D, codec: Codec, size: (u32, u32), fps: u32, bitrate: u32, on_frame: OnFrame) -> anyhow::Result<Self> {
        init_mf();
        let mut last = anyhow::anyhow!("nenhum encoder de hardware para {}", codec.name());
        for act in enum_encoders(codec) {
            let name = attr_string(&act, &MFT_FRIENDLY_NAME_Attribute).unwrap_or_else(|| "?".into());
            match Self::open(d3d, &act, codec, size, fps, bitrate, on_frame.clone()) {
                Ok(mut e) => {
                    e.name = name;
                    tracing::info!("encoder {} ({}) {}x{} {fps} fps {} kbps", e.name, codec.name(), size.0, size.1, bitrate / 1000);
                    return Ok(e);
                }
                Err(e) => {
                    tracing::debug!("encoder {name} recusou: {e:#}");
                    last = e;
                }
            }
        }
        Err(last)
    }

    fn open(
        d3d: &D3D,
        act: &IMFActivate,
        codec: Codec,
        size: (u32, u32),
        fps: u32,
        bitrate: u32,
        on_frame: OnFrame,
    ) -> anyhow::Result<Self> {
        let (transform, codec_api, manager) = {
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
            // Baixa latência, sem B-frames (cada quadro sai na hora) e GOP
            // longo: keyframe só sob pedido (SPEC-04 §2). Nem todo encoder
            // aceita tudo; o que ele recusar fica no padrão dele.
            set_bool(&codec_api, &CODECAPI_AVLowLatencyMode).ok();
            set_u32(&codec_api, &CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_LowDelayVBR.0 as u32)
                .or_else(|_| set_u32(&codec_api, &CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_CBR.0 as u32))
                .ok();
            set_u32(&codec_api, &CODECAPI_AVEncCommonMeanBitRate, bitrate).ok();
            set_u32(&codec_api, &CODECAPI_AVEncMPVDefaultBPictureCount, 0).ok();
            set_u32(&codec_api, &CODECAPI_AVEncMPVGOPSize, 0xffff).ok();

            let frame_size = (u64::from(size.0) << 32) | u64::from(size.1);
            let rate = (u64::from(fps) << 32) | 1;
            let out = unsafe { MFCreateMediaType()? };
            unsafe {
                out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                out.SetGUID(&MF_MT_SUBTYPE, &subtype(codec))?;
                out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
                out.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
                out.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
                out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)?;
                out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
                match codec {
                    Codec::H264 => out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?,
                    Codec::Hevc => out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH265VProfile_Main_420_8.0 as u32)?,
                    Codec::Av1 => {}
                }
                transform.SetOutputType(0, &out, 0)?;
            }
            let inp = unsafe { MFCreateMediaType()? };
            unsafe {
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
            (transform, codec_api, manager)
        };
        let inner = Arc::new(Inner {
            transform,
            codec_api,
            credits: Mutex::default(),
            signal: Condvar::new(),
            on_frame,
            codec,
            sequence_header: Mutex::new(None),
            failed: AtomicBool::new(false),
        });
        let i = inner.clone();
        let events = std::thread::Builder::new()
            .name("encoder".into())
            .spawn(move || event_loop(&i))?;
        Ok(Self { inner, events: Some(events), _manager: manager, codec, name: String::new(), size, frame_index: 0, fps })
    }

    /// Codifica a textura NV12 (tamanho do encoder). `keyframe` força um
    /// quadro independente (alguém começou a assistir ou perdeu dados).
    /// A textura não pode ser reescrita até o quadro sair.
    pub fn encode(&mut self, nv12: &ID3D11Texture2D, keyframe: bool) -> anyhow::Result<()> {
        if self.inner.failed.load(Ordering::Relaxed) {
            bail!("o encoder parou");
        }
        {
            let credits = self.inner.credits.lock().unwrap();
            let (mut credits, _) = self
                .inner
                .signal
                .wait_timeout_while(credits, INPUT_WAIT, |c| c.available == 0 && !c.stopped)
                .unwrap();
            if credits.stopped {
                bail!("o encoder parou");
            }
            if credits.available == 0 {
                bail!("o encoder não pediu entrada a tempo");
            }
            credits.available -= 1;
        }
        unsafe {
            if keyframe {
                set_u32(&self.inner.codec_api, &CODECAPI_AVEncVideoForceKeyFrame, 1).ok();
            }
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, nv12, 0, false)?;
            if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
                buffer.SetCurrentLength(b2.GetContiguousLength()?)?;
            }
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            // Tempo em unidades de 100 ns.
            let duration = 10_000_000 / i64::from(self.fps);
            sample.SetSampleTime(self.frame_index * duration)?;
            sample.SetSampleDuration(duration)?;
            self.frame_index += 1;
            self.inner.transform.ProcessInput(0, &sample, 0)?;
        }
        Ok(())
    }

    /// Bitrate novo (bits/s), sem reiniciar.
    pub fn set_bitrate(&self, bitrate: u32) {
        if let Err(e) = set_u32(&self.inner.codec_api, &CODECAPI_AVEncCommonMeanBitRate, bitrate) {
            tracing::debug!("encoder não aceitou bitrate novo: {e}");
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        self.inner.credits.lock().unwrap().stopped = true;
        self.inner.signal.notify_all();
        // Desliga o MFT: o `GetEvent` da thread de eventos volta com erro.
        if let Ok(s) = self.inner.transform.cast::<IMFShutdown>() {
            unsafe {
                let _ = s.Shutdown();
            }
        }
        if let Some(t) = self.events.take() {
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
    if keyframe && !has_parameter_sets(&data, inner.codec) {
        let mut header = inner.sequence_header.lock().unwrap();
        if header.is_none() {
            *header = sequence_header(&inner.transform);
        }
        if let Some(h) = header.as_ref() {
            data.splice(0..0, h.iter().copied());
        }
    }
    (inner.on_frame)(EncodedFrame { data, keyframe, codec: inner.codec });
    Ok(())
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
