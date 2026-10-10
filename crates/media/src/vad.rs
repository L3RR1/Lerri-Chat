//! Detecção de voz (SPEC-04 §1): silêncio não vira pacote.
//!
//! Roda depois da supressão de ruído e do ganho, então nível de energia acima
//! do piso de ruído já separa bem fala de fundo. Com a supressão forte
//! (RNNoise) ligada, a probabilidade de voz dela também tem de concordar.

/// Quadros de 20 ms que a transmissão continua depois da última fala (evita
/// cortar fim de palavra).
const HANGOVER: u32 = 15;
/// Fala precisa estar este tanto acima do piso de ruído.
const MARGIN_DB: f32 = 9.0;
/// Abaixo disto é silêncio, qualquer que seja o piso.
const FLOOR_DB: f32 = -55.0;
/// O piso sobe devagar (ruído novo) e desce na hora.
const RISE_DB_PER_FRAME: f32 = 0.05;
const RNNOISE_THRESHOLD: f32 = 0.5;

pub struct Vad {
    /// Piso de ruído; `None` até o primeiro quadro.
    noise_db: Option<f32>,
    hang: u32,
}

impl Default for Vad {
    fn default() -> Self {
        Self::new()
    }
}

impl Vad {
    pub fn new() -> Self {
        Self { noise_db: None, hang: 0 }
    }

    /// `true` se este quadro deve ser transmitido. `voice_prob`: do RNNoise,
    /// quando ligado.
    pub fn update(&mut self, frame: &[f32], voice_prob: Option<f32>) -> bool {
        let db = level_db(frame);
        let noise = match self.noise_db {
            Some(n) if db >= n => n + RISE_DB_PER_FRAME,
            _ => db,
        };
        self.noise_db = Some(noise);
        let loud = db > FLOOR_DB && db > noise + MARGIN_DB;
        let speech = loud && voice_prob.is_none_or(|p| p >= RNNOISE_THRESHOLD);
        if speech {
            self.hang = HANGOVER;
            true
        } else if self.hang > 0 {
            self.hang -= 1;
            true
        } else {
            false
        }
    }
}

/// Nível RMS em dBFS (amostras em -1..1).
pub fn level_db(frame: &[f32]) -> f32 {
    let ms = frame.iter().map(|s| s * s).sum::<f32>() / frame.len().max(1) as f32;
    10.0 * (ms + 1e-12).log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(amp: f32, seed: &mut u32) -> Vec<f32> {
        (0..960)
            .map(|_| {
                *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                amp * ((*seed >> 8) as f32 / (1 << 24) as f32 - 0.5)
            })
            .collect()
    }

    #[test]
    fn speech_over_noise_then_hangover() {
        let mut vad = Vad::new();
        let mut seed = 1;
        // Ruído de fundo constante: aprende o piso e fica quieto.
        let mut on = 0;
        for _ in 0..200 {
            on += usize::from(vad.update(&noise(0.01, &mut seed), None));
        }
        assert!(on < 20, "ruído transmitiu {on} quadros");
        assert!(!vad.update(&noise(0.01, &mut seed), None));
        // Fala (bem mais alta) liga na hora.
        assert!(vad.update(&noise(0.3, &mut seed), None));
        // Volta o ruído: segura o hangover e desliga.
        let tail = (0..HANGOVER + 2).filter(|_| vad.update(&noise(0.01, &mut seed), None)).count();
        assert_eq!(tail, HANGOVER as usize);
    }

    #[test]
    fn rnnoise_vetoes() {
        let mut vad = Vad::new();
        let mut seed = 2;
        for _ in 0..10 {
            vad.update(&noise(0.01, &mut seed), None);
        }
        assert!(!vad.update(&noise(0.3, &mut seed), Some(0.1)));
        assert!(vad.update(&noise(0.3, &mut seed), Some(0.9)));
    }

    #[test]
    fn digital_silence_is_silence() {
        let mut vad = Vad::new();
        assert!(!vad.update(&[0.0; 960], None));
    }
}
