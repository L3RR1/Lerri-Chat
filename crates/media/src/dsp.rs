//! Tratamento do microfone (SPEC-04 §1): filtro passa-alta, cancelamento de
//! eco (AEC3), supressão de ruído e controle de ganho (AGC2), todos do
//! WebRTC portados para Rust (`aec3`). A "supressão forte" passa o resultado
//! ainda pelo RNNoise (`nnnoiseless`).
//!
//! Trabalha em quadros de 10 ms a 48 kHz mono. O AEC precisa do áudio que
//! está saindo nas caixas (referência) para tirá-lo do microfone.

use aec3::nodes::audio::AudioFormat;
use aec3::pipelines::linear::{self, LinearPipeline};
use nnnoiseless::DenoiseState;

use crate::opus::RATE;

/// 10 ms a 48 kHz.
pub const TEN_MS: usize = (RATE / 100) as usize;
/// Atraso inicial suposto entre tocar e o microfone ouvir; o AEC3 ajusta
/// sozinho depois.
const INITIAL_DELAY_MS: i32 = 60;
/// O RNNoise trabalha na escala de amostras de 16 bits.
const S16: f32 = 32768.0;

pub struct Dsp {
    apm: Option<LinearPipeline>,
    rnnoise: Option<Box<DenoiseState<'static>>>,
    scratch: Vec<f32>,
}

impl Dsp {
    pub fn new(strong_ns: bool) -> Self {
        let format = AudioFormat::ten_ms(RATE, 1);
        let apm = linear::builder(format, format)
            .initial_delay_ms(INITIAL_DELAY_MS)
            .build()
            .map_err(|e| tracing::warn!("tratamento de áudio indisponível, seguindo sem: {e}"))
            .ok();
        Self { apm, rnnoise: strong_ns.then(DenoiseState::new), scratch: vec![0.0; TEN_MS] }
    }

    pub fn set_strong_ns(&mut self, on: bool) {
        if on != self.rnnoise.is_some() {
            self.rnnoise = on.then(DenoiseState::new);
        }
    }

    /// Áudio que acabou de sair nas caixas (10 ms).
    pub fn far_end(&mut self, frame: &[f32]) {
        if let Some(apm) = &mut self.apm
            && let Err(e) = apm.handle_render_frame(frame)
        {
            tracing::debug!("referência do AEC recusada: {e}");
        }
    }

    /// Trata 10 ms do microfone no lugar. Devolve a probabilidade de voz do
    /// RNNoise, quando ligado.
    pub fn capture(&mut self, frame: &mut [f32]) -> Option<f32> {
        if let Some(apm) = &mut self.apm {
            match apm.process_capture_frame(frame, &mut self.scratch) {
                Ok(true) => frame.copy_from_slice(&self.scratch),
                Ok(false) => {}
                Err(e) => tracing::debug!("tratamento do microfone falhou: {e}"),
            }
        }
        let rn = self.rnnoise.as_mut()?;
        for s in frame.iter_mut() {
            *s *= S16;
        }
        self.scratch.copy_from_slice(frame);
        let prob = rn.process_frame(frame, &self.scratch);
        for s in frame.iter_mut() {
            *s /= S16;
        }
        Some(prob)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vad::level_db;

    fn tone(n: usize, start: usize, amp: f32) -> Vec<f32> {
        (start..start + n).map(|i| amp * (i as f32 * 300.0 * std::f32::consts::TAU / RATE as f32).sin()).collect()
    }

    #[test]
    fn echo_is_reduced() {
        // A "caixa" toca um tom e o microfone ouve o mesmo tom 30 ms depois,
        // sem ninguém falando: depois de convergir, o eco tem de cair bastante.
        let mut dsp = Dsp::new(false);
        let delay = 3 * TEN_MS;
        let far: Vec<f32> = tone(TEN_MS * 400, 0, 0.3);
        let mut before = Vec::new();
        let mut after = Vec::new();
        for i in 0..400 {
            let at = i * TEN_MS;
            dsp.far_end(&far[at..at + TEN_MS]);
            let mut mic: Vec<f32> = (at..at + TEN_MS).map(|k| if k >= delay { 0.5 * far[k - delay] } else { 0.0 }).collect();
            before.push(level_db(&mic));
            dsp.capture(&mut mic);
            after.push(level_db(&mic));
        }
        let tail = |v: &[f32]| v[300..].iter().sum::<f32>() / 100.0;
        let (b, a) = (tail(&before), tail(&after));
        assert!(a < b - 15.0, "eco de {b:.1} dB só caiu para {a:.1} dB");
    }

    #[test]
    fn strong_ns_runs() {
        let mut dsp = Dsp::new(true);
        let mut frame = tone(TEN_MS, 0, 0.2);
        assert!(dsp.capture(&mut frame).is_some());
        dsp.set_strong_ns(false);
        assert!(dsp.capture(&mut frame).is_none());
    }
}
