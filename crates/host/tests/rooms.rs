//! Fase 1 de ponta a ponta na mesma máquina: salas nos dois modos, convites de
//! uso único, presença, persistência e fixação de chaves.

use std::path::PathBuf;
use std::time::Duration;

use lerri_client::{Event, Session, refusal};
use lerri_core::invite::{HostAddr, Link, RouteToken};
use lerri_core::protocol::{Auth, Refusal, Role, RoomMode};
use lerri_crypto::Identity;
use lerri_host::{Relay, RelayConfig, X1Host};

fn temp_dir() -> PathBuf {
    std::env::temp_dir().join(format!("lerri-rooms-{}", lerri_core::ids::hex(&lerri_crypto::random::<8>())))
}

fn id() -> Identity {
    Identity::generate().unwrap()
}

async fn start_relay(dir: &std::path::Path) -> Relay {
    Relay::start(&RelayConfig { data_dir: dir.to_path_buf(), port: Some(0), public: vec![], meet: true }).await.unwrap()
}

async fn next(s: &mut Session) -> Event {
    tokio::time::timeout(Duration::from_secs(10), s.next_event()).await.expect("evento não chegou").unwrap()
}

async fn refused(identity: &Identity, host: &HostAddr, auth: Auth) -> Refusal {
    let err = Session::connect(identity, host, auth).await.err().expect("deveria ser recusado");
    refusal(&err).unwrap_or_else(|| panic!("erro sem Refusal: {err:#}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn server_mode_rooms_and_invites() {
    let dir = temp_dir();
    let relay = start_relay(&dir).await;
    let host = relay.host_addr().clone();
    let admin = relay.admin_link().admin.unwrap();
    let (owner_id, guest_id, intruder_id) = (id(), id(), id());

    // Criar sala exige o segredo de administração.
    let bad = Auth::CreateRoom { admin: lerri_core::invite::AdminKey([0; 32]), mode: RoomMode::Server };
    assert_eq!(refused(&owner_id, &host, bad).await, Refusal::BadAdmin);
    let mut owner = Session::connect(&owner_id, &host, Auth::CreateRoom { admin, mode: RoomMode::Server }).await.unwrap();
    assert_eq!((owner.role, owner.mode), (Role::Owner, RoomMode::Server));
    let room = owner.room;

    // Convite: o link sobrevive ao vaivém de texto e entra uma vez só.
    let invite = owner.issue_invite(&host, 3600).await.unwrap();
    let Link::Join(parsed) = Link::parse(&invite.to_link()).unwrap() else { panic!() };
    assert_eq!(parsed, invite);
    let join = Auth::Join { room, token: parsed.token };
    let guest = Session::connect(&guest_id, &parsed.host, join.clone()).await.unwrap();
    assert_eq!(guest.role, Role::Member);
    assert_eq!(guest.online, vec![owner_id.member_id()]);
    assert_eq!(next(&mut owner).await, Event::Online { member: guest_id.member_id(), first_time: true });
    assert_eq!(refused(&intruder_id, &host, join).await, Refusal::InviteUsed);

    // Convite desconhecido, expirado, emitido por quem não é dono.
    let unknown = Auth::Join { room, token: RouteToken([9; 16]) };
    assert_eq!(refused(&intruder_id, &host, unknown).await, Refusal::InviteUnknown);
    let expired = owner.issue_invite(&host, 0).await.unwrap();
    let auth = Auth::Join { room, token: expired.token };
    assert_eq!(refused(&intruder_id, &host, auth).await, Refusal::InviteExpired);
    let err = guest.issue_invite(&host, 3600).await.unwrap_err();
    assert_eq!(refusal(&err), Some(Refusal::NotOwner));

    // Quem não é membro não volta; quem é, volta sem convite.
    assert_eq!(refused(&intruder_id, &host, Auth::Rejoin { room, epoch: 0 }).await, Refusal::NotMember);
    guest.close().await;
    assert_eq!(next(&mut owner).await, Event::Offline { member: guest_id.member_id() });
    let guest = Session::connect(&guest_id, &host, Auth::Rejoin { room, epoch: 0 }).await.unwrap();
    assert_eq!(next(&mut owner).await, Event::Online { member: guest_id.member_id(), first_time: false });
    guest.close().await;
    owner.close().await;

    // Reiniciar o relay mantém chave, porta-chave do convite e salas.
    relay.shutdown().await;
    let relay = start_relay(&dir).await;
    assert_eq!(relay.host_addr().key, host.key);
    assert_eq!(relay.room_count(), 1);
    let guest = Session::connect(&guest_id, relay.host_addr(), Auth::Rejoin { room, epoch: 0 }).await.unwrap();
    assert_eq!(guest.role, Role::Member);
    guest.close().await;
    relay.shutdown().await;
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn x1_without_infra() {
    let x1 = X1Host::start(None, 0).await.unwrap();
    let (public, classified) = x1.host_addr().await;
    assert!(public.meet.is_none());
    assert!(!classified.is_empty(), "nenhum endereço utilizável neste PC (sem LAN?)");
    let (owner_id, guest_id) = (id(), id());

    let mut owner =
        Session::connect(&owner_id, &x1.loopback_addr(), Auth::CreateRoom { admin: x1.admin(), mode: RoomMode::X1 })
            .await
            .unwrap();
    let invite = owner.issue_invite(&public, 3600).await.unwrap();
    let guest = Session::connect(&guest_id, &invite.host, Auth::Join { room: invite.room, token: invite.token })
        .await
        .unwrap();
    assert_eq!(guest.mode, RoomMode::X1);
    assert!(guest.route().unwrap().direct, "x1 sem infra só tem caminho direto");
    assert_eq!(next(&mut owner).await, Event::Online { member: guest_id.member_id(), first_time: true });
    guest.close().await;
    owner.close().await;
    x1.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn x1_assisted_by_meeting_point() {
    let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).with_test_writer().try_init();
    let dir = temp_dir();
    let relay = start_relay(&dir).await;
    let meet = relay.host_addr().meet.clone().expect("relay sem ponto de encontro");

    let x1 = X1Host::start(Some(meet.clone()), 0).await.unwrap();
    let (owner_id, guest_id) = (id(), id());
    let owner =
        Session::connect(&owner_id, &x1.loopback_addr(), Auth::CreateRoom { admin: x1.admin(), mode: RoomMode::X1 })
            .await
            .unwrap();
    // Só a chave do host e o ponto de encontro: nenhum endereço direto. É o
    // caso do criador atrás de NAT; o caminho direto vem do hole punching.
    let only_meet = HostAddr { key: x1.loopback_addr().key, candidates: vec![], meet: Some(meet.clone()) };
    let invite = owner.issue_invite(&only_meet, 3600).await.unwrap();
    let guest = Session::connect(&guest_id, &invite.host, Auth::Join { room: invite.room, token: invite.token })
        .await
        .unwrap();
    let mut direct = false;
    for _ in 0..50 {
        if guest.route().is_some_and(|r| r.direct) {
            direct = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(direct, "não migrou para caminho direto: {:?}", guest.route());

    // Ponto de encontro com chave errada: o cliente recusa o relay.
    let mut wrong = meet;
    wrong.key = [7; 32];
    let host = HostAddr { key: only_meet.key, candidates: vec![], meet: Some(wrong) };
    let attempt = tokio::time::timeout(Duration::from_secs(8), Session::connect(&id(), &host, Auth::Rejoin { room: invite.room, epoch: 0 })).await;
    assert!(!matches!(attempt, Ok(Ok(_))), "conectou por um relay com chave errada");

    guest.close().await;
    owner.close().await;
    x1.shutdown().await;
    relay.shutdown().await;
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_host_key_is_rejected() {
    let x1 = X1Host::start(None, 0).await.unwrap();
    let mut host = x1.loopback_addr();
    host.key = *lerri_net::keys::ephemeral().public().as_bytes();
    let attempt = tokio::time::timeout(Duration::from_secs(8), Session::connect(&id(), &host, Auth::Rejoin { room: lerri_core::RoomId([0; 16]), epoch: 0 })).await;
    assert!(!matches!(attempt, Ok(Ok(_))), "conectou num host com chave diferente da fixada");
    x1.shutdown().await;
}

/// Com duas pessoas na sala, o relay conta a cada 3 s a ida e volta de cada
/// uma até ele.
#[tokio::test(flavor = "multi_thread")]
async fn relay_reports_latency_of_each_member() {
    let dir = temp_dir();
    let relay = start_relay(&dir).await;
    let host = relay.host_addr().clone();
    let admin = relay.admin_link().admin.unwrap();
    let (owner_id, guest_id) = (id(), id());
    let owner = Session::connect(&owner_id, &host, Auth::CreateRoom { admin, mode: RoomMode::Server }).await.unwrap();
    let invite = owner.issue_invite(&host, 3600).await.unwrap();
    let guest = Session::connect(&guest_id, &host, Auth::Join { room: owner.room, token: invite.token }).await.unwrap();

    let mut rtts = Vec::new();
    for _ in 0..80 {
        rtts = guest.latency();
        if rtts.len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut who: Vec<_> = rtts.iter().map(|(m, _)| *m).collect();
    who.sort();
    let mut want = vec![owner_id.member_id(), guest_id.member_id()];
    want.sort();
    assert_eq!(who, want, "latência dos dois");
    assert!(rtts.iter().all(|(_, ms)| *ms < 1000), "{rtts:?}");
    guest.close().await;
    owner.close().await;
    relay.shutdown().await;
    let _ = std::fs::remove_dir_all(dir);
}
