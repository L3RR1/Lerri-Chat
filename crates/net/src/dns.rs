//! Resolvedor de nomes do iroh.
//!
//! Duas razões para não usar o padrão do iroh:
//! - Ele tem servidores DNS públicos de reserva (terceiros, SPEC-02 §9).
//!   Aqui só existe o DNS do sistema.
//! - O cliente de relay do iroh 1.3 não aceita IPv6 literal na URL (passa
//!   `[::1]`, com colchetes, como nome TLS). Por isso o ponto de encontro dado
//!   por IP usa o nome sintético [`MEET_HOST`], que este resolvedor traduz para
//!   o IP do convite. O certificado é validado pela chave, não pelo nome.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use iroh::dns::{BoxIter, DnsError, Resolver, TxtRecordData};
use n0_error::e;
use n0_future::boxed::BoxFuture;

/// Nome reservado (`.invalid`, RFC 2606): nunca sai para a rede.
pub const MEET_HOST: &str = "meet.lerrichat.invalid";

#[derive(Debug, Clone, Default)]
pub struct LocalResolver {
    fixed: Arc<Vec<(String, Vec<IpAddr>)>>,
}

impl LocalResolver {
    pub fn with(name: &str, ips: Vec<IpAddr>) -> Self {
        Self { fixed: Arc::new(vec![(name.to_ascii_lowercase(), ips)]) }
    }

    fn lookup(&self, host: String) -> BoxFuture<Result<Vec<IpAddr>, DnsError>> {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if let Some((_, ips)) = self.fixed.iter().find(|(n, _)| *n == host) {
            let ips = ips.clone();
            return Box::pin(async move { Ok(ips) });
        }
        Box::pin(async move {
            if host.ends_with(".invalid") {
                return Err(e!(DnsError::NoResponse));
            }
            tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map(|it| it.map(|a| a.ip()).collect())
                .map_err(|_| e!(DnsError::NoResponse))
        })
    }
}

fn only<T: Send + 'static>(ips: Vec<IpAddr>, pick: fn(IpAddr) -> Option<T>) -> Result<BoxIter<T>, DnsError> {
    let v: Vec<T> = ips.into_iter().filter_map(pick).collect();
    if v.is_empty() {
        return Err(e!(DnsError::NoResponse));
    }
    Ok(Box::new(v.into_iter()))
}

impl Resolver for LocalResolver {
    fn lookup_ipv4(&self, host: String) -> BoxFuture<Result<BoxIter<Ipv4Addr>, DnsError>> {
        let fut = self.lookup(host);
        Box::pin(async move {
            only(fut.await?, |ip| match ip {
                IpAddr::V4(v4) => Some(v4),
                IpAddr::V6(v6) => v6.to_ipv4_mapped(),
            })
        })
    }

    fn lookup_ipv6(&self, host: String) -> BoxFuture<Result<BoxIter<Ipv6Addr>, DnsError>> {
        let fut = self.lookup(host);
        Box::pin(async move {
            only(fut.await?, |ip| match ip {
                IpAddr::V6(v6) if v6.to_ipv4_mapped().is_none() => Some(v6),
                _ => None,
            })
        })
    }

    fn lookup_txt(&self, _host: String) -> BoxFuture<Result<BoxIter<TxtRecordData>, DnsError>> {
        // Só a descoberta de endereços por DNS da n0 usa TXT, e ela está desligada.
        Box::pin(async { Err(e!(DnsError::NoResponse)) })
    }

    fn clear_cache(&self) {}

    fn reset(&self) -> Box<dyn Resolver> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fixed_names_and_families() {
        let r = LocalResolver::with(MEET_HOST, vec!["2804::1".parse().unwrap(), "1.2.3.4".parse().unwrap()]);
        let v6: Vec<_> = r.lookup_ipv6(MEET_HOST.to_uppercase()).await.unwrap().collect();
        assert_eq!(v6, ["2804::1".parse::<Ipv6Addr>().unwrap()]);
        let v4: Vec<_> = r.lookup_ipv4(format!("{MEET_HOST}.")).await.unwrap().collect();
        assert_eq!(v4, [Ipv4Addr::new(1, 2, 3, 4)]);
        assert!(r.lookup_ipv4("outro.lerrichat.invalid".into()).await.is_err());
        assert!(r.lookup_txt("x".into()).await.is_err());
    }

    #[tokio::test]
    async fn system_dns_for_other_names() {
        let r = LocalResolver::default();
        let v4: Vec<_> = r.lookup_ipv4("localhost".into()).await.unwrap().collect();
        assert!(v4.contains(&Ipv4Addr::LOCALHOST));
    }
}
