//! Datagramas de mídia (SPEC-02 §4.3, SPEC-03 §6).
//!
//! Cada datagrama QUIC é `cabeçalho de roteamento (4 bytes) || quadro SFrame`.
//! O cabeçalho vai em claro para o SFU rotear sem decifrar nada, mas entra
//! autenticado no SFrame (como metadado do AEAD): ninguém no caminho o altera
//! sem o quadro deixar de decifrar.
//!
//! ```text
//! byte 0: versão (4 bits) | tipo de stream (4 bits)
//! byte 1: bit 0 keyframe, bit 1 início de fala, bits 4–5 camada de simulcast
//! byte 2–3: número de sequência (u16, big-endian, dá a volta)
//! ```

pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 4;
/// Maior datagrama que mandamos: cabe no MTU mínimo do QUIC (1200) com folga
/// para o overhead de pacote.
pub const MAX_DATAGRAM: usize = 1100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Voice = 0,
    Screen = 1,
    Camera = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    pub keyframe: bool,
    /// Primeiro pacote depois de silêncio (VAD): o receptor reinicia o
    /// buffer de jitter.
    pub start: bool,
    pub layer: u8,
    pub seq: u16,
}

impl Header {
    pub fn voice(seq: u16, start: bool) -> Self {
        Self { kind: Kind::Voice, keyframe: false, start, layer: 0, seq }
    }

    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let flags = u8::from(self.keyframe) | (u8::from(self.start) << 1) | ((self.layer & 0x03) << 4);
        let seq = self.seq.to_be_bytes();
        [(VERSION << 4) | self.kind as u8, flags, seq[0], seq[1]]
    }

    /// Separa o cabeçalho do quadro SFrame. `None` para versão ou tipo
    /// desconhecidos.
    pub fn parse(datagram: &[u8]) -> Option<(Self, &[u8])> {
        let (head, rest) = datagram.split_at_checked(HEADER_LEN)?;
        if head[0] >> 4 != VERSION {
            return None;
        }
        let kind = match head[0] & 0x0f {
            0 => Kind::Voice,
            1 => Kind::Screen,
            2 => Kind::Camera,
            _ => return None,
        };
        let header = Self {
            kind,
            keyframe: head[1] & 0x01 != 0,
            start: head[1] & 0x02 != 0,
            layer: (head[1] >> 4) & 0x03,
            seq: u16::from_be_bytes([head[2], head[3]]),
        };
        Some((header, rest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let h = Header { kind: Kind::Screen, keyframe: true, start: false, layer: 2, seq: 0xbeef };
        let mut wire = h.encode().to_vec();
        wire.extend_from_slice(b"quadro");
        assert_eq!(Header::parse(&wire), Some((h, b"quadro".as_slice())));
        let v = Header::voice(1, true);
        assert_eq!(Header::parse(&v.encode()), Some((v, [].as_slice())));
    }

    #[test]
    fn rejects_unknown() {
        assert_eq!(Header::parse(&[0x10, 0, 0]), None);
        assert_eq!(Header::parse(&[0x20, 0, 0, 0]), None, "versão");
        assert_eq!(Header::parse(&[0x1f, 0, 0, 0]), None, "tipo");
    }
}

/// Codecs de vídeo, do melhor para o pior em compressão (SPEC-04 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub enum Codec {
    Av1,
    Hevc,
    H264,
}

impl Codec {
    pub const ALL: [Codec; 3] = [Codec::Av1, Codec::Hevc, Codec::H264];

    pub fn name(self) -> &'static str {
        match self {
            Codec::Av1 => "AV1",
            Codec::Hevc => "HEVC",
            Codec::H264 => "H.264",
        }
    }

    pub fn to_byte(self) -> u8 {
        match self {
            Codec::Av1 => 1,
            Codec::Hevc => 2,
            Codec::H264 => 3,
        }
    }

    pub fn from_byte(b: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.to_byte() == b)
    }
}

/// Negociação (SPEC-04 §2): o melhor codec que o transmissor codifica e que
/// **todos** os receptores decodificam. `None` se não há nenhum em comum.
pub fn choose_codec<'a>(encode: &[Codec], decoders: impl IntoIterator<Item = &'a [Codec]>) -> Option<Codec> {
    let mut candidates: Vec<Codec> = Codec::ALL.into_iter().filter(|c| encode.contains(c)).collect();
    for d in decoders {
        candidates.retain(|c| d.contains(c));
    }
    candidates.first().copied()
}

#[cfg(test)]
mod codec_tests {
    use super::*;

    #[test]
    fn negotiation_picks_best_common() {
        let enc = [Codec::Hevc, Codec::H264];
        let full: &[Codec] = &[Codec::Av1, Codec::Hevc, Codec::H264];
        let h264_only: &[Codec] = &[Codec::H264];
        assert_eq!(choose_codec(&enc, [full]), Some(Codec::Hevc));
        // Quem não decodifica HEVC entra: desce para H.264.
        assert_eq!(choose_codec(&enc, [full, h264_only]), Some(Codec::H264));
        assert_eq!(choose_codec(&enc, []), Some(Codec::Hevc));
        assert_eq!(choose_codec(&[Codec::Av1], [h264_only]), None);
        for c in Codec::ALL {
            assert_eq!(Codec::from_byte(c.to_byte()), Some(c));
        }
    }
}

/// Maior quadro de vídeo aceito num stream. Um keyframe 1080p fica em
/// ~50–300 KB; o teto limita a memória que um membro consegue ocupar no relay
/// (256 streams em paralelo × 1 MiB).
pub const MAX_VIDEO_FRAME: usize = 1024 * 1024;

/// Camadas de simulcast da tela (SPEC-04 §2): a alta (até 1080p) e a baixa
/// (360p), para quem está com a rede ruim. Cada camada tem a sua sequência.
pub const LAYER_HIGH: u8 = 0;
pub const LAYER_LOW: u8 = 1;

impl Header {
    pub fn video(video: Video, layer: u8, seq: u16, keyframe: bool) -> Self {
        Self { kind: video.kind(), keyframe, start: false, layer, seq }
    }
}

/// Streams de vídeo que um membro publica (SPEC-04 §2–3). A tela vem antes
/// da câmera na prioridade quando falta banda (SPEC-03 §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub enum Video {
    Screen,
    Camera,
}

impl Video {
    pub const ALL: [Video; 2] = [Video::Screen, Video::Camera];

    pub fn kind(self) -> Kind {
        match self {
            Video::Screen => Kind::Screen,
            Video::Camera => Kind::Camera,
        }
    }

    pub fn from_kind(kind: Kind) -> Option<Self> {
        match kind {
            Kind::Screen => Some(Video::Screen),
            Kind::Camera => Some(Video::Camera),
            Kind::Voice => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Video::Screen => "tela",
            Video::Camera => "câmera",
        }
    }
}
