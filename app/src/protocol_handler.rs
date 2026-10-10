//! Registro do `lerrichat://` para o usuário atual (RF-04). Fica em
//! `HKCU\Software\Classes`, então não pede administrador nem mexe em outras
//! contas. Abrir um link chama `lerrichat.exe --join "<link>"`.

#[cfg(windows)]
mod imp {
    use std::path::Path;

    use anyhow::bail;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
        RegCreateKeyExW, RegDeleteTreeW, RegGetValueW, RRF_RT_REG_SZ, RegSetValueExW,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn set(path: &str, name: Option<&str>, value: &str) -> anyhow::Result<()> {
        let mut key: HKEY = std::ptr::null_mut();
        let path_w = wide(path);
        // SAFETY: ponteiros válidos durante a chamada; a chave é fechada abaixo.
        let rc = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                path_w.as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                std::ptr::null(),
                &mut key,
                std::ptr::null_mut(),
            )
        };
        if rc != ERROR_SUCCESS {
            bail!("RegCreateKeyExW({path}) falhou: {rc}");
        }
        let name_w = name.map(wide);
        let data = wide(value);
        // SAFETY: `data` é UTF-16 terminado em zero; tamanho em bytes.
        let rc = unsafe {
            RegSetValueExW(
                key,
                name_w.as_ref().map_or(std::ptr::null(), |n| n.as_ptr()),
                0,
                REG_SZ,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        };
        // SAFETY: `key` foi aberta acima.
        unsafe { RegCloseKey(key) };
        if rc != ERROR_SUCCESS {
            bail!("RegSetValueExW({path}) falhou: {rc}");
        }
        Ok(())
    }

    pub fn register(scheme: &str, exe: &Path) -> anyhow::Result<()> {
        let base = format!(r"Software\Classes\{scheme}");
        let exe = exe.display();
        set(&base, None, "URL:Lerri Chat")?;
        set(&base, Some("URL Protocol"), "")?;
        set(&format!(r"{base}\DefaultIcon"), None, &format!("\"{exe}\",0"))?;
        set(&format!(r"{base}\shell\open\command"), None, &format!("\"{exe}\" --join \"%1\""))?;
        Ok(())
    }

    pub fn unregister(scheme: &str) -> anyhow::Result<()> {
        let path = wide(&format!(r"Software\Classes\{scheme}"));
        // SAFETY: caminho UTF-16 terminado em zero.
        let rc = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, path.as_ptr()) };
        if rc != ERROR_SUCCESS && rc != windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND {
            bail!("RegDeleteTreeW falhou: {rc}");
        }
        Ok(())
    }

    /// Comando registrado hoje, se houver.
    pub fn registered_command(scheme: &str) -> Option<String> {
        let path = wide(&format!(r"Software\Classes\{scheme}\shell\open\command"));
        let mut buf = vec![0u16; 2048];
        let mut len = (buf.len() * 2) as u32;
        // SAFETY: buffer com `len` bytes; a API escreve no máximo isso.
        let rc = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                std::ptr::null(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut len,
            )
        };
        if rc != ERROR_SUCCESS {
            return None;
        }
        let chars = (len as usize / 2).saturating_sub(1);
        Some(String::from_utf16_lossy(&buf[..chars]))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn register_roundtrip() {
            // Esquema próprio do teste, para não tocar no `lerrichat` real.
            let scheme = format!("lerrichat-test-{}", lerri_core::ids::hex(&lerri_crypto::random::<4>()));
            let exe = Path::new(r"C:\Program Files\Lerri Chat\lerrichat.exe");
            register(&scheme, exe).unwrap();
            assert_eq!(
                registered_command(&scheme).unwrap(),
                r#""C:\Program Files\Lerri Chat\lerrichat.exe" --join "%1""#
            );
            unregister(&scheme).unwrap();
            assert!(registered_command(&scheme).is_none());
            unregister(&scheme).unwrap();
        }
    }
}

#[cfg(windows)]
pub use imp::{register, registered_command, unregister};

#[cfg(not(windows))]
pub fn register(_scheme: &str, _exe: &std::path::Path) -> anyhow::Result<()> {
    anyhow::bail!("registro de protocolo só existe no Windows")
}

#[cfg(not(windows))]
pub fn unregister(_scheme: &str) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(not(windows))]
pub fn registered_command(_scheme: &str) -> Option<String> {
    None
}
