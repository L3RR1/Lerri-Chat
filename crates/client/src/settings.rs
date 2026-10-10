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
    /// Supressão forte de ruído na voz (RNNoise, SPEC-04 §1).
    pub strong_ns: bool,
}

/// Formato até a Fase 2, sem as opções de voz.
#[derive(Deserialize)]
struct SettingsV1 {
    nickname: String,
    keep_history: bool,
}

impl Settings {
    pub fn load(data_dir: &Path) -> Self {
        secret_store::load(&data_dir.join(FILE_NAME), PURPOSE)
            .ok()
            .flatten()
            .and_then(|b| {
                postcard::from_bytes(&b).ok().or_else(|| {
                    let v1: SettingsV1 = postcard::from_bytes(&b).ok()?;
                    Some(Self { nickname: v1.nickname, keep_history: v1.keep_history, ..Self::default() })
                })
            })
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
    fn reads_settings_from_before_voice() {
        let dir = std::env::temp_dir().join(format!("lerri-settings-{}", lerri_core::ids::hex(&lerri_crypto::random::<6>())));
        std::fs::create_dir_all(&dir).unwrap();
        #[derive(serde::Serialize)]
        struct Old<'a> {
            nickname: &'a str,
            keep_history: bool,
        }
        let old = postcard::to_allocvec(&Old { nickname: "Lerri", keep_history: true }).unwrap();
        super::secret_store::save(&dir.join(super::FILE_NAME), super::PURPOSE, &old).unwrap();
        let s = super::Settings::load(&dir);
        assert_eq!((s.nickname.as_str(), s.keep_history, s.strong_ns), ("Lerri", true, false));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn nick_is_cleaned() {
        assert_eq!(super::clean_nick("  Lerri\n\u{7} "), "Lerri");
        assert_eq!(super::clean_nick(&"x".repeat(50)).chars().count(), super::NICK_MAX_CHARS);
    }
}
