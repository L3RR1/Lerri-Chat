//! Segredos em repouso no disco (SPEC-02 §1, §8).
//!
//! - Windows: DPAPI no escopo do usuário. Só a mesma conta do Windows, na
//!   mesma máquina, consegue abrir o arquivo.
//! - Outros (relay Linux): arquivo com permissão 0600. O relay não guarda
//!   nenhum segredo de sala, só a chave TLS dele.
//!
//! Formato do arquivo: `b"LCSS" || versão (1) || esquema (1) || blob`.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

const MAGIC: &[u8; 4] = b"LCSS";
const VERSION: u8 = 1;
const SCHEME_PLAIN_0600: u8 = 0;
const SCHEME_DPAPI: u8 = 1;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("erro de E/S em {path}: {source}")]
    Io { path: String, source: io::Error },
    #[error("arquivo {0} não é um segredo do Lerri Chat ou está corrompido")]
    BadFormat(String),
    #[error("arquivo {0} foi protegido com um esquema que esta plataforma não abre")]
    WrongScheme(String),
    #[error("DPAPI falhou: {0}")]
    Dpapi(io::Error),
}

/// Grava `secret` protegido. `purpose` separa os usos (identidade, chave TLS)
/// para que um blob não possa ser trocado pelo outro.
pub fn save(path: &Path, purpose: &str, secret: &[u8]) -> Result<(), StoreError> {
    let (scheme, blob) = protect(purpose, secret)?;
    let mut data = Vec::with_capacity(6 + blob.len());
    data.extend_from_slice(MAGIC);
    data.push(VERSION);
    data.push(scheme);
    data.extend_from_slice(&blob);
    write_private(path, &data).map_err(|source| StoreError::Io { path: path.display().to_string(), source })
}

/// Lê um segredo gravado por [`save`]. `Ok(None)` se o arquivo não existe.
pub fn load(path: &Path, purpose: &str) -> Result<Option<zeroize::Zeroizing<Vec<u8>>>, StoreError> {
    let shown = || path.display().to_string();
    let data = match fs::read(path) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(StoreError::Io { path: shown(), source }),
    };
    if data.len() < 6 || &data[..4] != MAGIC || data[4] != VERSION {
        return Err(StoreError::BadFormat(shown()));
    }
    unprotect(purpose, data[5], &data[6..], shown).map(Some)
}

#[cfg(windows)]
fn protect(purpose: &str, secret: &[u8]) -> Result<(u8, Vec<u8>), StoreError> {
    dpapi::protect(secret, purpose.as_bytes()).map(|b| (SCHEME_DPAPI, b)).map_err(StoreError::Dpapi)
}

#[cfg(not(windows))]
fn protect(_purpose: &str, secret: &[u8]) -> Result<(u8, Vec<u8>), StoreError> {
    Ok((SCHEME_PLAIN_0600, secret.to_vec()))
}

fn unprotect(
    purpose: &str,
    scheme: u8,
    blob: &[u8],
    shown: impl Fn() -> String,
) -> Result<zeroize::Zeroizing<Vec<u8>>, StoreError> {
    let native = if cfg!(windows) { SCHEME_DPAPI } else { SCHEME_PLAIN_0600 };
    if scheme != native {
        return Err(match scheme {
            SCHEME_DPAPI | SCHEME_PLAIN_0600 => StoreError::WrongScheme(shown()),
            _ => StoreError::BadFormat(shown()),
        });
    }
    #[cfg(windows)]
    return dpapi::unprotect(blob, purpose.as_bytes()).map_err(StoreError::Dpapi);
    #[cfg(not(windows))]
    {
        let _ = purpose;
        Ok(zeroize::Zeroizing::new(blob.to_vec()))
    }
}

/// Escreve via arquivo temporário + rename, para nunca deixar um segredo pela
/// metade se o processo cair no meio da gravação.
fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

#[cfg(windows)]
mod dpapi {
    use std::io;
    use std::ptr;

    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };
    use zeroize::Zeroizing;

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 }
    }

    /// Copia a saída alocada pelo Windows e a libera com `LocalFree`.
    unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
        unsafe { LocalFree(out.pbData.cast()) };
        v
    }

    pub fn protect(secret: &[u8], entropy: &[u8]) -> io::Result<Vec<u8>> {
        let (input, ent) = (blob(secret), blob(entropy));
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: ptr::null_mut() };
        // SAFETY: entradas apontam para slices vivos durante a chamada; a
        // saída é alocada pelo Windows e liberada em `take`.
        let ok = unsafe {
            CryptProtectData(&input, ptr::null(), &ent, ptr::null(), ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { take(out) })
    }

    pub fn unprotect(blob_in: &[u8], entropy: &[u8]) -> io::Result<Zeroizing<Vec<u8>>> {
        let (input, ent) = (blob(blob_in), blob(entropy));
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: ptr::null_mut() };
        // SAFETY: idem `protect`.
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                ptr::null_mut(),
                &ent,
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // Zera o buffer do Windows antes de liberar: ele contém o segredo.
        let plain = unsafe {
            let s = std::slice::from_raw_parts_mut(out.pbData, out.cbData as usize);
            let v = Zeroizing::new(s.to_vec());
            zeroize::Zeroize::zeroize(s);
            LocalFree(out.pbData.cast());
            v
        };
        Ok(plain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut rnd = [0u8; 8];
        getrandom::fill(&mut rnd).unwrap();
        std::env::temp_dir().join(format!("lerri-test-{}-{name}", crate::hex(&rnd)))
    }

    #[test]
    fn roundtrip_and_missing() {
        let path = temp_path("secret.bin");
        assert!(load(&path, "teste").unwrap().is_none());
        save(&path, "teste", b"segredo").unwrap();
        assert_eq!(load(&path, "teste").unwrap().unwrap().as_slice(), b"segredo");
        let raw = fs::read(&path).unwrap();
        if cfg!(windows) {
            assert!(!raw.windows(7).any(|w| w == b"segredo"), "DPAPI não deve deixar o segredo em claro");
        }
        fs::remove_file(&path).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn purpose_is_bound() {
        let path = temp_path("bound.bin");
        save(&path, "identidade", b"x").unwrap();
        assert!(matches!(load(&path, "tls"), Err(StoreError::Dpapi(_))));
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn rejects_garbage() {
        let path = temp_path("garbage.bin");
        fs::write(&path, b"nada a ver").unwrap();
        assert!(matches!(load(&path, "teste"), Err(StoreError::BadFormat(_))));
        fs::remove_file(&path).unwrap();
    }
}
