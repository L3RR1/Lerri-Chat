//! Tipos compartilhados por todos os crates: IDs, mensagens de protocolo e
//! caminhos locais. Sem dependência de plataforma (SPEC-01 §3).

pub mod ids;
pub mod paths;
pub mod protocol;

pub use ids::{MemberId, RoomId};
