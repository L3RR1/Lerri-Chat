//! Fases 4 e 5 de ponta a ponta na mesma máquina, sem GPU: quadros de tela e
//! de câmera cifrados (SFrame) em streams QUIC passando pelo relay só para
//! quem assiste cada um, keyframe sob pedido e contagem de quem assiste
//! (SPEC-06, Fases 4–5).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use lerri_client::room::{self, Command, Exit, MemberView, OpenHow, PublishSource, RoomEvent, WatchTarget};
use lerri_client::screen::Outgoing;
use lerri_core::MemberId;
use lerri_core::invite::{HostAddr, Invite};
use lerri_core::media::{Codec, Video};
use lerri_core::protocol::RoomMode;
use lerri_crypto::Identity;
use lerri_host::{Relay, RelayConfig};
use lerri_media::screen::Frame;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const WAIT: Duration = Duration::from_secs(20);

fn temp_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("lerri-screen-{tag}-{}", lerri_core::ids::hex(&lerri_crypto::random::<6>())))
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

    async fn invite(&mut self) -> Invite {
        self.send(Command::NewInvite);
        match self.wait("convite", |e| matches!(e, RoomEvent::Invite(_))).await {
            RoomEvent::Invite(i) => i,
            _ => unreachable!(),
        }
    }

    async fn members(&mut self, what: &str, pred: impl Fn(&[MemberView]) -> bool) {
        self.wait(what, |e| matches!(e, RoomEvent::Members(m) if pred(m))).await;
    }

    async fn watchers(&mut self, video: Video, n: u32) {
        self.wait(&format!("{n} assistindo a {}", video.name()), |e| {
            matches!(e, RoomEvent::Stream { video: v, view } if *v == video && view.sharing.is_some() && view.watchers == n)
        })
        .await;
    }

    /// Publica sem GPU e devolve a saída para mandar quadros.
    async fn publish(&mut self, source: PublishSource) -> Arc<Outgoing> {
        let video = source.video();
        self.send(Command::Publish { source, gpu: false });
        match self.wait("vídeo no ar", |e| matches!(e, RoomEvent::OutgoingHandle { video: v, .. } if *v == video)).await {
            RoomEvent::OutgoingHandle { handle, .. } => handle.0,
            _ => unreachable!(),
        }
    }

    /// Assiste e devolve por onde chegam os quadros.
    fn watch(&self, member: MemberId, video: Video) -> mpsc::UnboundedReceiver<Frame> {
        let (tx, rx) = mpsc::unbounded_channel::<Frame>();
        let sink: lerri_client::screen::Sink = Arc::new(move |f| {
            let _ = tx.send(f);
        });
        self.send(Command::Watch { member, video, target: WatchTarget::Frames(sink) });
        rx
    }
}

fn sharing(members: &[MemberView], id: MemberId) -> bool {
    members.iter().any(|m| m.id == id && m.sharing)
}

fn camera(members: &[MemberView], id: MemberId) -> bool {
    members.iter().any(|m| m.id == id && m.camera)
}

async fn collect(rx: &mut mpsc::UnboundedReceiver<Frame>, n: usize) -> Vec<Frame> {
    let mut got = Vec::new();
    while got.len() < n {
        let Ok(f) = tokio::time::timeout(WAIT, rx.recv()).await else {
            panic!("só {} de {n} quadros chegaram: {:?}", got.len(), got.iter().map(|f: &Frame| f.seq).collect::<Vec<_>>())
        };
        got.push(f.unwrap());
    }
    got.sort_by_key(|f| f.seq);
    got
}

/// Quadro sintético de `len` bytes, diferente por `seq`.
fn payload(seq: u16, len: usize) -> Vec<u8> {
    (0..len).map(|i| (i as u16 ^ seq.wrapping_mul(31)) as u8).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn screen_through_blind_relay() {
    let _ = tracing_subscriber::fmt().with_env_filter(std::env::var("RUST_LOG").unwrap_or_default()).with_test_writer().try_init();
    let relay_dir = temp_dir("relay");
    let relay =
        Relay::start(&RelayConfig { data_dir: relay_dir.clone(), port: Some(0), public: vec![], meet: false }).await.unwrap();
    let host = relay.host_addr().clone();
    let admin = relay.admin_link().admin.unwrap();
    let ids: Vec<Identity> = (0..3).map(|_| Identity::generate().unwrap()).collect();

    let mut owner = Member::open(&ids[0], &host, OpenHow::Create { admin, mode: RoomMode::Server }, Some(host.clone())).await;
    let mut others = Vec::new();
    for id in &ids[1..] {
        let invite = owner.invite().await;
        let how = OpenHow::Join { room: invite.room, token: invite.token, secret: invite.secret.clone() };
        let mut m = Member::open(id, &invite.host, how, None).await;
        m.wait("admissão", |e| matches!(e, RoomEvent::Notice(n) if n.contains("admitido no grupo cifrado"))).await;
        others.push(m);
    }
    let (mut a, mut b) = (others.remove(0), others.remove(0));
    let o = owner.me;

    // O dono publica tela e câmera ao mesmo tempo; os outros veem.
    let screen = owner.publish(PublishSource::Screen { monitor: None }).await;
    let cam = owner.publish(PublishSource::Camera { device: None }).await;
    a.members("dono com tela e câmera", |m| sharing(m, o) && camera(m, o)).await;
    b.members("dono com tela e câmera", |m| sharing(m, o) && camera(m, o)).await;

    // A assiste à tela e B à câmera: o relay conta e pede keyframe de cada um.
    let mut a_frames = a.watch(o, Video::Screen);
    owner.watchers(Video::Screen, 1).await;
    owner.wait("keyframe da tela", |e| matches!(e, RoomEvent::KeyframeRequested { video: Video::Screen })).await;
    let mut b_frames = b.watch(o, Video::Camera);
    owner.watchers(Video::Camera, 1).await;
    owner.wait("keyframe da câmera", |e| matches!(e, RoomEvent::KeyframeRequested { video: Video::Camera })).await;

    // Keyframe grande da tela (muitos pacotes QUIC) e quadros seguintes,
    // intercalados com a câmera.
    screen.send(true, Codec::Hevc, &payload(0, 300_000));
    cam.send(true, Codec::H264, &payload(100, 20_000));
    for seq in 1..30u16 {
        // 50 fps, como um encoder: bem mais rápido que isso o relay começa a
        // pular quadros (de propósito). O intervalo também vem antes do
        // primeiro quadro depois do keyframe: o relay lê cada stream numa
        // task própria, e um P-frame colado no keyframe pode ter o cabeçalho
        // lido antes do dele, quando quem assiste ainda espera o keyframe.
        tokio::time::sleep(Duration::from_millis(20)).await;
        screen.send(false, Codec::Hevc, &payload(seq, 2_000 + usize::from(seq) * 100));
        if seq < 15 {
            cam.send(false, Codec::H264, &payload(100 + seq, 3_000));
        }
    }
    for (i, f) in collect(&mut a_frames, 30).await.iter().enumerate() {
        let seq = i as u16;
        assert_eq!((f.from, f.video, f.seq, f.keyframe, f.codec), (o, Video::Screen, seq, seq == 0, Codec::Hevc));
        let len = if seq == 0 { 300_000 } else { 2_000 + usize::from(seq) * 100 };
        assert!(f.data == payload(seq, len), "quadro {seq} da tela corrompido");
    }
    for (i, f) in collect(&mut b_frames, 15).await.iter().enumerate() {
        let seq = i as u16;
        assert_eq!((f.from, f.video, f.seq, f.keyframe, f.codec), (o, Video::Camera, seq, seq == 0, Codec::H264));
        let len = if seq == 0 { 20_000 } else { 3_000 };
        assert!(f.data == payload(100 + seq, len), "quadro {seq} da câmera corrompido");
    }
    // Cada um recebeu só o que pediu.
    assert!(a_frames.try_recv().is_err() && b_frames.try_recv().is_err());

    // Simulcast: com a rede boa, A continua na camada alta; a baixa (sequência
    // própria) não chega para ele.
    for seq in 0..5u16 {
        screen.send_layer(lerri_core::media::LAYER_LOW, seq == 0, Codec::Hevc, &payload(500 + seq, 1_000));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    screen.send(false, Codec::Hevc, &payload(30, 2_000));
    let f = collect(&mut a_frames, 1).await.remove(0);
    assert_eq!((f.layer, f.seq), (lerri_core::media::LAYER_HIGH, 30), "só a camada alta");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(a_frames.try_recv().is_err(), "quadro da camada baixa chegou para quem está bem");

    // SVC: o descartável passa pelo relay cego, pendurado no quadro de base
    // (mesma sequência), e não avança a sequência.
    screen.send_frame(lerri_core::media::LAYER_HIGH, false, true, Codec::Av1, &payload(31, 900));
    screen.send(false, Codec::Av1, &payload(32, 900));
    let got = collect(&mut a_frames, 2).await;
    let summary: Vec<(u16, bool)> = got.iter().map(|f| (f.seq, f.droppable)).collect();
    assert!(summary.contains(&(30, true)) && summary.contains(&(31, false)), "{summary:?}");

    // A para de assistir: a tela fica sem plateia e A não recebe mais nada.
    a.send(Command::Unwatch { member: o, video: Video::Screen });
    owner.watchers(Video::Screen, 0).await;
    screen.send(false, Codec::Hevc, b"depois");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(a_frames.try_recv().is_err(), "quadro chegou para quem não assiste");

    // O dono desliga a câmera: B para de assistir sozinho.
    owner.send(Command::Unpublish(Video::Camera));
    b.wait("câmera fechada", |e| matches!(e, RoomEvent::Watching(w) if w.is_empty())).await;
    a.members("dono sem câmera", |m| sharing(m, o) && !camera(m, o)).await;
    owner.send(Command::Unpublish(Video::Screen));
    a.members("dono parou a tela", |m| !sharing(m, o)).await;

    for m in [owner, a, b] {
        let _ = m.cmds.send(Command::Close);
        let _ = tokio::time::timeout(WAIT, m.task).await;
    }
    relay.shutdown().await;
    let _ = std::fs::remove_dir_all(relay_dir);
}
