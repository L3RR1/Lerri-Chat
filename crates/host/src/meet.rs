//! Ponto de encontro embutido no relay: um `iroh-relay` com TLS fixado pela
//! chave do convite (SPEC-03 §4.3). Coordena o hole punching, informa a cada
//! peer seu endereço público (QAD, no lugar do STUN) e repassa o tráfego
//! cifrado quando o caminho direto falha.
//!
//! Portas, para a porta `P` do relay:
//! - UDP `P`: o SFU (endpoint iroh do relay);
//! - TCP `P`: HTTPS do relay iroh;
//! - UDP `P+1`: descoberta de endereço (QAD).

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use anyhow::Context;
use iroh_relay::server::{AllowAll, CertConfig, QuicConfig, RelayConfig, Server, ServerConfig, TlsConfig};
use lerri_net::iroh::SecretKey;
use tracing::warn;

pub struct MeetingServer {
    server: Server,
    pub https: SocketAddr,
}

impl MeetingServer {
    /// No Linux sobe em `[::]`, que lá é dual-stack, e cai para IPv4 se a máquina
    /// não tiver IPv6. No Windows, `[::]` atenderia só IPv6 e o iroh-relay não
    /// deixa mudar isso; lá o ponto de encontro fica só em IPv4.
    pub async fn spawn(key: &SecretKey, port: u16) -> anyhow::Result<Self> {
        if cfg!(windows) {
            return Self::spawn_on(key, Ipv4Addr::UNSPECIFIED.into(), port).await;
        }
        match Self::spawn_on(key, Ipv6Addr::UNSPECIFIED.into(), port).await {
            Ok(s) => Ok(s),
            Err(e) => {
                warn!("ponto de encontro sem IPv6 ({e:#}); usando só IPv4");
                Self::spawn_on(key, Ipv4Addr::UNSPECIFIED.into(), port).await
            }
        }
    }

    async fn spawn_on(key: &SecretKey, ip: std::net::IpAddr, port: u16) -> anyhow::Result<Self> {
        let tls = lerri_net::pin::server_tls(key)?;
        // O iroh-relay sempre abre também um HTTP sem TLS (detecção de portal
        // cativo). Os clientes não usam; fica preso no loopback.
        let mut relay = RelayConfig::new((Ipv4Addr::LOCALHOST, 0));
        relay.tls = Some(TlsConfig::new((ip, port), CertConfig::Manual { server_config: tls.clone() }));
        relay.access = Arc::new(AllowAll);
        let mut quic = QuicConfig::new((ip, if port == 0 { 0 } else { port.wrapping_add(1) }));
        quic.server_config = Some(tls);
        let mut cfg = ServerConfig::default();
        cfg.relay = Some(relay);
        cfg.quic = Some(quic);
        let server = Server::spawn(cfg).await.context("ponto de encontro não subiu")?;
        let https = server.https_addr().context("ponto de encontro sem HTTPS")?;
        Ok(Self { server, https })
    }

    /// Atende clientes IPv4? (Em `[::]` no Linux, sim.)
    pub fn serves_ipv4(&self) -> bool {
        self.https.is_ipv4() || !cfg!(windows)
    }

    /// Atende clientes IPv6?
    pub fn serves_ipv6(&self) -> bool {
        self.https.is_ipv6()
    }

    pub fn quic_port(&self) -> Option<u16> {
        self.server.quic_addr().map(|a| a.port())
    }

    pub async fn shutdown(self) {
        let _ = self.server.shutdown().await;
    }
}
