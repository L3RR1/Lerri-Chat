use std::path::PathBuf;

/// Diretório de dados local (identidade, certificado do relay, config).
///
/// - Windows: `%APPDATA%\LerriChat`
/// - Outros: `$XDG_DATA_HOME/lerrichat` ou `~/.local/share/lerrichat`
pub fn default_data_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return std::env::var_os("APPDATA").map(|p| PathBuf::from(p).join("LerriChat"));
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("lerrichat"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/lerrichat"))
}
