//! Tipos compartilhados por todos os crates: IDs, convites, mensagens de
//! protocolo e caminhos locais. Sem dependência de plataforma (SPEC-01 §3).

pub mod ids;
pub mod invite;
pub mod media;
pub mod paths;
pub mod protocol;

pub use ids::{MemberId, RoomId};

/// Segundos Unix agora.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}
