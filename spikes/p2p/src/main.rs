//! Spike D-01 (SPEC-06, Fase 0): iroh rodando **só** com relay próprio.
//!
//! Sobe um relay iroh local (o papel do relay do Lerri Chat), dois endpoints
//! sem nenhum serviço da n0, e verifica:
//! 1. conexão usando só o relay como endereço inicial (como num convite);
//! 2. se o caminho migra para direto (hole punching) depois;
//! 3. stream confiável (eco) e datagramas (o que a mídia usa);
//! 4. que nenhum host de terceiros aparece na configuração.
//!
//! Limitação: tudo roda na mesma máquina, então não há NAT de verdade. O
//! teste com NATs reais fica para a Fase 1.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, RelayMap, RelayMode};
use iroh_relay::server::{AllowAll, CertConfig, QuicConfig, RelayConfig, Server, ServerConfig, TlsConfig};
use iroh_relay::{RelayConfig as RelayEntry, RelayQuicConfig};

const ALPN: &[u8] = b"lerrichat/spike-p2p/0";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Relay com certificado autoassinado (o relay do Lerri Chat terá o seu).
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()])?;
    let cert = ck.cert.der().clone();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der());
    let tls = || -> anyhow::Result<rustls::ServerConfig> {
        Ok(rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone()], key.clone_key().into())?)
    };

    let mut relay_cfg = RelayConfig::new((Ipv4Addr::LOCALHOST, 0));
    relay_cfg.tls = Some(TlsConfig::new((Ipv4Addr::LOCALHOST, 0), CertConfig::Manual { server_config: tls()? }));
    relay_cfg.access = Arc::new(AllowAll);
    let mut quic_cfg = QuicConfig::new((Ipv4Addr::LOCALHOST, 0));
    quic_cfg.server_config = Some(tls()?);
    let mut server_cfg = ServerConfig::default();
    server_cfg.relay = Some(relay_cfg);
    server_cfg.quic = Some(quic_cfg);
    let server = Server::spawn(server_cfg).await?;
    let https = server.https_addr().context("relay sem HTTPS")?;
    let url: iroh::RelayUrl = format!("https://localhost:{}", https.port()).parse()?;
    let quic_port = server.quic_addr().context("relay sem QUIC")?.port();
    println!("relay local: {url} (QUIC na porta {quic_port})");

    let mut entry = RelayEntry::from(url.clone());
    entry.quic = Some(RelayQuicConfig::new(quic_port));
    let relay_map = RelayMap::from_iter([entry]);

    // `Minimal`: só o provedor de criptografia. Sem DNS/pkarr da n0, sem relays da n0.
    let make = |alpns: Vec<Vec<u8>>| {
        Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Custom(relay_map.clone()))
            .ca_tls_config(iroh::tls::CaTlsConfig::custom_roots([cert.clone()]))
            .alpns(alpns)
            .bind()
    };
    let a = make(vec![ALPN.to_vec()]).await?;
    let b = make(vec![]).await?;
    tokio::time::timeout(Duration::from_secs(10), a.online()).await.context("A não ficou online no relay")?;
    println!("A online: id {}", a.id().fmt_short());

    // Servidor de eco em A: streams e datagramas.
    let a_accept = a.clone();
    tokio::spawn(async move {
        while let Some(incoming) = a_accept.accept().await {
            tokio::spawn(async move {
                let conn = incoming.await?;
                let dg = conn.clone();
                tokio::spawn(async move {
                    while let Ok(d) = dg.read_datagram().await {
                        let _ = dg.send_datagram(d);
                    }
                });
                while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                    let data = recv.read_to_end(1 << 20).await?;
                    send.write_all(&data).await?;
                    send.finish()?;
                }
                anyhow::Ok(())
            });
        }
    });

    // Como num convite: B só conhece o id de A e o relay.
    let addr = EndpointAddr::new(a.id()).with_relay_url(url.clone());
    let t0 = Instant::now();
    let conn = b.connect(addr, ALPN).await?;
    println!("B conectou em {:?}", t0.elapsed());
    print_paths("logo após conectar", &conn);

    // Eco por stream.
    let rtt = echo(&conn).await?;
    println!("eco por stream: {rtt:?}");

    // Espera o hole punching promover o caminho direto.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !conn.paths().iter().any(|p| p.is_selected() && p.is_ip()) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    print_paths(&format!("após {:?}", t0.elapsed()), &conn);
    let rtt = echo(&conn).await?;
    println!("eco por stream (caminho atual): {rtt:?}");

    // Datagramas: rajada de 1000 × 1000 bytes, conta quantos voltam.
    println!("tamanho máximo de datagrama: {:?}", conn.max_datagram_size());
    let sent = 1000usize;
    let reader = conn.clone();
    let counter = tokio::spawn(async move {
        let mut got = 0;
        while let Ok(Ok(_)) = tokio::time::timeout(Duration::from_millis(500), reader.read_datagram()).await {
            got += 1;
        }
        got
    });
    let payload = bytes::Bytes::from(vec![7u8; 1000]);
    for _ in 0..sent {
        conn.send_datagram(payload.clone())?;
        tokio::time::sleep(Duration::from_micros(200)).await;
    }
    let got = counter.await?;
    println!("datagramas: {got}/{sent} voltaram ({:.1}% de perda local)", 100.0 * (sent - got) as f64 / sent as f64);

    conn.close(0u32.into(), b"fim");
    b.close().await;
    a.close().await;
    server.shutdown().await?;
    Ok(())
}

async fn echo(conn: &iroh::endpoint::Connection) -> anyhow::Result<Duration> {
    let t = Instant::now();
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(b"ola").await?;
    send.finish()?;
    let back = recv.read_to_end(1024).await?;
    anyhow::ensure!(back == b"ola", "eco diferente");
    Ok(t.elapsed())
}

fn print_paths(when: &str, conn: &iroh::endpoint::Connection) {
    println!("caminhos ({when}):");
    for p in conn.paths().iter() {
        let kind = if p.is_relay() { "relay" } else { "direto" };
        let sel = if p.is_selected() { " ← em uso" } else { "" };
        println!("  {kind:6} {:?} rtt {:?}{sel}", p.remote_addr(), p.rtt());
    }
}
