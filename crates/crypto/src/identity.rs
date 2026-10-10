//! Identidade local: par de chaves Ed25519 gerado no primeiro uso (RF-01).

use std::path::Path;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use lerri_core::MemberId;
use lerri_core::protocol::{MemberProof, hello_message};
use sha2::{Digest, Sha256};

use crate::secret_store::{self, StoreError};

const FILE_NAME: &str = "identity.bin";
const PURPOSE: &str = "lerrichat/identity/v1";

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("arquivo de identidade com tamanho inválido")]
    BadKey,
    #[error("gerador aleatório do sistema falhou: {0}")]
    Rng(getrandom::Error),
}

pub struct Identity {
    key: SigningKey,
}

impl Identity {
    /// Gera uma identidade nova a partir do RNG do sistema.
    pub fn generate() -> Result<Self, IdentityError> {
        let mut secret = zeroize::Zeroizing::new([0u8; 32]);
        getrandom::fill(secret.as_mut()).map_err(IdentityError::Rng)?;
        Ok(Self { key: SigningKey::from_bytes(&secret) })
    }

    /// Carrega a identidade de `data_dir`, criando uma se ainda não existir.
    /// O booleano indica se ela acabou de ser criada.
    pub fn load_or_create(data_dir: &Path) -> Result<(Self, bool), IdentityError> {
        let path = data_dir.join(FILE_NAME);
        if let Some(bytes) = secret_store::load(&path, PURPOSE)? {
            let secret: &[u8; 32] = bytes.as_slice().try_into().map_err(|_| IdentityError::BadKey)?;
            return Ok((Self { key: SigningKey::from_bytes(secret) }, false));
        }
        let id = Self::generate()?;
        secret_store::save(&path, PURPOSE, id.key.as_bytes())?;
        Ok((id, true))
    }

    pub fn member_id(&self) -> MemberId {
        member_id_of(&self.key.verifying_key())
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.key.verifying_key()
    }

    /// Semente Ed25519 (32 bytes). Só para o assinante MLS da própria pessoa.
    pub(crate) fn secret_bytes(&self) -> zeroize::Zeroizing<[u8; 32]> {
        zeroize::Zeroizing::new(self.key.to_bytes())
    }

    pub fn sign(&self, msg: &[u8]) -> Signature {
        self.key.sign(msg)
    }

    /// Assina a entrada nesta conexão (ver [`hello_message`]).
    pub fn prove(&self, host_key: &[u8; 32], client_key: &[u8; 32]) -> MemberProof {
        MemberProof {
            identity_key: self.key.verifying_key().to_bytes(),
            signature: self.sign(&hello_message(host_key, client_key)).to_bytes().to_vec(),
        }
    }
}

/// Confere uma [`MemberProof`] e devolve o `MemberId` provado.
pub fn verify_proof(proof: &MemberProof, host_key: &[u8; 32], client_key: &[u8; 32]) -> Option<MemberId> {
    let key = VerifyingKey::from_bytes(&proof.identity_key).ok()?;
    let sig = Signature::from_slice(&proof.signature).ok()?;
    key.verify_strict(&hello_message(host_key, client_key), &sig).ok()?;
    Some(member_id_of(&key))
}

pub fn member_id_of(key: &VerifyingKey) -> MemberId {
    MemberId(Sha256::digest(key.as_bytes()).into())
}

pub fn verify(key: &VerifyingKey, msg: &[u8], sig: &Signature) -> bool {
    key.verify(msg, sig).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_between_loads() {
        let mut rnd = [0u8; 8];
        getrandom::fill(&mut rnd).unwrap();
        let dir = std::env::temp_dir().join(format!("lerri-id-{}", crate::hex(&rnd)));

        let (a, created) = Identity::load_or_create(&dir).unwrap();
        assert!(created);
        let (b, created) = Identity::load_or_create(&dir).unwrap();
        assert!(!created);
        assert_eq!(a.member_id(), b.member_id());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sign_and_verify() {
        let id = Identity::generate().unwrap();
        let sig = id.sign(b"oi");
        assert!(verify(&id.verifying_key(), b"oi", &sig));
        assert!(!verify(&id.verifying_key(), b"tchau", &sig));
        assert_ne!(id.member_id(), Identity::generate().unwrap().member_id());
    }

    #[test]
    fn proof_is_bound_to_connection() {
        let id = Identity::generate().unwrap();
        let proof = id.prove(&[1; 32], &[2; 32]);
        assert_eq!(verify_proof(&proof, &[1; 32], &[2; 32]), Some(id.member_id()));
        assert_eq!(verify_proof(&proof, &[1; 32], &[3; 32]), None);
        assert_eq!(verify_proof(&proof, &[9; 32], &[2; 32]), None);
        let mut bad = proof.clone();
        bad.identity_key = Identity::generate().unwrap().verifying_key().to_bytes();
        assert_eq!(verify_proof(&bad, &[1; 32], &[2; 32]), None);
    }
}
