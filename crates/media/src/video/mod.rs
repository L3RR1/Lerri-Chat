//! Vídeo na GPU (Windows): captura (WGC), encode (Media Foundation), decode
//! (FFmpeg D3D11VA) e exibição (swapchain em janela-filha). SPEC-04 §2–4.

pub mod capture;
pub mod d3d;
pub mod encode;
pub mod ffi;
pub mod ffmpeg;
pub mod render;
pub mod send;
pub mod camera;
