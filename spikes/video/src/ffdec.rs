//! Decode por hardware (D3D11VA) via FFmpeg, usando o dispositivo D3D11 do
//! app para que a textura decodificada já esteja na GPU certa.

use std::ffi::CString;
use std::ptr;

use anyhow::{Context, bail};
use ffmpeg_sys_next as ff;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::core::Interface;

pub struct Decoder {
    fmt: *mut ff::AVFormatContext,
    codec: *mut ff::AVCodecContext,
    stream: i32,
    pkt: *mut ff::AVPacket,
    frame: *mut ff::AVFrame,
    pub width: u32,
    pub height: u32,
    pub codec_name: String,
}

unsafe impl Send for Decoder {}

fn check(ret: i32, what: &str) -> anyhow::Result<i32> {
    if ret < 0 {
        let mut buf = [0i8; 128];
        unsafe { ff::av_strerror(ret, buf.as_mut_ptr(), buf.len()) };
        let msg = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_string_lossy();
        bail!("{what}: {msg} ({ret})");
    }
    Ok(ret)
}

unsafe extern "C" fn pick_d3d11(_: *mut ff::AVCodecContext, fmts: *const ff::AVPixelFormat) -> ff::AVPixelFormat {
    let mut p = fmts;
    unsafe {
        while *p != ff::AVPixelFormat::AV_PIX_FMT_NONE {
            if *p == ff::AVPixelFormat::AV_PIX_FMT_D3D11 {
                return *p;
            }
            p = p.add(1);
        }
    }
    // Sem D3D11 → falha explícita em vez de cair para decode na CPU.
    ff::AVPixelFormat::AV_PIX_FMT_NONE
}

impl Decoder {
    pub fn open(path: &str, device: &ID3D11Device) -> anyhow::Result<Self> {
        let cpath = CString::new(path)?;
        let mut fmt = ptr::null_mut();
        unsafe {
            check(ff::avformat_open_input(&mut fmt, cpath.as_ptr(), ptr::null(), ptr::null_mut()), "abrir arquivo")?;
            check(ff::avformat_find_stream_info(fmt, ptr::null_mut()), "ler streams")?;
            let mut dec: *const ff::AVCodec = ptr::null();
            let stream = check(
                ff::av_find_best_stream(fmt, ff::AVMediaType::AVMEDIA_TYPE_VIDEO, -1, -1, &mut dec, 0),
                "achar stream de vídeo",
            )?;
            let codec = ff::avcodec_alloc_context3(dec);
            let par = (*(*(*fmt).streams.add(stream as usize))).codecpar;
            check(ff::avcodec_parameters_to_context(codec, par), "parâmetros do codec")?;

            // Contexto D3D11VA apontando para o NOSSO dispositivo.
            let hw = ff::av_hwdevice_ctx_alloc(ff::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA);
            let devctx = (*hw).data as *mut ff::AVHWDeviceContext;
            let d3d = (*devctx).hwctx as *mut ff::AVD3D11VADeviceContext;
            // O FFmpeg libera essa referência ao destruir o contexto.
            (*d3d).device = device.clone().into_raw() as *mut _;
            check(ff::av_hwdevice_ctx_init(hw), "iniciar D3D11VA")?;
            (*codec).hw_device_ctx = ff::av_buffer_ref(hw);
            let mut hw = hw;
            ff::av_buffer_unref(&mut hw);
            (*codec).get_format = Some(pick_d3d11);
            check(ff::avcodec_open2(codec, dec, ptr::null_mut()), "abrir decoder")?;

            let codec_name = std::ffi::CStr::from_ptr((*dec).name).to_string_lossy().into_owned();
            Ok(Self {
                fmt,
                width: (*codec).width as u32,
                height: (*codec).height as u32,
                codec,
                stream,
                pkt: ff::av_packet_alloc(),
                frame: ff::av_frame_alloc(),
                codec_name,
            })
        }
    }

    /// Próximo quadro decodificado: textura (array) do pool do FFmpeg e o
    /// índice da fatia. Reinicia o arquivo ao chegar no fim (laço infinito).
    /// A textura vale até a próxima chamada.
    pub fn next(&mut self) -> anyhow::Result<(ID3D11Texture2D, u32)> {
        unsafe {
            ff::av_frame_unref(self.frame);
            loop {
                let r = ff::avcodec_receive_frame(self.codec, self.frame);
                if r >= 0 {
                    if (*self.frame).format != ff::AVPixelFormat::AV_PIX_FMT_D3D11 as i32 {
                        bail!("decoder entregou quadro fora da GPU (formato {})", (*self.frame).format);
                    }
                    let raw = (*self.frame).data[0] as *mut std::ffi::c_void;
                    let index = (*self.frame).data[1] as usize as u32;
                    let tex = ID3D11Texture2D::from_raw_borrowed(&raw).context("textura nula")?.clone();
                    return Ok((tex, index));
                }
                if r == ff::AVERROR_EOF {
                    // Fim do arquivo: volta ao início.
                    check(ff::av_seek_frame(self.fmt, self.stream, 0, ff::AVSEEK_FLAG_BACKWARD), "seek")?;
                    ff::avcodec_flush_buffers(self.codec);
                    continue;
                }
                if r != ff::AVERROR(ff::EAGAIN) {
                    check(r, "decodificar")?;
                }
                // Precisa de mais dados.
                let r = ff::av_read_frame(self.fmt, self.pkt);
                if r == ff::AVERROR_EOF {
                    check(ff::avcodec_send_packet(self.codec, ptr::null()), "flush")?;
                    continue;
                }
                check(r, "ler pacote")?;
                if (*self.pkt).stream_index == self.stream {
                    check(ff::avcodec_send_packet(self.codec, self.pkt), "enviar pacote")?;
                }
                ff::av_packet_unref(self.pkt);
            }
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            ff::av_frame_free(&mut self.frame);
            ff::av_packet_free(&mut self.pkt);
            ff::avcodec_free_context(&mut self.codec);
            ff::avformat_close_input(&mut self.fmt);
        }
    }
}
