//! Captura de tela pelo Windows.Graphics.Capture (SPEC-04 §2): o quadro fica
//! na GPU, e o WGC só entrega quadro novo quando algo muda na tela.
//!
//! O callback do WGC só copia o quadro para a textura "mais recente" (cópia
//! na GPU) e avisa. Quem codifica pega essa textura no próprio ritmo, então
//! uma rajada de mudanças vira um quadro só, e o último estado de uma tela
//! que parou nunca se perde.
//!
//! No Windows 10 o WGC sempre desenha uma borda amarela em volta do que está
//! sendo capturado; lá, monitores vão pela Desktop Duplication
//! ([`super::duplication`]), sem borda.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use windows::Foundation::Metadata::ApiInformation;
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Direct3D11::{D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW};
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetWindow, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsWindowVisible, WS_EX_TOOLWINDOW,
};
use windows::core::{BOOL, Interface, h};

use super::d3d::D3D;
use super::duplication::Duplication;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monitor {
    pub handle: isize,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// Monitores ligados, o principal primeiro.
pub fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn each(m: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        // SAFETY: `data` é o `Vec` passado abaixo, vivo durante a enumeração.
        let list = unsafe { &mut *(data.0 as *mut Vec<Monitor>) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(m, &mut info as *mut _ as *mut _) }.as_bool() {
            let r = info.monitorInfo.rcMonitor;
            let name = String::from_utf16_lossy(&info.szDevice).trim_end_matches('\0').trim_start_matches("\\\\.\\").to_string();
            list.push(Monitor {
                handle: m.0 as isize,
                name,
                width: (r.right - r.left) as u32,
                height: (r.bottom - r.top) as u32,
                primary: info.monitorInfo.dwFlags & 1 != 0,
            });
        }
        true.into()
    }
    let mut list: Vec<Monitor> = Vec::new();
    // SAFETY: callback acima; `list` vive até o fim da chamada.
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(each), LPARAM(&mut list as *mut _ as isize));
    }
    list.sort_by_key(|m| !m.primary);
    list
}

/// Uma janela que dá para compartilhar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub handle: isize,
    pub title: String,
}

/// Janelas de aplicativo abertas, na ordem do Windows (a da frente primeiro):
/// visíveis, com título, sem dono, fora as de ferramenta, as ocultas pelo
/// sistema (abas de outras áreas de trabalho, apps suspensos) e as deste
/// processo.
pub fn windows() -> Vec<Window> {
    unsafe extern "system" fn each(h: HWND, data: LPARAM) -> BOOL {
        // SAFETY: `data` é o `Vec` passado abaixo, vivo durante a enumeração.
        let list = unsafe { &mut *(data.0 as *mut Vec<Window>) };
        // SAFETY: consultas sobre uma janela que o próprio Windows enumerou.
        unsafe {
            if !IsWindowVisible(h).as_bool() || GetWindow(h, GW_OWNER).is_ok_and(|o| !o.is_invalid()) {
                return true.into();
            }
            if GetWindowLongPtrW(h, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0 {
                return true.into();
            }
            let mut cloaked = 0u32;
            let _ = DwmGetWindowAttribute(h, DWMWA_CLOAKED, (&raw mut cloaked).cast(), size_of::<u32>() as u32);
            if cloaked != 0 {
                return true.into();
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(h, Some(&mut pid));
            if pid == std::process::id() {
                return true.into();
            }
            let len = GetWindowTextLengthW(h);
            if len <= 0 {
                return true.into();
            }
            let mut buf = vec![0u16; len as usize + 1];
            let n = GetWindowTextW(h, &mut buf);
            let title = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
            if !title.trim().is_empty() && title != "Program Manager" {
                list.push(Window { handle: h.0 as isize, title });
            }
        }
        true.into()
    }
    let mut list: Vec<Window> = Vec::new();
    // SAFETY: callback acima; `list` vive até o fim da chamada.
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut list as *mut _ as isize));
    }
    list
}

/// O que capturar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Monitor(isize),
    Window(isize),
}

/// Último quadro capturado, esperando o encoder.
pub struct Latest {
    pub texture: ID3D11Texture2D,
    pub size: (u32, u32),
    /// Chegou quadro novo desde a última vez que o encoder pegou.
    pub fresh: bool,
    pub at: Instant,
}

pub struct Shared {
    pub latest: Mutex<Option<Latest>>,
    pub signal: Condvar,
    /// A janela capturada foi fechada (ou o monitor sumiu).
    pub closed: AtomicBool,
}

// SAFETY: texturas D3D11 de um dispositivo free-threaded com proteção multithread.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Shared {
    pub fn closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// Espera um quadro novo por até `timeout`.
    pub fn wait_fresh(&self, timeout: Duration) -> bool {
        let guard = self.latest.lock().unwrap();
        let (guard, _) =
            self.signal.wait_timeout_while(guard, timeout, |l| !l.as_ref().is_some_and(|l| l.fresh)).unwrap();
        guard.as_ref().is_some_and(|l| l.fresh)
    }
}

pub struct Capture {
    pub shared: Arc<Shared>,
    backend: Backend,
    pub size: (u32, u32),
}

enum Backend {
    Wgc { pool: Direct3D11CaptureFramePool, session: GraphicsCaptureSession },
    // Guardada só para parar a thread no `Drop`.
    Duplication(#[allow(dead_code)] Duplication),
}

/// O WGC deixa tirar a borda amarela (Windows 11 em diante).
fn borderless_wgc() -> bool {
    ApiInformation::IsPropertyPresent(h!("Windows.Graphics.Capture.GraphicsCaptureSession"), h!("IsBorderRequired"))
        .unwrap_or(false)
}

fn new_shared() -> Arc<Shared> {
    Arc::new(Shared { latest: Mutex::new(None), signal: Condvar::new(), closed: AtomicBool::new(false) })
}

impl Capture {
    pub fn is_supported() -> bool {
        GraphicsCaptureSession::IsSupported().unwrap_or(false)
    }

    pub fn start(d3d: &D3D, target: Target) -> anyhow::Result<Self> {
        if let Target::Monitor(m) = target
            && !borderless_wgc()
        {
            let shared = new_shared();
            match Duplication::start(d3d, m, shared.clone()) {
                Ok((dup, size)) => return Ok(Self { shared, backend: Backend::Duplication(dup), size }),
                Err(e) => tracing::info!("duplicação indisponível, captura com borda (WGC): {e:#}"),
            }
        }
        anyhow::ensure!(Self::is_supported(), "este Windows não tem a captura de tela moderna (WGC)");
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        // SAFETY: identificadores vindos de `monitors()`/`windows()`; o WGC
        // recusa os que não valem mais.
        let item: GraphicsCaptureItem = match target {
            Target::Monitor(m) => unsafe { interop.CreateForMonitor(HMONITOR(m as *mut _)) }.context("monitor não pode ser capturado")?,
            Target::Window(w) => unsafe { interop.CreateForWindow(HWND(w as *mut _)) }
                .context("essa janela não pode ser capturada (fechou ou é protegida)")?,
        };
        let size = item.Size()?;
        let winrt_device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&d3d.device.cast::<IDXGIDevice>()?)? }.cast()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )?;
        let shared = new_shared();
        let s = shared.clone();
        item.Closed(&TypedEventHandler::new(move |_, _| {
            s.closed.store(true, Ordering::Relaxed);
            s.signal.notify_all();
            Ok(())
        }))?;
        let (s, dev) = (shared.clone(), d3d.clone());
        let current = Mutex::new((size.Width, size.Height));
        pool.FrameArrived(&TypedEventHandler::new(move |pool: windows::core::Ref<Direct3D11CaptureFramePool>, _| {
            let Some(pool) = pool.as_ref() else { return Ok(()) };
            let frame = pool.TryGetNextFrame()?;
            let content = frame.ContentSize()?;
            let mut cur = current.lock().unwrap();
            if (content.Width, content.Height) != *cur && content.Width > 0 && content.Height > 0 {
                // Resolução mudou: o pool passa a entregar no tamanho novo.
                *cur = (content.Width, content.Height);
                pool.Recreate(&winrt_device_of(&dev)?, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, content)?;
                return Ok(());
            }
            let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
            let src: ID3D11Texture2D = unsafe { access.GetInterface()? };
            let mut latest = s.latest.lock().unwrap();
            let size = (cur.0 as u32, cur.1 as u32);
            drop(cur);
            if latest.as_ref().is_none_or(|l| l.size != size) {
                match dev.texture(size.0, size.1, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE) {
                    Ok(texture) => *latest = Some(Latest { texture, size, fresh: false, at: Instant::now() }),
                    Err(e) => {
                        tracing::warn!("captura: {e:#}");
                        return Ok(());
                    }
                }
            }
            let l = latest.as_mut().expect("criada acima");
            // SAFETY: mesmo dispositivo, mesmo tamanho e formato.
            unsafe { dev.ctx.CopyResource(&l.texture, &src) };
            l.fresh = true;
            l.at = Instant::now();
            drop(latest);
            s.signal.notify_all();
            Ok(())
        }))?;
        let session = pool.CreateCaptureSession(&item)?;
        let _ = session.SetIsCursorCaptureEnabled(true);
        // Windows 11: sem a borda amarela em volta da tela compartilhada.
        let _ = session.SetIsBorderRequired(false);
        session.StartCapture()?;
        Ok(Self { shared, backend: Backend::Wgc { pool, session }, size: (size.Width as u32, size.Height as u32) })
    }
}

fn winrt_device_of(d3d: &D3D) -> windows::core::Result<IDirect3DDevice> {
    unsafe { CreateDirect3D11DeviceFromDXGIDevice(&d3d.device.cast::<IDXGIDevice>()?)? }.cast()
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Backend::Wgc { pool, session } = &self.backend {
            let _ = session.Close();
            let _ = pool.Close();
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "lista os monitores desta máquina"]
    fn list_monitors() {
        for m in super::monitors() {
            println!("{m:?}");
        }
        for w in super::windows() {
            println!("{w:?}");
        }
    }
}
