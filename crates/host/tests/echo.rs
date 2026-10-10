use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use lerri_client::RelayConnection;
use lerri_host::{Relay, RelayConfig};
use lerri_net::Fingerprint;

fn temp_dir() -> std::path::PathBuf {
    let n = getrandom::u64().unwrap();
    std::env::temp_dir().join(format!("lerri-echo-{n:016x}"))
}

fn start_relay(dir: &std::path::Path, bind: SocketAddr) -> Arc<Relay> {
    let relay = Arc::new(Relay::start(&RelayConfig { data_dir: dir.to_path_buf(), bind: Some(bind) }).unwrap());
    let r = relay.clone();
    tokio::spawn(async move { r.serve().await });
    relay
}

#[tokio::test]
async fn echo_over_quic() {
    let dir = temp_dir();
    let relay = start_relay(&dir, (Ipv4Addr::LOCALHOST, 0).into());
    let addr = relay.local_addr().unwrap();

    let conn = RelayConnection::connect(addr, relay.fingerprint()).await.unwrap();
    assert_eq!(conn.echo(b"ola, relay").await.unwrap(), b"ola, relay");
    // Vários streams na mesma conexão.
    for i in 0..5u8 {
        assert_eq!(conn.echo(&[i; 1000]).await.unwrap(), vec![i; 1000]);
    }
    conn.close().await;
    relay.close();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn dual_stack_accepts_ipv4() {
    let dir = temp_dir();
    let Ok(relay) = Relay::start(&RelayConfig { data_dir: dir.clone(), bind: Some((Ipv6Addr::UNSPECIFIED, 0).into()) })
    else {
        eprintln!("máquina sem IPv6; teste ignorado");
        return;
    };
    let relay = Arc::new(relay);
    let r = relay.clone();
    tokio::spawn(async move { r.serve().await });
    let port = relay.local_addr().unwrap().port();

    let conn = RelayConnection::connect((Ipv4Addr::LOCALHOST, port).into(), relay.fingerprint()).await.unwrap();
    assert_eq!(conn.echo(b"v4").await.unwrap(), b"v4");
    conn.close().await;
    relay.close();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn rejects_wrong_fingerprint() {
    let dir = temp_dir();
    let relay = start_relay(&dir, (Ipv4Addr::LOCALHOST, 0).into());
    let addr = relay.local_addr().unwrap();

    let mut wrong = relay.fingerprint().0;
    wrong[0] ^= 0xff;
    assert!(RelayConnection::connect(addr, Fingerprint(wrong)).await.is_err());
    relay.close();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn fingerprint_survives_restart() {
    let dir = temp_dir();
    let a = Relay::start(&RelayConfig { data_dir: dir.clone(), bind: Some((Ipv4Addr::LOCALHOST, 0).into()) }).unwrap();
    let fp = a.fingerprint();
    a.close();
    drop(a);
    let b = Relay::start(&RelayConfig { data_dir: dir.clone(), bind: Some((Ipv4Addr::LOCALHOST, 0).into()) }).unwrap();
    assert_eq!(b.fingerprint(), fp);
    b.close();
    std::fs::remove_dir_all(&dir).unwrap();
}
