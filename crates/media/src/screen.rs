//! Tela no fio (SPEC-02 §4.3, SPEC-03 §6.1), sem depender de GPU.
//!
//! Cada quadro codificado vai num stream QUIC unidirecional próprio:
//! `cabeçalho de roteamento (4 bytes) || SFrame(codec (1 byte) || bitstream)`.
//! O QUIC retransmite o que se perde (um keyframe tem centenas de pacotes;
//! em datagramas, 1% de perda já o estragaria quase sempre), e quadros em
//! streams diferentes não se bloqueiam. Como streams podem terminar fora de
//! ordem, quem recebe reordena ([`Reorder`]).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lerri_core::MemberId;
use lerri_core::media::{Codec, HEADER_LEN, Header, Video};
use lerri_crypto::sframe::{Keyring, SframeError};

/// Chaves SFrame da sala, as mesmas da voz: um contador só por remetente.
pub type Keys = Arc<Mutex<Keyring>>;

/// Monta o conteúdo do stream de um quadro da camada `layer`.
pub fn seal_frame(
    keys: &Keys,
    video: Video,
    layer: u8,
    seq: u16,
    keyframe: bool,
    codec: Codec,
    data: &[u8],
) -> Result<Vec<u8>, SframeError> {
    let header = Header::video(video, layer, seq, keyframe).encode();
    let mut plain = Vec::with_capacity(data.len() + 1);
    plain.push(codec.to_byte());
    plain.extend_from_slice(data);
    let mut out = Vec::with_capacity(data.len() + 64);
    out.extend_from_slice(&header);
    keys.lock().unwrap().seal(&header, &plain, &mut out)?;
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub from: MemberId,
    pub video: Video,
    /// Camada do simulcast (`LAYER_HIGH` ou `LAYER_LOW`).
    pub layer: u8,
    pub seq: u16,
    pub keyframe: bool,
    pub codec: Codec,
    pub data: Vec<u8>,
}

/// Decifra o conteúdo de um stream de quadro.
pub fn open_frame(keys: &Keys, stream: &[u8]) -> Result<Frame, SframeError> {
    let (header, body) = Header::parse(stream).ok_or(SframeError::Malformed)?;
    let video = Video::from_kind(header.kind).ok_or(SframeError::Malformed)?;
    let (from, plain) = keys.lock().unwrap().open(&stream[..HEADER_LEN], body)?;
    let (&codec, data) = plain.split_first().ok_or(SframeError::Malformed)?;
    let codec = Codec::from_byte(codec).ok_or(SframeError::Malformed)?;
    Ok(Frame { from, video, layer: header.layer, seq: header.seq, keyframe: header.keyframe, codec, data: data.to_vec() })
}

/// Quanto esperar um quadro que ficou para trás antes de desistir e pedir um
/// keyframe.
const GAP_TIMEOUT: Duration = Duration::from_millis(250);
const MAX_HELD: usize = 30;

/// Põe em ordem os quadros de um transmissor. Começa (e recomeça depois de
/// um buraco) sempre num keyframe, porque o decoder precisa dele.
#[derive(Default)]
pub struct Reorder {
    /// Camada do simulcast que está chegando. O relay troca de camada sempre
    /// num keyframe; cada camada tem a sua sequência, então a troca recomeça
    /// a ordem.
    layer: u8,
    /// Próximo esperado, com o número de sequência estendido para 64 bits.
    next: Option<u64>,
    held: BTreeMap<u64, Frame>,
    waiting_since: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Gap {
    /// Nada a fazer.
    None,
    /// Um quadro não veio: peça um keyframe.
    NeedKeyframe,
}

impl Reorder {
    /// Estende o `u16` pelo valor mais próximo do próximo esperado.
    fn extend(&self, seq: u16) -> u64 {
        let Some(n) = self.next else { return u64::from(seq) + (1 << 16) };
        let base = n & !0xffff;
        [base.saturating_sub(1 << 16), base, base + (1 << 16)]
            .into_iter()
            .map(|b| b + u64::from(seq))
            .min_by_key(|c| c.abs_diff(n))
            .expect("três candidatos")
    }

    /// Recebe um quadro; devolve os que já podem ir para o decoder, em ordem.
    pub fn push(&mut self, frame: Frame, now: Instant) -> Vec<Frame> {
        let mut out = Vec::new();
        if frame.layer != self.layer {
            if !frame.keyframe {
                // Sobra da camada anterior, ou da nova antes do keyframe.
                return out;
            }
            *self = Self { layer: frame.layer, ..Self::default() };
        }
        let e = self.extend(frame.seq);
        match self.next {
            // Keyframe zera a dependência: o que esperava antes dele não serve mais.
            _ if frame.keyframe && self.next.is_none_or(|n| e >= n) => {
                self.held.retain(|k, _| *k > e);
                self.next = Some(e + 1);
                out.push(frame);
            }
            None => return out, // ainda esperando o primeiro keyframe
            Some(n) if e < n => return out, // atrasado demais ou repetido
            Some(n) if e == n => {
                self.next = Some(e + 1);
                out.push(frame);
            }
            Some(_) => {
                if self.held.len() < MAX_HELD {
                    self.held.insert(e, frame);
                }
                self.waiting_since.get_or_insert(now);
                return out;
            }
        }
        // Libera os que estavam esperando e agora estão em sequência.
        while let Some(n) = self.next
            && let Some(f) = self.held.remove(&n)
        {
            self.next = Some(n + 1);
            out.push(f);
        }
        if self.held.is_empty() {
            self.waiting_since = None;
        } else {
            self.waiting_since.get_or_insert(now);
        }
        out
    }

    /// Chame de tempos em tempos: um buraco que não fechou vira pedido de
    /// keyframe, e a ordem recomeça no próximo.
    pub fn poll(&mut self, now: Instant) -> Gap {
        match self.waiting_since {
            Some(t) if now.duration_since(t) >= GAP_TIMEOUT => {
                self.held.clear();
                self.next = None;
                self.waiting_since = None;
                Gap::NeedKeyframe
            }
            _ => Gap::None,
        }
    }

    /// O decoder falhou: recomeça no próximo keyframe.
    pub fn reset(&mut self) {
        *self = Self { layer: self.layer, ..Self::default() };
    }
}

/// Bitrate da tela guiado pelo atraso (SPEC-03 §6.1): quadros acumulando em
/// trânsito = rede não dá conta, desce; tudo fluindo por um tempo, sobe.
pub struct RateControl {
    pub bitrate: u32,
    min: u32,
    max: u32,
    last_change: Option<Instant>,
    calm_since: Option<Instant>,
}

/// Tela.
pub const START_BITRATE: u32 = 3_000_000;
pub const MIN_BITRATE: u32 = 300_000;
/// Teto para 1080p60 com movimento (RNF-05: 2–5 Mbps).
pub const MAX_BITRATE: u32 = 5_000_000;
const CONGESTED: usize = 3;
const STEP_EVERY: Duration = Duration::from_millis(500);
const CALM_FOR: Duration = Duration::from_secs(3);

impl Default for RateControl {
    fn default() -> Self {
        Self::new(START_BITRATE, MIN_BITRATE, MAX_BITRATE)
    }
}

impl RateControl {
    /// Câmera 720p30: ~1–1,5 Mbps (SPEC-04 §3).
    pub fn camera() -> Self {
        Self::new(1_200_000, 200_000, 1_500_000)
    }

    pub fn new(start: u32, min: u32, max: u32) -> Self {
        Self { bitrate: start, min, max, last_change: None, calm_since: None }
    }

    /// `in_flight`: quadros enviados ainda sem confirmação. Devolve o bitrate
    /// novo quando muda.
    pub fn update(&mut self, in_flight: usize, now: Instant) -> Option<u32> {
        if self.last_change.is_some_and(|l| now.duration_since(l) < STEP_EVERY) {
            return None;
        }
        let new = if in_flight >= CONGESTED {
            self.calm_since = None;
            (self.bitrate as f64 * 0.75) as u32
        } else if in_flight <= 1 {
            let calm = *self.calm_since.get_or_insert(now);
            if now.duration_since(calm) < CALM_FOR {
                return None;
            }
            self.calm_since = Some(now);
            (self.bitrate as f64 * 1.15) as u32
        } else {
            return None;
        };
        let new = new.clamp(self.min, self.max);
        if new == self.bitrate {
            return None;
        }
        self.bitrate = new;
        self.last_change = Some(now);
        Some(new)
    }

    /// Congestionado agora: melhor não codificar mais um quadro.
    pub fn should_skip(&self, in_flight: usize) -> bool {
        in_flight >= 2 * CONGESTED
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lerri_crypto::sframe::MediaEpoch;
    use zeroize::Zeroizing;

    fn frame(seq: u16, keyframe: bool) -> Frame {
        Frame { from: MemberId([1; 32]), video: Video::Screen, layer: 0, seq, keyframe, codec: Codec::Hevc, data: vec![seq as u8] }
    }

    fn low(seq: u16, keyframe: bool) -> Frame {
        Frame { layer: 1, ..frame(seq, keyframe) }
    }

    fn seqs(v: &[Frame]) -> Vec<u16> {
        v.iter().map(|f| f.seq).collect()
    }

    #[test]
    fn seal_and_open() {
        let keys = |me| {
            let mut k = Keyring::new();
            k.set_epoch(MediaEpoch {
                epoch: 1,
                secret: Zeroizing::new([5; 32]),
                me,
                members: vec![(0, MemberId([1; 32])), (1, MemberId([2; 32]))],
            });
            Arc::new(Mutex::new(k))
        };
        let (a, b) = (keys(0), keys(1));
        let wire = seal_frame(&a, Video::Camera, 0, 7, true, Codec::H264, b"quadro").unwrap();
        assert!(!wire.windows(6).any(|w| w == b"quadro"));
        let (h, _) = Header::parse(&wire).unwrap();
        assert!(h.keyframe && h.kind == lerri_core::media::Kind::Camera, "o relay lê o cabeçalho");
        let f = open_frame(&b, &wire).unwrap();
        assert_eq!(f, Frame { from: MemberId([1; 32]), video: Video::Camera, layer: 0, seq: 7, keyframe: true, codec: Codec::H264, data: b"quadro".to_vec() });
        // Cabeçalho adulterado (ex.: relay marcando keyframe) não decifra.
        let mut bad = wire.clone();
        bad[1] ^= 1;
        assert!(open_frame(&b, &bad).is_err());
    }

    #[test]
    fn layer_switch_restarts_on_the_new_keyframe() {
        let t = Instant::now();
        let mut r = Reorder::default();
        assert_eq!(seqs(&r.push(frame(100, true), t)), [100]);
        assert_eq!(seqs(&r.push(frame(101, false), t)), [101]);
        // A camada baixa tem a sua sequência: antes do keyframe dela, nada passa.
        assert!(r.push(low(3, false), t).is_empty());
        assert_eq!(seqs(&r.push(low(4, true), t)), [4], "keyframe com seq menor ainda vale");
        assert_eq!(seqs(&r.push(low(5, false), t)), [5]);
        // Sobra atrasada da camada alta é descartada sem pedir keyframe.
        assert!(r.push(frame(102, false), t).is_empty());
        assert_eq!(r.poll(t + Duration::from_secs(1)), Gap::None);
        // E volta para a alta no keyframe dela.
        assert_eq!(seqs(&r.push(frame(140, true), t)), [140]);
    }

    #[test]
    fn starts_on_keyframe_and_reorders() {
        let t = Instant::now();
        let mut r = Reorder::default();
        assert!(r.push(frame(4, false), t).is_empty(), "sem keyframe ainda");
        assert_eq!(seqs(&r.push(frame(5, true), t)), [5]);
        assert!(r.push(frame(7, false), t).is_empty());
        assert_eq!(seqs(&r.push(frame(6, false), t)), [6, 7]);
        assert!(r.push(frame(6, false), t).is_empty(), "repetido");
        assert_eq!(r.poll(t + Duration::from_secs(1)), Gap::None);
    }

    #[test]
    fn gap_asks_for_keyframe_and_keyframe_jumps_ahead() {
        let t = Instant::now();
        let mut r = Reorder::default();
        r.push(frame(u16::MAX, true), t);
        assert!(r.push(frame(1, false), t).is_empty(), "0 faltando (com volta do u16)");
        assert_eq!(r.poll(t + Duration::from_millis(100)), Gap::None);
        assert_eq!(r.poll(t + GAP_TIMEOUT), Gap::NeedKeyframe);
        assert!(r.push(frame(2, false), t).is_empty());
        assert_eq!(seqs(&r.push(frame(3, true), t)), [3]);
        // Keyframe à frente de um buraco resolve sem esperar.
        r.push(frame(5, false), t);
        assert_eq!(seqs(&r.push(frame(6, true), t)), [6]);
        assert_eq!(seqs(&r.push(frame(7, false), t)), [7]);
    }

    #[test]
    fn rate_control_backs_off_and_recovers() {
        let t = Instant::now();
        let mut rc = RateControl::default();
        let down = rc.update(5, t).unwrap();
        assert_eq!(rc.update(5, t + STEP_EVERY / 2), None, "cedo demais");
        assert!(down < START_BITRATE);
        let mut now = t + STEP_EVERY;
        for _ in 0..20 {
            now += STEP_EVERY;
            rc.update(10, now);
        }
        assert_eq!(rc.bitrate, MIN_BITRATE);
        for _ in 0..200 {
            now += STEP_EVERY;
            rc.update(0, now);
        }
        assert_eq!(rc.bitrate, MAX_BITRATE);
        assert!(rc.should_skip(10) && !rc.should_skip(1));
    }
}
