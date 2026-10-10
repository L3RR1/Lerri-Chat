//! Fase 2 de ponta a ponta na mesma máquina: grupo MLS, admissão com `s`,
//! chat cifrado, remoção, saída e commits concorrentes (SPEC-06, Fase 2).

use std::path::PathBuf;
use std::time::Duration;

use lerri_client::Session;
use lerri_client::room::{self, Command, Exit, OpenHow, RoomEvent};
use lerri_core::MemberId;
use lerri_core::invite::{AdmissionSecret, HostAddr, Invite};
use lerri_core::protocol::RoomMode;
use lerri_crypto::Identity;
use lerri_host::{Relay, RelayConfig};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const WAIT: Duration = Duration::from_secs(20);

fn temp_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("lerri-chat-{tag}-{}", lerri_core::ids::hex(&lerri_crypto::random::<6>())))
}

struct Member {
    me: MemberId,
    dir: PathBuf,
    cmds: mpsc::UnboundedSender<Command>,
    ev: mpsc::UnboundedReceiver<RoomEvent>,
    task: JoinHandle<Exit>,
}

impl Member {
    async fn open(identity: &Identity, dir: PathBuf, host: &HostAddr, how: OpenHow, invite_host: Option<HostAddr>) -> Self {
        let (session, core) = room::open(identity, &dir, host, how).await.expect("abrir sala");
        Self::run(identity.member_id(), dir, session, core, invite_host)
    }

    fn run(me: MemberId, dir: PathBuf, session: Session, core: room::RoomCore, invite_host: Option<HostAddr>) -> Self {
        let (cmds, rx) = mpsc::unbounded_channel();
        let (tx, ev) = mpsc::unbounded_channel();
        let d = dir.clone();
        let task = tokio::spawn(async move {
            room::run(session, core, d, invite_host, rx, move |e| {
                let _ = tx.send(e);
            }, |_| {})
            .await
        });
        Self { me, dir, cmds, ev, task }
    }

    fn send(&self, cmd: Command) {
        self.cmds.send(cmd).unwrap();
    }

    /// Espera um evento que satisfaça `pred` (descarta os outros).
    async fn wait(&mut self, what: &str, pred: impl Fn(&RoomEvent) -> bool) -> RoomEvent {
        let mut seen = Vec::new();
        let fut = async {
            loop {
                let e = self.ev.recv().await?;
                if pred(&e) {
                    return Some(e);
                }
                seen.push(e);
            }
        };
        match tokio::time::timeout(WAIT, fut).await {
            Ok(Some(e)) => e,
            Ok(None) => panic!("{what} ({}): sala terminou; eventos: {seen:#?}", self.me.short()),
            Err(_) => panic!("{what} ({}): não aconteceu; eventos: {seen:#?}", self.me.short()),
        }
    }

    async fn admitted(&mut self) {
        self.wait("admissão", |e| matches!(e, RoomEvent::Notice(n) if n.contains("admitido no grupo cifrado"))).await;
    }

    async fn chat_from(&mut self, from: MemberId, text: &str) {
        let t = text.to_string();
        self.wait(&format!("mensagem '{text}'"), move |e| matches!(e, RoomEvent::Chat(l) if l.from == from && l.text == t))
            .await;
    }

    async fn invite(&mut self) -> Invite {
        self.send(Command::NewInvite);
        match self.wait("convite", |e| matches!(e, RoomEvent::Invite(_))).await {
            RoomEvent::Invite(i) => i,
            _ => unreachable!(),
        }
    }

    async fn finish(self, cmd: Command) -> Exit {
        let _ = self.cmds.send(cmd);
        tokio::time::timeout(WAIT, self.task).await.expect("sala não terminou").unwrap()
    }
}

async fn join(identity: &Identity, tag: &str, invite: &Invite) -> Member {
    let how = OpenHow::Join { room: invite.room, token: invite.token, secret: invite.secret.clone() };
    Member::open(identity, temp_dir(tag), &invite.host, how, None).await
}

async fn relay(dir: &std::path::Path) -> Relay {
    Relay::start(&RelayConfig { data_dir: dir.to_path_buf(), port: Some(0), public: vec![], meet: false }).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn e2ee_chat_admission_removal_and_concurrency() {
    let relay_dir = temp_dir("relay");
    let relay = relay(&relay_dir).await;
    let host = relay.host_addr().clone();
    let admin = relay.admin_link().admin.unwrap();
    let ids: Vec<Identity> = (0..4).map(|_| Identity::generate().unwrap()).collect();
    let (o_id, a_id, b_id, c_id) = (&ids[0], &ids[1], &ids[2], &ids[3]);

    // Dono cria a sala; A entra por convite e é admitido automaticamente.
    let mut owner = Member::open(o_id, temp_dir("owner"), &host, OpenHow::Create { admin, mode: RoomMode::Server }, Some(host.clone())).await;
    let invite = owner.invite().await;
    let mut a = join(a_id, "a", &invite).await;
    a.admitted().await;

    // Chat nos dois sentidos.
    owner.send(Command::Send("oi, A".into()));
    a.chat_from(owner.me, "oi, A").await;
    a.send(Command::Send("oi, dono".into()));
    owner.chat_from(a.me, "oi, dono").await;

    // O relay nunca guarda texto em claro (ele só tem o log de commits).
    let stored = std::fs::read(relay_dir.join("rooms-v2.bin")).unwrap();
    assert!(!stored.windows(5).any(|w| w == b"oi, A"), "texto em claro no relay");

    // Relay emite um `t` sem conhecer `s`: o intruso entra no relay mas
    // ninguém o admite no grupo.
    let rogue_id = Identity::generate().unwrap();
    let rogue_token = {
        let s = lerri_client::Session::connect(o_id, &host, lerri_core::protocol::Auth::Rejoin { room: invite.room, epoch: 0 })
            .await
            .unwrap();
        let inv = s.issue_invite(&host, 3600).await.unwrap();
        s.close().await;
        inv
    };
    // A sessão crua do dono derrubou a sala do dono; ele reconecta.
    owner = reopen_after_kick(owner, o_id, &host, invite.room).await;
    let rogue_inv = Invite { secret: AdmissionSecret([0x55; 32]), ..rogue_token };
    let mut rogue = join(&rogue_id, "rogue", &rogue_inv).await;
    rogue.wait("intruso aguardando", |e| matches!(e, RoomEvent::Status(Some(_)))).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        !drain(&mut rogue).iter().any(|e| matches!(e, RoomEvent::Notice(n) if n.contains("admitido no grupo cifrado"))),
        "relay sem `s` conseguiu admitir alguém"
    );
    rogue.finish(Command::Close).await;

    // B e C entram; agora são quatro no grupo.
    let inv_b = owner.invite().await;
    let mut b = join(b_id, "b", &inv_b).await;
    b.admitted().await;
    let inv_c = owner.invite().await;
    let mut c = join(c_id, "c", &inv_c).await;
    c.admitted().await;
    c.send(Command::Send("C chegou".into()));
    b.chat_from(c.me, "C chegou").await;
    a.chat_from(c.me, "C chegou").await;

    // C sai: dono, A e B disputam o commit de remoção ao mesmo tempo; um
    // ganha, os outros descartam o seu e aplicam o vencedor.
    assert_eq!(c.finish(Command::LeaveRoom).await, Exit::Left);
    for m in [&mut owner, &mut a, &mut b] {
        m.wait("remoção de C aplicada", |e| matches!(e, RoomEvent::Notice(n) if n.contains("não faz mais parte"))).await;
    }
    // Todos no mesmo epoch: uma mensagem de cada um chega aos outros.
    owner.send(Command::Send("depois de C".into()));
    a.chat_from(owner.me, "depois de C").await;
    b.chat_from(owner.me, "depois de C").await;
    b.send(Command::Send("B ainda aqui".into()));
    a.chat_from(b.me, "B ainda aqui").await;

    // Dono remove A: a conexão de A cai e ele não lê o que vem depois.
    owner.send(Command::Remove(a.me));
    owner.wait("remoção de A", |e| matches!(e, RoomEvent::Notice(n) if n.contains("não faz mais parte"))).await;
    assert_eq!(tokio::time::timeout(WAIT, a.task).await.unwrap().unwrap(), Exit::Removed);
    owner.send(Command::Send("A não lê isto".into()));
    b.chat_from(owner.me, "A não lê isto").await;
    assert!(!a.dir.join("groups").read_dir().map(|d| d.count() > 0).unwrap_or(false), "estado do removido ficou no disco");

    // B fica offline, o dono faz um commit, B volta e sincroniza pelo log.
    let b_dir = b.dir.clone();
    b.finish(Command::Close).await;
    let inv_d = owner.invite().await;
    let d_id = Identity::generate().unwrap();
    let mut d = join(&d_id, "d", &inv_d).await;
    d.admitted().await;
    let mut b = Member::open(b_id, b_dir, &host, OpenHow::Rejoin { room: invite.room, mode: RoomMode::Server }, None).await;
    d.send(Command::Send("D para todos".into()));
    b.chat_from(d.me, "D para todos").await;
    owner.chat_from(d.me, "D para todos").await;

    for m in [owner, b, d] {
        m.finish(Command::Close).await;
    }
    relay.shutdown().await;
    let _ = std::fs::remove_dir_all(&relay_dir);
}

fn drain(m: &mut Member) -> Vec<RoomEvent> {
    let mut out = Vec::new();
    while let Ok(e) = m.ev.try_recv() {
        out.push(e);
    }
    out
}

/// A sessão crua de teste "conectou em outro lugar" com a identidade do dono;
/// a sala dele cai e reabrimos pelo disco.
async fn reopen_after_kick(owner: Member, id: &Identity, host: &HostAddr, room_id: lerri_core::RoomId) -> Member {
    let dir = owner.dir.clone();
    let _ = tokio::time::timeout(WAIT, owner.task).await;
    Member::open(id, dir, host, OpenHow::Rejoin { room: room_id, mode: RoomMode::Server }, Some(host.clone())).await
}
