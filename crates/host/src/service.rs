//! Lógica de sala do relay (SPEC-02 §2–4): autentica quem conecta, consome
//! convites, avisa presença e é o Delivery Service do MLS: ordena commits
//! (um por epoch), guarda o log para quem volta, entrega Welcomes e repassa
//! mensagens cifradas. Ele nunca tem a chave do grupo.
//!
//! Igual no modo servidor e no x1; muda só onde o estado fica (disco ou
//! memória).

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use bytes::Bytes;
use lerri_core::invite::{AdminKey, MAX_INVITE_TTL_SECS, TokenHash};
use lerri_core::media::{self, Video};
use lerri_core::protocol::{Auth, Frame, Refusal, Role, RoomMode};
use lerri_core::{MemberId, RoomId, unix_now};
use lerri_crypto::identity::verify_proof;
use lerri_net::framing::{read_frame, write_frame};
use lerri_net::iroh::Endpoint;
use lerri_net::iroh::endpoint::{Connection, RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Quanto tempo quem conecta tem para mandar o `Hello`.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Convites pendentes por sala, para um dono descuidado não encher o disco.
const MAX_PENDING_INVITES: usize = 1000;
/// Commits guardados por sala. Quem ficar mais atrás que isso pede para ser
/// readicionado.
const MAX_COMMIT_LOG: usize = 5000;
/// Pedidos de entrada pendentes por sala.
const MAX_PENDING_JOINS: usize = 100;
/// Quadros de vídeo em trânsito para um membro antes de o relay começar a
/// pular quadros para ele (~130 ms a 60 fps).
const MAX_IN_FLIGHT: usize = 8;
/// Para a câmera o limite é menor: sob congestionamento ela perde quadros
/// antes da tela (voz > tela > câmera, SPEC-03 §6.1).
const MAX_IN_FLIGHT_CAMERA: usize = 4;
/// Intervalo mínimo entre pedidos de keyframe repassados a quem compartilha.
const KEYFRAME_EVERY: Duration = Duration::from_millis(500);
/// Quadro que não chega neste tempo é abandonado.
const VIDEO_DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);
/// De quanto em quanto tempo o relay conta a latência de cada um à sala.
const LATENCY_EVERY: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Room {
    mode: RoomMode,
    owner: MemberId,
    /// Quem pode conectar (consumiu um `t` ou criou a sala). Pode ainda não
    /// estar no grupo MLS.
    members: BTreeSet<MemberId>,
    invites: HashMap<TokenHash, Invite>,
    /// Último epoch MLS ordenado.
    epoch: u64,
    commits: Vec<StoredCommit>,
    pending_joins: HashMap<MemberId, JoinRequest>,
    pending_welcomes: HashMap<MemberId, Vec<u8>>,
    /// Saíram, mas ninguém fez o commit de remoção ainda.
    pending_removals: BTreeSet<MemberId>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Invite {
    expires_at: u64,
    used: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredCommit {
    epoch: u64,
    from: MemberId,
    data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JoinRequest {
    token_hash: Option<TokenHash>,
    key_package: Vec<u8>,
    mac: [u8; 32],
}

impl JoinRequest {
    fn frame(&self, member: MemberId) -> Frame {
        Frame::JoinRequested {
            member,
            token_hash: self.token_hash,
            key_package: self.key_package.clone(),
            mac: self.mac,
        }
    }
}

struct Peer {
    conn_id: u64,
    conn: Connection,
    tx: mpsc::UnboundedSender<Frame>,
    /// Está no canal de voz: recebe e pode mandar datagramas de voz.
    voice: bool,
    /// Está compartilhando a tela.
    publishing: BTreeSet<Video>,
    /// De quem recebe a tela.
    watching: BTreeSet<(MemberId, Video)>,
    /// Telas que este membro só volta a receber a partir de um keyframe
    /// (começou a assistir ou o relay pulou quadros por congestionamento).
    resume: HashMap<(MemberId, Video), Resume>,
    /// Quadros de vídeo em trânsito para este membro (ainda sem confirmação).
    in_flight: Arc<AtomicUsize>,
    /// Último pedido de keyframe repassado a este membro (como transmissor).
    last_key_request: HashMap<Video, Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resume {
    /// Nada até um keyframe começar a chegar.
    WaitingKey,
    /// O keyframe de número `0` está chegando: repassa ele e os seguintes.
    From(u16),
}

/// `a` vem antes de `b` (números de sequência de 16 bits que dão a volta).
fn seq_before(a: u16, b: u16) -> bool {
    (a.wrapping_sub(b) as i16) < 0
}

impl Peer {
    fn new(conn_id: u64, conn: Connection, tx: mpsc::UnboundedSender<Frame>) -> Self {
        Self {
            conn_id,
            conn,
            tx,
            voice: false,
            publishing: BTreeSet::new(),
            watching: BTreeSet::new(),
            resume: HashMap::new(),
            in_flight: Arc::default(),
            last_key_request: HashMap::new(),
        }
    }
}

#[derive(Default)]
struct State {
    rooms: HashMap<RoomId, Room>,
    online: HashMap<RoomId, HashMap<MemberId, Peer>>,
}

impl State {
    fn broadcast(&self, room: RoomId, except: MemberId, frame: &Frame) {
        if let Some(peers) = self.online.get(&room) {
            for (m, p) in peers {
                if *m != except {
                    let _ = p.tx.send(frame.clone());
                }
            }
        }
    }

    fn send_to(&self, room: RoomId, member: MemberId, frame: Frame) -> bool {
        self.online.get(&room).and_then(|p| p.get(&member)).is_some_and(|p| p.tx.send(frame).is_ok())
    }

    /// Conta para quem compartilha quantos assistem à tela dele.
    fn send_watchers(&self, room: RoomId, sharer: MemberId, video: Video) {
        let Some(peers) = self.online.get(&room) else { return };
        let count = peers.iter().filter(|(m, p)| **m != sharer && p.watching.contains(&(sharer, video))).count();
        self.send_to(room, sharer, Frame::Watchers { video, count: count as u32 });
    }

    /// Repassa um pedido de keyframe a quem compartilha, no máximo um a cada
    /// [`KEYFRAME_EVERY`] (`force`: alguém começou a assistir agora).
    fn request_keyframe(&mut self, room: RoomId, sharer: MemberId, video: Video, force: bool) {
        let Some(p) = self.online.get_mut(&room).and_then(|p| p.get_mut(&sharer)) else { return };
        if !p.publishing.contains(&video) {
            return;
        }
        if !force && p.last_key_request.get(&video).is_some_and(|t| t.elapsed() < KEYFRAME_EVERY) {
            return;
        }
        p.last_key_request.insert(video, Instant::now());
        let _ = p.tx.send(Frame::KeyframeRequested { video });
    }
}

pub struct RoomService {
    host_key: [u8; 32],
    admin: AdminKey,
    /// Arquivo das salas (modo servidor). `None`: só memória (x1).
    store: Option<PathBuf>,
    state: Mutex<State>,
    next_conn: AtomicU64,
}

enum Admitted {
    Created,
    Joined,
    Rejoined { epoch: u64 },
}

impl RoomService {
    pub fn new(host_key: [u8; 32], admin: AdminKey, store: Option<PathBuf>) -> anyhow::Result<Self> {
        let rooms = match &store {
            Some(path) => load_rooms(path)?,
            None => HashMap::new(),
        };
        Ok(Self {
            host_key,
            admin,
            store,
            state: Mutex::new(State { rooms, online: HashMap::new() }),
            next_conn: AtomicU64::new(0),
        })
    }

    pub fn room_count(&self) -> usize {
        self.state.lock().unwrap().rooms.len()
    }

    /// Aceita conexões até o endpoint fechar.
    pub async fn serve(self: Arc<Self>, endpoint: Endpoint) {
        let svc = self.clone();
        let latency = tokio::spawn(async move {
            let mut every = tokio::time::interval(LATENCY_EVERY);
            loop {
                every.tick().await;
                svc.broadcast_latency();
            }
        });
        self.accept_loop(endpoint).await;
        latency.abort();
    }

    async fn accept_loop(self: &Arc<Self>, endpoint: Endpoint) {
        while let Some(incoming) = endpoint.accept().await {
            let svc = self.clone();
            tokio::spawn(async move {
                let conn = match incoming.await {
                    Ok(c) => c,
                    Err(e) => return debug!("handshake falhou: {e}"),
                };
                let remote = conn.remote_id();
                if let Err(e) = svc.handle(conn).await {
                    debug!(remote = %remote.fmt_short(), "conexão encerrada: {e:#}");
                }
            });
        }
    }

    async fn handle(self: &Arc<Self>, conn: Connection) -> anyhow::Result<()> {
        let (mut send, mut recv) = tokio::time::timeout(HELLO_TIMEOUT, conn.accept_bi())
            .await
            .context("sem stream de controle")??;
        let hello = tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut recv)).await.context("sem Hello")??;
        let Some(Frame::Hello { auth, proof }) = hello else {
            return refuse(&conn, &mut send, Refusal::Protocol).await;
        };
        let Some(me) = verify_proof(&proof, &self.host_key, conn.remote_id().as_bytes()) else {
            return refuse(&conn, &mut send, Refusal::BadProof).await;
        };
        let (room, how) = match self.admit(&auth, me) {
            Ok(r) => r,
            Err(refusal) => {
                info!(member = %me.short(), ?refusal, "entrada recusada");
                return refuse(&conn, &mut send, refusal).await;
            }
        };
        let first_time = matches!(how, Admitted::Joined);

        let conn_id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        let (tx, mut rx) = mpsc::unbounded_channel();
        {
            let mut st = self.state.lock().unwrap();
            let r = &st.rooms[&room];
            let welcome = Frame::Welcome {
                room,
                mode: r.mode,
                role: if r.owner == me { Role::Owner } else { Role::Member },
                online: st.online.get(&room).map_or_else(Vec::new, |p| p.keys().copied().filter(|m| *m != me).collect()),
                owner: r.owner,
                epoch: r.epoch,
                voice: st.online.get(&room).map_or_else(Vec::new, |p| p.iter().filter(|(_, p)| p.voice).map(|(m, _)| *m).collect()),
                publishing: st
                    .online
                    .get(&room)
                    .map_or_else(Vec::new, |p| p.iter().flat_map(|(m, p)| p.publishing.iter().map(|v| (*m, *v))).collect()),
            };
            // Tudo o que este membro perdeu, na ordem: commits do log, o
            // Welcome dele, pedidos de entrada e saídas sem commit.
            let mut backlog = vec![welcome];
            if let Admitted::Rejoined { epoch } = how {
                backlog.extend(
                    r.commits
                        .iter()
                        .filter(|c| c.epoch >= epoch)
                        .map(|c| Frame::CommitFrom { from: c.from, epoch: c.epoch, data: c.data.clone() }),
                );
            }
            if let Some(w) = r.pending_welcomes.get(&me) {
                backlog.push(Frame::GroupWelcome { data: w.clone() });
            }
            backlog.extend(r.pending_joins.iter().filter(|(m, _)| **m != me).map(|(m, j)| j.frame(*m)));
            backlog.extend(r.pending_removals.iter().map(|m| Frame::MemberRemoved { member: *m, left: true }));
            for f in backlog {
                let _ = tx.send(f);
            }

            let peers = st.online.entry(room).or_default();
            if let Some(old) = peers.insert(me, Peer::new(conn_id, conn.clone(), tx.clone())) {
                old.conn.close(0u32.into(), b"conectado em outro lugar");
            }
            st.broadcast(room, me, &Frame::MemberOnline { member: me, first_time });
        }
        info!(room = %room, member = %me.short(), first_time, "membro online");

        let writer = async {
            while let Some(f) = rx.recv().await {
                let last = matches!(f, Frame::Removed);
                write_frame(&mut send, &f).await?;
                if last {
                    let _ = send.finish();
                    // Dá tempo do aviso chegar antes de fechar.
                    let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
                    break;
                }
            }
            anyhow::Ok(())
        };
        let reader = async {
            while let Some(frame) = read_frame(&mut recv).await? {
                if matches!(frame, Frame::Leave) {
                    self.leave(room, me);
                    break;
                }
                self.on_frame(room, me, frame);
            }
            anyhow::Ok(())
        };
        let datagrams = async {
            loop {
                let d = conn.read_datagram().await?;
                self.forward(room, me, d);
            }
        };
        let video = async {
            loop {
                let recv = conn.accept_uni().await?;
                let svc = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = svc.forward_video(room, me, recv).await {
                        debug!(member = %me.short(), "quadro de vídeo descartado: {e:#}");
                    }
                });
            }
        };
        let result = tokio::select! {
            r = writer => r,
            r = reader => r,
            r = datagrams => r,
            r = video => r,
        };
        conn.close(0u32.into(), b"fim");

        let mut st = self.state.lock().unwrap();
        if let Some(peers) = st.online.get_mut(&room)
            && peers.get(&me).is_some_and(|p| p.conn_id == conn_id)
        {
            let gone = peers.remove(&me);
            st.broadcast(room, me, &Frame::MemberOffline { member: me });
            // Quem compartilha a tela que ele assistia fica sabendo.
            for (sharer, video) in gone.map(|p| p.watching).unwrap_or_default() {
                st.send_watchers(room, sharer, video);
            }
            info!(room = %room, member = %me.short(), "membro offline");
        }
        result
    }

    /// Conta a cada sala com duas pessoas ou mais a ida e volta de cada uma
    /// até aqui.
    fn broadcast_latency(&self) {
        let st = self.state.lock().unwrap();
        for peers in st.online.values().filter(|p| p.len() > 1) {
            let rtts: Vec<(MemberId, u32)> = peers
                .iter()
                .filter_map(|(m, p)| {
                    let paths = p.conn.paths();
                    let path = paths.iter().find(|path| path.is_selected())?;
                    Some((*m, u32::try_from(path.rtt().as_millis()).unwrap_or(u32::MAX)))
                })
                .collect();
            let frame = Frame::Latency { rtts };
            for p in peers.values() {
                let _ = p.tx.send(frame.clone());
            }
        }
    }

    fn admit(&self, auth: &Auth, member: MemberId) -> Result<(RoomId, Admitted), Refusal> {
        let mut st = self.state.lock().unwrap();
        let result = match *auth {
            Auth::CreateRoom { admin, mode } => {
                if !lerri_crypto::ct_eq(&admin.0, &self.admin.0) {
                    return Err(Refusal::BadAdmin);
                }
                let id = RoomId(lerri_crypto::random());
                let room = Room {
                    mode,
                    owner: member,
                    members: BTreeSet::from([member]),
                    invites: HashMap::new(),
                    epoch: 0,
                    commits: Vec::new(),
                    pending_joins: HashMap::new(),
                    pending_welcomes: HashMap::new(),
                    pending_removals: BTreeSet::new(),
                };
                st.rooms.insert(id, room);
                info!(room = %id, ?mode, owner = %member.short(), "sala criada");
                (id, Admitted::Created)
            }
            Auth::Join { room, token } => {
                let r = st.rooms.get_mut(&room).ok_or(Refusal::NoSuchRoom)?;
                let inv = r.invites.get_mut(&token.hash()).ok_or(Refusal::InviteUnknown)?;
                if inv.used {
                    return Err(Refusal::InviteUsed);
                }
                if inv.expires_at <= unix_now() {
                    return Err(Refusal::InviteExpired);
                }
                inv.used = true;
                r.members.insert(member);
                (room, Admitted::Joined)
            }
            Auth::Rejoin { room, epoch } => {
                let r = st.rooms.get(&room).ok_or(Refusal::NoSuchRoom)?;
                if !r.members.contains(&member) {
                    return Err(Refusal::NotMember);
                }
                return Ok((room, Admitted::Rejoined { epoch }));
            }
        };
        self.persist(&st);
        Ok(result)
    }

    fn on_frame(&self, room: RoomId, me: MemberId, frame: Frame) {
        let mut st = self.state.lock().unwrap();
        let reply = match frame {
            Frame::IssueInvite { token_hash, expires_at } => Some(issue_invite(&mut st, room, me, token_hash, expires_at)),
            Frame::JoinRequest { token_hash, key_package, mac } => {
                let Some(r) = st.rooms.get_mut(&room) else { return };
                if r.pending_joins.len() >= MAX_PENDING_JOINS && !r.pending_joins.contains_key(&me) {
                    Some(Frame::Refused(Refusal::Protocol))
                } else {
                    let req = JoinRequest { token_hash, key_package, mac };
                    let frame = req.frame(me);
                    r.pending_joins.insert(me, req);
                    st.broadcast(room, me, &frame);
                    None
                }
            }
            Frame::Commit { epoch, data, welcome, adds, removes } => {
                Some(commit(&mut st, room, me, epoch, data, welcome, adds, removes))
            }
            Frame::Voice { on } => {
                if let Some(p) = st.online.get_mut(&room).and_then(|p| p.get_mut(&me))
                    && p.voice != on
                {
                    p.voice = on;
                    st.broadcast(room, me, &Frame::MemberVoice { member: me, on });
                    info!(room = %room, member = %me.short(), on, "canal de voz");
                }
                return;
            }
            Frame::Publish { video, on } => {
                if let Some(p) = st.online.get_mut(&room).and_then(|p| p.get_mut(&me))
                    && p.publishing.contains(&video) != on
                {
                    if on {
                        p.publishing.insert(video);
                    } else {
                        p.publishing.remove(&video);
                    }
                    st.broadcast(room, me, &Frame::MemberPublish { member: me, video, on });
                    info!(room = %room, member = %me.short(), on, "publicando {}", video.name());
                    if on {
                        st.send_watchers(room, me, video);
                    }
                }
                return;
            }
            Frame::Watch { member, video, on } => {
                if member == me {
                    return;
                }
                if let Some(p) = st.online.get_mut(&room).and_then(|p| p.get_mut(&me)) {
                    if on {
                        p.watching.insert((member, video));
                        // Só faz sentido começar de um quadro completo.
                        p.resume.insert((member, video), Resume::WaitingKey);
                    } else {
                        p.watching.remove(&(member, video));
                        p.resume.remove(&(member, video));
                    }
                }
                st.send_watchers(room, member, video);
                if on {
                    st.request_keyframe(room, member, video, true);
                }
                return;
            }
            Frame::KeyframeRequest { member, video } => {
                st.request_keyframe(room, member, video, false);
                return;
            }
            Frame::App { data } => {
                if let Some(r) = st.rooms.get_mut(&room) {
                    // Quem manda mensagem já está no grupo.
                    r.pending_welcomes.remove(&me);
                }
                st.broadcast(room, me, &Frame::AppFrom { from: me, data });
                None
            }
            other => {
                warn!(member = %me.short(), "frame inesperado: {other:?}");
                Some(Frame::Refused(Refusal::Protocol))
            }
        };
        if let Some(reply) = reply {
            st.send_to(room, me, reply);
        }
        self.persist(&st);
    }

    fn leave(&self, room: RoomId, me: MemberId) {
        let mut st = self.state.lock().unwrap();
        let Some(r) = st.rooms.get_mut(&room) else { return };
        if r.owner == me {
            // Transferir a posse entra numa fase futura; o dono não sai.
            st.send_to(room, me, Frame::Refused(Refusal::Protocol));
            return;
        }
        r.members.remove(&me);
        r.pending_joins.remove(&me);
        r.pending_welcomes.remove(&me);
        r.pending_removals.insert(me);
        st.broadcast(room, me, &Frame::MemberRemoved { member: me, left: true });
        info!(room = %room, member = %me.short(), "membro saiu da sala");
        self.persist(&st);
    }

    /// Repassa um datagrama de mídia sem decifrar (SFU cego, SPEC-02 §4.3).
    /// Lê só o cabeçalho de roteamento; voz vai de quem está no canal de voz
    /// para os outros que estão nele.
    fn forward(&self, room: RoomId, me: MemberId, datagram: Bytes) {
        if datagram.len() > media::MAX_DATAGRAM {
            return;
        }
        let Some((header, _)) = media::Header::parse(&datagram) else { return };
        if header.kind != media::Kind::Voice {
            // Tela e câmera chegam na Fase 4.
            return;
        }
        let st = self.state.lock().unwrap();
        let Some(peers) = st.online.get(&room) else { return };
        if !peers.get(&me).is_some_and(|p| p.voice) {
            return;
        }
        for (m, p) in peers {
            if *m != me && p.voice {
                // Fila cheia descarta o mais velho: melhor perder que atrasar.
                let _ = p.conn.send_datagram(datagram.clone());
            }
        }
    }

    /// Repassa um quadro de vídeo (tela ou câmera; um stream unidirecional)
    /// a quem assiste, sem decifrar (SPEC-02 §4.3, SPEC-03 §6.3). Para quem
    /// está atrasado, pula quadros até o próximo keyframe em vez de acumular
    /// atraso; a câmera desiste antes da tela.
    ///
    /// Streams terminam fora de ordem (um keyframe grande chega depois dos
    /// P-frames pequenos que vêm atrás dele). Por isso o cabeçalho é lido
    /// logo no início: quando um keyframe *começa* a chegar, quem esperava
    /// por ele já pode receber os quadros seguintes.
    async fn forward_video(self: Arc<Self>, room: RoomId, me: MemberId, mut recv: RecvStream) -> anyhow::Result<()> {
        let mut head = [0u8; media::HEADER_LEN];
        recv.read_exact(&mut head).await?;
        let (header, _) = media::Header::parse(&head).context("cabeçalho inválido")?;
        let video = Video::from_kind(header.kind).context("stream que não é de vídeo")?;
        let key = (me, video);
        {
            let mut st = self.state.lock().unwrap();
            let Some(peers) = st.online.get_mut(&room) else { return Ok(()) };
            // Quem não está publicando nem chega a ter o corpo lido.
            anyhow::ensure!(peers.get(&me).is_some_and(|p| p.publishing.contains(&video)), "quadro de vídeo não publicado");
            for p in peers.values_mut().filter(|_| header.keyframe) {
                if let Some(r @ Resume::WaitingKey) = p.resume.get_mut(&key) {
                    *r = Resume::From(header.seq);
                }
            }
        }
        let rest = recv.read_to_end(media::MAX_VIDEO_FRAME - media::HEADER_LEN).await?;
        let data = Bytes::from([head.as_slice(), &rest].concat());
        let limit = match video {
            Video::Screen => MAX_IN_FLIGHT,
            Video::Camera => MAX_IN_FLIGHT_CAMERA,
        };
        let mut targets = Vec::new();
        {
            let mut st = self.state.lock().unwrap();
            let Some(peers) = st.online.get_mut(&room) else { return Ok(()) };
            if !peers.get(&me).is_some_and(|p| p.publishing.contains(&video)) {
                return Ok(());
            }
            let mut congested = false;
            for (m, p) in peers.iter_mut() {
                if *m == me || !p.watching.contains(&key) {
                    continue;
                }
                let in_flight = p.in_flight.load(Ordering::Relaxed);
                let send = match (header.keyframe, p.resume.get(&key)) {
                    // Keyframe recomeça a imagem; só desiste se o atraso já é enorme.
                    (true, _) => in_flight < 2 * limit,
                    (false, Some(Resume::WaitingKey)) => false,
                    (false, Some(Resume::From(k))) => !seq_before(header.seq, *k) && in_flight < limit,
                    (false, None) => in_flight < limit,
                };
                if send {
                    if header.keyframe {
                        p.resume.remove(&key);
                    }
                    p.in_flight.fetch_add(1, Ordering::Relaxed);
                    targets.push((p.conn.clone(), p.in_flight.clone()));
                } else if !matches!(p.resume.get(&key), Some(Resume::From(k)) if seq_before(header.seq, *k)) {
                    // Atrasado (não só um quadro anterior ao keyframe que já vem):
                    // espera o próximo keyframe.
                    debug!(to = %m.short(), video = video.name(), seq = header.seq, in_flight, "quadro de vídeo pulado");
                    p.resume.insert(key, Resume::WaitingKey);
                    congested = true;
                }
            }
            if congested {
                st.request_keyframe(room, me, video, false);
            }
        }
        for (conn, in_flight) in targets {
            let data = data.clone();
            tokio::spawn(async move {
                let sent = tokio::time::timeout(VIDEO_DELIVERY_TIMEOUT, async {
                    let mut s = conn.open_uni().await?;
                    s.write_chunk(data).await?;
                    s.finish()?;
                    // Conta como em trânsito até o outro lado confirmar tudo.
                    let _ = s.stopped().await;
                    anyhow::Ok(())
                })
                .await;
                if !matches!(sent, Ok(Ok(()))) {
                    debug!("quadro de vídeo não entregue");
                }
                in_flight.fetch_sub(1, Ordering::Relaxed);
            });
        }
        Ok(())
    }

    fn persist(&self, st: &State) {
        if let Some(path) = &self.store
            && let Err(e) = save_rooms(path, &st.rooms)
        {
            warn!("não foi possível salvar as salas: {e:#}");
        }
    }
}

fn issue_invite(st: &mut State, room: RoomId, me: MemberId, token_hash: TokenHash, expires_at: u64) -> Frame {
    let now = unix_now();
    let Some(r) = st.rooms.get_mut(&room) else { return Frame::Refused(Refusal::NoSuchRoom) };
    if r.owner != me {
        return Frame::Refused(Refusal::NotOwner);
    }
    r.invites.retain(|_, i| i.expires_at > now);
    if r.invites.len() >= MAX_PENDING_INVITES {
        return Frame::Refused(Refusal::Protocol);
    }
    let expires_at = expires_at.min(now + MAX_INVITE_TTL_SECS);
    r.invites.entry(token_hash).or_insert(Invite { expires_at, used: false });
    Frame::InviteRegistered { token_hash }
}

/// Ordena um commit (SPEC-02 §4.2): só o primeiro de cada epoch passa.
#[allow(clippy::too_many_arguments)]
fn commit(
    st: &mut State,
    room: RoomId,
    me: MemberId,
    epoch: u64,
    data: Vec<u8>,
    welcome: Option<Vec<u8>>,
    adds: Vec<MemberId>,
    removes: Vec<MemberId>,
) -> Frame {
    let Some(r) = st.rooms.get_mut(&room) else { return Frame::Refused(Refusal::NoSuchRoom) };
    if epoch != r.epoch {
        return Frame::CommitRejected { current_epoch: r.epoch };
    }
    // Tirar alguém que não saiu por conta própria é remoção: só o dono.
    let kicks: Vec<MemberId> =
        removes.iter().copied().filter(|m| r.members.contains(m) && !r.pending_removals.contains(m)).collect();
    if !kicks.is_empty() && me != r.owner {
        return Frame::Refused(Refusal::NotOwner);
    }
    if removes.contains(&r.owner) {
        return Frame::Refused(Refusal::NotOwner);
    }

    r.epoch += 1;
    r.commits.push(StoredCommit { epoch, from: me, data: data.clone() });
    if r.commits.len() > MAX_COMMIT_LOG {
        let excess = r.commits.len() - MAX_COMMIT_LOG;
        r.commits.drain(..excess);
    }
    r.pending_welcomes.remove(&me);
    for a in &adds {
        r.pending_joins.remove(a);
        if let Some(w) = &welcome {
            r.pending_welcomes.insert(*a, w.clone());
        }
    }
    for m in &removes {
        r.pending_removals.remove(m);
        r.pending_welcomes.remove(m);
        r.members.remove(m);
    }
    let new_epoch = r.epoch;

    st.broadcast(room, me, &Frame::CommitFrom { from: me, epoch, data });
    if let Some(w) = &welcome {
        for a in &adds {
            st.send_to(room, *a, Frame::GroupWelcome { data: w.clone() });
        }
    }
    for m in &kicks {
        st.send_to(room, *m, Frame::Removed);
        st.broadcast(room, *m, &Frame::MemberRemoved { member: *m, left: false });
        info!(room = %room, member = %m.short(), "membro removido pelo dono");
    }
    Frame::CommitAccepted { epoch: new_epoch }
}

async fn refuse(conn: &Connection, send: &mut SendStream, refusal: Refusal) -> anyhow::Result<()> {
    let _ = write_frame(send, &Frame::Refused(refusal)).await;
    let _ = send.finish();
    // Dá tempo do frame chegar antes de fechar.
    let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
    conn.close(1u32.into(), b"recusado");
    Ok(())
}

fn load_rooms(path: &Path) -> anyhow::Result<HashMap<RoomId, Room>> {
    match std::fs::read(path) {
        Ok(bytes) => postcard::from_bytes(&bytes).with_context(|| format!("{} corrompido", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(e) => Err(e).with_context(|| format!("lendo {}", path.display())),
    }
}

fn save_rooms(path: &Path, rooms: &HashMap<RoomId, Room>) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, postcard::to_allocvec(rooms)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
