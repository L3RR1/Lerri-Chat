//! Mensagens trocadas sobre streams QUIC confiáveis.
//!
//! Cada frame no fio é `u32 big-endian (tamanho) || postcard(Frame)`.

use serde::{Deserialize, Serialize};

/// ALPN do protocolo. Muda quando o formato do fio quebra compatibilidade.
pub const ALPN: &[u8] = b"lerrichat/0";

/// Nome TLS do relay. O cliente não confia em CA: valida o certificado pelo
/// fingerprint fixado (ver `lerri-net`), então o nome é só um rótulo fixo.
pub const RELAY_SERVER_NAME: &str = "lerrichat-relay";

/// Tamanho máximo de um frame, para que um peer não force alocações gigantes.
pub const MAX_FRAME_LEN: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Frame {
    /// Fase 0: o relay devolve o mesmo payload.
    Echo { payload: Vec<u8> },
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("frame de {0} bytes excede o máximo de {MAX_FRAME_LEN}")]
    TooLarge(usize),
    #[error("frame malformado: {0}")]
    Malformed(#[from] postcard::Error),
}

impl Frame {
    pub fn encode(&self) -> Result<Vec<u8>, CodecError> {
        let body = postcard::to_allocvec(self)?;
        if body.len() > MAX_FRAME_LEN {
            return Err(CodecError::TooLarge(body.len()));
        }
        let mut out = Vec::with_capacity(4 + body.len());
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// Decodifica o corpo (sem o prefixo de tamanho).
    pub fn decode_body(body: &[u8]) -> Result<Self, CodecError> {
        if body.len() > MAX_FRAME_LEN {
            return Err(CodecError::TooLarge(body.len()));
        }
        Ok(postcard::from_bytes(body)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let frame = Frame::Echo { payload: b"oi".to_vec() };
        let wire = frame.encode().unwrap();
        let len = u32::from_be_bytes(wire[..4].try_into().unwrap()) as usize;
        assert_eq!(len, wire.len() - 4);
        assert_eq!(Frame::decode_body(&wire[4..]).unwrap(), frame);
    }

    #[test]
    fn rejects_oversized() {
        let frame = Frame::Echo { payload: vec![0; MAX_FRAME_LEN + 1] };
        assert!(matches!(frame.encode(), Err(CodecError::TooLarge(_))));
    }
}
