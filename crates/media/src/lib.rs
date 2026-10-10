//! Mídia (SPEC-04): voz (Fase 3) e tela (Fase 4).
//!
//! A voz é portável e testável sem dispositivo, menos [`device`], que liga o
//! pipeline ao microfone e às caixas pelo WASAPI. O vídeo ([`video`]) depende
//! da GPU pelo D3D11 e só existe no Windows; o empacotamento dele
//! ([`screen`]) é portável.

pub mod device;
pub mod dsp;
pub mod jitter;
pub mod opus;
pub mod resample;
pub mod screen;
pub mod vad;
#[cfg(windows)]
pub mod video;
pub mod voice;

pub use voice::{Voice, VoiceIo, VoiceStats};
