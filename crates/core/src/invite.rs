//! Convites e links `lerrichat://` (SPEC-02 §3, SPEC-03 §4.1).
//!
//! Três tipos de link:
//!
//! - `lerrichat://join?r=&h=&t=&s=&f=&e=`: convite de uso único para uma sala.
//! - `lerrichat://relay?h=`: endereço público de um relay (ponto de encontro
//!   para o x1 assistido). Não dá acesso a nada.
//! - `lerrichat://admin?h=&a=`: endereço do relay + segredo de administração,
//!   que permite criar salas nele. Só para quem opera o servidor.
//!
//! Campos binários vão em base64url sem padding. `h` é um [`HostAddr`]
//! serializado com postcard, porque carrega uma lista de candidatos.

use std::fmt;
use std::net::{IpAddr, SocketAddr};

use data_encoding::BASE64URL_NOPAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{MemberId, RoomId};

pub const SCHEME: &str = "lerrichat";

/// Validade padrão de um convite (SPEC-02 §3.2).
pub const DEFAULT_INVITE_TTL_SECS: u64 = 24 * 3600;
/// Validade máxima que o relay aceita registrar.
pub const MAX_INVITE_TTL_SECS: u64 = 7 * 24 * 3600;

/// Um caminho para chegar ao host da sala (SPEC-03 §4.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Candidate {
    Ip(SocketAddr),
    /// Hostname resolvido pelo DNS do sistema na hora de conectar.
    Name(String, u16),
}

impl fmt::Display for Candidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ip(a) => write!(f, "{a}"),
            Self::Name(h, p) => write!(f, "{h}:{p}"),
        }
    }
}

/// Relay iroh usado como ponto de encontro: coordena o hole punching e
/// repassa o tráfego cifrado quando o caminho direto falha (SPEC-03 §4.3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeetingPoint {
    pub host: String,
    /// Porta TCP do HTTPS do relay iroh.
    pub port: u16,
    /// Porta UDP da descoberta de endereço público (QAD).
    pub quic_port: u16,
    /// Chave Ed25519 do certificado TLS do ponto de encontro (fixada).
    pub key: [u8; 32],
}

impl MeetingPoint {
    pub fn url(&self) -> String {
        match self.host.parse::<IpAddr>() {
            Ok(IpAddr::V6(ip)) => format!("https://[{ip}]:{}", self.port),
            _ => format!("https://{}:{}", self.host, self.port),
        }
    }
}

/// Como achar o host de uma sala: o relay do modo servidor ou o criador no x1.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostAddr {
    /// Chave pública de transporte (iroh) do host. Fixa a conexão: só quem
    /// tem a chave privada correspondente completa o handshake.
    pub key: [u8; 32],
    pub candidates: Vec<Candidate>,
    pub meet: Option<MeetingPoint>,
}

impl HostAddr {
    fn encode(&self) -> String {
        BASE64URL_NOPAD.encode(&postcard::to_allocvec(self).expect("serializar HostAddr não falha"))
    }

    fn decode(s: &str) -> Result<Self, LinkError> {
        postcard::from_bytes(&b64(s)?).map_err(|_| LinkError::Field("h"))
    }
}

/// Token de roteamento `t` (128 bits). O relay só guarda o hash.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteToken(pub [u8; 16]);

impl RouteToken {
    pub fn hash(&self) -> TokenHash {
        let mut h = Sha256::new();
        h.update(b"lerrichat/route-token/v1");
        h.update(self.0);
        TokenHash(h.finalize().into())
    }
}

impl fmt::Debug for RouteToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RouteToken(..)")
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TokenHash(pub [u8; 32]);

impl fmt::Debug for TokenHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TokenHash({})", crate::ids::hex(&self.0[..6]))
    }
}

/// Segredo de admissão `s` (256 bits). Nunca vai para o relay; a prova sobre
/// ele entra na Fase 2, com o MLS.
#[derive(Clone, PartialEq, Eq)]
pub struct AdmissionSecret(pub [u8; 32]);

impl fmt::Debug for AdmissionSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AdmissionSecret(..)")
    }
}

/// Segredo que autoriza criar salas num relay de modo servidor.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminKey(pub [u8; 32]);

impl fmt::Debug for AdminKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AdminKey(..)")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invite {
    pub room: RoomId,
    pub host: HostAddr,
    pub token: RouteToken,
    pub secret: AdmissionSecret,
    /// Quem convidou (`f`), para o novato conferir quem o admitiu.
    pub inviter: MemberId,
    /// Validade em segundos Unix (`e`).
    pub expires_at: u64,
}

/// Link `lerrichat://relay` ou `lerrichat://admin`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayLink {
    pub host: HostAddr,
    pub admin: Option<AdminKey>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    Join(Invite),
    Relay(RelayLink),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LinkError {
    #[error("não é um link lerrichat://")]
    Scheme,
    #[error("tipo de link desconhecido: {0}")]
    Kind(String),
    #[error("campo `{0}` ausente")]
    Missing(&'static str),
    #[error("campo `{0}` inválido")]
    Field(&'static str),
}

impl Invite {
    pub fn to_link(&self) -> String {
        format!(
            "{SCHEME}://join?r={}&h={}&t={}&s={}&f={}&e={}",
            BASE64URL_NOPAD.encode(&self.room.0),
            self.host.encode(),
            BASE64URL_NOPAD.encode(&self.token.0),
            BASE64URL_NOPAD.encode(&self.secret.0),
            BASE64URL_NOPAD.encode(&self.inviter.0),
            self.expires_at,
        )
    }
}

impl RelayLink {
    pub fn to_link(&self) -> String {
        match &self.admin {
            None => format!("{SCHEME}://relay?h={}", self.host.encode()),
            Some(a) => format!("{SCHEME}://admin?h={}&a={}", self.host.encode(), BASE64URL_NOPAD.encode(&a.0)),
        }
    }
}

impl Link {
    /// Aceita o link como colado pelo usuário ou como o Windows o entrega ao
    /// handler do protocolo (com aspas, barra extra ou espaços em volta).
    pub fn parse(s: &str) -> Result<Self, LinkError> {
        let s = s.trim().trim_matches('"');
        let rest = s
            .strip_prefix(SCHEME)
            .and_then(|r| r.strip_prefix("://"))
            .ok_or(LinkError::Scheme)?;
        let (kind, query) = rest.split_once('?').unwrap_or((rest, ""));
        let kind = kind.trim_end_matches('/');
        let get = |key: &'static str| {
            query
                .split('&')
                .filter_map(|kv| kv.split_once('='))
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v)
                .ok_or(LinkError::Missing(key))
        };
        match kind {
            "join" => Ok(Link::Join(Invite {
                room: RoomId(fixed(get("r")?, "r")?),
                host: HostAddr::decode(get("h")?)?,
                token: RouteToken(fixed(get("t")?, "t")?),
                secret: AdmissionSecret(fixed(get("s")?, "s")?),
                inviter: MemberId(fixed(get("f")?, "f")?),
                expires_at: get("e")?.parse().map_err(|_| LinkError::Field("e"))?,
            })),
            "relay" => Ok(Link::Relay(RelayLink { host: HostAddr::decode(get("h")?)?, admin: None })),
            "admin" => Ok(Link::Relay(RelayLink {
                host: HostAddr::decode(get("h")?)?,
                admin: Some(AdminKey(fixed(get("a")?, "a")?)),
            })),
            other => Err(LinkError::Kind(other.to_string())),
        }
    }
}

fn b64(s: &str) -> Result<Vec<u8>, LinkError> {
    BASE64URL_NOPAD.decode(s.as_bytes()).map_err(|_| LinkError::Field("base64"))
}

fn fixed<const N: usize>(s: &str, field: &'static str) -> Result<[u8; N], LinkError> {
    BASE64URL_NOPAD
        .decode(s.as_bytes())
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or(LinkError::Field(field))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> HostAddr {
        HostAddr {
            key: [7; 32],
            candidates: vec![
                Candidate::Ip("[2804:14c::1]:47123".parse().unwrap()),
                Candidate::Ip("192.168.0.10:47123".parse().unwrap()),
                Candidate::Name("casa.example.org".into(), 47123),
            ],
            meet: Some(MeetingPoint { host: "2804:14c::1".into(), port: 47123, quic_port: 47123, key: [9; 32] }),
        }
    }

    #[test]
    fn invite_roundtrip() {
        let inv = Invite {
            room: RoomId([1; 16]),
            host: host(),
            token: RouteToken([2; 16]),
            secret: AdmissionSecret([3; 32]),
            inviter: MemberId([4; 32]),
            expires_at: 1_800_000_000,
        };
        let link = inv.to_link();
        assert!(link.starts_with("lerrichat://join?r="));
        assert_eq!(Link::parse(&link).unwrap(), Link::Join(inv.clone()));
        // Como o Windows entrega ao handler.
        let quoted = format!("\"{}\"", link.replacen("://join?", "://join/?", 1));
        assert_eq!(Link::parse(&quoted).unwrap(), Link::Join(inv));
    }

    #[test]
    fn relay_links() {
        let public = RelayLink { host: host(), admin: None };
        assert_eq!(Link::parse(&public.to_link()).unwrap(), Link::Relay(public));
        let admin = RelayLink { host: host(), admin: Some(AdminKey([5; 32])) };
        let link = admin.to_link();
        assert!(link.starts_with("lerrichat://admin?"));
        assert_eq!(Link::parse(&link).unwrap(), Link::Relay(admin));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(Link::parse("https://x"), Err(LinkError::Scheme));
        assert_eq!(Link::parse("lerrichat://nada"), Err(LinkError::Kind("nada".into())));
        assert_eq!(Link::parse("lerrichat://join?r=AAAA"), Err(LinkError::Field("r")));
        assert!(matches!(Link::parse("lerrichat://relay?h=!!"), Err(LinkError::Field(_))));
    }

    #[test]
    fn meeting_point_url_brackets_ipv6() {
        let m = MeetingPoint { host: "2804::1".into(), port: 1, quic_port: 1, key: [0; 32] };
        assert_eq!(m.url(), "https://[2804::1]:1");
        let m = MeetingPoint { host: "1.2.3.4".into(), port: 1, quic_port: 1, key: [0; 32] };
        assert_eq!(m.url(), "https://1.2.3.4:1");
    }

    #[test]
    fn token_hash_is_domain_separated() {
        let t = RouteToken([0; 16]);
        assert_ne!(t.hash().0, <[u8; 32]>::from(Sha256::digest([0u8; 16])));
    }
}
