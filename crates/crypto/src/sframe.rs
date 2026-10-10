//! SFrame (RFC 9605) com chaves do MLS (SPEC-02 §4.3).
//!
//! Suíte `AES_128_GCM_SHA256_128` (0x0004). As chaves seguem o RFC 9605 §5.2:
//!
//! ```text
//! sframe_epoch_secret = MLS-Exporter("SFrame 1.0", "", 32)
//! base_key            = HKDF-Expand(sframe_epoch_secret, sender_index (8 bytes BE), 32)
//! KID                 = (sender_index << 4) | (epoch % 16)
//! ```
//!
//! Cada remetente tem a própria chave, então dois transmissores nunca
//! repetem nonce. O contador começa num valor sorteado a cada sessão: o mesmo
//! epoch MLS pode sobreviver a um reinício do app (salas de servidor guardam o
//! estado), e recomeçar do zero repetiria nonce com a mesma chave.
//!
//! Quem é do grupo deriva a chave de todos os remetentes (é o mesmo segredo
//! do epoch), então o SFrame não autentica *qual* membro mandou, só que foi
//! alguém do grupo. Igual ao uso do RFC com MLS.

use std::collections::{HashMap, VecDeque};

use aes_gcm::aead::{AeadInPlace, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce, Tag};
use lerri_core::MemberId;
use sha2::Sha256;
use zeroize::Zeroizing;

/// `AES_128_GCM_SHA256_128`.
pub const CIPHER_SUITE: u16 = 0x0004;
/// Rótulo do exporter MLS (RFC 9605 §5.2).
pub const EXPORTER_LABEL: &str = "SFrame 1.0";
const EPOCH_BITS: u32 = 4;
/// Epochs cujas chaves ficam guardadas: quadros que chegam logo depois de um
/// commit ainda decifram.
const KEEP_EPOCHS: usize = 3;
const NK: usize = 16;
const NN: usize = 12;
const NT: usize = 16;
/// Janela anti-replay por remetente.
const REPLAY_WINDOW: u64 = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SframeError {
    #[error("sem chave de mídia (fora do grupo)")]
    NoKey,
    #[error("quadro SFrame malformado")]
    Malformed,
    #[error("remetente desconhecido no epoch")]
    UnknownSender,
    #[error("quadro não autentica")]
    Auth,
    #[error("quadro repetido ou velho demais")]
    Replay,
    #[error("quadro enviado por nós mesmos")]
    Own,
}

/// Segredo de mídia de um epoch MLS, exportado pela sala.
#[derive(Clone)]
pub struct MediaEpoch {
    pub epoch: u64,
    pub secret: Zeroizing<[u8; 32]>,
    /// Nosso índice de folha.
    pub me: u32,
    /// Índice de folha → membro, para saber quem mandou.
    pub members: Vec<(u32, MemberId)>,
}

impl std::fmt::Debug for MediaEpoch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaEpoch").field("epoch", &self.epoch).field("me", &self.me).finish_non_exhaustive()
    }
}

struct Key {
    aead: Aes128Gcm,
    salt: [u8; NN],
}

impl Key {
    /// RFC 9605 §4.4.2.
    fn derive(base_key: &[u8], kid: u64) -> Self {
        let hk = hkdf::Hkdf::<Sha256>::new(Some(&[]), base_key);
        let mut key = Zeroizing::new([0u8; NK]);
        let mut salt = [0u8; NN];
        let label = |what: &str| [what.as_bytes(), &kid.to_be_bytes(), &CIPHER_SUITE.to_be_bytes()].concat();
        hk.expand(&label("SFrame 1.0 Secret key "), key.as_mut()).expect("16 bytes cabem no HKDF");
        hk.expand(&label("SFrame 1.0 Secret salt "), &mut salt).expect("12 bytes cabem no HKDF");
        Self { aead: Aes128Gcm::new_from_slice(key.as_ref()).expect("chave de 16 bytes"), salt }
    }

    fn nonce(&self, ctr: u64) -> [u8; NN] {
        let mut n = self.salt;
        for (b, c) in n[NN - 8..].iter_mut().zip(ctr.to_be_bytes()) {
            *b ^= c;
        }
        n
    }
}

fn base_key(epoch_secret: &[u8; 32], sender: u32) -> Zeroizing<[u8; 32]> {
    let hk = hkdf::Hkdf::<Sha256>::from_prk(epoch_secret).expect("PRK de 32 bytes");
    let mut out = Zeroizing::new([0u8; 32]);
    hk.expand(&u64::from(sender).to_be_bytes(), out.as_mut()).expect("32 bytes cabem no HKDF");
    out
}

fn kid_of(sender: u32, epoch: u64) -> u64 {
    (u64::from(sender) << EPOCH_BITS) | (epoch % (1 << EPOCH_BITS))
}

/// Janela anti-replay (estilo IPsec) sobre o contador.
#[derive(Default)]
struct Replay {
    top: Option<u64>,
    seen: u128,
}

impl Replay {
    fn check(&self, ctr: u64) -> bool {
        match self.top {
            None => true,
            Some(top) if ctr > top => true,
            Some(top) => top - ctr < REPLAY_WINDOW && self.seen & (1 << (top - ctr)) == 0,
        }
    }

    fn mark(&mut self, ctr: u64) {
        match self.top {
            Some(top) if ctr <= top => self.seen |= 1 << (top - ctr),
            Some(top) => {
                let shift = ctr - top;
                self.seen = if shift >= REPLAY_WINDOW { 0 } else { self.seen << shift };
                self.seen |= 1;
                self.top = Some(ctr);
            }
            None => {
                self.seen = 1;
                self.top = Some(ctr);
            }
        }
    }
}

struct Epoch {
    epoch: u64,
    secret: Zeroizing<[u8; 32]>,
    me: u32,
    members: HashMap<u32, MemberId>,
    send: Key,
    recv: HashMap<u32, (Key, Replay)>,
}

/// Chaves de mídia da sala: cifra o que mandamos e decifra o que chega.
pub struct Keyring {
    epochs: VecDeque<Epoch>,
    ctr: u64,
}

impl Default for Keyring {
    fn default() -> Self {
        Self::new()
    }
}

impl Keyring {
    pub fn new() -> Self {
        // 47 bits sorteados: o cabeçalho fica com 6 bytes de contador e sobra
        // espaço para o contador nunca dar a volta.
        Self { epochs: VecDeque::new(), ctr: u64::from_le_bytes(crate::random()) >> 17 }
    }

    /// Passa a cifrar com o epoch novo; os anteriores ficam para decifrar.
    pub fn set_epoch(&mut self, e: MediaEpoch) {
        if self.epochs.front().is_some_and(|cur| cur.epoch == e.epoch && cur.me == e.me) {
            // Mesmo epoch: só a lista de membros pode ter mudado de leitura.
            self.epochs[0].members = e.members.into_iter().collect();
            return;
        }
        self.epochs.retain(|old| old.epoch != e.epoch);
        let send = Key::derive(base_key(&e.secret, e.me).as_ref(), kid_of(e.me, e.epoch));
        self.epochs.push_front(Epoch {
            epoch: e.epoch,
            secret: e.secret,
            me: e.me,
            members: e.members.into_iter().collect(),
            send,
            recv: HashMap::new(),
        });
        self.epochs.truncate(KEEP_EPOCHS);
    }

    /// Esquece tudo (saiu do grupo).
    pub fn clear(&mut self) {
        self.epochs.clear();
    }

    pub fn ready(&self) -> bool {
        !self.epochs.is_empty()
    }

    /// Cifra `plaintext` e acrescenta o quadro SFrame (cabeçalho, cifrado,
    /// tag) em `out`. `meta` entra autenticado sem ir no quadro (o cabeçalho
    /// de roteamento que o SFU lê).
    pub fn seal(&mut self, meta: &[u8], plaintext: &[u8], out: &mut Vec<u8>) -> Result<(), SframeError> {
        let cur = self.epochs.front().ok_or(SframeError::NoKey)?;
        let ctr = self.ctr;
        self.ctr += 1;
        let start = out.len();
        encode_header(kid_of(cur.me, cur.epoch), ctr, out);
        let header_end = out.len();
        out.extend_from_slice(plaintext);
        let aad = [&out[start..header_end], meta].concat();
        let (_, body) = out.split_at_mut(header_end);
        let tag = cur
            .send
            .aead
            .encrypt_in_place_detached(Nonce::from_slice(&cur.send.nonce(ctr)), &aad, body)
            .map_err(|_| SframeError::Auth)?;
        out.extend_from_slice(&tag);
        Ok(())
    }

    /// Decifra um quadro. Devolve quem mandou e o conteúdo.
    pub fn open(&mut self, meta: &[u8], frame: &[u8]) -> Result<(MemberId, Vec<u8>), SframeError> {
        let (kid, ctr, header_len) = decode_header(frame)?;
        if frame.len() < header_len + NT {
            return Err(SframeError::Malformed);
        }
        let sender = u32::try_from(kid >> EPOCH_BITS).map_err(|_| SframeError::Malformed)?;
        let low = kid % (1 << EPOCH_BITS);
        let e = self.epochs.iter_mut().find(|e| e.epoch % (1 << EPOCH_BITS) == low).ok_or(SframeError::NoKey)?;
        if sender == e.me {
            return Err(SframeError::Own);
        }
        let member = *e.members.get(&sender).ok_or(SframeError::UnknownSender)?;
        let secret = &e.secret;
        let epoch = e.epoch;
        let (key, replay) =
            e.recv.entry(sender).or_insert_with(|| (Key::derive(base_key(secret, sender).as_ref(), kid_of(sender, epoch)), Replay::default()));
        if !replay.check(ctr) {
            return Err(SframeError::Replay);
        }
        let aad = [&frame[..header_len], meta].concat();
        let (body, tag) = frame[header_len..].split_at(frame.len() - header_len - NT);
        let mut plain = body.to_vec();
        key.aead
            .decrypt_in_place_detached(Nonce::from_slice(&key.nonce(ctr)), &aad, &mut plain, Tag::from_slice(tag))
            .map_err(|_| SframeError::Auth)?;
        replay.mark(ctr);
        Ok((member, plain))
    }
}

/// Bytes mínimos (big-endian) para `v`, ao menos 1.
fn min_bytes(v: u64) -> usize {
    (8 - (v.leading_zeros() / 8) as usize).max(1)
}

/// Cabeçalho SFrame (RFC 9605 §4.3): `X KKK Y CCC`, depois KID e CTR.
pub fn encode_header(kid: u64, ctr: u64, out: &mut Vec<u8>) {
    let mut config = 0u8;
    let kid_len = (kid >= 8).then(|| min_bytes(kid));
    let ctr_len = (ctr >= 8).then(|| min_bytes(ctr));
    match kid_len {
        Some(n) => config |= 0x80 | (((n - 1) as u8) << 4),
        None => config |= (kid as u8) << 4,
    }
    match ctr_len {
        Some(n) => config |= 0x08 | (n - 1) as u8,
        None => config |= ctr as u8,
    }
    out.push(config);
    if let Some(n) = kid_len {
        out.extend_from_slice(&kid.to_be_bytes()[8 - n..]);
    }
    if let Some(n) = ctr_len {
        out.extend_from_slice(&ctr.to_be_bytes()[8 - n..]);
    }
}

/// Devolve `(kid, ctr, tamanho do cabeçalho)`.
pub fn decode_header(frame: &[u8]) -> Result<(u64, u64, usize), SframeError> {
    let &config = frame.first().ok_or(SframeError::Malformed)?;
    let mut pos = 1;
    let mut read = |extended: bool, low: u8| -> Result<u64, SframeError> {
        if !extended {
            return Ok(u64::from(low));
        }
        let n = usize::from(low) + 1;
        let bytes = frame.get(pos..pos + n).ok_or(SframeError::Malformed)?;
        pos += n;
        Ok(bytes.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b)))
    };
    let kid = read(config & 0x80 != 0, (config >> 4) & 0x07)?;
    let ctr = read(config & 0x08 != 0, config & 0x07)?;
    Ok((kid, ctr, pos))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(kid: u64, ctr: u64) -> Vec<u8> {
        let mut out = Vec::new();
        encode_header(kid, ctr, &mut out);
        assert_eq!(decode_header(&out).unwrap(), (kid, ctr, out.len()));
        out
    }

    #[test]
    fn header_encoding() {
        // Exemplos do RFC 9605 §4.3 / Apêndice C.1.
        assert_eq!(header(0, 0), [0x00]);
        assert_eq!(header(0, 7), [0x07]);
        assert_eq!(header(7, 0), [0x70]);
        assert_eq!(header(0, 8), [0x08, 0x08]);
        assert_eq!(header(8, 0), [0x80, 0x08]);
        assert_eq!(header(0xff, 0x100), [0x89, 0xff, 0x01, 0x00]);
        assert_eq!(header(u64::MAX, u64::MAX), [[0xff].as_slice(), &[0xff; 16]].concat());
        assert_eq!(decode_header(&[0x89, 0xff, 0x01]), Err(SframeError::Malformed));
    }

    fn epoch(epoch: u64, me: u32, secret: u8) -> MediaEpoch {
        MediaEpoch {
            epoch,
            secret: Zeroizing::new([secret; 32]),
            me,
            members: vec![(0, MemberId([10; 32])), (1, MemberId([11; 32])), (2, MemberId([12; 32]))],
        }
    }

    fn pair() -> (Keyring, Keyring) {
        let (mut a, mut b) = (Keyring::new(), Keyring::new());
        a.set_epoch(epoch(5, 0, 1));
        b.set_epoch(epoch(5, 1, 1));
        (a, b)
    }

    #[test]
    fn roundtrip_and_sender() {
        let (mut a, mut b) = pair();
        let mut frame = Vec::new();
        a.seal(b"meta", b"voz", &mut frame).unwrap();
        assert!(!frame.windows(3).any(|w| w == b"voz"));
        assert_eq!(b.open(b"meta", &frame).unwrap(), (MemberId([10; 32]), b"voz".to_vec()));
        // O próprio quadro voltando não é aceito como de outro.
        assert_eq!(a.open(b"meta", &frame), Err(SframeError::Own));
    }

    #[test]
    fn tamper_and_replay() {
        let (mut a, mut b) = pair();
        let mut frame = Vec::new();
        a.seal(b"meta", b"voz", &mut frame).unwrap();
        assert_eq!(b.open(b"outro", &frame), Err(SframeError::Auth), "metadado faz parte da autenticação");
        let mut bad = frame.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert_eq!(b.open(b"meta", &bad), Err(SframeError::Auth));
        b.open(b"meta", &frame).unwrap();
        assert_eq!(b.open(b"meta", &frame), Err(SframeError::Replay));
    }

    #[test]
    fn out_of_order_within_window() {
        let (mut a, mut b) = pair();
        let frames: Vec<Vec<u8>> = (0..5)
            .map(|_| {
                let mut f = Vec::new();
                a.seal(&[], b"x", &mut f).unwrap();
                f
            })
            .collect();
        for i in [4, 0, 2, 1, 3] {
            b.open(&[], &frames[i]).unwrap();
        }
        assert_eq!(b.open(&[], &frames[2]), Err(SframeError::Replay));
    }

    #[test]
    fn other_epoch_or_group_cannot_read() {
        let (mut a, mut b) = pair();
        let mut frame = Vec::new();
        a.seal(&[], b"x", &mut frame).unwrap();
        // Quem tem outro segredo (removido, ou o relay) não decifra.
        let mut outsider = Keyring::new();
        outsider.set_epoch(epoch(5, 2, 9));
        assert_eq!(outsider.open(&[], &frame), Err(SframeError::Auth));
        // Novo epoch: o quadro antigo ainda decifra (está na janela de epochs).
        b.set_epoch(epoch(6, 1, 2));
        b.open(&[], &frame).unwrap();
        let mut empty = Keyring::new();
        assert_eq!(empty.open(&[], &frame), Err(SframeError::NoKey));
        assert_eq!(empty.seal(&[], b"x", &mut Vec::new()), Err(SframeError::NoKey));
    }

    #[test]
    fn counters_start_apart() {
        // Duas sessões no mesmo epoch não começam no mesmo contador.
        let (a, b) = (Keyring::new(), Keyring::new());
        assert_ne!(a.ctr, b.ctr);
    }
}
