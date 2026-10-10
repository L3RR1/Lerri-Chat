//! `lerrichat-host` — relay headless para Linux (Proxmox/VPS), sem UI nem
//! mídia (SPEC-01 §2).

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, bail};
use lerri_host::RelayConfig;

const USAGE: &str = "\
lerrichat-host — relay do Lerri Chat

Uso: lerrichat-host [--bind ENDEREÇO] [--data-dir PASTA]

  --bind      endereço UDP (padrão: [::] na porta sorteada desta instalação)
  --data-dir  pasta de dados (padrão: $XDG_DATA_HOME/lerrichat)";

#[tokio::main]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
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
    let mut bind: Option<SocketAddr> = None;
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
            "--bind" => bind = Some(value()?.parse().context("--bind inválido")?),
            "--data-dir" => data_dir = Some(value()?.into()),
            other => bail!("opção desconhecida: {other}\n\n{USAGE}"),
        }
    }
    let data_dir = match data_dir {
        Some(d) => d,
        None => lerri_core::paths::default_data_dir().context("pasta de dados não encontrada")?,
    };
    lerri_host::run_foreground(&RelayConfig { data_dir, bind }).await
}
