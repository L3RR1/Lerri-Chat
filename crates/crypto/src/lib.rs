//! Identidade, segredos em repouso e (nas próximas fases) MLS, SFrame e
//! convites (SPEC-02).

pub mod identity;
pub mod secret_store;

pub use identity::Identity;
pub use lerri_core::ids::hex;
