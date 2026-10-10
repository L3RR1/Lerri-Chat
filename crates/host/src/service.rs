//! Lógica de sala do relay (SPEC-02 §2–3): autentica quem conecta, consome
//! convites, mantém a lista de membros e avisa presença. Igual no modo
//! servidor e no x1; muda só onde o estado fica (disco ou memória).

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Context;
use lerri_core::invite::{AdminKey, MAX_INVITE_TTL_SECS, TokenHash};
use lerri_core::protocol::{Auth, Frame, Refusal, Role, RoomMode};
use lerri_core::{MemberId, RoomId, unix_now};
use lerri_crypto::identity::verify_proof;
use lerri_net::framing::{read_frame, write_frame};
use lerri_net::iroh::Endpoint;
use lerri_net::iroh::endpoint::{Connection, SendStream};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Quanto tempo quem conecta tem para mandar o `Hello`.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Convites pendentes por sala, para um dono descuidado não encher o disco.
const MAX_PENDING_INVITES: usize = 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Room {
    mode: RoomMode,
    owner: MemberId,
    members: BTreeSet<MemberId>,
    invites: HashMap<TokenHash, Invite>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Invite {
    expires_at: u64,
    used: bool,
}

struct Peer {
    conn_id: u64,
    conn: Connection,
    tx: mpsc::UnboundedSender<Frame>,
}

#[derive(Default)]
struct State {
    rooms: HashMap<RoomId, Room>,
    online: HashMap<RoomId, HashMap<MemberId, Peer>>,
}

pub struct RoomService {
    host_key: [u8; 32],
    admin: AdminKey,
    /// Arquivo das salas (modo servidor). `None`: só memória (x1).
    store: Option<PathBuf>,
    state: Mutex<State>,
    next_conn: AtomicU64,
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
    pub async fn serve(self: std::sync::Arc<Self>, endpoint: Endpoint) {
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

    async fn handle(&self, conn: Connection) -> anyhow::Result<()> {
        let (mut send, mut recv) = tokio::time::timeout(HELLO_TIMEOUT, conn.accept_bi())
            .await
            .context("sem stream de controle")??;
        let hello = tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut recv)).await.context("sem Hello")??;
        let Some(Frame::Hello { auth, proof }) = hello else {
            return refuse(&conn, &mut send, Refusal::Protocol).await;
        };
        let Some(member) = verify_proof(&proof, &self.host_key, conn.remote_id().as_bytes()) else {
            return refuse(&conn, &mut send, Refusal::BadProof).await;
        };
        let (room, first_time) = match self.admit(&auth, member) {
            Ok(r) => r,
            Err(refusal) => {
                info!(member = %member.short(), ?refusal, "entrada recusada");
                return refuse(&conn, &mut send, refusal).await;
            }
        };

        let conn_id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let welcome = {
            let mut st = self.state.lock().unwrap();
            let r = &st.rooms[&room];
            let (mode, role) = (r.mode, if r.owner == member { Role::Owner } else { Role::Member });
            let peers = st.online.entry(room).or_default();
            let online = peers.keys().copied().filter(|m| *m != member).collect();
            if let Some(old) = peers.insert(member, Peer { conn_id, conn: conn.clone(), tx: tx.clone() }) {
                old.conn.close(0u32.into(), b"conectado em outro lugar");
            }
            for (m, p) in peers.iter() {
                if *m != member {
                    let _ = p.tx.send(Frame::MemberOnline { member, first_time });
                }
            }
            Frame::Welcome { room, mode, role, online }
        };
        info!(room = %room, member = %member.short(), first_time, "membro online");
        write_frame(&mut send, &welcome).await?;

        let writer = async {
            while let Some(f) = rx.recv().await {
                write_frame(&mut send, &f).await?;
            }
            anyhow::Ok(())
        };
        let reader = async {
            while let Some(frame) = read_frame(&mut recv).await? {
                let reply = match frame {
                    Frame::IssueInvite { token_hash, expires_at } => self.issue_invite(room, member, token_hash, expires_at),
                    other => {
                        warn!(member = %member.short(), "frame inesperado: {other:?}");
                        Frame::Refused(Refusal::Protocol)
                    }
                };
                let _ = tx.send(reply);
            }
            anyhow::Ok(())
        };
        let result = tokio::select! {
            r = writer => r,
            r = reader => r,
        };

        let mut st = self.state.lock().unwrap();
        if let Some(peers) = st.online.get_mut(&room)
            && peers.get(&member).is_some_and(|p| p.conn_id == conn_id)
        {
            peers.remove(&member);
            for p in peers.values() {
                let _ = p.tx.send(Frame::MemberOffline { member });
            }
            info!(room = %room, member = %member.short(), "membro offline");
        }
        result
    }

    fn admit(&self, auth: &Auth, member: MemberId) -> Result<(RoomId, bool), Refusal> {
        let mut st = self.state.lock().unwrap();
        let result = match *auth {
            Auth::CreateRoom { admin, mode } => {
                if !lerri_crypto::ct_eq(&admin.0, &self.admin.0) {
                    return Err(Refusal::BadAdmin);
                }
                let id = RoomId(lerri_crypto::random());
                let room = Room { mode, owner: member, members: BTreeSet::from([member]), invites: HashMap::new() };
                st.rooms.insert(id, room);
                info!(room = %id, ?mode, owner = %member.short(), "sala criada");
                (id, false)
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
                (room, true)
            }
            Auth::Rejoin { room } => {
                let r = st.rooms.get(&room).ok_or(Refusal::NoSuchRoom)?;
                if !r.members.contains(&member) {
                    return Err(Refusal::NotMember);
                }
                return Ok((room, false));
            }
        };
        self.persist(&st);
        Ok(result)
    }

    fn issue_invite(&self, room: RoomId, member: MemberId, token_hash: TokenHash, expires_at: u64) -> Frame {
        let mut st = self.state.lock().unwrap();
        let now = unix_now();
        let Some(r) = st.rooms.get_mut(&room) else { return Frame::Refused(Refusal::NoSuchRoom) };
        if r.owner != member {
            return Frame::Refused(Refusal::NotOwner);
        }
        r.invites.retain(|_, i| i.expires_at > now);
        if r.invites.len() >= MAX_PENDING_INVITES {
            return Frame::Refused(Refusal::Protocol);
        }
        let expires_at = expires_at.min(now + MAX_INVITE_TTL_SECS);
        r.invites.entry(token_hash).or_insert(Invite { expires_at, used: false });
        self.persist(&st);
        Frame::InviteRegistered { token_hash }
    }

    fn persist(&self, st: &State) {
        if let Some(path) = &self.store
            && let Err(e) = save_rooms(path, &st.rooms)
        {
            warn!("não foi possível salvar as salas: {e:#}");
        }
    }
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
