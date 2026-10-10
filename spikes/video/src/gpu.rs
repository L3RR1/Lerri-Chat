//! Lado D3D11: dispositivo na mesma GPU do wgpu, texturas RGBA compartilhadas
//! por handle NT e conversão NV12→RGBA pelo processador de vídeo da GPU.

use anyhow::{Context, bail};
use windows::Win32::Foundation::{GENERIC_ALL, HANDLE, LUID};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

pub struct D3D {
    pub device: ID3D11Device,
    pub ctx: ID3D11DeviceContext,
    video: ID3D11VideoDevice,
    vctx: ID3D11VideoContext1,
}

// O ID3D11Device é free-threaded; o contexto imediato só é usado pela thread
// de decode depois de movido para lá.
unsafe impl Send for D3D {}

impl D3D {
    /// Cria o dispositivo no adaptador `luid` (o mesmo do wgpu), ou no
    /// primeiro adaptador de hardware se `None`.
    pub fn new(luid: Option<LUID>) -> anyhow::Result<Self> {
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
        let mut chosen = None;
        for i in 0.. {
            let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else { break };
            let desc = unsafe { adapter.GetDesc1()? };
            let is_sw = desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0;
            let matches = match luid {
                Some(l) => desc.AdapterLuid.LowPart == l.LowPart && desc.AdapterLuid.HighPart == l.HighPart,
                // Sem LUID: a GPU com mais VRAM dedicada (ignora adaptadores virtuais).
                None => !is_sw,
            };
            if matches {
                let vram = desc.DedicatedVideoMemory;
                if chosen.as_ref().is_none_or(|(_, v)| vram > *v) {
                    chosen = Some((adapter, vram));
                }
            }
        }
        let (adapter, _) = chosen.context("adaptador D3D11 não encontrado")?;
        let name = unsafe { adapter.GetDesc1()? }.Description;
        let name = String::from_utf16_lossy(&name).trim_end_matches('\0').to_string();
        eprintln!("D3D11 em: {name}");

        let mut device = None;
        let mut ctx = None;
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
        let device = device.unwrap();
        let ctx = ctx.unwrap();
        // O FFmpeg recomenda proteção multithread quando o dispositivo é compartilhado.
        let _ = unsafe { ctx.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true) };
        Ok(Self { video: device.cast()?, vctx: ctx.cast()?, device, ctx })
    }

    /// Textura RGBA (o único formato que o Slint importa) que o D3D12 (wgpu)
    /// consegue abrir pelo handle NT devolvido.
    pub fn shared_texture(&self, width: u32, height: u32) -> anyhow::Result<(ID3D11Texture2D, HANDLE)> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0) as u32,
        };
        let mut tex = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
        let tex = tex.unwrap();
        let handle = unsafe { tex.cast::<IDXGIResource1>()?.CreateSharedHandle(None, GENERIC_ALL.0, None)? };
        Ok((tex, handle))
    }

    /// Só para verificação (`spike-video dump`): copia a textura RGBA para a
    /// CPU. O caminho de exibição nunca faz isso.
    pub fn read_back(&self, tex: &ID3D11Texture2D) -> anyhow::Result<(u32, u32, Vec<u8>)> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            ..desc
        };
        let mut staging = None;
        unsafe { self.device.CreateTexture2D(&staging_desc, None, Some(&mut staging))? };
        let staging = staging.unwrap();
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.ctx.CopyResource(&staging, tex);
            self.ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        }
        let row = desc.Width as usize * 4;
        let mut out = Vec::with_capacity(row * desc.Height as usize);
        for y in 0..desc.Height as usize {
            let src = unsafe { std::slice::from_raw_parts((mapped.pData as *const u8).add(y * mapped.RowPitch as usize), row) };
            out.extend_from_slice(src);
        }
        unsafe { self.ctx.Unmap(&staging, 0) };
        Ok((desc.Width, desc.Height, out))
    }

    /// Conversor de quadros `width`×`height` para saídas do mesmo tamanho.
    pub fn converter(&self, width: u32, height: u32, outputs: &[ID3D11Texture2D]) -> anyhow::Result<Converter> {
        let mut conv = self.scaling_converter((width, height), (width, height))?;
        for tex in outputs {
            conv.output_view(self, tex)?;
        }
        Ok(conv)
    }

    /// Conversor que também redimensiona (`input` → `output`), na GPU.
    pub fn scaling_converter(&self, input: (u32, u32), output: (u32, u32)) -> anyhow::Result<Converter> {
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
        let rect = |(w, h): (u32, u32)| windows::Win32::Foundation::RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
        unsafe {
            // O pool do decoder tem altura alinhada (ex.: 1080 → 1152); sem o
            // retângulo de origem, o padding entra na imagem e ela achata.
            self.vctx.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&rect(input)));
            self.vctx.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&rect(output)));
            self.vctx.VideoProcessorSetOutputTargetRect(&processor, true, Some(&rect(output)));
            // Vídeo HEVC/H.264 típico: BT.709 faixa limitada → RGB faixa completa.
            self.vctx.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            self.vctx.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
        }
        let mut query = None;
        unsafe { self.device.CreateQuery(&D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 }, Some(&mut query))? };
        Ok(Converter { enumerator, processor, outputs: Vec::new(), inputs: Vec::new(), query: query.unwrap() })
    }
}

pub struct Converter {
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    /// Views de saída por textura de destino (rodízio ou backbuffers).
    outputs: Vec<(usize, ID3D11VideoProcessorOutputView)>,
    /// Views de entrada por fatia do array de texturas do decoder (o FFmpeg
    /// reaproveita sempre o mesmo pool).
    inputs: Vec<(usize, u32, ID3D11VideoProcessorInputView)>,
    query: ID3D11Query,
}

impl Converter {
    fn output_view(&mut self, d3d: &D3D, tex: &ID3D11Texture2D) -> anyhow::Result<ID3D11VideoProcessorOutputView> {
        let key = tex.as_raw() as usize;
        if let Some((_, v)) = self.outputs.iter().find(|(k, _)| *k == key) {
            return Ok(v.clone());
        }
        let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
        };
        let mut view = None;
        unsafe { d3d.video.CreateVideoProcessorOutputView(tex, &self.enumerator, &desc, Some(&mut view))? };
        let view = view.unwrap();
        self.outputs.push((key, view.clone()));
        Ok(view)
    }

    /// Converte a fatia `index` de `src` (NV12, saída do decoder) para a
    /// textura de saída nº `out` (do rodízio) e espera a GPU terminar, para
    /// outra API (D3D12) poder ler a textura em seguida.
    pub fn convert(&mut self, d3d: &D3D, src: &ID3D11Texture2D, index: u32, out: usize) -> anyhow::Result<()> {
        let view = self.outputs[out].1.clone();
        self.blt(d3d, src, index, &view)?;
        self.wait(d3d)
    }

    /// Converte direto para `target` (ex.: backbuffer de uma swapchain). Não
    /// espera: o `Present` da swapchain já ordena o trabalho na GPU.
    pub fn convert_into(&mut self, d3d: &D3D, src: &ID3D11Texture2D, index: u32, target: &ID3D11Texture2D) -> anyhow::Result<()> {
        let view = self.output_view(d3d, target)?;
        self.blt(d3d, src, index, &view)
    }

    fn blt(&mut self, d3d: &D3D, src: &ID3D11Texture2D, index: u32, out: &ID3D11VideoProcessorOutputView) -> anyhow::Result<()> {
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
                unsafe { d3d.video.CreateVideoProcessorInputView(src, &self.enumerator, &desc, Some(&mut view)) }
                    .context("CreateVideoProcessorInputView")?;
                let view = view.unwrap();
                self.inputs.push((key, index, view.clone()));
                view
            }
        };
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(input)),
            ..Default::default()
        };
        let streams = [stream];
        let r = unsafe { d3d.vctx.VideoProcessorBlt(&self.processor, out, 0, &streams) };
        // Devolve a referência que o ManuallyDrop segurou.
        let [mut stream] = streams;
        unsafe { std::mem::ManuallyDrop::drop(&mut stream.pInputSurface) };
        r.context("VideoProcessorBlt")
    }

    /// Espera a GPU concluir antes de o D3D12 ler a textura (spike: sem fence
    /// compartilhada ainda; ver README).
    fn wait(&self, d3d: &D3D) -> anyhow::Result<()> {
        unsafe {
            d3d.ctx.End(&self.query);
            d3d.ctx.Flush();
        }
        let mut done = windows::core::BOOL(0);
        let start = std::time::Instant::now();
        loop {
            let hr = unsafe {
                d3d.ctx.GetData(&self.query, Some(&mut done as *mut _ as *mut _), size_of_val(&done) as u32, 0)
            };
            if hr.is_ok() && done.as_bool() {
                return Ok(());
            }
            if start.elapsed() > std::time::Duration::from_secs(1) {
                bail!("GPU não concluiu a conversão em 1 s");
            }
            // Dormir em vez de girar: girar inflaria a medição de CPU.
            std::thread::sleep(std::time::Duration::from_micros(250));
        }
    }
}
