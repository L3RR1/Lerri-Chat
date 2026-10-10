//! TLS 1.3 do QUIC entre cliente e relay (SPEC-02 §4.4).
//!
//! Não há CA nem domínio: o relay usa um certificado autoassinado e o cliente
//! o valida pelo **fingerprint SHA-256** fixado (vindo do convite, nas fases
//! seguintes; da linha de comando, na Fase 0). Certificado diferente → recusa.

use std::fmt;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, bail};
use lerri_core::ids::{from_hex, hex};
use lerri_core::protocol::{ALPN, RELAY_SERVER_NAME};
use lerri_crypto::secret_store;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};

const FILE_NAME: &str = "relay-tls.bin";
const PURPOSE: &str = "lerrichat/relay-tls/v1";

/// SHA-256 do certificado DER do relay.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    pub fn of(cert: &CertificateDer<'_>) -> Self {
        Self(Sha256::digest(cert.as_ref()).into())
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex(&self.0))
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

impl FromStr for Fingerprint {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        let bytes = from_hex(s).context("fingerprint deve ser hexadecimal")?;
        let arr: [u8; 32] = bytes.try_into().ok().context("fingerprint deve ter 64 dígitos hex")?;
        Ok(Self(arr))
    }
}

/// Certificado + chave do relay, persistidos para o fingerprint não mudar
/// entre reinícios (senão todo convite emitido ficaria inválido).
pub struct RelayCert {
    cert: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
}

impl RelayCert {
    pub fn generate() -> anyhow::Result<Self> {
        let ck = rcgen::generate_simple_self_signed(vec![RELAY_SERVER_NAME.to_string()])?;
        Ok(Self {
            cert: ck.cert.der().clone(),
            key: PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()),
        })
    }

    pub fn load_or_create(data_dir: &Path) -> anyhow::Result<Self> {
        let path = data_dir.join(FILE_NAME);
        if let Some(blob) = secret_store::load(&path, PURPOSE)? {
            return Self::decode(&blob).with_context(|| format!("{} corrompido", path.display()));
        }
        let rc = Self::generate()?;
        secret_store::save(&path, PURPOSE, &rc.encode())?;
        Ok(rc)
    }

    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint::of(&self.cert)
    }

    // Formato: u32 BE (tamanho do certificado) || certificado DER || chave PKCS#8.
    fn encode(&self) -> zeroize::Zeroizing<Vec<u8>> {
        let (c, k) = (self.cert.as_ref(), self.key.secret_pkcs8_der());
        let mut out = Vec::with_capacity(4 + c.len() + k.len());
        out.extend_from_slice(&(c.len() as u32).to_be_bytes());
        out.extend_from_slice(c);
        out.extend_from_slice(k);
        zeroize::Zeroizing::new(out)
    }

    fn decode(blob: &[u8]) -> anyhow::Result<Self> {
        let Some((len, rest)) = blob.split_first_chunk::<4>() else { bail!("curto demais") };
        let len = u32::from_be_bytes(*len) as usize;
        if rest.len() <= len {
            bail!("tamanho do certificado inválido");
        }
        let (cert, key) = rest.split_at(len);
        Ok(Self { cert: CertificateDer::from(cert.to_vec()), key: PrivatePkcs8KeyDer::from(key.to_vec()) })
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

pub(crate) fn server_crypto(rc: &RelayCert) -> anyhow::Result<rustls::ServerConfig> {
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![rc.cert.clone()], PrivateKeyDer::Pkcs8(rc.key.clone_key()))?;
    cfg.alpn_protocols = vec![ALPN.to_vec()];
    Ok(cfg)
}

pub(crate) fn client_crypto(pinned: Fingerprint) -> anyhow::Result<rustls::ClientConfig> {
    let provider = provider();
    let verifier = PinnedVerifier { pinned, algs: provider.signature_verification_algorithms };
    let mut cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    cfg.alpn_protocols = vec![ALPN.to_vec()];
    Ok(cfg)
}

/// Aceita exatamente um certificado: o do fingerprint fixado. A assinatura do
/// handshake continua sendo verificada, então quem não tem a chave privada
/// correspondente não passa nem reapresentando o certificado.
#[derive(Debug)]
struct PinnedVerifier {
    pinned: Fingerprint,
    algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if Fingerprint::of(end_entity) == self.pinned {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // Só TLS 1.3 é habilitado (QUIC exige).
        Err(rustls::Error::PeerIncompatible(rustls::PeerIncompatible::Tls12NotOffered))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cert_encoding_roundtrip() {
        let rc = RelayCert::generate().unwrap();
        let back = RelayCert::decode(&rc.encode()).unwrap();
        assert_eq!(rc.fingerprint(), back.fingerprint());
        assert_eq!(rc.key.secret_pkcs8_der(), back.key.secret_pkcs8_der());
    }

    #[test]
    fn fingerprint_parse() {
        let fp = RelayCert::generate().unwrap().fingerprint();
        assert_eq!(fp.to_string().parse::<Fingerprint>().unwrap(), fp);
        assert!("abcd".parse::<Fingerprint>().is_err());
    }
}
