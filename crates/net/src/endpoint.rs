use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use anyhow::Context;
use iroh::dns::DnsResolver;
use iroh::endpoint::{BindOpts, PortmapperConfig, QuicTransportConfig, presets};
use iroh::{Endpoint, EndpointAddr, NetReportConfig, PublicKey, RelayConfig, RelayMap, RelayMode, RelayUrl, SecretKey, TransportAddr};
use iroh_relay::RelayQuicConfig;
use lerri_core::invite::{Candidate, HostAddr, MeetingPoint};
use lerri_core::protocol::ALPN;

use crate::dns::{LocalResolver, MEET_HOST};
use crate::pin;

/// Bytes de datagramas esperando para sair, por conexão.
const DATAGRAM_SEND_BUFFER: usize = 32 * 1024;
/// Quadros de tela em trânsito por conexão (um stream cada).
const MAX_UNI_STREAMS: u32 = 256;

pub struct EndpointOpts {
    pub secret: SecretKey,
    /// Porta UDP fixa (relay e host do x1) ou `0` para uma qualquer.
    pub port: u16,
    /// Ponto de encontro (relay iroh). Sem ele, só caminhos diretos.
    pub meet: Option<MeetingPoint>,
    /// Aceita conexões do protocolo do Lerri Chat (host de sala).
    pub accept: bool,
    /// Pede mapeamento de porta ao roteador (PCP/NAT-PMP/UPnP, SPEC-03 §4.2).
    /// Só faz sentido para quem recebe conexões.
    pub portmap: bool,
}

/// Relay iroh de um ponto de encontro. Dado por IP, usa o nome sintético
/// [`MEET_HOST`] (ver [`crate::dns`]).
pub fn meeting_relay(meet: &MeetingPoint) -> anyhow::Result<RelayConfig> {
    let host = if meet.host.parse::<IpAddr>().is_ok() { MEET_HOST } else { meet.host.as_str() };
    let url: RelayUrl = format!("https://{host}:{}", meet.port)
        .parse()
        .with_context(|| format!("ponto de encontro inválido: {}", meet.url()))?;
    let mut cfg = RelayConfig::from(url);
    cfg.quic = Some(RelayQuicConfig::new(meet.quic_port));
    Ok(cfg)
}

pub async fn bind(opts: EndpointOpts) -> anyhow::Result<Endpoint> {
    let mut b = Endpoint::builder(presets::Minimal)
        .secret_key(opts.secret)
        .alpns(if opts.accept { vec![ALPN.to_vec()] } else { vec![] })
        .portmapper_config(if opts.portmap { PortmapperConfig::default() } else { PortmapperConfig::Disabled })
        .net_report_config(net_report())
        .transport_config(transport())
        .dns_resolver(DnsResolver::custom(resolver(opts.meet.as_ref())));
    b = match &opts.meet {
        Some(m) => b
            .relay_mode(RelayMode::Custom(RelayMap::from_iter([meeting_relay(m)?])))
            .ca_tls_config(pin::ca_config(m.key)),
        None => b.relay_mode(RelayMode::Disabled),
    };
    if opts.port != 0 {
        b = b
            .bind_addr((Ipv4Addr::UNSPECIFIED, opts.port))?
            // Sem IPv6 na máquina, segue só com IPv4.
            .bind_addr_with_opts((Ipv6Addr::UNSPECIFIED, opts.port), BindOpts::default().set_is_required(false))?;
    }
    b.bind().await.with_context(|| format!("não foi possível abrir a porta UDP {}", opts.port))
}

/// Converte o endereço do convite para o iroh, resolvendo hostnames pelo DNS
/// do sistema.
pub async fn endpoint_addr(host: &HostAddr) -> anyhow::Result<EndpointAddr> {
    let key = PublicKey::from_bytes(&host.key).context("chave do host inválida")?;
    let mut addrs = Vec::new();
    for c in &host.candidates {
        match c {
            Candidate::Ip(a) => addrs.push(TransportAddr::Ip(*a)),
            Candidate::Name(name, port) => match tokio::net::lookup_host((name.as_str(), *port)).await {
                Ok(resolved) => addrs.extend(resolved.map(TransportAddr::Ip)),
                Err(e) => tracing::warn!("{name} não resolveu: {e}"),
            },
        }
    }
    if let Some(m) = &host.meet {
        addrs.push(TransportAddr::Relay(meeting_relay(m)?.url));
    }
    anyhow::ensure!(!addrs.is_empty(), "o convite não tem nenhum caminho até o host");
    Ok(EndpointAddr::from_parts(key, addrs))
}

/// QUIC ajustado para tempo real (SPEC-03 §6.1):
/// - BBR no lugar do Cubic: não enche o buffer do roteador e sofre menos com
///   perda isolada;
/// - fila de datagramas curta: datagrama velho é descartado, nunca atrasado
///   (~50 ms a 5 Mbps).
fn transport() -> QuicTransportConfig {
    QuicTransportConfig::builder()
        .congestion_controller_factory(std::sync::Arc::new(noq_proto::congestion::Bbr3Config::default()))
        .datagram_send_buffer_size(DATAGRAM_SEND_BUFFER)
        // Tela: um stream unidirecional por quadro (SPEC-03 §6.1).
        .max_concurrent_uni_streams(iroh::endpoint::VarInt::from_u32(MAX_UNI_STREAMS))
        .build()
}

/// Sem a detecção de portal cativo: ela faz um GET em HTTP puro na porta 80
/// do relay, que o ponto de encontro do Lerri Chat não expõe.
fn net_report() -> NetReportConfig {
    let mut cfg = NetReportConfig::default();
    cfg.captive_portal_check = false;
    cfg
}

fn resolver(meet: Option<&MeetingPoint>) -> LocalResolver {
    match meet.and_then(|m| m.host.parse::<IpAddr>().ok()) {
        Some(ip) => LocalResolver::with(MEET_HOST, vec![ip]),
        None => LocalResolver::default(),
    }
}
