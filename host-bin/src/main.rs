//! `lerrichat-host` — relay headless para Linux (Proxmox/VPS), sem UI nem
//! mídia (SPEC-01 §2).

use std::path::PathBuf;

use anyhow::{Context, bail};
use lerri_host::RelayConfig;

const USAGE: &str = "\
lerrichat-host — relay do Lerri Chat

Uso: lerrichat-host [--port N] [--public HOST]... [--no-meet] [--data-dir PASTA]

  --port      porta UDP das salas (padrão: sorteada uma vez nesta instalação).
              O ponto de encontro usa a mesma porta em TCP e a seguinte em UDP.
  --public    IP ou domínio pelo qual a internet chega aqui (repetível). Use
              quando o relay está atrás de um roteador com porta aberta.
  --no-meet   não sobe o ponto de encontro (sem x1 assistido nem fallback TCP)
  --data-dir  pasta de dados (padrão: $XDG_DATA_HOME/lerrichat)";

#[tokio::main]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,iroh_relay::server=error".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("erro: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> anyhow::Result<()> {
    let mut cfg = RelayConfig { data_dir: PathBuf::new(), port: None, public: Vec::new(), meet: true };
    let mut data_dir: Option<PathBuf> = None;
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().with_context(|| format!("{flag} precisa de um valor"));
        match flag.as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            "--version" | "-V" => {
                println!("lerrichat-host {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--port" => cfg.port = Some(value()?.parse().context("--port inválido")?),
            "--public" => cfg.public.push(value()?),
            "--no-meet" => cfg.meet = false,
            "--data-dir" => data_dir = Some(value()?.into()),
            other => bail!("opção desconhecida: {other}\n\n{USAGE}"),
        }
    }
    cfg.data_dir = match data_dir {
        Some(d) => d,
        None => lerri_core::paths::default_data_dir().context("pasta de dados não encontrada")?,
    };
    lerri_host::run_foreground(&cfg).await
}
