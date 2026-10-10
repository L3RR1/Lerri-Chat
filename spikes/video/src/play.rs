//! Caminho completo do spike: FFmpeg (D3D11VA) → processador de vídeo D3D11
//! (NV12→RGBA, textura compartilhada) → D3D12 do wgpu → `slint::Image`.
//! Nenhum quadro passa pela memória da CPU.

use std::time::{Duration, Instant};

use anyhow::Context;
use slint::wgpu_30::{WGPUConfiguration, wgpu};
use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Direct3D12::{ID3D12Device, ID3D12Resource};

use crate::cpu::CpuMeter;
use crate::ffdec::Decoder;
use crate::gpu::D3D;

slint::slint! {
    export component Player inherits Window {
        title: "Spike D-02 — vídeo na GPU";
        preferred-width: 1280px;
        preferred-height: 720px;
        background: #0B0710;
        in property <image> frame;
        in property <string> stats;
        Image {
            source: root.frame;
            width: parent.width;
            height: parent.height;
            image-fit: contain;
        }
        Text {
            x: 12px;
            y: 12px;
            text: root.stats;
            color: #F4F0FA;
            font-size: 14px;
        }
    }
}

/// Número de texturas de saída em rodízio: o decoder escreve numa enquanto o
/// Slint ainda pode estar desenhando a anterior.
const RING: usize = 3;

struct SendBox<T>(T);
unsafe impl<T> Send for SendBox<T> {}

pub fn run(path: &str, seconds: u64) -> anyhow::Result<()> {
    // 1. wgpu em D3D12 na GPU de alto desempenho.
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::DX12,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        ..Default::default()
    }))?;
    eprintln!("wgpu em: {} ({:?})", adapter.get_info().name, adapter.get_info().backend);
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))?;

    let luid = unsafe {
        let hal = adapter.as_hal::<wgpu::hal::api::Dx12>().context("adaptador não é D3D12")?;
        let desc = hal.as_raw().GetDesc1()?;
        LUID { LowPart: desc.AdapterLuid.LowPart, HighPart: desc.AdapterLuid.HighPart }
    };
    let d3d12: ID3D12Device =
        unsafe { device.as_hal::<wgpu::hal::api::Dx12>().context("device não é D3D12")?.raw_device().clone() };

    // 2. D3D11 na MESMA GPU (pelo LUID) + decoder + texturas compartilhadas.
    let d3d = D3D::new(Some(luid))?;
    let decoder = Decoder::open(path, &d3d.device)?;
    let (w, h) = (decoder.width, decoder.height);
    eprintln!("decoder: {} {w}x{h} via D3D11VA", decoder.codec_name);

    let mut d3d11_out = Vec::new();
    let mut textures = Vec::new();
    for _ in 0..RING {
        let (tex, handle) = d3d.shared_texture(w, h)?;
        textures.push(import_into_wgpu(&d3d12, &device, handle, w, h)?);
        d3d11_out.push(tex);
    }

    // 3. Slint usando exatamente esse device/queue.
    slint::BackendSelector::new()
        .require_wgpu_30(WGPUConfiguration::Manual { instance, adapter, device, queue })
        .select()?;
    let app = Player::new()?;

    // Falha cedo se o Slint recusar a textura (formato/uso), em vez de exibir
    // uma janela vazia.
    slint::Image::try_from(textures[0].clone()).context("Slint recusou a textura de vídeo")?;

    let weak = app.as_weak();
    let worker = SendBox((d3d, decoder, d3d11_out));
    let handle = std::thread::spawn(move || -> anyhow::Result<Report> {
        let SendBox((d3d, mut decoder, outputs)) = worker;
        let result = decode_loop(&d3d, &mut decoder, &outputs, seconds, |slot, stats| {
            let tex = textures[slot].clone();
            let _ = weak.upgrade_in_event_loop(move |app| {
                match slint::Image::try_from(tex) {
                    Ok(img) => app.set_frame(img),
                    Err(e) => eprintln!("quadro descartado: {e}"),
                }
                if let Some(s) = stats {
                    app.set_stats(s.into());
                }
            });
        });
        // Fecha a janela também quando o decode falha, senão ela fica aberta vazia.
        let _ = slint::quit_event_loop();
        result
    });

    app.run()?;
    let report = handle.join().expect("thread de decode entrou em pânico")?;
    report.print("decode + conversão + exibição no Slint");
    Ok(())
}

/// Só decode + conversão, sem janela: isola o custo do pipeline de vídeo.
pub fn run_headless(path: &str, seconds: u64) -> anyhow::Result<()> {
    let d3d = D3D::new(None)?;
    let mut decoder = Decoder::open(path, &d3d.device)?;
    eprintln!("decoder: {} {}x{} via D3D11VA", decoder.codec_name, decoder.width, decoder.height);
    let outputs: Vec<_> =
        (0..RING).map(|_| d3d.shared_texture(decoder.width, decoder.height).map(|(t, _)| t)).collect::<Result<_, _>>()?;
    let report = decode_loop(&d3d, &mut decoder, &outputs, seconds, |_, _| {})?;
    report.print("decode + conversão (sem janela)");
    Ok(())
}

/// Janela parada, sem vídeo, para medir o custo base de cada renderer do
/// Slint (`wgpu`, `femtovg` = OpenGL, `software`).
pub fn idle(renderer: &str, seconds: u64) -> anyhow::Result<()> {
    let selector = slint::BackendSelector::new();
    let selector = match renderer {
        "wgpu" => selector.require_wgpu_30(WGPUConfiguration::default()),
        "femtovg" | "software" => selector.renderer_name(renderer.into()),
        other => anyhow::bail!("renderer desconhecido: {other}"),
    };
    selector.select()?;
    let app = Player::new()?;
    app.set_stats(format!("ocioso — renderer {renderer}").into());
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::SingleShot, Duration::from_secs(seconds), || {
        let _ = slint::quit_event_loop();
    });
    app.run()?;
    Ok(())
}

/// Decodifica até o quadro `n`, converte e grava um BMP para conferência visual.
pub fn dump(path: &str, n: u32, out: &str) -> anyhow::Result<()> {
    let d3d = D3D::new(None)?;
    let mut decoder = Decoder::open(path, &d3d.device)?;
    let (tex, _) = d3d.shared_texture(decoder.width, decoder.height)?;
    let mut conv = d3d.converter(decoder.width, decoder.height, std::slice::from_ref(&tex))?;
    for _ in 0..n {
        decoder.next()?;
    }
    let (src, index) = decoder.next()?;
    conv.convert(&d3d, &src, index, 0)?;
    let (w, h, mut px) = d3d.read_back(&tex)?;
    // A textura é RGBA; o BMP 32 bits espera BGRA.
    px.as_chunks_mut::<4>().0.iter_mut().for_each(|p| p.swap(0, 2));

    // BMP 32 bits top-down (altura negativa), sem dependências.
    let mut bmp = Vec::with_capacity(54 + px.len());
    let file_len = 54 + px.len() as u32;
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&file_len.to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&54u32.to_le_bytes());
    bmp.extend_from_slice(&40u32.to_le_bytes());
    bmp.extend_from_slice(&(w as i32).to_le_bytes());
    bmp.extend_from_slice(&(-(h as i32)).to_le_bytes());
    bmp.extend_from_slice(&1u16.to_le_bytes());
    bmp.extend_from_slice(&32u16.to_le_bytes());
    bmp.extend_from_slice(&[0; 24]);
    bmp.extend_from_slice(&px);
    std::fs::write(out, bmp)?;
    println!("quadro {n} gravado em {out} ({w}x{h})");
    Ok(())
}

pub(crate) struct Report {
    pub frames: u64,
    pub wall: Duration,
    pub late: u64,
    pub cpu_machine: f64,
    pub cpu_core: f64,
}

impl Report {
    pub fn print(&self, what: &str) {
        println!("\n== {what} ==");
        println!("quadros: {} em {:.1} s ({:.1} fps)", self.frames, self.wall.as_secs_f64(), self.frames as f64 / self.wall.as_secs_f64());
        println!("quadros atrasados (>1 quadro de atraso): {}", self.late);
        println!("CPU do processo: {:.2}% da máquina ({:.1}% de um núcleo)", self.cpu_machine, self.cpu_core);
    }
}

/// Decodifica a 60 fps (ritmo de reprodução real, não o máximo da GPU) e
/// entrega cada quadro convertido ao `show`.
fn decode_loop(
    d3d: &D3D,
    decoder: &mut Decoder,
    outputs: &[ID3D11Texture2D],
    seconds: u64,
    mut show: impl FnMut(usize, Option<String>),
) -> anyhow::Result<Report> {
    let mut conv = d3d.converter(decoder.width, decoder.height, outputs)?;
    let frame_time = Duration::from_secs_f64(1.0 / 60.0);
    let meter = CpuMeter::start();
    let start = Instant::now();
    let mut window = (Instant::now(), 0u64, CpuMeter::start());
    let (mut frames, mut late) = (0u64, 0u64);
    let mut deadline = start;

    while start.elapsed() < Duration::from_secs(seconds) {
        let (tex, index) = decoder.next()?;
        let slot = (frames as usize) % outputs.len();
        conv.convert(d3d, &tex, index, slot)?;
        frames += 1;

        let stats = (window.0.elapsed() >= Duration::from_secs(1)).then(|| {
            let fps = (frames - window.1) as f64 / window.0.elapsed().as_secs_f64();
            let (m, c) = window.2.read();
            window = (Instant::now(), frames, CpuMeter::start());
            format!("{fps:.0} fps · CPU {m:.2}% ({c:.1}% de 1 núcleo)")
        });
        show(slot, stats);

        deadline += frame_time;
        let now = Instant::now();
        if now < deadline {
            std::thread::sleep(deadline - now);
        } else if now - deadline > frame_time {
            late += 1;
            deadline = now; // não tenta "recuperar" quadros atrasados
        }
    }
    let (cpu_machine, cpu_core) = meter.read();
    Ok(Report { frames, wall: start.elapsed(), late, cpu_machine, cpu_core })
}

fn import_into_wgpu(
    d3d12: &ID3D12Device,
    device: &wgpu::Device,
    handle: HANDLE,
    w: u32,
    h: u32,
) -> anyhow::Result<wgpu::Texture> {
    let mut resource: Option<ID3D12Resource> = None;
    let opened = unsafe { d3d12.OpenSharedHandle(handle, &mut resource) };
    unsafe { CloseHandle(handle)? };
    opened.context("D3D12 não abriu a textura compartilhada do D3D11")?;
    let size = wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 };
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let hal = unsafe {
        wgpu::hal::dx12::Device::texture_from_raw(resource.unwrap(), format, wgpu::TextureDimension::D2, size, 1, 1)
    };
    let desc = wgpu::TextureDescriptor {
        label: Some("quadro de vídeo"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        // O Slint exige os dois usos para importar (ver i-slint-core wgpu_30.rs).
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    };
    Ok(unsafe { device.create_texture_from_hal::<wgpu::hal::api::Dx12>(hal, &desc, wgpu::wgt::TextureUses::RESOURCE) })
}
