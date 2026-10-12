//! Dispositivo D3D11 do vídeo e o processador de vídeo da GPU, que converte
//! formato de cor e redimensiona sem passar pela CPU (D-02, `spikes/video`).
//!
//! Um dispositivo só é usado pela captura (WGC), pelo encoder (Media
//! Foundation), pelo decoder (FFmpeg D3D11VA) e pela swapchain: as texturas
//! passam de um para o outro sem cópia.

use anyhow::Context;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

#[derive(Clone)]
pub struct D3D {
    pub device: ID3D11Device,
    pub ctx: ID3D11DeviceContext,
    pub video: ID3D11VideoDevice,
    pub vctx: ID3D11VideoContext1,
    pub adapter: String,
}

// O ID3D11Device é free-threaded e o contexto imediato está com proteção
// multithread ligada (o FFmpeg e o Media Foundation também o usam).
unsafe impl Send for D3D {}
unsafe impl Sync for D3D {}

impl D3D {
    /// Dispositivo na GPU com mais memória dedicada (ignora adaptadores de
    /// software e virtuais).
    pub fn new() -> anyhow::Result<Self> {
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
        let mut chosen: Option<(IDXGIAdapter1, usize)> = None;
        for i in 0.. {
            let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else { break };
            let desc = unsafe { adapter.GetDesc1()? };
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                continue;
            }
            if chosen.as_ref().is_none_or(|(_, v)| desc.DedicatedVideoMemory > *v) {
                chosen = Some((adapter, desc.DedicatedVideoMemory));
            }
        }
        let (adapter, _) = chosen.context("nenhuma GPU encontrada")?;
        let name = unsafe { adapter.GetDesc1()? }.Description;
        let adapter_name = String::from_utf16_lossy(&name).trim_end_matches('\0').to_string();
        let (mut device, mut ctx) = (None, None);
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                Default::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )?
        };
        let device = device.context("D3D11CreateDevice sem dispositivo")?;
        let ctx = ctx.context("D3D11CreateDevice sem contexto")?;
        unsafe {
            let _ = ctx.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true);
        }
        Ok(Self { video: device.cast()?, vctx: ctx.cast()?, device, ctx, adapter: adapter_name })
    }

    pub fn texture(&self, width: u32, height: u32, format: DXGI_FORMAT, bind: D3D11_BIND_FLAG) -> anyhow::Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
        tex.context("CreateTexture2D")
    }

    /// Lê a fatia `index` de uma textura para a CPU. Só para testes e
    /// diagnóstico.
    pub fn read_back(&self, tex: &ID3D11Texture2D, index: u32) -> anyhow::Result<(D3D11_TEXTURE2D_DESC, Vec<u8>)> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        let staging_desc = D3D11_TEXTURE2D_DESC {
            ArraySize: 1,
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            ..desc
        };
        let mut staging = None;
        unsafe { self.device.CreateTexture2D(&staging_desc, None, Some(&mut staging))? };
        let staging = staging.context("textura de leitura")?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.ctx.CopySubresourceRegion(&staging, 0, 0, 0, 0, tex, index, None);
            self.ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        }
        let rows = if desc.Format == DXGI_FORMAT_NV12 { desc.Height * 3 / 2 } else { desc.Height } as usize;
        let row = if desc.Format == DXGI_FORMAT_NV12 { desc.Width as usize } else { desc.Width as usize * 4 };
        let mut out = Vec::with_capacity(row * rows);
        for y in 0..rows {
            // SAFETY: a textura mapeada tem `rows` linhas de `RowPitch` bytes.
            let src = unsafe { std::slice::from_raw_parts((mapped.pData as *const u8).add(y * mapped.RowPitch as usize), row) };
            out.extend_from_slice(src);
        }
        unsafe { self.ctx.Unmap(&staging, 0) };
        Ok((desc, out))
    }

    /// Conversor `input` → `output` (tamanhos em pixels) entre os espaços de
    /// cor dados.
    pub fn converter(&self, input: (u32, u32), output: (u32, u32), colors: Colors) -> anyhow::Result<Converter> {
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
            InputWidth: input.0,
            InputHeight: input.1,
            OutputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
            OutputWidth: output.0,
            OutputHeight: output.1,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = unsafe { self.video.CreateVideoProcessorEnumerator(&content)? };
        let processor = unsafe { self.video.CreateVideoProcessor(&enumerator, 0)? };
        let (src_space, dst_space) = match colors {
            Colors::RgbToYuv => (DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709),
            Colors::YuvToRgb => (DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709),
            Colors::YuvToYuv => (DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709),
        };
        unsafe {
            self.vctx.VideoProcessorSetStreamColorSpace1(&processor, 0, src_space);
            self.vctx.VideoProcessorSetOutputColorSpace1(&processor, dst_space);
            // Fundo preto nas barras de letterbox.
            let black = D3D11_VIDEO_COLOR { Anonymous: D3D11_VIDEO_COLOR_0 { RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 } } };
            self.vctx.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);
            // Sem isso alguns drivers repetem o último quadro em vez de processar o novo.
            self.vctx.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
        }
        let mut conv = Converter { d3d: self.clone(), enumerator, processor, outputs: Vec::new(), inputs: Vec::new(), input, output };
        conv.set_rects(input, (0, 0, output.0, output.1), output);
        Ok(conv)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Colors {
    /// Captura (BGRA) → encoder (NV12, BT.709 faixa limitada).
    RgbToYuv,
    /// Decoder (NV12) → tela (BGRA).
    YuvToRgb,
    /// Câmera (NV12) → encoder (NV12), só redimensiona.
    YuvToYuv,
}

fn rect(x: u32, y: u32, w: u32, h: u32) -> RECT {
    RECT { left: x as i32, top: y as i32, right: (x + w) as i32, bottom: (y + h) as i32 }
}

/// Maior retângulo com a proporção de `src` que cabe em `dst`, centralizado.
pub fn fit(src: (u32, u32), dst: (u32, u32)) -> (u32, u32, u32, u32) {
    if src.0 == 0 || src.1 == 0 {
        return (0, 0, dst.0, dst.1);
    }
    let scale = (dst.0 as f64 / src.0 as f64).min(dst.1 as f64 / src.1 as f64);
    let (w, h) = (((src.0 as f64 * scale).round() as u32).max(1), ((src.1 as f64 * scale).round() as u32).max(1));
    ((dst.0 - w.min(dst.0)) / 2, (dst.1 - h.min(dst.1)) / 2, w.min(dst.0), h.min(dst.1))
}

pub struct Converter {
    d3d: D3D,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    outputs: Vec<(usize, ID3D11VideoProcessorOutputView)>,
    inputs: Vec<(usize, u32, ID3D11VideoProcessorInputView)>,
    pub input: (u32, u32),
    pub output: (u32, u32),
}

// Os objetos de vídeo do D3D11 são do dispositivo (free-threaded); cada
// conversor é usado por uma thread de cada vez.
unsafe impl Send for Converter {}

impl Converter {
    /// Área útil da entrada (o pool do decoder tem altura alinhada, ex.: 1080
    /// → 1088) e onde ela vai na saída; o resto da saída fica preto.
    pub fn set_rects(&mut self, src: (u32, u32), dst: (u32, u32, u32, u32), target: (u32, u32)) {
        unsafe {
            let v = &self.d3d.vctx;
            v.VideoProcessorSetStreamSourceRect(&self.processor, 0, true, Some(&rect(0, 0, src.0, src.1)));
            v.VideoProcessorSetStreamDestRect(&self.processor, 0, true, Some(&rect(dst.0, dst.1, dst.2, dst.3)));
            v.VideoProcessorSetOutputTargetRect(&self.processor, true, Some(&rect(0, 0, target.0, target.1)));
        }
    }

    /// Espelha a imagem na horizontal.
    pub fn set_mirror(&mut self, on: bool) {
        unsafe { self.d3d.vctx.VideoProcessorSetStreamMirror(&self.processor, 0, on, on, false) };
    }

    fn output_view(&mut self, tex: &ID3D11Texture2D) -> anyhow::Result<ID3D11VideoProcessorOutputView> {
        let key = tex.as_raw() as usize;
        if let Some((_, v)) = self.outputs.iter().find(|(k, _)| *k == key) {
            return Ok(v.clone());
        }
        let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
        };
        let mut view = None;
        unsafe { self.d3d.video.CreateVideoProcessorOutputView(tex, &self.enumerator, &desc, Some(&mut view))? };
        let view = view.context("CreateVideoProcessorOutputView")?;
        // Swapchains trocam de buffer; poucos alvos distintos por conversor.
        if self.outputs.len() >= 8 {
            self.outputs.remove(0);
        }
        self.outputs.push((key, view.clone()));
        Ok(view)
    }

    /// Esquece as views (texturas de saída recriadas, ex.: `ResizeBuffers`).
    pub fn forget_outputs(&mut self) {
        self.outputs.clear();
    }

    /// Converte a fatia `index` de `src` para `dst`. Não espera a GPU: quem
    /// consome (encoder, `Present`) ordena o trabalho.
    pub fn convert(&mut self, src: &ID3D11Texture2D, index: u32, dst: &ID3D11Texture2D) -> anyhow::Result<()> {
        let out = self.output_view(dst)?;
        let key = src.as_raw() as usize;
        let input = match self.inputs.iter().find(|(k, i, _)| *k == key && *i == index) {
            Some((_, _, v)) => v.clone(),
            None => {
                let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                    FourCC: 0,
                    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: index },
                    },
                };
                let mut view = None;
                unsafe { self.d3d.video.CreateVideoProcessorInputView(src, &self.enumerator, &desc, Some(&mut view)) }
                    .context("CreateVideoProcessorInputView")?;
                let view = view.context("view de entrada")?;
                if self.inputs.len() >= 32 {
                    self.inputs.remove(0);
                }
                self.inputs.push((key, index, view.clone()));
                view
            }
        };
        let streams = [D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(input)),
            ..Default::default()
        }];
        let r = unsafe { self.d3d.vctx.VideoProcessorBlt(&self.processor, &out, 0, &streams) };
        let [mut stream] = streams;
        // SAFETY: devolve a referência que o ManuallyDrop segurou.
        unsafe { std::mem::ManuallyDrop::drop(&mut stream.pInputSurface) };
        r.context("VideoProcessorBlt")
    }
}

#[cfg(test)]
mod tests {
    use super::fit;

    #[test]
    fn letterbox() {
        assert_eq!(fit((1920, 1080), (1920, 1080)), (0, 0, 1920, 1080));
        assert_eq!(fit((1920, 1080), (800, 800)), (0, 175, 800, 450));
        assert_eq!(fit((1080, 1920), (1000, 500)), (359, 0, 281, 500));
        assert_eq!(fit((0, 0), (10, 10)), (0, 0, 10, 10));
    }
}
