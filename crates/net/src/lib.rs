//! Transporte (SPEC-03): QUIC sobre UDP via iroh, só com relay próprio (D-01).
//!
//! Regras que valem para todo endpoint criado aqui (SPEC-03 §4.4):
//! - `presets::Minimal`: nenhum DNS, pkarr ou relay da n0.
//! - Relay iroh só o ponto de encontro do próprio usuário, com a chave TLS
//!   fixada pelo convite; sem ponto de encontro, nenhum relay.
//! - A chave de transporte nunca é a chave de identidade do membro.

pub mod dns;
pub mod endpoint;
pub mod framing;
pub mod keys;
pub mod pin;
pub mod reach;

pub use endpoint::{EndpointOpts, bind, endpoint_addr};
pub use iroh;
