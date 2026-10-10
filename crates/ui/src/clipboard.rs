//! Copiar texto para a área de transferência (o Slint não expõe isso).

#[cfg(windows)]
pub fn set_text(text: &str) -> Result<(), String> {
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;

    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    // SAFETY: sequência documentada da API; o bloco global passa a ser do
    // sistema depois de um SetClipboardData bem-sucedido, senão é liberado.
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err("área de transferência ocupada".into());
        }
        let result = (|| {
            EmptyClipboard();
            let mem = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2);
            if mem.is_null() {
                return Err("sem memória".to_string());
            }
            let dst = GlobalLock(mem).cast::<u16>();
            if dst.is_null() {
                GlobalFree(mem);
                return Err("GlobalLock falhou".to_string());
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
            GlobalUnlock(mem);
            if SetClipboardData(CF_UNICODETEXT as u32, mem).is_null() {
                GlobalFree(mem);
                return Err("SetClipboardData falhou".to_string());
            }
            Ok(())
        })();
        CloseClipboard();
        result
    }
}

#[cfg(not(windows))]
pub fn set_text(_text: &str) -> Result<(), String> {
    Err("só implementado no Windows".into())
}
