//! Chaves de transporte (iroh). Separadas da identidade do membro para o
//! relay não ligar a mesma pessoa entre salas (SPEC-03 §4.4).

use std::path::Path;

use anyhow::Context;
use iroh::SecretKey;
use lerri_crypto::secret_store;

/// Chave nova e descartável: cada sessão de cliente usa uma.
pub fn ephemeral() -> SecretKey {
    SecretKey::from_bytes(&lerri_crypto::random())
}

/// Chave persistida em `data_dir/file`, criada no primeiro uso. O relay usa
/// para a chave não mudar entre reinícios: ela está fixada nos convites.
pub fn load_or_create(data_dir: &Path, file: &str, purpose: &str) -> anyhow::Result<SecretKey> {
    let path = data_dir.join(file);
    if let Some(bytes) = secret_store::load(&path, purpose)? {
        let bytes: &[u8; 32] = bytes.as_slice().try_into().ok().with_context(|| format!("{} corrompido", path.display()))?;
        return Ok(SecretKey::from_bytes(bytes));
    }
    let key = ephemeral();
    secret_store::save(&path, purpose, zeroize::Zeroizing::new(key.to_bytes()).as_slice())?;
    Ok(key)
}
