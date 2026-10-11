//! Decode de vídeo por hardware (D3D11VA) via FFmpeg (D-03).
//!
//! As DLLs do FFmpeg (LGPL, só `avcodec` e `avutil` e suas dependências)
//! são carregadas em tempo de execução: o app abre sem elas e só a recepção
//! de vídeo fica indisponível. Procura ao lado do `.exe`, em `ffmpeg\` ao
//! lado do `.exe` e no `PATH`. A versão é conferida antes de usar, porque os
//! tipos em `ffi.rs` são da versão 9 (libavcodec 63, libavutil 61).

use std::ffi::{c_char, c_int, c_uint, c_void};
use std::path::PathBuf;
use std::ptr;
use std::sync::OnceLock;

use anyhow::{Context, bail};
use lerri_core::media::Codec;
use libloading::Library;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::core::{GUID, Interface};

use super::ffi::{self, AVBufferRef, AVCodec, AVCodecContext, AVCodecID, AVFrame, AVHWDeviceType, AVPacket, AVPixelFormat};

const AVUTIL_DLL: &str = "avutil-61.dll";
const AVCODEC_DLL: &str = "avcodec-63.dll";
const AVERROR_EAGAIN: c_int = -11;
const AV_CODEC_FLAG_LOW_DELAY: c_int = 1 << 19;
const AV_LOG_FATAL: c_int = 8;

macro_rules! table {
    ($($lib:ident { $($name:ident: fn($($arg:ty),*) $(-> $ret:ty)?;)* })*) => {
        #[allow(non_snake_case)]
        pub struct Ffmpeg {
            _libs: Vec<Library>,
            $($($name: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)*)*
        }

        impl Ffmpeg {
            fn load(avutil: Library, avcodec: Library) -> Result<Self, String> {
                // SAFETY: os tipos das funções seguem os headers da versão conferida em `open`.
                unsafe {
                    let f = Self {
                        $($($name: *table!(@lib $lib, avutil, avcodec)
                            .get::<unsafe extern "C" fn($($arg),*) $(-> $ret)?>(concat!(stringify!($name), "\0").as_bytes())
                            .map_err(|e| format!("{}: {e}", stringify!($name)))?,)*)*
                        _libs: Vec::new(),
                    };
                    Ok(Self { _libs: vec![avutil, avcodec], ..f })
                }
            }
        }
    };
    (@lib avutil, $u:ident, $c:ident) => { $u };
    (@lib avcodec, $u:ident, $c:ident) => { $c };
}

table! {
    avutil {
        avutil_version: fn() -> c_uint;
        av_hwdevice_ctx_alloc: fn(AVHWDeviceType) -> *mut AVBufferRef;
        av_hwdevice_ctx_init: fn(*mut AVBufferRef) -> c_int;
        av_buffer_ref: fn(*const AVBufferRef) -> *mut AVBufferRef;
        av_buffer_unref: fn(*mut *mut AVBufferRef);
        av_frame_alloc: fn() -> *mut AVFrame;
        av_frame_free: fn(*mut *mut AVFrame);
        av_frame_unref: fn(*mut AVFrame);
        av_strerror: fn(c_int, *mut c_char, usize) -> c_int;
        av_log_set_level: fn(c_int);
    }
    avcodec {
        avcodec_version: fn() -> c_uint;
        avcodec_find_decoder: fn(AVCodecID) -> *const AVCodec;
        avcodec_alloc_context3: fn(*const AVCodec) -> *mut AVCodecContext;
        avcodec_free_context: fn(*mut *mut AVCodecContext);
        avcodec_open2: fn(*mut AVCodecContext, *const AVCodec, *mut c_void) -> c_int;
        avcodec_send_packet: fn(*mut AVCodecContext, *const AVPacket) -> c_int;
        avcodec_receive_frame: fn(*mut AVCodecContext, *mut AVFrame) -> c_int;
        av_packet_alloc: fn() -> *mut AVPacket;
        av_packet_free: fn(*mut *mut AVPacket);
        av_new_packet: fn(*mut AVPacket, c_int) -> c_int;
        av_packet_unref: fn(*mut AVPacket);
    }
}

/// Pastas onde as DLLs podem estar, em ordem.
fn search_dirs() -> Vec<Option<PathBuf>> {
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(PathBuf::from));
    let mut dirs = Vec::new();
    if let Some(d) = &exe_dir {
        dirs.push(Some(d.clone()));
        dirs.push(Some(d.join("ffmpeg")));
    }
    dirs.push(None); // busca padrão do Windows (inclui o PATH)
    dirs
}

fn load_from(dir: Option<&PathBuf>, name: &str) -> Result<Library, String> {
    use libloading::os::windows::{LOAD_WITH_ALTERED_SEARCH_PATH, Library as WinLib};
    let r = match dir {
        // Caminho completo + busca alterada: as dependências vêm da mesma pasta.
        Some(d) => unsafe { WinLib::load_with_flags(d.join(name), LOAD_WITH_ALTERED_SEARCH_PATH) },
        None => unsafe { WinLib::new(name) },
    };
    r.map(Library::from).map_err(|e| e.to_string())
}

/// O FFmpeg carregado, ou por que não foi possível.
pub fn ffmpeg() -> Result<&'static Ffmpeg, &'static str> {
    static FF: OnceLock<Result<Ffmpeg, String>> = OnceLock::new();
    FF.get_or_init(|| {
        let mut last = String::from("DLLs do FFmpeg não encontradas");
        for dir in search_dirs() {
            let found = load_from(dir.as_ref(), AVUTIL_DLL)
                .and_then(|u| Ok((u, load_from(dir.as_ref(), AVCODEC_DLL)?)))
                .and_then(|(u, c)| Ffmpeg::load(u, c));
            match found {
                Ok(ff) => {
                    // SAFETY: funções sem argumentos.
                    let (u, c) = unsafe { ((ff.avutil_version)() >> 16, (ff.avcodec_version)() >> 16) };
                    if u != ffi::LIBAVUTIL_VERSION_MAJOR || c != ffi::LIBAVCODEC_VERSION_MAJOR {
                        last = format!("FFmpeg incompatível (avutil {u}, avcodec {c}; esperado 61 e 63)");
                        continue;
                    }
                    // Os erros voltam pela API; o log do FFmpeg iria para o stderr.
                    unsafe { (ff.av_log_set_level)(AV_LOG_FATAL) };
                    tracing::info!("FFmpeg carregado ({})", dir.map_or("PATH".into(), |d| d.display().to_string()));
                    return Ok(ff);
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    })
    .as_ref()
    .map_err(String::as_str)
}

// Perfis DXVA (dxva.h).
const PROFILE_H264: GUID = GUID::from_u128(0x1b81be68_a0c7_11d3_b984_00c04f2e73c5);
const PROFILE_HEVC: GUID = GUID::from_u128(0x5b11d51b_2f4c_4452_bcc3_09f2a1160cc0);
const PROFILE_AV1: GUID = GUID::from_u128(0xb8be4ccb_cf53_46ba_8d59_d6b8a6da5d2a);

/// Codecs que esta máquina decodifica na GPU (FFmpeg presente + perfil
/// D3D11VA disponível).
pub fn decode_caps(device: &ID3D11Device) -> Vec<Codec> {
    if ffmpeg().is_err() {
        return Vec::new();
    }
    let Ok(video) = device.cast::<windows::Win32::Graphics::Direct3D11::ID3D11VideoDevice>() else { return Vec::new() };
    let n = unsafe { video.GetVideoDecoderProfileCount() };
    let profiles: Vec<GUID> = (0..n).filter_map(|i| unsafe { video.GetVideoDecoderProfile(i) }.ok()).collect();
    [(Codec::Av1, PROFILE_AV1), (Codec::Hevc, PROFILE_HEVC), (Codec::H264, PROFILE_H264)]
        .into_iter()
        .filter(|(_, p)| profiles.contains(p))
        .map(|(c, _)| c)
        .collect()
}

fn check(ff: &Ffmpeg, ret: c_int, what: &str) -> anyhow::Result<c_int> {
    if ret < 0 {
        let mut buf = [0 as c_char; 128];
        // SAFETY: buffer com o tamanho informado; o FFmpeg termina em zero.
        let msg = unsafe {
            (ff.av_strerror)(ret, buf.as_mut_ptr(), buf.len());
            std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
        };
        bail!("{what}: {msg} ({ret})");
    }
    Ok(ret)
}

unsafe extern "C" fn pick_d3d11(_: *mut AVCodecContext, fmts: *const AVPixelFormat) -> AVPixelFormat {
    let mut p = fmts;
    // SAFETY: lista terminada em AV_PIX_FMT_NONE, do próprio FFmpeg.
    unsafe {
        while *p != AVPixelFormat::AV_PIX_FMT_NONE {
            if *p == AVPixelFormat::AV_PIX_FMT_D3D11 {
                return *p;
            }
            p = p.add(1);
        }
    }
    // Sem D3D11 o decode falha em vez de cair para a CPU.
    AVPixelFormat::AV_PIX_FMT_NONE
}

/// Quadro decodificado: fatia `index` de uma textura (array) do pool do
/// FFmpeg. Vale até a próxima chamada a [`Decoder::decode`].
pub struct Decoded {
    pub texture: ID3D11Texture2D,
    pub index: u32,
    pub width: u32,
    pub height: u32,
}

pub struct Decoder {
    ff: &'static Ffmpeg,
    ctx: *mut AVCodecContext,
    pkt: *mut AVPacket,
    frame: *mut AVFrame,
    pub codec: Codec,
}

// SAFETY: o contexto é usado por uma thread de cada vez.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new(codec: Codec, device: &ID3D11Device) -> anyhow::Result<Self> {
        let ff = ffmpeg().map_err(anyhow::Error::msg)?;
        let id = match codec {
            Codec::H264 => AVCodecID::AV_CODEC_ID_H264,
            Codec::Hevc => AVCodecID::AV_CODEC_ID_HEVC,
            Codec::Av1 => AVCodecID::AV_CODEC_ID_AV1,
        };
        // SAFETY: segue a API do FFmpeg; ponteiros checados.
        unsafe {
            let dec = (ff.avcodec_find_decoder)(id);
            if dec.is_null() {
                bail!("este FFmpeg não tem decoder {}", codec.name());
            }
            let mut ctx = (ff.avcodec_alloc_context3)(dec);
            if ctx.is_null() {
                bail!("avcodec_alloc_context3");
            }
            let mut hw = (ff.av_hwdevice_ctx_alloc)(AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA);
            let devctx = (*hw).data as *mut ffi::AVHWDeviceContext;
            let d3d = (*devctx).hwctx as *mut ffi::AVD3D11VADeviceContext;
            // O FFmpeg libera essa referência ao destruir o contexto.
            (*d3d).device = device.clone().into_raw();
            if let Err(e) = check(ff, (ff.av_hwdevice_ctx_init)(hw), "iniciar D3D11VA") {
                (ff.av_buffer_unref)(&mut hw);
                (ff.avcodec_free_context)(&mut ctx);
                return Err(e);
            }
            (*ctx).hw_device_ctx = (ff.av_buffer_ref)(hw);
            (ff.av_buffer_unref)(&mut hw);
            (*ctx).get_format = Some(pick_d3d11);
            (*ctx).flags |= AV_CODEC_FLAG_LOW_DELAY;
            (*ctx).thread_count = 1;
            if let Err(e) = check(ff, (ff.avcodec_open2)(ctx, dec, ptr::null_mut()), "abrir decoder") {
                (ff.avcodec_free_context)(&mut ctx);
                return Err(e);
            }
            Ok(Self { ff, ctx, pkt: (ff.av_packet_alloc)(), frame: (ff.av_frame_alloc)(), codec })
        }
    }

    /// Decodifica um quadro inteiro (unidade de acesso). Com `low delay`, o
    /// quadro sai na mesma chamada; `None` se o decoder ainda não tem imagem
    /// (ex.: começou sem keyframe).
    pub fn decode(&mut self, data: &[u8]) -> anyhow::Result<Option<Decoded>> {
        let ff = self.ff;
        // SAFETY: segue a API do FFmpeg; o pacote tem `data.len()` bytes.
        unsafe {
            (ff.av_frame_unref)(self.frame);
            check(ff, (ff.av_new_packet)(self.pkt, data.len() as c_int), "alocar pacote")?;
            ptr::copy_nonoverlapping(data.as_ptr(), (*self.pkt).data, data.len());
            let sent = (ff.avcodec_send_packet)(self.ctx, self.pkt);
            (ff.av_packet_unref)(self.pkt);
            check(ff, sent, "decodificar")?;
            let r = (ff.avcodec_receive_frame)(self.ctx, self.frame);
            if r == AVERROR_EAGAIN {
                return Ok(None);
            }
            check(ff, r, "receber quadro")?;
            if (*self.frame).format != AVPixelFormat::AV_PIX_FMT_D3D11 as c_int {
                bail!("o decoder entregou quadro fora da GPU");
            }
            let raw = (*self.frame).data[0] as *mut c_void;
            let texture = ID3D11Texture2D::from_raw_borrowed(&raw).context("textura nula")?.clone();
            Ok(Some(Decoded {
                texture,
                index: (*self.frame).data[1] as usize as u32,
                width: (*self.frame).width as u32,
                height: (*self.frame).height as u32,
            }))
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let ff = self.ff;
        // SAFETY: alocados em `new`, liberados uma vez.
        unsafe {
            (ff.av_frame_free)(&mut self.frame);
            (ff.av_packet_free)(&mut self.pkt);
            (ff.avcodec_free_context)(&mut self.ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A CI roda com as DLLs de `scripts/ffmpeg-min.sh` no PATH.
    #[test]
    #[ignore = "precisa das DLLs do FFmpeg"]
    fn dlls_tem_os_tres_decoders() {
        let ff = ffmpeg().unwrap();
        for id in [AVCodecID::AV_CODEC_ID_H264, AVCodecID::AV_CODEC_ID_HEVC, AVCodecID::AV_CODEC_ID_AV1] {
            // SAFETY: só consulta a tabela de decoders.
            assert!(!unsafe { (ff.avcodec_find_decoder)(id) }.is_null(), "sem decoder {id:?}");
        }
    }
}
