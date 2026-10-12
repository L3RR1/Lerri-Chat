//! Vídeos assistidos na sala, em grade (D-02): até [`MAX_TILES`] telas e
//! câmeras ao mesmo tempo, cada uma com a sua janela-filha D3D11 em cima da
//! caixa preta da sua célula. O Windows exige que as janelas-filhas sejam
//! criadas, movidas e destruídas pela thread da UI, então o estado fica aqui,
//! por thread. A imagem em si é desenhada pelas threads de vídeo da sala.
//!
//! O Rust divide a área da grade em células ([`grid`]) e passa a posição de
//! cada uma para o Slint, que só desenha cabeçalho e caixa preta nelas.

use std::cell::RefCell;

use lerri_core::MemberId;
use lerri_core::media::Video;
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::{AppWindow, VideoTile};

/// Vídeos assistidos ao mesmo tempo (cada um tem decoder e janela próprios).
pub const MAX_TILES: usize = lerri_client::room::MAX_WATCHING;
/// Espaço entre as células, em pixels lógicos.
const GAP: f32 = 10.0;
/// Cabeçalho da célula (botão compacto de 30 px + 8 de espaço): a caixa do
/// vídeo começa abaixo dele. Igual ao `app.slint`.
const HEADER: f32 = 38.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// Célula da grade, em pixels lógicos relativos à área da grade.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Cell {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Divide `w`×`h` em `n` células iguais, com o número de colunas que deixa o
/// maior vídeo 16:9 em cada uma. Última linha incompleta fica centralizada.
pub fn grid(w: f32, h: f32, n: usize) -> Vec<Cell> {
    if n == 0 || w <= 0.0 || h <= 0.0 {
        return Vec::new();
    }
    let cell = |cols: usize| {
        let rows = n.div_ceil(cols);
        let cw = (w - GAP * (cols - 1) as f32) / cols as f32;
        let ch = (h - GAP * (rows - 1) as f32) / rows as f32;
        (cw, ch)
    };
    let video_width = |cols: usize| {
        let (cw, ch) = cell(cols);
        cw.min((ch - HEADER) * 16.0 / 9.0).max(0.0)
    };
    let cols = (1..=n).max_by(|a, b| video_width(*a).total_cmp(&video_width(*b))).unwrap_or(1);
    let (cw, ch) = cell(cols);
    (0..n)
        .map(|i| {
            let (row, col) = (i / cols, i % cols);
            let in_row = (n - row * cols).min(cols);
            let indent = (cols - in_row) as f32 * (cw + GAP) / 2.0;
            Cell { x: indent + col as f32 * (cw + GAP), y: row as f32 * (ch + GAP), w: cw, h: ch }
        })
        .collect()
}

struct Tile {
    member: MemberId,
    video: Video,
    label: String,
    /// Prévia da nossa câmera (não conta no limite dos assistidos).
    own: bool,
    cell: Cell,
    /// Janela-filha e onde ela está (pixels físicos). `None` até abrir.
    window: Option<(isize, Rect)>,
}

#[derive(Default)]
struct State {
    tiles: Vec<Tile>,
    minimized: bool,
    monitors: Vec<isize>,
    /// Janelas do menu "O que compartilhar".
    share_windows: Vec<isize>,
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

/// Troca as câmeras; devolve o índice da que estava escolhida, se continua ligada.
pub fn set_cameras(cameras: Vec<String>, chosen: usize) -> Option<usize> {
    STATE.with_borrow_mut(|s| {
        let keep = s.cameras.get(chosen).and_then(|id| cameras.iter().position(|c| c == id));
        s.cameras = cameras;
        keep
    })
}

/// Janelas mostradas no menu "O que compartilhar", depois dos monitores.
pub fn set_windows(windows: Vec<isize>) {
    STATE.with_borrow_mut(|s| s.share_windows = windows);
}

/// Item `index` do menu "O que compartilhar".
pub enum ShareTarget {
    Monitor(Option<isize>),
    Window(isize),
}

pub fn share_target(index: usize) -> ShareTarget {
    STATE.with_borrow(|s| match index.checked_sub(s.monitors.len()) {
        None => ShareTarget::Monitor(s.monitors.get(index).copied()),
        Some(w) => s.share_windows.get(w).map_or(ShareTarget::Monitor(None), |h| ShareTarget::Window(*h)),
    })
}

pub fn camera(index: usize) -> Option<String> {
    STATE.with_borrow(|s| s.cameras.get(index).cloned())
}

/// Põe uma célula na grade para o vídeo (ainda sem janela). `false` se ele
/// já está lá ou a grade está cheia. `own`: a prévia da nossa câmera, que
/// fica fora do limite de [`MAX_TILES`] assistidos.
pub fn add(ui: &AppWindow, member: MemberId, video: Video, label: String, own: bool) -> bool {
    let added = STATE.with_borrow_mut(|s| {
        let watched = s.tiles.iter().filter(|t| !t.own).count();
        if (!own && watched >= MAX_TILES) || s.tiles.iter().any(|t| t.member == member && t.video == video) {
            return false;
        }
        s.tiles.push(Tile { member, video, label, own, cell: Cell::default(), window: None });
        true
    });
    if added {
        push(ui);
    }
    added
}

/// Vídeo da célula `index` e se é a nossa prévia.
pub fn tile(index: usize) -> Option<(MemberId, Video, bool)> {
    STATE.with_borrow(|s| s.tiles.get(index).map(|t| (t.member, t.video, t.own)))
}

/// A prévia da nossa câmera está na grade.
pub fn has_preview() -> bool {
    STATE.with_borrow(|s| s.tiles.iter().any(|t| t.own))
}

/// Tira a célula (a janela, se houver, a sala devolve por `closed`).
pub fn remove(ui: &AppWindow, member: MemberId, video: Video) {
    let removed = STATE.with_borrow_mut(|s| {
        let before = s.tiles.len();
        s.tiles.retain(|t| t.window.is_some() || !(t.member == member && t.video == video));
        s.tiles.len() != before
    });
    if removed {
        push(ui);
    }
}

/// Saiu da sala: destrói o que sobrou.
pub fn clear(ui: &AppWindow) {
    let windows: Vec<isize> = STATE.with_borrow_mut(|s| {
        s.minimized = false;
        s.tiles.drain(..).filter_map(|t| t.window.map(|(h, _)| h)).collect()
    });
    for hwnd in windows {
        imp::destroy(hwnd);
    }
    push(ui);
}

/// Manda as células para o Slint.
fn push(ui: &AppWindow) {
    let tiles: Vec<VideoTile> = STATE.with_borrow(|s| {
        s.tiles
            .iter()
            .map(|t| VideoTile {
                label: t.label.clone().into(),
                camera: t.video == Video::Camera,
                own: t.own,
                x: t.cell.x,
                y: t.cell.y,
                width: t.cell.w,
                height: t.cell.h,
            })
            .collect()
    });
    let model = ui.get_tiles();
    let same = model.row_count() == tiles.len() && model.iter().zip(&tiles).all(|(a, b)| a == *b);
    if !same {
        ui.set_preview_on(tiles.iter().any(|t| t.own));
        ui.set_tiles(ModelRc::new(VecModel::from(tiles)));
    }
}

/// Caixa do vídeo de uma célula na janela, em pixels físicos.
fn video_rect(ui: &AppWindow, cell: Cell) -> Rect {
    let s = ui.window().scale_factor();
    let (x, y) = (ui.get_stage_x() + cell.x, ui.get_stage_y() + cell.y + HEADER);
    Rect {
        x: (x * s).round() as i32,
        y: (y * s).round() as i32,
        w: (cell.w * s).round().max(0.0) as u32,
        h: ((cell.h - HEADER) * s).round().max(0.0) as u32,
    }
}

/// O que mudou desde a última olhada.
pub enum Change {
    Resized { window: isize, size: (u32, u32) },
    Minimized(bool),
}

/// Refaz a grade com o tamanho atual da área (layout mudou, janela
/// redimensionada, vídeo entrou ou saiu) e devolve o que a sala precisa
/// saber.
pub fn follow(ui: &AppWindow) -> Vec<Change> {
    let n = STATE.with_borrow(|s| s.tiles.len());
    if n == 0 {
        return Vec::new();
    }
    let cells = grid(ui.get_stage_width(), ui.get_stage_height(), n);
    STATE.with_borrow_mut(|s| {
        for (t, c) in s.tiles.iter_mut().zip(&cells) {
            t.cell = *c;
        }
    });
    push(ui);
    let mut out = Vec::new();
    let mut moved = false;
    let min = imp::minimized(ui);
    let rects: Vec<Rect> = cells.iter().map(|c| video_rect(ui, *c)).collect();
    STATE.with_borrow_mut(|s| {
        if min != s.minimized {
            s.minimized = min;
            out.push(Change::Minimized(min));
        }
        if min {
            return;
        }
        for (t, r) in s.tiles.iter_mut().zip(rects) {
            let Some((hwnd, last)) = t.window.as_mut() else { continue };
            if r == *last || r.w == 0 || r.h == 0 {
                continue;
            }
            imp::move_to(*hwnd, r);
            moved = true;
            if (r.w, r.h) != (last.w, last.h) {
                out.push(Change::Resized { window: *hwnd, size: (r.w, r.h) });
            }
            *last = r;
        }
    });
    if moved {
        repaint_all(ui);
    }
    out
}

/// Redesenha a janela inteira: a área de onde uma janela-filha saiu ficou
/// sem pintar (o renderer só repinta o que mudou, e embaixo dela ele pintou
/// sem aparecer).
fn repaint_all(ui: &AppWindow) {
    ui.set_repaint_all(!ui.get_repaint_all());
}

/// Cria a janela-filha sobre a caixa do vídeo e devolve o handle e o tamanho.
pub fn open(ui: &AppWindow, member: MemberId, video: Video) -> anyhow::Result<(isize, (u32, u32))> {
    let cell = STATE
        .with_borrow(|s| s.tiles.iter().find(|t| t.member == member && t.video == video).map(|t| t.cell))
        .ok_or_else(|| anyhow::anyhow!("o vídeo saiu da grade"))?;
    let r = video_rect(ui, cell);
    anyhow::ensure!(r.w > 0 && r.h > 0, "a área do vídeo ainda não tem tamanho");
    let hwnd = imp::create(ui, r)?;
    STATE.with_borrow_mut(|s| {
        if let Some(t) = s.tiles.iter_mut().find(|t| t.member == member && t.video == video) {
            t.window = Some((hwnd, r));
        }
    });
    Ok((hwnd, (r.w, r.h)))
}

/// A sala largou a janela: destrói e tira a célula.
pub fn closed(ui: &AppWindow, hwnd: isize) {
    imp::destroy(hwnd);
    STATE.with_borrow_mut(|s| {
        s.tiles.retain(|t| t.window.is_none_or(|(h, _)| h != hwnd));
        if s.tiles.is_empty() {
            s.minimized = false;
        }
    });
    push(ui);
    repaint_all(ui);
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

    pub fn minimized(ui: &AppWindow) -> bool {
        use windows_sys::Win32::UI::WindowsAndMessaging::IsIconic;
        let Ok(parent) = parent(ui) else { return false };
        // SAFETY: só consulta o estado da janela.
        unsafe { IsIconic(parent as _) != 0 }
    }

    pub fn create(ui: &AppWindow, r: Rect) -> anyhow::Result<isize> {
        render::create_child(parent(ui)?, to_render(r))
    }

    pub fn move_to(hwnd: isize, r: Rect) {
        render::move_child(hwnd, to_render(r));
    }

    pub fn destroy(hwnd: isize) {
        render::destroy_child(hwnd);
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    pub fn minimized(_: &AppWindow) -> bool {
        false
    }

    pub fn create(_: &AppWindow, _: Rect) -> anyhow::Result<isize> {
        anyhow::bail!("vídeo ainda só no Windows")
    }

    pub fn move_to(_: isize, _: Rect) {}

    pub fn destroy(_: isize) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols(cells: &[Cell]) -> usize {
        cells.iter().filter(|c| c.y == 0.0).count()
    }

    #[test]
    fn grid_picks_the_layout_with_the_biggest_video() {
        assert!(grid(800.0, 400.0, 0).is_empty());
        let one = grid(800.0, 400.0, 1);
        assert_eq!(one, [Cell { x: 0.0, y: 0.0, w: 800.0, h: 400.0 }]);
        // Área larga: dois lado a lado; área alta: um sobre o outro.
        assert_eq!(cols(&grid(1200.0, 400.0, 2)), 2);
        assert_eq!(cols(&grid(500.0, 800.0, 2)), 1);
        // Quatro numa área 16:9: 2×2.
        let four = grid(1280.0, 720.0, 4);
        assert_eq!(cols(&four), 2);
        assert!(four.iter().all(|c| c.w > 600.0 && c.h > 300.0));
        // Três em 2×2: a última linha, de um só, fica no meio.
        let three = grid(1000.0, 800.0, 3);
        assert_eq!(cols(&three), 2);
        let last = three[2];
        assert!((last.x - (1000.0 - last.w) / 2.0).abs() < 0.01, "{last:?}");
        // Nenhuma célula sai da área nem encosta na outra.
        for (w, h, n) in [(900.0, 500.0, 3), (1600.0, 300.0, 4), (300.0, 900.0, 4)] {
            let cells = grid(w, h, n);
            for (i, a) in cells.iter().enumerate() {
                assert!(a.x >= 0.0 && a.y >= 0.0 && a.x + a.w <= w + 0.01 && a.y + a.h <= h + 0.01, "{a:?} fora de {w}×{h}");
                for b in &cells[i + 1..] {
                    let apart = a.x + a.w <= b.x + 0.01 || b.x + b.w <= a.x + 0.01 || a.y + a.h <= b.y + 0.01 || b.y + b.h <= a.y + 0.01;
                    assert!(apart, "{a:?} encosta em {b:?}");
                }
            }
        }
    }
}
