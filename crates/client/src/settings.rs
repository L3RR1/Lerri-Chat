//! Preferências locais (SPEC-02 §1: apelido opcional, local e livre).

use std::path::Path;

use lerri_crypto::secret_store;
use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "settings.bin";
const PURPOSE: &str = "lerrichat/settings/v1";
pub const NICK_MAX_CHARS: usize = 32;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Apelido mostrado aos outros membros (vai cifrado pelo MLS).
    pub nickname: String,
    /// Guarda o histórico do chat neste PC (cifrado). Desligado por padrão
    /// (SPEC-02 §8).
    pub keep_history: bool,
}

impl Settings {
    pub fn load(data_dir: &Path) -> Self {
        secret_store::load(&data_dir.join(FILE_NAME), PURPOSE)
            .ok()
            .flatten()
            .and_then(|b| postcard::from_bytes(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, data_dir: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(data_dir)?;
        secret_store::save(&data_dir.join(FILE_NAME), PURPOSE, &postcard::to_allocvec(self)?)?;
        Ok(())
    }
}

/// Apelido limpo: sem controle, sem espaços nas pontas, no máximo 32 caracteres.
pub fn clean_nick(raw: &str) -> String {
    raw.chars().filter(|c| !c.is_control()).collect::<String>().trim().chars().take(NICK_MAX_CHARS).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn nick_is_cleaned() {
        assert_eq!(super::clean_nick("  Lerri\n\u{7} "), "Lerri");
        assert_eq!(super::clean_nick(&"x".repeat(50)).chars().count(), super::NICK_MAX_CHARS);
    }
}
