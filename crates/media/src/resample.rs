//! Conversão de taxa e de canais entre o dispositivo e os 48 kHz mono do
//! pipeline. O WASAPI compartilhado quase sempre já está em 48 kHz; para os
//! outros casos (44,1 kHz) a interpolação linear basta para voz.

pub struct Resampler {
    /// Passo na entrada por amostra de saída.
    step: f64,
    pos: f64,
    prev: f32,
}

impl Resampler {
    pub fn new(from_hz: u32, to_hz: u32) -> Self {
        Self { step: f64::from(from_hz) / f64::from(to_hz), pos: 0.0, prev: 0.0 }
    }

    pub fn is_identity(&self) -> bool {
        self.step == 1.0
    }

    /// Converte `input` acrescentando em `out`. Mantém a fase entre chamadas.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.is_identity() {
            out.extend_from_slice(input);
            return;
        }
        // `pos` conta a partir da última amostra da chamada anterior (índice
        // -1); só interpola enquanto a amostra seguinte já chegou.
        let n = input.len() as f64;
        let prev = self.prev;
        let at = |k: isize| if k < 0 { prev } else { input[k as usize] };
        while self.pos < n - 1.0 {
            let i = self.pos.floor() as isize;
            let frac = (self.pos - i as f64) as f32;
            let (a, b) = (at(i), at(i + 1));
            out.push(a + (b - a) * frac);
            self.pos += self.step;
        }
        self.pos -= n;
        if let Some(&last) = input.last() {
            self.prev = last;
        }
    }
}

/// Mistura os canais intercalados num só.
pub fn downmix(interleaved: &[f32], channels: usize, out: &mut Vec<f32>) {
    if channels == 1 {
        out.extend_from_slice(interleaved);
        return;
    }
    out.extend(interleaved.chunks_exact(channels).map(|f| f.iter().sum::<f32>() / channels as f32));
}

/// Repete o mono em todos os canais.
pub fn upmix(mono: &[f32], channels: usize, out: &mut [f32]) {
    for (frame, &s) in out.chunks_exact_mut(channels).zip(mono) {
        frame.fill(s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_conversion_keeps_length_and_shape() {
        let mut r = Resampler::new(44_100, 48_000);
        let mut out = Vec::new();
        // 1 s em pedaços de 441.
        for chunk in 0..100 {
            let input: Vec<f32> = (0..441).map(|i| ((chunk * 441 + i) as f32 * 0.01).sin()).collect();
            r.process(&input, &mut out);
        }
        assert!((47_990..=48_010).contains(&out.len()), "{} amostras", out.len());
        // Contínuo (sem degraus nas emendas).
        let max_step = out.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
        assert!(max_step < 0.02, "degrau de {max_step}");
    }

    #[test]
    fn channels() {
        let mut mono = Vec::new();
        downmix(&[1.0, 0.0, 0.5, 0.5], 2, &mut mono);
        assert_eq!(mono, [0.5, 0.5]);
        let mut st = [0.0; 4];
        upmix(&[0.1, 0.2], 2, &mut st);
        assert_eq!(st, [0.1, 0.1, 0.2, 0.2]);
    }
}
