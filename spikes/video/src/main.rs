//! Spike D-02/D-03 (SPEC-06, Fase 0): o que a GPU desta máquina oferece para
//! encode/decode e se dá para exibir o vídeo decodificado sem passar pela CPU.
//!
//! Uso (com `.deps/ffmpeg/bin` no PATH):
//!   spike-video probe
//!   spike-video headless <arquivo> [segundos]
//!   spike-video play <arquivo> [segundos]

mod cpu;
mod ffdec;
mod overlay;
mod gpu;
mod play;
mod probe;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let seconds = |i: usize| args.get(i).and_then(|s| s.parse().ok()).unwrap_or(20);
    let file = || args.get(1).map(String::as_str).ok_or_else(|| anyhow::anyhow!("falta o arquivo de vídeo"));
    match args.first().map(String::as_str) {
        None | Some("probe") => probe::run(),
        Some("headless") => play::run_headless(file()?, seconds(2)),
        Some("play") => play::run(file()?, seconds(2)),
        Some("play-overlay") => overlay::run(file()?, seconds(2)),
        Some("idle") => play::idle(args.get(1).map(String::as_str).unwrap_or("wgpu"), seconds(2)),
        Some("dump") => play::dump(file()?, seconds(2) as u32, args.get(3).map(String::as_str).unwrap_or("frame.bmp")),
        Some(other) => anyhow::bail!("comando desconhecido: {other}"),
    }
}
