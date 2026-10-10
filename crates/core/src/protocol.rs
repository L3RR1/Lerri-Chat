//! Mensagens do stream de controle entre membro e relay.
//!
//! Cada frame no fio é `u32 big-endian (tamanho) || postcard(Frame)`.

use serde::{Deserialize, Serialize};

use crate::invite::{AdminKey, RouteToken, TokenHash};
use crate::{MemberId, RoomId};

/// ALPN do protocolo. Muda quando o formato do fio quebra compatibilidade.
pub const ALPN: &[u8] = b"lerrichat/1";

/// Tamanho máximo de um frame, para que um peer não force alocações gigantes.
pub const MAX_FRAME_LEN: usize = 64 * 1024;

/// Contexto assinado pela identidade do membro ao abrir uma conexão.
const HELLO_CONTEXT: &[u8] = b"lerrichat/hello/v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoomMode {
    /// P2P: o criador é dono, membro e relay ao mesmo tempo (SPEC-03 §3).
    X1,
    /// Relay dedicado (SFU).
    Server,
}

/// Papel de um membro dentro da sala (SPEC-02 §2). O relay não é membro.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Owner,
    Member,
}

/// O que a conexão quer fazer, enviado no primeiro frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Auth {
    /// Cria uma sala e vira dono dela.
    CreateRoom { admin: AdminKey, mode: RoomMode },
    /// Entra pela primeira vez com um convite. O relay consome o `t`.
    Join { room: RoomId, token: RouteToken },
    /// Volta a uma sala da qual já é membro.
    Rejoin { room: RoomId },
}

/// Prova de que o dono da identidade autorizou **esta** conexão: assinatura
/// Ed25519 sobre [`hello_message`], que amarra as chaves de transporte dos
/// dois lados. Não serve para outra conexão.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberProof {
    pub identity_key: [u8; 32],
    pub signature: Vec<u8>,
}

/// Mensagem assinada em [`MemberProof`].
pub fn hello_message(host_key: &[u8; 32], client_key: &[u8; 32]) -> Vec<u8> {
    [HELLO_CONTEXT, host_key, client_key].concat()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum Refusal {
    #[error("prova de identidade inválida")]
    BadProof,
    #[error("segredo de administração do relay incorreto")]
    BadAdmin,
    #[error("a sala não existe neste relay")]
    NoSuchRoom,
    #[error("convite desconhecido ou revogado")]
    InviteUnknown,
    #[error("convite já foi usado")]
    InviteUsed,
    #[error("convite expirado")]
    InviteExpired,
    #[error("você não é membro desta sala")]
    NotMember,
    #[error("só o dono da sala pode fazer isso")]
    NotOwner,
    #[error("mensagem fora de ordem ou inválida")]
    Protocol,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Frame {
    // Cliente → relay.
    Hello { auth: Auth, proof: MemberProof },
    /// Registra o hash de um convite novo (só o dono, SPEC-02 §2).
    IssueInvite { token_hash: TokenHash, expires_at: u64 },

    // Relay → cliente.
    Welcome { room: RoomId, mode: RoomMode, role: Role, online: Vec<MemberId> },
    Refused(Refusal),
    InviteRegistered { token_hash: TokenHash },
    /// Alguém ficou online. `first_time` quando acabou de entrar pelo convite.
    MemberOnline { member: MemberId, first_time: bool },
    MemberOffline { member: MemberId },
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
        let frame = Frame::MemberOnline { member: MemberId([1; 32]), first_time: true };
        let wire = frame.encode().unwrap();
        let len = u32::from_be_bytes(wire[..4].try_into().unwrap()) as usize;
        assert_eq!(len, wire.len() - 4);
        assert_eq!(Frame::decode_body(&wire[4..]).unwrap(), frame);
    }

    #[test]
    fn rejects_oversized() {
        let proof = MemberProof { identity_key: [0; 32], signature: vec![0; MAX_FRAME_LEN + 1] };
        let frame = Frame::Hello { auth: Auth::Rejoin { room: RoomId([0; 16]) }, proof };
        assert!(matches!(frame.encode(), Err(CodecError::TooLarge(_))));
    }

    #[test]
    fn hello_message_binds_both_keys() {
        assert_ne!(hello_message(&[1; 32], &[2; 32]), hello_message(&[2; 32], &[1; 32]));
    }
}
