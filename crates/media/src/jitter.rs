//! Buffer de jitter adaptativo de um remetente de voz (SPEC-04 §6).
//!
//! Pacotes chegam fora de ordem e com atraso variável; a saída de áudio puxa
//! um quadro de 20 ms por vez. O alvo de atraso acompanha o jitter medido
//! (estimador do RFC 3550) e fica no mínimo que evita engasgo. Como o VAD só
//! manda quando há fala, cada trecho de fala recomeça do buffer vazio.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const FRAME_MS: f32 = 20.0;
/// Alvo mínimo e máximo, em quadros de 20 ms.
const MIN_TARGET: usize = 1;
const MAX_TARGET: usize = 8;
/// Acima do alvo por mais que isto, descarta o mais velho (atraso crescente).
const SLACK: usize = 4;
/// Quadros seguidos sem nada antes de considerar que a fala acabou.
const END_OF_TALK: u32 = 5;
/// Pacotes guardados, no máximo (remetente que manda lixo não enche a memória).
const MAX_BUFFERED: usize = 64;

#[derive(Debug, PartialEq, Eq)]
pub enum Pop {
    /// Toca este pacote.
    Packet(Vec<u8>),
    /// O quadro se perdeu, mas o próximo chegou: recupera pelo FEC dele.
    Fec(Vec<u8>),
    /// Perdeu: o decoder inventa (PLC).
    Lost,
    /// Ninguém falando (ou ainda enchendo o buffer).
    Silence,
}

pub struct Jitter {
    packets: BTreeMap<u64, Vec<u8>>,
    last_ext: Option<u64>,
    /// Próximo a tocar; `None` enquanto enche o buffer.
    next: Option<u64>,
    /// Quando o primeiro pacote do trecho atual chegou (para não esperar
    /// para sempre um buffer que não enche).
    waiting_since: Option<Instant>,
    missing: u32,
    /// Jitter estimado, em ms.
    jitter_ms: f32,
    prev_transit: Option<f32>,
    epoch: Instant,
}

impl Default for Jitter {
    fn default() -> Self {
        Self::new()
    }
}

impl Jitter {
    pub fn new() -> Self {
        Self {
            packets: BTreeMap::new(),
            last_ext: None,
            next: None,
            waiting_since: None,
            missing: 0,
            jitter_ms: 0.0,
            prev_transit: None,
            epoch: Instant::now(),
        }
    }

    /// Alvo de atraso atual, em quadros.
    pub fn target(&self) -> usize {
        let frames = 1.0 + (3.0 * self.jitter_ms / FRAME_MS).ceil();
        (frames as usize).clamp(MIN_TARGET, MAX_TARGET)
    }

    pub fn target_delay(&self) -> Duration {
        Duration::from_millis(self.target() as u64 * FRAME_MS as u64)
    }

    /// Estende o número de sequência de 16 bits pelo mais próximo do último.
    fn extend(&mut self, seq: u16) -> u64 {
        let ext = match self.last_ext {
            None => u64::from(seq) + (1 << 16),
            Some(last) => {
                let base = last & !0xffff;
                [base.wrapping_sub(1 << 16), base, base + (1 << 16)]
                    .into_iter()
                    .map(|b| b.wrapping_add(u64::from(seq)))
                    .min_by_key(|c| c.abs_diff(last))
                    .expect("três candidatos")
            }
        };
        if self.last_ext.is_none_or(|l| ext > l) {
            self.last_ext = Some(ext);
        }
        ext
    }

    pub fn push(&mut self, seq: u16, start: bool, payload: Vec<u8>, now: Instant) {
        let ext = self.extend(seq);
        // Jitter (RFC 3550 §6.4.1): variação do tempo de trânsito relativo.
        let transit = now.duration_since(self.epoch).as_secs_f32() * 1000.0 - ext as f32 * FRAME_MS;
        if let Some(prev) = self.prev_transit
            && !start
        {
            self.jitter_ms += ((transit - prev).abs() - self.jitter_ms) / 16.0;
        }
        self.prev_transit = Some(transit);

        if start && self.next.is_some() && self.packets.is_empty() {
            // Fala nova depois de silêncio: volta a encher o buffer.
            self.next = None;
        }
        if let Some(next) = self.next
            && ext < next
        {
            return; // chegou tarde demais
        }
        if self.packets.len() >= MAX_BUFFERED {
            self.packets.pop_first();
        }
        self.packets.insert(ext, payload);
        if self.next.is_none() && self.waiting_since.is_none() {
            self.waiting_since = Some(now);
        }
    }

    /// Próximo quadro de 20 ms.
    pub fn pop(&mut self, now: Instant) -> Pop {
        let Some(next) = self.next else {
            let Some((&first, _)) = self.packets.first_key_value() else { return Pop::Silence };
            let waited = self.waiting_since.is_some_and(|t| now.duration_since(t) >= self.target_delay());
            if self.packets.len() < self.target() && !waited {
                return Pop::Silence;
            }
            self.next = Some(first);
            self.waiting_since = None;
            self.missing = 0;
            return self.pop(now);
        };
        // Atraso acumulado demais: pula os mais velhos e segue do que sobrou.
        let mut next = next;
        if self.packets.len() > self.target() + SLACK {
            while self.packets.len() > self.target() + SLACK {
                self.packets.pop_first();
            }
            if let Some((&first, _)) = self.packets.first_key_value() {
                next = next.max(first);
            }
        }
        self.next = Some(next + 1);
        if let Some(p) = self.packets.remove(&next) {
            self.missing = 0;
            return Pop::Packet(p);
        }
        self.missing += 1;
        if self.packets.is_empty() && self.missing >= END_OF_TALK {
            self.next = None;
            return Pop::Silence;
        }
        if let Some(p) = self.packets.get(&(next + 1)) {
            return Pop::Fec(p.clone());
        }
        if self.packets.is_empty() && self.missing > 1 {
            // Provavelmente o fim da fala (VAD): silêncio em vez de PLC.
            return Pop::Silence;
        }
        Pop::Lost
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(n: u8) -> Vec<u8> {
        vec![n]
    }

    #[test]
    fn plays_in_order_after_buffering() {
        let mut j = Jitter::new();
        let t = Instant::now();
        // Chegam fora de ordem.
        j.push(2, false, pkt(2), t);
        j.push(1, true, pkt(1), t);
        j.push(3, false, pkt(3), t);
        assert_eq!(j.pop(t), Pop::Packet(pkt(1)));
        assert_eq!(j.pop(t), Pop::Packet(pkt(2)));
        assert_eq!(j.pop(t), Pop::Packet(pkt(3)));
        // Acabou a fala: alguns quadros de PLC no máximo, depois silêncio.
        let mut silent = false;
        for _ in 0..END_OF_TALK + 1 {
            silent |= j.pop(t) == Pop::Silence;
        }
        assert!(silent);
    }

    #[test]
    fn loss_uses_fec_from_next() {
        let mut j = Jitter::new();
        let t = Instant::now();
        j.push(10, true, pkt(10), t);
        j.push(12, false, pkt(12), t);
        j.push(13, false, pkt(13), t);
        assert_eq!(j.pop(t), Pop::Packet(pkt(10)));
        assert_eq!(j.pop(t), Pop::Fec(pkt(12)));
        assert_eq!(j.pop(t), Pop::Packet(pkt(12)));
    }

    #[test]
    fn late_packet_dropped_and_seq_wraps() {
        let mut j = Jitter::new();
        let t = Instant::now();
        j.push(u16::MAX, true, pkt(1), t);
        j.push(0, false, pkt(2), t);
        assert_eq!(j.pop(t), Pop::Packet(pkt(1)));
        assert_eq!(j.pop(t), Pop::Packet(pkt(2)));
        j.push(u16::MAX, false, pkt(9), t);
        j.push(1, false, pkt(3), t);
        assert_eq!(j.pop(t), Pop::Packet(pkt(3)));
    }

    #[test]
    fn starts_after_waiting_even_if_buffer_is_short() {
        let mut j = Jitter::new();
        let t = Instant::now();
        // Jitter alto: alvo maior que 1 quadro.
        for i in 0..40u16 {
            let wobble = if i % 2 == 0 { 0 } else { 35 };
            j.push(i, i == 0, pkt(0), t + Duration::from_millis(u64::from(i) * 20 + wobble));
        }
        assert!(j.target() > 1, "alvo {}", j.target());
        let mut j2 = Jitter { jitter_ms: j.jitter_ms, ..Jitter::new() };
        j2.push(0, true, pkt(7), t);
        assert_eq!(j2.pop(t), Pop::Silence);
        assert_eq!(j2.pop(t + j2.target_delay()), Pop::Packet(pkt(7)));
    }

    #[test]
    fn caps_latency() {
        let mut j = Jitter::new();
        let t = Instant::now();
        for i in 0..30u16 {
            j.push(i, i == 0, pkt(i as u8), t);
        }
        // Buffer bem acima do alvo: pula os velhos em vez de acumular atraso.
        let Pop::Packet(p) = j.pop(t) else { panic!() };
        assert!(p[0] > 20, "tocou {} (deveria pular os velhos)", p[0]);
    }
}
