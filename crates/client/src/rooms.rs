//! Salas salvas no PC do membro, para voltar a elas sem convite. Só salas de
//! modo servidor: a sala x1 acaba quando o criador fecha o app.
//!
//! Fica cifrado em repouso (DPAPI), porque revela de quais salas a pessoa
//! participa e onde estão os hosts.

use std::path::Path;

use anyhow::Context;
use lerri_core::RoomId;
use lerri_core::invite::HostAddr;
use lerri_core::protocol::{Role, RoomMode};
use lerri_crypto::secret_store;
use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "rooms.bin";
const PURPOSE: &str = "lerrichat/rooms/v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedRoom {
    pub room: RoomId,
    pub host: HostAddr,
    pub mode: RoomMode,
    pub role: Role,
}

pub fn load(data_dir: &Path) -> anyhow::Result<Vec<SavedRoom>> {
    let path = data_dir.join(FILE_NAME);
    match secret_store::load(&path, PURPOSE)? {
        Some(bytes) => postcard::from_bytes(&bytes).with_context(|| format!("{} corrompido", path.display())),
        None => Ok(Vec::new()),
    }
}

/// Adiciona ou atualiza a sala.
pub fn remember(data_dir: &Path, room: SavedRoom) -> anyhow::Result<()> {
    let mut all = load(data_dir)?;
    all.retain(|r| r.room != room.room);
    all.push(room);
    std::fs::create_dir_all(data_dir)?;
    secret_store::save(&data_dir.join(FILE_NAME), PURPOSE, &postcard::to_allocvec(&all)?)?;
    Ok(())
}

/// Procura por prefixo do id em hexadecimal.
pub fn find(data_dir: &Path, prefix: &str) -> anyhow::Result<SavedRoom> {
    let prefix = prefix.to_ascii_lowercase();
    let mut hits = load(data_dir)?.into_iter().filter(|r| r.room.to_string().starts_with(&prefix));
    let first = hits.next().with_context(|| format!("nenhuma sala salva começa com {prefix}"))?;
    anyhow::ensure!(hits.next().is_none(), "mais de uma sala começa com {prefix}; use mais dígitos");
    Ok(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remember_and_find() {
        let dir = std::env::temp_dir().join(format!("lerri-rooms-{}", lerri_core::ids::hex(&lerri_crypto::random::<8>())));
        let host = HostAddr { key: [1; 32], candidates: vec![], meet: None };
        let a = SavedRoom { room: RoomId([0xab; 16]), host: host.clone(), mode: RoomMode::Server, role: Role::Owner };
        let b = SavedRoom { room: RoomId([0xcd; 16]), host, mode: RoomMode::Server, role: Role::Member };
        remember(&dir, a.clone()).unwrap();
        remember(&dir, b.clone()).unwrap();
        remember(&dir, a.clone()).unwrap();
        assert_eq!(load(&dir).unwrap().len(), 2);
        assert_eq!(find(&dir, "ABAB").unwrap(), a);
        assert_eq!(find(&dir, "cd").unwrap(), b);
        assert!(find(&dir, "ff").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
