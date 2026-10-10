//! Transporte QUIC (SPEC-03). NAT traversal entra na Fase 1, após a decisão
//! D-01 (iroh × implementação própria).

pub mod endpoint;
pub mod framing;
pub mod tls;

pub use endpoint::{client_endpoint, server_endpoint};
pub use quinn;
pub use tls::{Fingerprint, RelayCert};
