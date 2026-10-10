use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Endpoint, EndpointConfig, TokioRuntime, TransportConfig};
use socket2::{Domain, Protocol, Socket, Type};

use crate::tls::{self, Fingerprint, RelayCert};

/// Abre um socket UDP. Endereço IPv6 não especificado (`[::]`) vira
/// dual-stack: o Windows cria sockets IPv6 só-v6 por padrão, e o relay precisa
/// aceitar IPv4 e IPv6 na mesma porta (SPEC-03 §4.1).
pub fn bind_udp(addr: SocketAddr) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    if let SocketAddr::V6(v6) = addr
        && v6.ip().is_unspecified()
    {
        socket.set_only_v6(false)?;
    }
    socket.bind(&addr.into())?;
    Ok(socket.into())
}

fn transport() -> Arc<TransportConfig> {
    let mut t = TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_secs(10)));
    Arc::new(t)
}

/// Endpoint do relay, escutando em `bind`.
pub fn server_endpoint(bind: SocketAddr, cert: &RelayCert) -> anyhow::Result<Endpoint> {
    let crypto = QuicServerConfig::try_from(tls::server_crypto(cert)?)?;
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    cfg.transport_config(transport());
    let socket = bind_udp(bind).with_context(|| format!("não foi possível escutar em {bind}"))?;
    Ok(Endpoint::new(EndpointConfig::default(), Some(cfg), socket, Arc::new(TokioRuntime))?)
}

/// Endpoint de cliente que só confia no relay com o fingerprint `pinned`.
/// Prefere um socket dual-stack e cai para só-IPv4 em máquinas sem IPv6.
pub fn client_endpoint(pinned: Fingerprint) -> anyhow::Result<Endpoint> {
    let socket = bind_udp((Ipv6Addr::UNSPECIFIED, 0).into())
        .or_else(|_| bind_udp((Ipv4Addr::UNSPECIFIED, 0).into()))
        .context("não foi possível abrir socket UDP")?;
    let mut endpoint = Endpoint::new(EndpointConfig::default(), None, socket, Arc::new(TokioRuntime))?;
    let crypto = QuicClientConfig::try_from(tls::client_crypto(pinned)?)?;
    let mut cfg = quinn::ClientConfig::new(Arc::new(crypto));
    cfg.transport_config(transport());
    endpoint.set_default_client_config(cfg);
    Ok(endpoint)
}
