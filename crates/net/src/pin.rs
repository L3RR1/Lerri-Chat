//! TLS do ponto de encontro (o relay iroh): sem CA nem domínio. O certificado
//! é autoassinado com uma chave Ed25519 e o cliente aceita **só** quem prova
//! ter a chave fixada no convite (SPEC-02 §4.4).

use std::sync::Arc;

use iroh::SecretKey;
use iroh::tls::CaTlsConfig;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, SubjectPublicKeyInfoDer, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

// Prefixos DER fixos de uma chave Ed25519 (RFC 8410).
const PKCS8_PREFIX: [u8; 16] = [0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20];
const SPKI_PREFIX: [u8; 12] = [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Configuração TLS 1.3 do servidor do ponto de encontro.
pub fn server_tls(key: &SecretKey) -> anyhow::Result<rustls::ServerConfig> {
    let pkcs8 = PrivatePkcs8KeyDer::from([&PKCS8_PREFIX[..], &key.to_bytes()].concat());
    let kp = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(&pkcs8, &rcgen::PKCS_ED25519)?;
    let cert = rcgen::CertificateParams::new(vec!["lerrichat-meet".to_string()])?.self_signed(&kp)?;
    Ok(rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], PrivateKeyDer::Pkcs8(pkcs8))?)
}

/// Configuração de CA para o endpoint iroh: só aceita o ponto de encontro
/// cuja chave pública é `key`.
pub fn ca_config(key: [u8; 32]) -> CaTlsConfig {
    CaTlsConfig::custom_server_cert_verifier(Arc::new(move |provider: Arc<CryptoProvider>| {
        let verifier: Arc<dyn ServerCertVerifier> =
            Arc::new(KeyPinned { spki: [&SPKI_PREFIX[..], &key].concat(), algs: provider.signature_verification_algorithms });
        Ok(verifier)
    }))
}

/// Ignora o conteúdo do certificado e verifica a assinatura do handshake
/// contra a chave fixada, como em chaves públicas cruas (RFC 7250). Quem não
/// tem a chave privada não completa o handshake.
#[derive(Debug)]
struct KeyPinned {
    spki: Vec<u8>,
    algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for KeyPinned {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // Sem a assinatura do TLS 1.3 sobre o transcript, o pin não vale nada.
        Err(rustls::Error::PeerIncompatible(rustls::PeerIncompatible::Tls12NotOffered))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        _cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        let spki = SubjectPublicKeyInfoDer::from(self.spki.as_slice());
        rustls::crypto::verify_tls13_signature_with_raw_key(message, &spki, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}
