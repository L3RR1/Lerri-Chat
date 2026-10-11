//! Tecla de apertar para falar (push-to-talk): global, funciona com o jogo
//! em foco. Lida por `GetAsyncKeyState` na thread de áudio, a cada 10 ms;
//! sem hook de teclado, sem permissão especial.

use std::time::{Duration, Instant};

/// A tecla (código virtual do Windows) está apertada agora.
pub fn key_down(vk: u32) -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        // SAFETY: só consulta o estado da tecla.
        (unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000) != 0
    }
    #[cfg(not(windows))]
    {
        let _ = vk;
        false
    }
}

/// Nome da tecla para a interface ("Mouse 4", "Ctrl direito", "V"...).
pub fn key_name(vk: u32) -> String {
    match vk {
        0x04 => return "Botão do meio do mouse".into(),
        0x05 => return "Mouse 4 (lateral de trás)".into(),
        0x06 => return "Mouse 5 (lateral da frente)".into(),
        0xA0 => return "Shift esquerdo".into(),
        0xA1 => return "Shift direito".into(),
        0xA2 => return "Ctrl esquerdo".into(),
        0xA3 => return "Ctrl direito".into(),
        0xA4 => return "Alt esquerdo".into(),
        0xA5 => return "Alt direito".into(),
        _ => {}
    }
    #[cfg(windows)]
    {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyNameTextW, MAPVK_VK_TO_VSC_EX, MapVirtualKeyW};
        // SAFETY: conversão de código de tecla, sem ponteiros.
        let scan = unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_VSC_EX) };
        let extended = (scan & 0xff00) != 0;
        let lparam = ((scan & 0xff) << 16) as i32 | if extended { 1 << 24 } else { 0 };
        let mut buf = [0u16; 64];
        // SAFETY: o buffer tem o tamanho informado.
        let n = unsafe { GetKeyNameTextW(lparam, &mut buf) };
        if n > 0 {
            return String::from_utf16_lossy(&buf[..n as usize]);
        }
    }
    format!("Tecla {vk:#04x}")
}

/// Foi apertada desde a última consulta (pega toques mais curtos que o
/// intervalo de leitura).
fn pressed_since(vk: u32) -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        // SAFETY: só consulta o estado da tecla.
        (unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8001) != 0
    }
    #[cfg(not(windows))]
    {
        let _ = vk;
        false
    }
}

/// Códigos que não fazem sentido como tecla de falar: botões principais do
/// mouse (o clique em "Mudar tecla" ainda está apertado) e as versões
/// genéricas de Shift/Ctrl/Alt (valem as de lado, 0xA0–0xA5).
fn ignored(vk: u32) -> bool {
    matches!(vk, 0x01 | 0x02 | 0x10 | 0x11 | 0x12)
}

/// Espera a próxima tecla ou botão do mouse apertado e devolve o código.
/// `None`: Esc (cancelar) ou passou o `timeout`.
pub fn capture(timeout: Duration) -> Option<u32> {
    const ESC: u32 = 0x1B;
    // A primeira leitura também zera o "apertada desde a última consulta".
    let held: Vec<u32> = (1..255).filter(|&vk| pressed_since(vk) && key_down(vk)).collect();
    let start = Instant::now();
    while start.elapsed() < timeout {
        std::thread::sleep(Duration::from_millis(10));
        for vk in 1..255 {
            if ignored(vk) || held.contains(&vk) || !pressed_since(vk) {
                continue;
            }
            return (vk != ESC).then_some(vk);
        }
    }
    None
}
