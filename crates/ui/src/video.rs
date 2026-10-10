//! Janelas-filhas do vídeo assistido, uma para a tela e uma para a câmera
//! (D-02): o Windows exige que elas sejam criadas, movidas e destruídas pela
//! thread da UI, então o estado fica aqui, por thread. A imagem em si é
//! desenhada pelas threads de vídeo da sala.

use std::cell::RefCell;
use std::collections::HashMap;

use lerri_core::media::Video;
use slint::ComponentHandle;

use crate::AppWindow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

#[derive(Default)]
struct State {
    windows: HashMap<Video, (isize, Rect)>,
    minimized: bool,
    monitors: Vec<isize>,
    cameras: Vec<String>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::default();
}

pub fn set_devices(monitors: Vec<isize>, cameras: Vec<String>) {
    STATE.with_borrow_mut(|s| {
        s.monitors = monitors;
        s.cameras = cameras;
    });
}

pub fn monitor(index: usize) -> Option<isize> {
    STATE.with_borrow(|s| s.monitors.get(index).copied())
}

pub fn camera(index: usize) -> Option<String> {
    STATE.with_borrow(|s| s.cameras.get(index).cloned())
}

/// Área de vídeo de cada tipo na tela da sala, em pixels físicos.
fn area(ui: &AppWindow, video: Video) -> Rect {
    let s = ui.window().scale_factor();
    let (x, y, w, h) = match video {
        Video::Screen => (ui.get_video_x(), ui.get_video_y(), ui.get_video_width(), ui.get_video_height()),
        Video::Camera => (ui.get_cam_x(), ui.get_cam_y(), ui.get_cam_width(), ui.get_cam_height()),
    };
    Rect {
        x: (x * s).round() as i32,
        y: (y * s).round() as i32,
        w: (w * s).round().max(0.0) as u32,
        h: (h * s).round().max(0.0) as u32,
    }
}

/// O que mudou desde a última olhada.
pub enum Change {
    Resized { window: isize, size: (u32, u32) },
    Minimized(bool),
}

#[cfg(windows)]
mod imp {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    use super::*;
    use lerri_media::video::render;

    fn to_render(r: Rect) -> render::Rect {
        render::Rect { x: r.x, y: r.y, w: r.w, h: r.h }
    }

    fn parent(ui: &AppWindow) -> anyhow::Result<isize> {
        match ui.window().window_handle().window_handle()?.as_raw() {
            RawWindowHandle::Win32(h) => Ok(h.hwnd.get()),
            other => anyhow::bail!("janela não é Win32: {other:?}"),
        }
    }

    fn minimized(parent: isize) -> bool {
        use windows_sys::Win32::UI::WindowsAndMessaging::IsIconic;
        // SAFETY: só consulta o estado da janela.
        unsafe { IsIconic(parent as _) != 0 }
    }

    /// Cria a janela-filha sobre a área de vídeo do tipo e devolve o handle
    /// e o tamanho.
    pub fn open(ui: &AppWindow, video: Video) -> anyhow::Result<(isize, (u32, u32))> {
        let r = area(ui, video);
        let hwnd = render::create_child(parent(ui)?, to_render(r))?;
        STATE.with_borrow_mut(|s| s.windows.insert(video, (hwnd, r)));
        Ok((hwnd, (r.w, r.h)))
    }

    /// Acompanha as áreas de vídeo (layout mudou, janela redimensionada ou
    /// minimizada).
    pub fn follow(ui: &AppWindow) -> Vec<Change> {
        let Ok(parent) = parent(ui) else { return Vec::new() };
        let areas: Vec<(Video, Rect)> = Video::ALL.into_iter().map(|v| (v, area(ui, v))).collect();
        STATE.with_borrow_mut(|s| {
            let mut out = Vec::new();
            if s.windows.is_empty() {
                return out;
            }
            let min = minimized(parent);
            if min != s.minimized {
                s.minimized = min;
                out.push(Change::Minimized(min));
            }
            if min {
                return out;
            }
            for (video, r) in areas {
                let Some((hwnd, last)) = s.windows.get_mut(&video) else { continue };
                if r == *last || r.w == 0 || r.h == 0 {
                    continue;
                }
                render::move_child(*hwnd, to_render(r));
                if (r.w, r.h) != (last.w, last.h) {
                    out.push(Change::Resized { window: *hwnd, size: (r.w, r.h) });
                }
                *last = r;
            }
            out
        })
    }

    /// A sala largou a janela: destrói. Devolve de que tipo ela era.
    pub fn closed(hwnd: isize) -> Option<Video> {
        render::destroy_child(hwnd);
        STATE.with_borrow_mut(|s| {
            let video = s.windows.iter().find(|(_, (h, _))| *h == hwnd).map(|(v, _)| *v)?;
            s.windows.remove(&video);
            if s.windows.is_empty() {
                s.minimized = false;
            }
            Some(video)
        })
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    pub fn open(_: &AppWindow, _: Video) -> anyhow::Result<(isize, (u32, u32))> {
        anyhow::bail!("vídeo ainda só no Windows")
    }

    pub fn follow(ui: &AppWindow) -> Vec<Change> {
        let _ = area(ui, Video::Screen);
        Vec::new()
    }

    pub fn closed(_: isize) -> Option<Video> {
        None
    }
}

pub use imp::{closed, follow, open};
