//! `lerrichat.exe` — cliente e relay no mesmo binário (SPEC-01 §2).
//!
//! Fase 0: sem UI. O modo cliente só mostra a identidade ou faz um eco com um
//! relay; a interface chega com a decisão D-02.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, bail};
use lerri_client::RelayConnection;
use lerri_crypto::Identity;
use lerri_host::RelayConfig;
use lerri_net::Fingerprint;

const USAGE: &str = "\
Lerri Chat (Fase 0)

Uso:
  lerrichat                                   mostra sua identidade
  lerrichat --host [--bind ENDEREÇO]          roda o relay
  lerrichat --connect ENDEREÇO --fingerprint HEX [--message TEXTO]
                                              troca uma mensagem com um relay

Opções comuns:
  --data-dir PASTA    pasta de dados (padrão: %APPDATA%\\LerriChat)
  --version, --help";

#[derive(Default)]
struct Args {
    host: bool,
    bind: Option<SocketAddr>,
    connect: Option<String>,
    fingerprint: Option<Fingerprint>,
    message: Option<String>,
    data_dir: Option<PathBuf>,
}

fn parse_args() -> anyhow::Result<Option<Args>> {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().with_context(|| format!("{flag} precisa de um valor"));
        match flag.as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(None);
            }
            "--version" | "-V" => {
                println!("lerrichat {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--host" => args.host = true,
            "--bind" => args.bind = Some(value()?.parse().context("--bind inválido (ex.: [::]:47123)")?),
            "--connect" => args.connect = Some(value()?),
            "--fingerprint" => args.fingerprint = Some(value()?.parse()?),
            "--message" => args.message = Some(value()?),
            "--data-dir" => args.data_dir = Some(value()?.into()),
            other => bail!("opção desconhecida: {other}\n\n{USAGE}"),
        }
    }
    if args.host && args.connect.is_some() {
        bail!("use --host ou --connect, não os dois");
    }
    Ok(Some(args))
}

fn main() -> std::process::ExitCode {
    let started = Instant::now();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let result = (|| {
        let Some(args) = parse_args()? else { return Ok(()) };
        let data_dir = match args.data_dir.clone() {
            Some(d) => d,
            None => lerri_core::paths::default_data_dir().context("pasta de dados não encontrada")?,
        };
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(run(args, data_dir, started))
    })();

    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("erro: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(args: Args, data_dir: PathBuf, started: Instant) -> anyhow::Result<()> {
    if args.host {
        return lerri_host::run_foreground(&RelayConfig { data_dir, bind: args.bind }).await;
    }

    let (identity, created) = Identity::load_or_create(&data_dir)?;
    if created {
        println!("Identidade nova criada em {}", data_dir.display());
    }
    println!("Seu MemberId: {}", identity.member_id());
    tracing::debug!("pronto em {:?}", started.elapsed());

    let Some(target) = args.connect else {
        println!("\n{USAGE}");
        return Ok(());
    };
    let fingerprint = args.fingerprint.context("--connect precisa de --fingerprint (o relay mostra ao subir)")?;
    let addr = tokio::net::lookup_host(&target)
        .await
        .with_context(|| format!("endereço inválido: {target}"))?
        .next()
        .with_context(|| format!("{target} não resolveu para nenhum endereço"))?;

    let conn = RelayConnection::connect(addr, fingerprint).await?;
    let message = args.message.unwrap_or_else(|| "olá do Lerri Chat".into());
    let reply = conn.echo(message.as_bytes()).await?;
    println!("Relay respondeu: {} (RTT {:?})", String::from_utf8_lossy(&reply), conn.rtt());
    conn.close().await;
    Ok(())
}
