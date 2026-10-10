//! Opus (SPEC-04 §1) sobre a libopus traduzida para Rust (`unsafe-libopus`):
//! sem compilador C nem cmake no build. Mono, 48 kHz, quadros de 20 ms.

use unsafe_libopus as unsafe_opus;

pub const RATE: u32 = 48_000;
/// 20 ms a 48 kHz.
pub const FRAME: usize = 960;
/// Maior pacote que pedimos ao encoder (cabe com folga num datagrama).
pub const MAX_PACKET: usize = 400;

#[derive(Debug, thiserror::Error)]
#[error("opus: {0}")]
pub struct OpusError(String);

fn check(code: i32) -> Result<i32, OpusError> {
    if code < 0 {
        Err(OpusError(unsafe_opus::opus_strerror(code).to_string()))
    } else {
        Ok(code)
    }
}

pub struct Encoder {
    st: *mut unsafe_opus::OpusEncoder,
}

// SAFETY: o estado é só memória do encoder, usada por uma thread de cada vez.
unsafe impl Send for Encoder {}

impl Encoder {
    /// Encoder de voz (`OPUS_APPLICATION_VOIP`) com FEC in-band ligado.
    pub fn new(bitrate: i32) -> Result<Self, OpusError> {
        let mut err = 0;
        // SAFETY: parâmetros válidos; o ponteiro é checado abaixo.
        let st = unsafe { unsafe_opus::opus_encoder_create(RATE as i32, 1, unsafe_opus::OPUS_APPLICATION_VOIP, &mut err) };
        check(err)?;
        if st.is_null() {
            return Err(OpusError("encoder nulo".into()));
        }
        let enc = Self { st };
        enc.ctl(unsafe_opus::OPUS_SET_BITRATE_REQUEST, bitrate)?;
        enc.ctl(unsafe_opus::OPUS_SET_SIGNAL_REQUEST, unsafe_opus::OPUS_SIGNAL_VOICE)?;
        enc.ctl(unsafe_opus::OPUS_SET_INBAND_FEC_REQUEST, 1)?;
        enc.ctl(unsafe_opus::OPUS_SET_PACKET_LOSS_PERC_REQUEST, 5)?;
        // Complexidade 5: metade do custo do padrão (9) com perda de qualidade
        // inaudível em voz.
        enc.ctl(unsafe_opus::OPUS_SET_COMPLEXITY_REQUEST, 5)?;
        Ok(enc)
    }

    fn ctl(&self, request: i32, value: i32) -> Result<(), OpusError> {
        // SAFETY: `st` é válido; o pedido recebe um i32.
        check(unsafe { unsafe_opus::opus_encoder_ctl!(self.st, request, value) }).map(drop)
    }

    pub fn set_bitrate(&mut self, bitrate: i32) -> Result<(), OpusError> {
        self.ctl(unsafe_opus::OPUS_SET_BITRATE_REQUEST, bitrate)
    }

    /// Perda esperada, em %: o encoder põe mais redundância (FEC) quanto maior.
    pub fn set_expected_loss(&mut self, percent: i32) -> Result<(), OpusError> {
        self.ctl(unsafe_opus::OPUS_SET_PACKET_LOSS_PERC_REQUEST, percent.clamp(0, 100))
    }

    /// Codifica um quadro de 20 ms ([`FRAME`] amostras em -1..1).
    pub fn encode(&mut self, pcm: &[f32], out: &mut Vec<u8>) -> Result<(), OpusError> {
        assert_eq!(pcm.len(), FRAME);
        out.resize(MAX_PACKET, 0);
        // SAFETY: tamanhos conferidos acima.
        let n = check(unsafe {
            unsafe_opus::opus_encode_float(self.st, pcm.as_ptr(), FRAME as i32, out.as_mut_ptr(), MAX_PACKET as i32)
        })?;
        out.truncate(n as usize);
        Ok(())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: criado por `opus_encoder_create`, liberado uma vez.
        unsafe { unsafe_opus::opus_encoder_destroy(self.st) }
    }
}

pub struct Decoder {
    st: *mut unsafe_opus::OpusDecoder,
}

// SAFETY: como o encoder.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new() -> Result<Self, OpusError> {
        let mut err = 0;
        // SAFETY: parâmetros válidos; o ponteiro é checado abaixo.
        let st = unsafe { unsafe_opus::opus_decoder_create(RATE as i32, 1, &mut err) };
        check(err)?;
        if st.is_null() {
            return Err(OpusError("decoder nulo".into()));
        }
        Ok(Self { st })
    }

    /// Decodifica 20 ms em `out` ([`FRAME`] amostras).
    /// - `Some(p), fec: false`: o pacote `p`;
    /// - `Some(p), fec: true`: recupera o quadro *anterior* a `p` pela
    ///   redundância (FEC) que veio nele;
    /// - `None`: perda sem nada para recuperar; o decoder inventa (PLC).
    pub fn decode(&mut self, packet: Option<&[u8]>, fec: bool, out: &mut [f32]) -> Result<(), OpusError> {
        assert_eq!(out.len(), FRAME);
        let (ptr, len) = packet.map_or((std::ptr::null(), 0), |p| (p.as_ptr(), p.len() as i32));
        // SAFETY: `out` tem FRAME amostras; ponteiro nulo é PLC.
        let n = check(unsafe { unsafe_opus::opus_decode_float(self.st, ptr, len, out.as_mut_ptr(), FRAME as i32, i32::from(fec)) })?;
        if (n as usize) < FRAME {
            out[n as usize..].fill(0.0);
        }
        Ok(())
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: criado por `opus_decoder_create`, liberado uma vez.
        unsafe { unsafe_opus::opus_decoder_destroy(self.st) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn tone(freq: f32, n: usize, start: usize) -> Vec<f32> {
        (start..start + n).map(|i| 0.3 * (i as f32 * freq * std::f32::consts::TAU / RATE as f32).sin()).collect()
    }

    fn energy(x: &[f32]) -> f32 {
        x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32
    }

    #[test]
    fn encode_decode_tone() {
        let mut enc = Encoder::new(32_000).unwrap();
        let mut dec = Decoder::new().unwrap();
        let mut packet = Vec::new();
        let mut out = vec![0.0; FRAME];
        let mut sizes = Vec::new();
        for i in 0..20 {
            enc.encode(&tone(440.0, FRAME, i * FRAME), &mut packet).unwrap();
            sizes.push(packet.len());
            dec.decode(Some(&packet), false, &mut out).unwrap();
        }
        // ~32 kbps = 80 bytes por quadro de 20 ms.
        let avg = sizes.iter().sum::<usize>() / sizes.len();
        assert!((40..=160).contains(&avg), "pacote médio de {avg} bytes");
        let e = energy(&out);
        assert!((0.02..0.08).contains(&e), "energia {e} (entrada 0,045)");
        // PLC e FEC não falham.
        dec.decode(None, false, &mut out).unwrap();
        dec.decode(Some(&packet), true, &mut out).unwrap();
    }
}
