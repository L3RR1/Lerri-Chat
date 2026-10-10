//! Identidade, segredos em repouso e (nas próximas fases) MLS, SFrame e
//! convites (SPEC-02).

pub mod group;
pub mod identity;
pub mod secret_store;

pub use identity::Identity;
pub use lerri_core::ids::hex;

/// Bytes aleatórios do RNG do sistema.
pub fn random<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).expect("RNG do sistema indisponível");
    out
}

/// Compara em tempo constante (segredos de administração, tokens).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
