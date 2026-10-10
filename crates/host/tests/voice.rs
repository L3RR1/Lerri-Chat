//! Fase 3 de ponta a ponta na mesma máquina: voz cifrada (SFrame com chaves
//! do MLS) passando pelo relay, só entre quem está no canal de voz, e
//! presença de "falando agora" (SPEC-06, Fase 3).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use lerri_client::room::{self, Command, Exit, MemberView, OpenHow, RoomEvent};
use lerri_core::MemberId;
use lerri_core::invite::{HostAddr, Invite};
use lerri_core::protocol::RoomMode;
use lerri_crypto::Identity;
use lerri_host::{Relay, RelayConfig};
use lerri_media::Voice;
use lerri_media::vad::level_db;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const WAIT: Duration = Duration::from_secs(20);
const TEN_MS: usize = 480;

fn temp_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("lerri-voice-{tag}-{}", lerri_core::ids::hex(&lerri_crypto::random::<6>())))
}

struct Member {
    me: MemberId,
    cmds: mpsc::UnboundedSender<Command>,
    ev: mpsc::UnboundedReceiver<RoomEvent>,
    task: JoinHandle<Exit>,
}

impl Member {
    async fn open(identity: &Identity, host: &HostAddr, how: OpenHow, invite_host: Option<HostAddr>) -> Self {
        let dir = temp_dir(&identity.member_id().short()[..6]);
        let (session, core) = room::open(identity, &dir, host, how).await.expect("abrir sala");
        let (cmds, rx) = mpsc::unbounded_channel();
        let (tx, ev) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            room::run(session, core, dir, invite_host, rx, move |e| {
                let _ = tx.send(e);
            }, |_| {})
            .await
        });
        Self { me: identity.member_id(), cmds, ev, task }
    }

    fn send(&self, cmd: Command) {
        self.cmds.send(cmd).unwrap();
    }

    async fn wait(&mut self, what: &str, pred: impl Fn(&RoomEvent) -> bool) -> RoomEvent {
        let fut = async {
            loop {
                let e = self.ev.recv().await?;
                if pred(&e) {
                    return Some(e);
                }
            }
        };
        match tokio::time::timeout(WAIT, fut).await {
            Ok(Some(e)) => e,
            Ok(None) => panic!("{what} ({}): sala terminou", self.me.short()),
            Err(_) => panic!("{what} ({}): não aconteceu", self.me.short()),
        }
    }

    async fn admitted(&mut self) {
        self.wait("admissão", |e| matches!(e, RoomEvent::Notice(n) if n.contains("admitido no grupo cifrado"))).await;
    }

    async fn invite(&mut self) -> Invite {
        self.send(Command::NewInvite);
        match self.wait("convite", |e| matches!(e, RoomEvent::Invite(_))).await {
            RoomEvent::Invite(i) => i,
            _ => unreachable!(),
        }
    }

    /// Entra na voz sem dispositivos e devolve o pipeline para o teste.
    async fn join_voice(&mut self) -> Arc<Voice> {
        self.send(Command::JoinVoice { devices: false });
        match self.wait("voz aberta", |e| matches!(e, RoomEvent::VoiceHandle(_))).await {
            RoomEvent::VoiceHandle(h) => h.0,
            _ => unreachable!(),
        }
    }

    /// Espera a lista de membros satisfazer `pred`.
    async fn members(&mut self, what: &str, pred: impl Fn(&[MemberView]) -> bool) {
        self.wait(what, |e| matches!(e, RoomEvent::Members(m) if pred(m))).await;
    }
}

fn view(members: &[MemberView], id: MemberId) -> Option<&MemberView> {
    members.iter().find(|m| m.id == id)
}

/// Fala sintética: tom com envelope de sílabas.
fn speech(i: usize) -> Vec<f32> {
    (i * TEN_MS..(i + 1) * TEN_MS)
        .map(|k| {
            let t = k as f32 / 48_000.0;
            0.3 * (t * 220.0 * std::f32::consts::TAU).sin() * (1.0 + (t * 3.0 * std::f32::consts::TAU).sin()) / 2.0
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn voice_through_blind_relay() {
    let relay_dir = temp_dir("relay");
    let relay =
        Relay::start(&RelayConfig { data_dir: relay_dir.clone(), port: Some(0), public: vec![], meet: false }).await.unwrap();
    let host = relay.host_addr().clone();
    let admin = relay.admin_link().admin.unwrap();
    let ids: Vec<Identity> = (0..3).map(|_| Identity::generate().unwrap()).collect();

    let mut owner = Member::open(&ids[0], &host, OpenHow::Create { admin, mode: RoomMode::Server }, Some(host.clone())).await;
    let mut members = Vec::new();
    for id in &ids[1..] {
        let invite = owner.invite().await;
        let how = OpenHow::Join { room: invite.room, token: invite.token, secret: invite.secret.clone() };
        let mut m = Member::open(id, &invite.host, how, None).await;
        m.admitted().await;
        members.push(m);
    }
    let (mut a, mut b) = (members.remove(0), members.remove(0));

    // Dono e A entram na voz; B fica só no texto e vê os dois na voz.
    let owner_voice = owner.join_voice().await;
    let a_voice = a.join_voice().await;
    let (o, ai) = (owner.me, a.me);
    b.members("dono e A na voz", |m| {
        view(m, o).is_some_and(|v| v.in_voice) && view(m, ai).is_some_and(|v| v.in_voice) && m.iter().any(|v| v.me && !v.in_voice)
    })
    .await;

    // O dono fala ~1,5 s em tempo real; A toca ao mesmo tempo.
    let mut io = owner_voice.io(false).unwrap();
    let mut heard = Vec::new();
    let mut out = vec![0.0; TEN_MS];
    for i in 0..150 {
        io.capture(&speech(i));
        a_voice.render(&mut out);
        heard.extend_from_slice(&out);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (sent, got) = (owner_voice.stats().sent, a_voice.stats().received);
    assert!(sent >= 60, "dono mandou só {sent} pacotes");
    assert!(got * 10 >= sent * 9, "A recebeu {got} de {sent}");
    assert_eq!(a_voice.stats().rejected, 0);
    let level = level_db(&heard[heard.len() / 3..]);
    assert!(level > -35.0, "A ouviu a {level:.1} dBFS");

    // A vê o dono falando (RF-09).
    a.members("dono falando", |m| view(m, o).is_some_and(|v| v.speaking)).await;

    // B entra na voz depois e passa a receber; mudo, o dono para de mandar.
    let b_voice = b.join_voice().await;
    owner.members("B na voz", |m| m.iter().filter(|v| v.in_voice).count() == 3).await;
    for i in 0..50 {
        io.capture(&speech(i));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(b_voice.stats().received >= 15, "B recebeu {}", b_voice.stats().received);
    owner.send(Command::Mute(true));
    owner.wait("mudo", |e| matches!(e, RoomEvent::Voice(v) if v.muted)).await;
    let before = owner_voice.stats().sent;
    for i in 0..50 {
        io.capture(&speech(i));
    }
    assert_eq!(owner_voice.stats().sent, before, "mandou voz no mudo");
    owner.send(Command::Mute(false));

    // A sai da voz: o relay para de mandar para A.
    a.send(Command::LeaveVoice);
    owner.members("A fora da voz", |m| view(m, ai).is_some_and(|v| !v.in_voice)).await;
    let a_before = a_voice.stats().received;
    for i in 0..50 {
        io.capture(&speech(i));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(a_voice.stats().received, a_before, "A recebeu voz fora do canal");

    // Remoção troca o epoch: B continua ouvindo com as chaves novas.
    let b_got = b_voice.stats().received;
    owner.send(Command::Remove(ai));
    owner.wait("A removido", |e| matches!(e, RoomEvent::Notice(n) if n.contains("chaves renovadas"))).await;
    b.wait("commit de remoção", |e| matches!(e, RoomEvent::Notice(n) if n.contains("chaves renovadas"))).await;
    for i in 0..50 {
        io.capture(&speech(i));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(b_voice.stats().received >= b_got + 15, "B parou de ouvir depois do epoch novo");
    assert_eq!(b_voice.stats().rejected, 0);

    for m in [owner, b] {
        let _ = m.cmds.send(Command::Close);
        let _ = tokio::time::timeout(WAIT, m.task).await;
    }
    let _ = tokio::time::timeout(WAIT, a.task).await;
    relay.shutdown().await;
    let _ = std::fs::remove_dir_all(relay_dir);
}
