//! Captura de monitor pela DXGI Desktop Duplication (SPEC-04 §2), para o
//! Windows 10: lá o WGC sempre desenha uma borda amarela em volta da tela
//! compartilhada, e a duplicação não. Alimenta o mesmo [`Shared`] do WGC.
//!
//! Enquanto segura um quadro da duplicação, o DWM espera; se nesse meio
//! tempo a cópia esperasse o lock do dispositivo do vídeo (preso num
//! `Present`, que espera o DWM), travava tudo. Por isso a duplicação tem um
//! dispositivo só dela, solta o quadro logo depois de copiar, e entrega a
//! imagem por uma textura compartilhada (keyed mutex) entre os dois.
//!
//! A duplicação não inclui o cursor: ele é desenhado por cima com GDI, numa
//! cópia da imagem limpa da área de trabalho (assim, mexer o mouse não deixa
//! rastro).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context;
use windows::Win32::Foundation::{HANDLE, POINT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_FLAG, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_RESOURCE_MISC_FLAG, D3D11_RESOURCE_MISC_GDI_COMPATIBLE, D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter, IDXGIDevice,
    IDXGIKeyedMutex, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, IDXGISurface1,
};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::UI::WindowsAndMessaging::{CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo, HICON};
use windows::core::Interface;

use super::capture::{Latest, Shared};
use super::d3d::D3D;

/// Espera máxima por um quadro: também é o tempo para notar o `stop`.
const ACQUIRE_MS: u32 = 100;

pub struct Duplication {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Duplication {
    /// Começa a duplicar `monitor`. Falha se ele está em outra GPU, girado,
    /// ou o sistema recusa; quem chama volta para o WGC. Devolve o tamanho.
    pub fn start(d3d: &D3D, monitor: isize, shared: Arc<Shared>) -> anyhow::Result<(Self, (u32, u32))> {
        let adapter = unsafe { d3d.device.cast::<IDXGIDevice>()?.GetAdapter()? };
        let (device, ctx) = device(&adapter)?;
        let dup = duplicate(&adapter, &device, monitor)?.context("monitor não está na GPU do vídeo")?;
        let desc = unsafe { dup.GetDesc() };
        anyhow::ensure!(
            matches!(desc.Rotation, DXGI_MODE_ROTATION_IDENTITY | DXGI_MODE_ROTATION_UNSPECIFIED),
            "monitor girado"
        );
        let size = (desc.ModeDesc.Width, desc.ModeDesc.Height);
        let grab = Grab { main: d3d.clone(), adapter, device, ctx, monitor, desk: None, canvas: None, bridge: None };
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        let thread = std::thread::Builder::new()
            .name("duplicação".into())
            .spawn(move || run(grab, dup, shared, st))
            .context("thread da duplicação")?;
        Ok((Self { stop, thread: Some(thread) }, size))
    }
}

impl Drop for Duplication {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn device(adapter: &IDXGIAdapter) -> anyhow::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let (mut device, mut ctx) = (None, None);
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )?
    };
    Ok((device.context("D3D11CreateDevice sem dispositivo")?, ctx.context("D3D11CreateDevice sem contexto")?))
}

/// Duplicação da saída de `monitor` em `adapter`; `None` se nenhuma saída
/// dela é esse monitor (desligado, ou ligado em outra placa).
fn duplicate(adapter: &IDXGIAdapter, device: &ID3D11Device, monitor: isize) -> anyhow::Result<Option<IDXGIOutputDuplication>> {
    for i in 0.. {
        let Ok(output) = (unsafe { adapter.EnumOutputs(i) }) else { return Ok(None) };
        if unsafe { output.GetDesc()? }.Monitor != HMONITOR(monitor as *mut _) {
            continue;
        }
        let dup = unsafe { output.cast::<IDXGIOutput1>()?.DuplicateOutput(device) }.context("DuplicateOutput")?;
        return Ok(Some(dup));
    }
    unreachable!()
}

/// Onde está o cursor (canto do desenho, relativo ao monitor).
#[derive(Default)]
struct Pointer {
    pos: POINT,
    visible: bool,
}

/// Textura vista pelos dois dispositivos.
struct Bridge {
    size: (u32, u32),
    /// Lado da duplicação.
    ours: ID3D11Texture2D,
    ours_lock: IDXGIKeyedMutex,
    /// Lado do vídeo.
    theirs: ID3D11Texture2D,
    theirs_lock: IDXGIKeyedMutex,
}

/// Estado da thread: dispositivo próprio e as texturas dele.
struct Grab {
    main: D3D,
    adapter: IDXGIAdapter,
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    monitor: isize,
    /// Imagem limpa da área de trabalho.
    desk: Option<ID3D11Texture2D>,
    /// `desk` mais o cursor.
    canvas: Option<ID3D11Texture2D>,
    bridge: Option<Bridge>,
}

// SAFETY: tudo é usado só pela thread da duplicação; o dispositivo do vídeo
// (`main`) já é free-threaded.
unsafe impl Send for Grab {}

fn run(mut grab: Grab, dup: IDXGIOutputDuplication, shared: Arc<Shared>, stop: Arc<AtomicBool>) {
    let mut dup = Some(dup);
    let mut pointer = Pointer::default();
    while !stop.load(Ordering::Relaxed) {
        let Some(d) = &dup else {
            // Acesso perdido (resolução mudou, tela de UAC/bloqueio): tenta de novo.
            match duplicate(&grab.adapter, &grab.device, grab.monitor) {
                Ok(Some(d)) => dup = Some(d),
                Ok(None) => {
                    shared.closed.store(true, Ordering::Relaxed);
                    shared.signal.notify_all();
                    return;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(200)),
            }
            continue;
        };
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe { d.AcquireNextFrame(ACQUIRE_MS, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => continue,
            Err(e) => {
                if e.code() != DXGI_ERROR_ACCESS_LOST {
                    tracing::warn!("duplicação: {e}");
                }
                dup = None;
                continue;
            }
        }
        let copied = match resource.filter(|_| info.LastPresentTime != 0) {
            Some(image) => grab.keep(&image),
            None => Ok(()),
        };
        let _ = unsafe { d.ReleaseFrame() };
        if info.LastMouseUpdateTime != 0 {
            pointer.pos = info.PointerPosition.Position;
            pointer.visible = info.PointerPosition.Visible.as_bool();
        }
        if info.LastPresentTime == 0 && info.LastMouseUpdateTime == 0 {
            continue;
        }
        if let Err(e) = copied.and_then(|()| grab.deliver(&shared, &pointer)) {
            tracing::warn!("duplicação: {e:#}");
        }
    }
}

impl Grab {
    fn texture(&self, size: (u32, u32), bind: D3D11_BIND_FLAG, misc: D3D11_RESOURCE_MISC_FLAG) -> anyhow::Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: size.0,
            Height: size.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: misc.0 as u32,
        };
        let mut tex = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
        tex.context("CreateTexture2D")
    }

    /// Copia a imagem nova da duplicação para `desk`.
    fn keep(&mut self, image: &IDXGIResource) -> anyhow::Result<()> {
        let src: ID3D11Texture2D = image.cast()?;
        let size = texture_size(&src);
        if self.desk.as_ref().is_none_or(|t| texture_size(t) != size) {
            self.desk = Some(self.texture(size, D3D11_BIND_SHADER_RESOURCE, D3D11_RESOURCE_MISC_FLAG(0))?);
        }
        // SAFETY: mesmo dispositivo, mesmo tamanho e formato.
        unsafe { self.ctx.CopyResource(self.desk.as_ref().expect("criada acima"), &src) };
        Ok(())
    }

    /// Monta `desk` mais o cursor e põe no quadro mais recente do vídeo.
    fn deliver(&mut self, shared: &Shared, pointer: &Pointer) -> anyhow::Result<()> {
        let Some(desk) = self.desk.clone() else { return Ok(()) };
        let size = texture_size(&desk);
        let source = if pointer.visible {
            if self.canvas.as_ref().is_none_or(|t| texture_size(t) != size) {
                self.canvas = Some(self.texture(
                    size,
                    D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE,
                    D3D11_RESOURCE_MISC_GDI_COMPATIBLE,
                )?);
            }
            let canvas = self.canvas.clone().expect("criada acima");
            unsafe { self.ctx.CopyResource(&canvas, &desk) };
            draw_cursor(&canvas, pointer.pos)?;
            canvas
        } else {
            desk
        };
        if self.bridge.as_ref().is_none_or(|b| b.size != size) {
            self.bridge = Some(self.bridge(size)?);
        }
        let b = self.bridge.as_ref().expect("criada acima");
        unsafe {
            b.ours_lock.AcquireSync(0, u32::MAX)?;
            self.ctx.CopyResource(&b.ours, &source);
            b.ours_lock.ReleaseSync(0)?;
        }
        let mut latest = shared.latest.lock().unwrap();
        if latest.as_ref().is_none_or(|l| l.size != size) {
            let texture =
                self.main.texture(size.0, size.1, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE)?;
            *latest = Some(Latest { texture, size, fresh: false, at: Instant::now() });
        }
        let l = latest.as_mut().expect("criada acima");
        unsafe {
            b.theirs_lock.AcquireSync(0, u32::MAX)?;
            self.main.ctx.CopyResource(&l.texture, &b.theirs);
            b.theirs_lock.ReleaseSync(0)?;
        }
        l.fresh = true;
        l.at = Instant::now();
        drop(latest);
        shared.signal.notify_all();
        Ok(())
    }

    fn bridge(&self, size: (u32, u32)) -> anyhow::Result<Bridge> {
        let ours = self.texture(size, D3D11_BIND_SHADER_RESOURCE, D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX)?;
        let handle: HANDLE = unsafe { ours.cast::<IDXGIResource>()?.GetSharedHandle()? };
        let theirs: ID3D11Texture2D = unsafe {
            let mut t: Option<ID3D11Texture2D> = None;
            self.main.device.OpenSharedResource(handle, &mut t)?;
            t.context("OpenSharedResource")?
        };
        Ok(Bridge { size, ours_lock: ours.cast()?, ours, theirs_lock: theirs.cast()?, theirs })
    }
}

fn texture_size(t: &ID3D11Texture2D) -> (u32, u32) {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { t.GetDesc(&mut desc) };
    (desc.Width, desc.Height)
}

/// Desenha o cursor atual do sistema com o canto em `pos`.
fn draw_cursor(texture: &ID3D11Texture2D, pos: POINT) -> anyhow::Result<()> {
    let mut info = CURSORINFO { cbSize: size_of::<CURSORINFO>() as u32, ..Default::default() };
    unsafe { GetCursorInfo(&mut info)? };
    if info.flags.0 & CURSOR_SHOWING.0 == 0 || info.hCursor.is_invalid() {
        return Ok(());
    }
    let surface: IDXGISurface1 = texture.cast()?;
    // SAFETY: o DC é devolvido logo abaixo, antes de o D3D usar a textura de novo.
    unsafe {
        let hdc = surface.GetDC(false)?;
        let drawn = DrawIconEx(hdc, pos.x, pos.y, HICON(info.hCursor.0), 0, 0, 0, None, DI_NORMAL);
        surface.ReleaseDC(None)?;
        drawn?;
    }
    Ok(())
}
