//! Exibição do vídeo numa janela-filha com swapchain D3D11 (D-02): o
//! processador de vídeo da GPU converte NV12 → RGB e redimensiona direto no
//! backbuffer, sem cópia para a CPU. A UI (Slint, renderer software) não
//! desenha por cima dessa área.
//!
//! O Windows exige que a janela seja criada, movida e destruída pela thread
//! dona da janela-mãe (a da UI): ver [`create_child`], [`move_child`] e
//! [`destroy_child`]. A swapchain fica com a thread de decode ([`Screen`]).

use anyhow::Context;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, w};

use super::d3d::{Colors, Converter, D3D, fit};
use super::ffmpeg::Decoded;

/// Retângulo em pixels físicos, relativo à área cliente da janela-mãe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// Cria a janela-filha de vídeo. Chamar na thread da UI.
pub fn create_child(parent: isize, r: Rect) -> anyhow::Result<isize> {
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_NOPARENTNOTIFY,
            w!("STATIC"),
            None,
            WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | WS_DISABLED,
            r.x,
            r.y,
            r.w.max(1) as i32,
            r.h.max(1) as i32,
            Some(HWND(parent as *mut _)),
            None,
            Some(GetModuleHandleW(None)?.into()),
            None,
        )?
    };
    Ok(hwnd.0 as isize)
}

/// Move/redimensiona a janela-filha. Chamar na thread da UI.
pub fn move_child(child: isize, r: Rect) {
    unsafe {
        let _ = SetWindowPos(
            HWND(child as *mut _),
            None,
            r.x,
            r.y,
            r.w.max(1) as i32,
            r.h.max(1) as i32,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

pub fn show_child(child: isize, visible: bool) {
    unsafe {
        let _ = ShowWindow(HWND(child as *mut _), if visible { SW_SHOWNA } else { SW_HIDE });
    }
}

/// Destrói a janela-filha. Chamar na thread da UI, depois que a swapchain
/// ([`Screen`]) já foi derrubada.
pub fn destroy_child(child: isize) {
    unsafe {
        let _ = DestroyWindow(HWND(child as *mut _));
    }
}

/// Swapchain de uma janela-filha.
pub struct Screen {
    d3d: D3D,
    swapchain: IDXGISwapChain1,
    size: (u32, u32),
    conv: Option<Converter>,
}

// SAFETY: a swapchain é usada só pela thread de decode, depois de criada.
unsafe impl Send for Screen {}

impl Screen {
    pub fn new(d3d: &D3D, child: isize, size: (u32, u32)) -> anyhow::Result<Self> {
        let size = (size.0.max(1), size.1.max(1));
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
        let swapchain = unsafe { factory.CreateSwapChainForHwnd(&d3d.device, HWND(child as *mut _), &desc, None, None)? };
        // Alt+Enter não deve virar tela cheia exclusiva.
        unsafe {
            let _ = factory.MakeWindowAssociation(HWND(child as *mut _), DXGI_MWA_NO_ALT_ENTER);
        }
        Ok(Self { d3d: d3d.clone(), swapchain, size, conv: None })
    }

    /// A janela mudou de tamanho.
    pub fn resize(&mut self, size: (u32, u32)) -> anyhow::Result<()> {
        let size = (size.0.max(1), size.1.max(1));
        if size == self.size {
            return Ok(());
        }
        // As views apontam para os buffers antigos: precisam sumir antes.
        self.conv = None;
        unsafe { self.swapchain.ResizeBuffers(0, size.0, size.1, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0))? };
        self.size = size;
        Ok(())
    }

    /// Mostra um quadro decodificado, com barras pretas se a proporção não bater.
    pub fn present(&mut self, frame: &Decoded) -> anyhow::Result<()> {
        let mut desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
        unsafe { frame.texture.GetDesc(&mut desc) };
        let pool = (desc.Width, desc.Height);
        if self.conv.as_ref().is_none_or(|c| c.input != pool) {
            self.conv = Some(self.d3d.converter(pool, self.size, Colors::YuvToRgb)?);
        }
        let conv = self.conv.as_mut().expect("criado acima");
        conv.set_rects((frame.width, frame.height), fit((frame.width, frame.height), self.size), self.size);
        let back: ID3D11Texture2D = unsafe { self.swapchain.GetBuffer(0)? };
        conv.convert(&frame.texture, frame.index, &back)?;
        unsafe { self.swapchain.Present(1, DXGI_PRESENT(0)).ok().context("Present")? };
        Ok(())
    }
}
