//! Classificação dos endereços que vão no convite (SPEC-03 §4.1–4.2) e o
//! aviso de "seu PC não é alcançável de fora".
//!
//! Sem um ponto externo não dá para testar de verdade se alguém de fora chega
//! aqui (firewall do roteador, IPv6 bloqueado na entrada). A classificação é
//! uma estimativa: diz se *existe* um endereço público.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// IPv6 global ou IPv4 público (de interface ou porta mapeada no roteador).
    Internet,
    /// Rede local (192.168/16, 10/8, IPv6 ULA...).
    Lan,
    /// IP de VPN overlay (Radmin, Tailscale, ZeroTier, WireGuard).
    Vpn(String),
    /// IPv4 de CGNAT (100.64/10) que não é de uma interface local: veio do
    /// roteador e não é alcançável de fora.
    Cgnat,
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub addr: SocketAddr,
    pub kind: Kind,
}

/// Interfaces de rede locais (nome + IPs), para reconhecer VPNs.
pub struct LocalInterfaces {
    ifaces: Vec<(String, Vec<IpAddr>)>,
}

impl LocalInterfaces {
    pub fn scan() -> Self {
        let ifaces = netdev::get_interfaces()
            .into_iter()
            .map(|i| {
                let ips = i.ipv4.iter().map(|n| IpAddr::V4(n.addr())).chain(i.ipv6.iter().map(|n| IpAddr::V6(n.addr()))).collect();
                let label = [Some(i.name), i.friendly_name, i.description].into_iter().flatten().collect::<Vec<_>>().join(" ");
                (label, ips)
            })
            .collect();
        Self { ifaces }
    }

    fn label_of(&self, ip: IpAddr) -> Option<&str> {
        self.ifaces.iter().find(|(_, ips)| ips.contains(&ip)).map(|(l, _)| l.as_str())
    }
}

fn vpn_name(label: &str) -> Option<&'static str> {
    let l = label.to_ascii_lowercase();
    [("radmin", "Radmin VPN"), ("tailscale", "Tailscale"), ("zerotier", "ZeroTier"), ("wireguard", "WireGuard"), ("hamachi", "Hamachi")]
        .into_iter()
        .find(|(needle, _)| l.contains(needle))
        .map(|(_, name)| name)
}

/// Classifica um endereço. `None` para o que nunca vai no convite (loopback,
/// link-local, não especificado, multicast).
pub fn classify(ip: IpAddr, local: &LocalInterfaces) -> Option<Kind> {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    };
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return None;
    }
    let iface = local.label_of(ip);
    if let Some(name) = iface.and_then(vpn_name) {
        return Some(Kind::Vpn(name.into()));
    }
    match ip {
        IpAddr::V4(v4) => classify_v4(v4, iface.is_some()),
        IpAddr::V6(v6) => classify_v6(v6),
    }
}

fn classify_v4(ip: Ipv4Addr, on_local_iface: bool) -> Option<Kind> {
    let [a, b, ..] = ip.octets();
    if ip.is_link_local() || ip.is_broadcast() || ip.is_documentation() {
        return None;
    }
    // O Radmin usa 26.0.0.0/8, que não aparece em rede doméstica.
    if a == 26 {
        return Some(Kind::Vpn("Radmin VPN".into()));
    }
    if a == 100 && (64..128).contains(&b) {
        // Numa interface local é overlay (Tailscale); vindo do roteador é a operadora.
        return Some(if on_local_iface { Kind::Vpn("Tailscale".into()) } else { Kind::Cgnat });
    }
    Some(if ip.is_private() { Kind::Lan } else { Kind::Internet })
}

fn classify_v6(ip: Ipv6Addr) -> Option<Kind> {
    let first = ip.segments()[0];
    if first & 0xfe00 == 0xfc00 {
        Some(Kind::Lan) // ULA
    } else if first & 0xe000 == 0x2000 {
        Some(Kind::Internet) // global unicast
    } else {
        None // link-local e o resto
    }
}

/// Candidatos classificados, na ordem do convite (SPEC-03 §4.1): IPv6 global,
/// IPv4 público, LAN, VPN. Descarta o que não serve.
pub fn candidates(addrs: impl IntoIterator<Item = SocketAddr>, local: &LocalInterfaces) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = addrs
        .into_iter()
        .filter_map(|addr| classify(addr.ip(), local).map(|kind| Candidate { addr, kind }))
        .filter(|c| c.kind != Kind::Cgnat)
        .collect();
    out.sort_by_key(|c| match (&c.kind, c.addr.is_ipv6()) {
        (Kind::Internet, true) => 0,
        (Kind::Internet, false) => 1,
        (Kind::Lan, _) => 2,
        (Kind::Vpn(_), _) => 3,
        (Kind::Cgnat, _) => 4,
    });
    let mut seen = Vec::new();
    out.retain(|c| {
        let new = !seen.contains(&c.addr);
        seen.push(c.addr);
        new
    });
    out
}

/// Há pelo menos um endereço público? Sem isso, no x1 sem ponto de encontro só
/// entra quem está na mesma LAN ou VPN.
pub fn reachable_from_internet(cands: &[Candidate]) -> bool {
    cands.iter().any(|c| c.kind == Kind::Internet)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> LocalInterfaces {
        LocalInterfaces { ifaces: vec![] }
    }

    fn k(s: &str) -> Option<Kind> {
        classify(s.parse().unwrap(), &none())
    }

    #[test]
    fn classifies() {
        assert_eq!(k("2804:14c:1::5"), Some(Kind::Internet));
        assert_eq!(k("fd7a:115c:a1e0::1"), Some(Kind::Lan));
        assert_eq!(k("fe80::1"), None);
        assert_eq!(k("::1"), None);
        assert_eq!(k("127.0.0.1"), None);
        assert_eq!(k("192.168.0.10"), Some(Kind::Lan));
        assert_eq!(k("10.1.2.3"), Some(Kind::Lan));
        assert_eq!(k("200.150.1.2"), Some(Kind::Internet));
        assert_eq!(k("26.12.34.56"), Some(Kind::Vpn("Radmin VPN".into())));
        assert_eq!(k("100.70.1.1"), Some(Kind::Cgnat));
        assert_eq!(k("169.254.1.1"), None);
    }

    #[test]
    fn overlay_on_local_interface_is_vpn() {
        let local = LocalInterfaces {
            ifaces: vec![
                ("Tailscale".into(), vec!["100.70.1.1".parse().unwrap()]),
                ("ZeroTier One [abc]".into(), vec!["10.147.0.2".parse().unwrap()]),
            ],
        };
        assert_eq!(classify("100.70.1.1".parse().unwrap(), &local), Some(Kind::Vpn("Tailscale".into())));
        assert_eq!(classify("10.147.0.2".parse().unwrap(), &local), Some(Kind::Vpn("ZeroTier".into())));
    }

    #[test]
    fn orders_and_filters() {
        let addrs = ["192.168.0.10:1", "100.70.1.1:1", "200.150.1.2:1", "[2804::5]:1", "26.1.1.1:1", "127.0.0.1:1", "[2804::5]:1"]
            .map(|s| s.parse().unwrap());
        let c = candidates(addrs, &none());
        let got: Vec<String> = c.iter().map(|c| c.addr.to_string()).collect();
        assert_eq!(got, ["[2804::5]:1", "200.150.1.2:1", "192.168.0.10:1", "26.1.1.1:1"]);
        assert!(reachable_from_internet(&c));
        assert!(!reachable_from_internet(&candidates(["192.168.0.10:1".parse().unwrap()], &none())));
    }
}
