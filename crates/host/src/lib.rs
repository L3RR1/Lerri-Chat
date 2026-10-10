//! Relay/SFU cego (SPEC-01 §2, SPEC-02 §2). Na Fase 0 ele só aceita
//! conexões QUIC e devolve frames `Echo`.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::Context;
use lerri_core::protocol::Frame;
use lerri_net::framing::{read_frame, write_frame};
use lerri_net::quinn::{self, Endpoint};
use lerri_net::{Fingerprint, RelayCert};
use tracing::{debug, info, warn};

const PORT_FILE: &str = "relay-port";

pub struct RelayConfig {
    pub data_dir: PathBuf,
    /// `None`: `[::]` (dual-stack) na porta persistida desta instalação.
    pub bind: Option<SocketAddr>,
}

pub struct Relay {
    endpoint: Endpoint,
    fingerprint: Fingerprint,
}

impl Relay {
    pub fn start(cfg: &RelayConfig) -> anyhow::Result<Self> {
        let cert = RelayCert::load_or_create(&cfg.data_dir)?;
        let endpoint = match cfg.bind {
            Some(addr) => lerri_net::server_endpoint(addr, &cert)?,
            None => {
                let port = load_or_pick_port(&cfg.data_dir)?;
                lerri_net::server_endpoint((Ipv6Addr::UNSPECIFIED, port).into(), &cert).or_else(|e| {
                    warn!("sem IPv6 ({e:#}); escutando só em IPv4");
                    lerri_net::server_endpoint((Ipv4Addr::UNSPECIFIED, port).into(), &cert)
                })?
            }
        };
        Ok(Self { endpoint, fingerprint: cert.fingerprint() })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    /// Aceita conexões até [`Relay::close`] ser chamado.
    pub async fn serve(&self) {
        while let Some(incoming) = self.endpoint.accept().await {
            tokio::spawn(async move {
                let remote = incoming.remote_address();
                match incoming.await {
                    Ok(conn) => {
                        info!(%remote, "conexão aceita");
                        if let Err(e) = handle_connection(conn).await {
                            debug!(%remote, "conexão encerrada: {e:#}");
                        }
                    }
                    Err(e) => debug!(%remote, "handshake falhou: {e}"),
                }
            });
        }
    }

    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"relay encerrado");
    }

    /// Espera as conexões abertas receberem o aviso de fechamento.
    pub async fn wait_idle(&self) {
        self.endpoint.wait_idle().await;
    }
}

/// Roda o relay em primeiro plano até Ctrl+C. Usado por `lerrichat --host`
/// e pelo `lerrichat-host`.
pub async fn run_foreground(cfg: &RelayConfig) -> anyhow::Result<()> {
    let relay = Relay::start(cfg)?;
    let addr = relay.local_addr()?;
    println!("Relay escutando em {addr} (UDP)");
    println!("Fingerprint: {}", relay.fingerprint());
    println!("Teste: lerrichat --connect <ip>:{} --fingerprint {}", addr.port(), relay.fingerprint());
    tokio::select! {
        () = relay.serve() => {}
        r = tokio::signal::ctrl_c() => {
            r.context("falha ao esperar Ctrl+C")?;
            info!("encerrando");
            relay.close();
            relay.wait_idle().await;
        }
    }
    Ok(())
}

async fn handle_connection(conn: quinn::Connection) -> anyhow::Result<()> {
    loop {
        let (mut send, mut recv) = conn.accept_bi().await?;
        tokio::spawn(async move {
            let result: anyhow::Result<()> = async {
                while let Some(frame) = read_frame(&mut recv).await? {
                    match frame {
                        Frame::Echo { payload } => write_frame(&mut send, &Frame::Echo { payload }).await?,
                    }
                }
                send.finish()?;
                Ok(())
            }
            .await;
            if let Err(e) = result {
                debug!("stream encerrado com erro: {e:#}");
            }
        });
    }
}

/// Porta UDP sorteada uma vez por instalação na faixa 47000–47999 e
/// reaproveitada depois (SPEC-03 §7), para os convites não envelhecerem.
fn load_or_pick_port(data_dir: &Path) -> anyhow::Result<u16> {
    let path = data_dir.join(PORT_FILE);
    match std::fs::read_to_string(&path) {
        Ok(s) => return s.trim().parse().with_context(|| format!("{} inválido", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("lendo {}", path.display())),
    }
    let port = 47000 + (getrandom::u32().context("RNG do sistema falhou")? % 1000) as u16;
    std::fs::create_dir_all(data_dir)?;
    std::fs::write(&path, port.to_string())?;
    Ok(port)
}
