//! Uso de CPU do processo em porcentagem da máquina inteira (critério do
//! Gerenciador de Tarefas).
//!
//! Não usa `GetProcessTimes`: o Windows cobra tempo de CPU por amostragem no
//! tick do relógio (~15,6 ms), e uma thread que acorda, trabalha < 1 ms e volta
//! a dormir — exatamente o padrão de um pipeline de vídeo na GPU — quase nunca é
//! cobrada. Aqui contamos **ciclos** (`QueryProcessCycleTime`, exato) e
//! convertemos para tempo com a frequência medida do núcleo.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread};
use windows::Win32::System::WindowsProgramming::{QueryProcessCycleTime, QueryThreadCycleTime};

fn process_cycles() -> u64 {
    let mut c = 0u64;
    let _ = unsafe { QueryProcessCycleTime(GetCurrentProcess(), &mut c) };
    c
}

/// Ciclos por segundo de um núcleo ocupado (inclui turbo), medido uma vez.
fn cycles_per_sec() -> f64 {
    static HZ: OnceLock<f64> = OnceLock::new();
    *HZ.get_or_init(|| {
        let thread_cycles = || {
            let mut c = 0u64;
            let _ = unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut c) };
            c
        };
        let (c0, t0) = (thread_cycles(), Instant::now());
        let mut x = 0u64;
        while t0.elapsed() < Duration::from_millis(200) {
            x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
        }
        (thread_cycles() - c0) as f64 / t0.elapsed().as_secs_f64()
    })
}

pub struct CpuMeter {
    wall: Instant,
    cycles: u64,
}

impl CpuMeter {
    pub fn start() -> Self {
        cycles_per_sec(); // calibra antes de começar a contar
        Self { wall: Instant::now(), cycles: process_cycles() }
    }

    /// (% da máquina, % de um núcleo) desde `start`.
    pub fn read(&self) -> (f64, f64) {
        let wall = self.wall.elapsed().as_secs_f64();
        let busy = (process_cycles() - self.cycles) as f64 / cycles_per_sec();
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
        let core_pct = 100.0 * busy / wall;
        (core_pct / cores, core_pct)
    }
}
