//! Variante leve da D-02: UI no renderer **software** do Slint (sem driver
//! gráfico carregado) + vídeo numa **janela-filha com swapchain D3D11**.
//!
//! O processador de vídeo escreve direto no backbuffer (NV12 → RGB e
//! redimensionamento na GPU) e o `Present` exibe. Continua sem cópia para a
//! CPU, e o D3D11 só existe enquanto há vídeo na tela.

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, w};

use crate::cpu::CpuMeter;
use crate::ffdec::Decoder;
use crate::gpu::D3D;
use crate::play::Report;

/// Altura da barra superior desenhada pelo Slint, em pixels lógicos.
const BAR: f32 = 40.0;

slint::slint! {
    export component Shell inherits Window {
        title: "Spike D-02 — Slint software + swapchain D3D11";
        preferred-width: 1280px;
        preferred-height: 760px;
        background: #0B0710;
        in property <string> stats;
        Rectangle {
            x: 0; y: 0;
            height: 40px;
            background: #140D1F;
            Text { x: 12px; text: root.stats; color: #F4F0FA; font-size: 14px; vertical-alignment: center; }
        }
    }
}

struct SendHwnd(isize);
unsafe impl Send for SendHwnd {}

type Worker = std::thread::JoinHandle<anyhow::Result<Report>>;

pub fn run(path: &str, seconds: u64) -> anyhow::Result<()> {
    slint::BackendSelector::new().renderer_name("software".into()).select()?;
    let app = Shell::new()?;
    app.set_stats("abrindo…".into());
    app.show()?;

    // O winit só cria a janela nativa quando o loop roda; a janela-filha e a
    // thread de decode são montadas logo no primeiro giro do loop.
    let worker: std::rc::Rc<std::cell::RefCell<Option<anyhow::Result<Worker>>>> = Default::default();
    let slot = worker.clone();
    let weak = app.as_weak();
    let path = path.to_string();
    slint::Timer::single_shot(Duration::from_millis(100), move || {
        let app = weak.upgrade().expect("janela viva");
        let started = start_video(&app, path, seconds);
        if started.is_err() {
            let _ = slint::quit_event_loop();
        }
        *slot.borrow_mut() = Some(started);
    });

    slint::run_event_loop()?;
    let worker = worker.borrow_mut().take().context("o loop terminou antes de iniciar o vídeo")??;
    let report = worker.join().expect("thread de decode entrou em pânico")?;
    report.print("decode + exibição (Slint software + swapchain D3D11)");
    Ok(())
}

fn start_video(app: &Shell, path: String, seconds: u64) -> anyhow::Result<Worker> {
    // Janela-filha na área abaixo da barra, em pixels físicos.
    let parent = match app.window().window_handle().window_handle()?.as_raw() {
        RawWindowHandle::Win32(h) => HWND(h.hwnd.get() as *mut _),
        other => bail!("janela não é Win32: {other:?}"),
    };
    let mut client = RECT::default();
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetClientRect(parent, &mut client)? };
    let bar = (BAR * app.window().scale_factor()).round() as i32;
    let (cw, ch) = (client.right - client.left, client.bottom - client.top - bar);
    let child = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("STATIC"),
            None,
            WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
            0,
            bar,
            cw,
            ch,
            Some(parent),
            None,
            Some(GetModuleHandleW(None)?.into()),
            None,
        )?
    };
    eprintln!("janela-filha de vídeo: {cw}x{ch}");

    let weak = app.as_weak();
    let child = SendHwnd(child.0 as isize);
    Ok(std::thread::spawn(move || -> anyhow::Result<Report> {
        let child = child;
        let result = decode_to_swapchain(&path, HWND(child.0 as *mut _), (cw as u32, ch as u32), seconds, |s| {
            let _ = weak.upgrade_in_event_loop(move |app| app.set_stats(s.into()));
        });
        // Fecha a janela também quando o decode falha.
        let _ = slint::quit_event_loop();
        result
    }))
}

fn decode_to_swapchain(
    path: &str,
    hwnd: HWND,
    size: (u32, u32),
    seconds: u64,
    mut stats: impl FnMut(String),
) -> anyhow::Result<Report> {
    let d3d = D3D::new(None)?;
    let mut decoder = Decoder::open(path, &d3d.device)?;
    eprintln!("decoder: {} {}x{} via D3D11VA", decoder.codec_name, decoder.width, decoder.height);

    let factory: IDXGIFactory2 = unsafe { d3d.device.cast::<IDXGIDevice>()?.GetAdapter()?.GetParent()? };
    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: size.0,
        Height: size.1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        AlphaMode: DXGI_ALPHA_MODE_UNSPECIFIED,
        Flags: 0,
        ..Default::default()
    };
    let swapchain = unsafe { factory.CreateSwapChainForHwnd(&d3d.device, hwnd, &desc, None, None)? };
    let mut conv = d3d.scaling_converter((decoder.width, decoder.height), size)?;

    let frame_time = Duration::from_secs_f64(1.0 / 60.0);
    let meter = CpuMeter::start();
    let start = Instant::now();
    let mut window = (Instant::now(), 0u64, CpuMeter::start());
    let (mut frames, mut late) = (0u64, 0u64);
    let mut deadline = start;
    while start.elapsed() < Duration::from_secs(seconds) {
        let (tex, index) = decoder.next()?;
        let back: ID3D11Texture2D = unsafe { swapchain.GetBuffer(0)? };
        conv.convert_into(&d3d, &tex, index, &back)?;
        unsafe { swapchain.Present(1, DXGI_PRESENT(0)).ok().context("Present")? };
        frames += 1;

        if window.0.elapsed() >= Duration::from_secs(1) {
            let fps = (frames - window.1) as f64 / window.0.elapsed().as_secs_f64();
            let (m, c) = window.2.read();
            window = (Instant::now(), frames, CpuMeter::start());
            stats(format!("{fps:.0} fps · CPU {m:.2}% ({c:.1}% de 1 núcleo) · UI software + swapchain D3D11"));
        }

        deadline += frame_time;
        let now = Instant::now();
        if now < deadline {
            std::thread::sleep(deadline - now);
        } else if now - deadline > frame_time {
            late += 1;
            deadline = now;
        }
    }
    let (cpu_machine, cpu_core) = meter.read();
    Ok(Report { frames, wall: start.elapsed(), late, cpu_machine, cpu_core })
}
