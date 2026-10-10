//! Relay cego (SPEC-01 §2, SPEC-02 §2): autentica membros, consome convites e
//! avisa presença. Dois jeitos de rodar:
//!
//! - [`Relay`]: modo servidor (`lerrichat --host`, `lerrichat-host`). Chaves e
//!   salas persistem em disco; embute o ponto de encontro (iroh-relay).
//! - [`X1Host`]: dentro do app de quem cria uma sala x1. Tudo em memória; a
//!   sala acaba quando o app fecha.

pub mod meet;
mod service;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use lerri_core::invite::{AdminKey, Candidate, HostAddr, MeetingPoint, RelayLink};
use lerri_crypto::secret_store;
use lerri_net::iroh::{Endpoint, Watcher};
use lerri_net::reach::{self, LocalInterfaces};
use lerri_net::{EndpointOpts, keys};
use tracing::info;

use crate::meet::MeetingServer;
pub use crate::service::RoomService;

const PORT_FILE: &str = "relay-port";
/// Quanto esperar o roteador responder ao mapeamento de porta e o ponto de
/// encontro informar o endereço público, antes de montar o convite.
const SETTLE: Duration = Duration::from_secs(3);

pub struct RelayConfig {
    pub data_dir: PathBuf,
    /// `None`: porta sorteada uma vez por instalação e reaproveitada.
    /// `Some(0)`: qualquer porta livre (testes).
    pub port: Option<u16>,
    /// Endereços públicos informados pelo operador (IP ou domínio), para quando
    /// o relay está atrás de um roteador com porta aberta à mão.
    pub public: Vec<String>,
    /// Sobe o ponto de encontro (iroh-relay) junto.
    pub meet: bool,
}

pub struct Relay {
    endpoint: Endpoint,
    service: Arc<RoomService>,
    meet: Option<MeetingServer>,
    host: HostAddr,
    admin: AdminKey,
}

impl Relay {
    pub async fn start(cfg: &RelayConfig) -> anyhow::Result<Self> {
        let dir = &cfg.data_dir;
        std::fs::create_dir_all(dir).with_context(|| format!("criando {}", dir.display()))?;
        let port = match cfg.port {
            Some(p) => p,
            None => load_or_pick_port(dir)?,
        };
        let host_key = keys::load_or_create(dir, "relay-key.bin", "lerrichat/relay-key/v1")?;
        let meet_key = keys::load_or_create(dir, "meet-key.bin", "lerrichat/meet-key/v1")?;
        let admin = load_or_create_admin(dir)?;

        let meet = if cfg.meet { Some(MeetingServer::spawn(&meet_key, port).await?) } else { None };
        // O próprio SFU usa o ponto de encontro pelo loopback, para quem não
        // tem UDP chegar até ele pelo relay.
        let local_meet = meet.as_ref().map(|m| MeetingPoint {
            host: if m.https.is_ipv6() { "::1".into() } else { "127.0.0.1".into() },
            port: m.https.port(),
            quic_port: m.quic_port().unwrap_or(0),
            key: *meet_key.public().as_bytes(),
        });
        let endpoint =
            lerri_net::bind(EndpointOpts { secret: host_key, port, meet: local_meet, accept: true, portmap: true }).await?;
        let service = Arc::new(RoomService::new(*endpoint.id().as_bytes(), admin, Some(dir.join("rooms-v2.bin")))?);
        tokio::spawn(service.clone().serve(endpoint.clone()));

        let udp_port = endpoint.bound_sockets().first().map_or(port, SocketAddr::port);
        let public: Vec<Candidate> = cfg.public.iter().map(|h| parse_public(h, udp_port)).collect();
        let (mut candidates, _) = discover(&endpoint, meet.is_some()).await;
        candidates.splice(0..0, public);
        // Primeiro endereço que o ponto de encontro de fato atende.
        let meet_host = cfg.public.first().map(|h| strip_port(h).to_string()).or_else(|| {
            let m = meet.as_ref()?;
            candidates.iter().find_map(|c| match c {
                Candidate::Ip(a) if (a.is_ipv4() && m.serves_ipv4()) || (a.is_ipv6() && m.serves_ipv6()) => {
                    Some(a.ip().to_string())
                }
                _ => None,
            })
        });
        let host = HostAddr {
            key: *endpoint.id().as_bytes(),
            meet: meet.as_ref().zip(meet_host).map(|(m, host)| MeetingPoint {
                host,
                port: m.https.port(),
                quic_port: m.quic_port().unwrap_or(0),
                key: *meet_key.public().as_bytes(),
            }),
            candidates,
        };
        Ok(Self { endpoint, service, meet, host, admin })
    }

    /// Como os clientes acham este relay (vai nos convites).
    pub fn host_addr(&self) -> &HostAddr {
        &self.host
    }

    /// Link público: serve de ponto de encontro para salas x1.
    pub fn public_link(&self) -> RelayLink {
        RelayLink { host: self.host.clone(), admin: None }
    }

    /// Link com o segredo que permite criar salas aqui.
    pub fn admin_link(&self) -> RelayLink {
        RelayLink { host: self.host.clone(), admin: Some(self.admin) }
    }

    pub fn udp_port(&self) -> Option<u16> {
        self.endpoint.bound_sockets().first().map(SocketAddr::port)
    }

    pub fn meet_ports(&self) -> Option<(u16, Option<u16>)> {
        self.meet.as_ref().map(|m| (m.https.port(), m.quic_port()))
    }

    pub fn room_count(&self) -> usize {
        self.service.room_count()
    }

    pub async fn shutdown(self) {
        self.endpoint.close().await;
        if let Some(m) = self.meet {
            m.shutdown().await;
        }
    }
}

/// Roda o relay em primeiro plano até Ctrl+C. Usado por `lerrichat --host` e
/// pelo `lerrichat-host`.
pub async fn run_foreground(cfg: &RelayConfig) -> anyhow::Result<()> {
    let relay = Relay::start(cfg).await?;
    let udp = relay.udp_port().unwrap_or(0);
    println!("Relay no ar. Salas: {}", relay.room_count());
    match relay.meet_ports() {
        Some((tcp, qad)) => println!(
            "Portas: UDP {udp} (salas), TCP {tcp} (ponto de encontro), UDP {} (descoberta de endereço)",
            qad.map_or("?".into(), |p| p.to_string())
        ),
        None => println!("Porta: UDP {udp} (salas)"),
    }
    println!("Endereços no convite:");
    for c in &relay.host_addr().candidates {
        println!("  {c}");
    }
    if cfg.public.is_empty() {
        println!("Atrás de um roteador? Abra essas portas para esta máquina e rode de novo com --public <IP ou domínio>.");
    }
    println!("\nLink de administração (cria salas; guarde só para você):\n{}", relay.admin_link().to_link());
    println!("\nLink público do relay (ponto de encontro para salas x1):\n{}", relay.public_link().to_link());
    tokio::signal::ctrl_c().await.context("falha ao esperar Ctrl+C")?;
    info!("encerrando");
    relay.shutdown().await;
    Ok(())
}

/// Host de uma sala x1, rodando dentro do app de quem criou.
pub struct X1Host {
    endpoint: Endpoint,
    service: Arc<RoomService>,
    admin: AdminKey,
    meet: Option<MeetingPoint>,
}

impl X1Host {
    /// `meet`: ponto de encontro de um relay do usuário (x1 assistido). Sem ele
    /// é o x1 sem infra: só caminhos diretos (SPEC-03 §4.2).
    pub async fn start(meet: Option<MeetingPoint>, port: u16) -> anyhow::Result<Self> {
        let endpoint =
            lerri_net::bind(EndpointOpts { secret: keys::ephemeral(), port, meet: meet.clone(), accept: true, portmap: true })
                .await?;
        let admin = AdminKey(lerri_crypto::random());
        let service = Arc::new(RoomService::new(*endpoint.id().as_bytes(), admin, None)?);
        tokio::spawn(service.clone().serve(endpoint.clone()));
        Ok(Self { endpoint, service, admin, meet })
    }

    /// Espera os endereços assentarem e devolve o que vai no convite, com a
    /// classificação usada no aviso de alcançabilidade.
    pub async fn host_addr(&self) -> (HostAddr, Vec<reach::Candidate>) {
        let (candidates, classified) = discover(&self.endpoint, self.meet.is_some()).await;
        (HostAddr { key: *self.endpoint.id().as_bytes(), candidates, meet: self.meet.clone() }, classified)
    }

    /// Endereço para o próprio criador entrar na sala, pelo loopback.
    pub fn loopback_addr(&self) -> HostAddr {
        let candidates = self
            .endpoint
            .bound_sockets()
            .into_iter()
            .map(|a| {
                let ip = match a.ip() {
                    IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
                    IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
                };
                Candidate::Ip(SocketAddr::new(ip, a.port()))
            })
            .collect();
        HostAddr { key: *self.endpoint.id().as_bytes(), candidates, meet: None }
    }

    pub fn admin(&self) -> AdminKey {
        self.admin
    }

    pub fn room_count(&self) -> usize {
        self.service.room_count()
    }

    pub async fn shutdown(self) {
        self.endpoint.close().await;
    }
}

/// Coleta os endereços diretos do endpoint (interfaces, porta mapeada no
/// roteador, endereço público visto pelo ponto de encontro).
async fn discover(endpoint: &Endpoint, has_meet: bool) -> (Vec<Candidate>, Vec<reach::Candidate>) {
    if has_meet {
        let _ = tokio::time::timeout(SETTLE, endpoint.online()).await;
    }
    // O mapeamento de porta chega depois; espera ele (ou o prazo).
    let mut watcher = endpoint.watch_addr();
    let _ = tokio::time::timeout(SETTLE, async {
        while watcher.updated().await.is_ok() {
            if endpoint.addr().ip_addrs().count() > endpoint.bound_sockets().len() + 1 {
                break;
            }
        }
    })
    .await;
    let addr = endpoint.addr();
    let classified = reach::candidates(addr.ip_addrs().copied(), &LocalInterfaces::scan());
    (classified.iter().map(|c| Candidate::Ip(c.addr)).collect(), classified)
}

/// `--public 1.2.3.4`, `--public casa.exemplo.org` ou com porta explícita.
fn parse_public(s: &str, default_port: u16) -> Candidate {
    if let Ok(addr) = s.parse::<SocketAddr>() {
        return Candidate::Ip(addr);
    }
    if let Ok(ip) = s.trim_matches(['[', ']']).parse::<IpAddr>() {
        return Candidate::Ip(SocketAddr::new(ip, default_port));
    }
    match s.rsplit_once(':') {
        Some((host, port)) if port.parse::<u16>().is_ok() => Candidate::Name(host.into(), port.parse().unwrap()),
        _ => Candidate::Name(s.into(), default_port),
    }
}

fn strip_port(s: &str) -> &str {
    if let Ok(addr) = s.parse::<SocketAddr>() {
        return match addr {
            SocketAddr::V4(_) => s.rsplit_once(':').map_or(s, |(h, _)| h),
            SocketAddr::V6(_) => s.rsplit_once("]:").map_or(s, |(h, _)| h.trim_start_matches('[')),
        };
    }
    if s.parse::<IpAddr>().is_ok() {
        return s;
    }
    match s.rsplit_once(':') {
        Some((host, port)) if port.parse::<u16>().is_ok() => host,
        _ => s.trim_matches(['[', ']']),
    }
}

fn load_or_create_admin(dir: &Path) -> anyhow::Result<AdminKey> {
    const PURPOSE: &str = "lerrichat/relay-admin/v1";
    let path = dir.join("relay-admin.bin");
    if let Some(bytes) = secret_store::load(&path, PURPOSE)? {
        return Ok(AdminKey(bytes.as_slice().try_into().ok().with_context(|| format!("{} corrompido", path.display()))?));
    }
    let key = AdminKey(lerri_crypto::random());
    secret_store::save(&path, PURPOSE, &key.0)?;
    Ok(key)
}

/// Porta UDP sorteada uma vez por instalação na faixa 47000–47998 e
/// reaproveitada depois (SPEC-03 §7), para os convites não envelhecerem. A
/// seguinte fica para a descoberta de endereço do ponto de encontro.
fn load_or_pick_port(data_dir: &Path) -> anyhow::Result<u16> {
    let path = data_dir.join(PORT_FILE);
    match std::fs::read_to_string(&path) {
        Ok(s) => return s.trim().parse().with_context(|| format!("{} inválido", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("lendo {}", path.display())),
    }
    let port = 47000 + (u32::from_le_bytes(lerri_crypto::random()) % 999) as u16;
    std::fs::write(&path, port.to_string())?;
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_addresses() {
        assert_eq!(parse_public("1.2.3.4", 47000), Candidate::Ip("1.2.3.4:47000".parse().unwrap()));
        assert_eq!(parse_public("1.2.3.4:9", 47000), Candidate::Ip("1.2.3.4:9".parse().unwrap()));
        assert_eq!(parse_public("2804::1", 47000), Candidate::Ip("[2804::1]:47000".parse().unwrap()));
        assert_eq!(parse_public("[2804::1]:9", 47000), Candidate::Ip("[2804::1]:9".parse().unwrap()));
        assert_eq!(parse_public("casa.org", 47000), Candidate::Name("casa.org".into(), 47000));
        assert_eq!(parse_public("casa.org:9", 47000), Candidate::Name("casa.org".into(), 9));
        assert_eq!(strip_port("1.2.3.4:9"), "1.2.3.4");
        assert_eq!(strip_port("[2804::1]:9"), "2804::1");
        assert_eq!(strip_port("2804::1"), "2804::1");
        assert_eq!(strip_port("casa.org:9"), "casa.org");
        assert_eq!(strip_port("casa.org"), "casa.org");
    }
}
