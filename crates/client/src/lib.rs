//! Orquestra rede, mídia e criptografia para a UI (SPEC-01 §3). Na Fase 1:
//! entrar em salas, emitir convites e acompanhar presença.

pub mod rooms;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use lerri_core::invite::{AdmissionSecret, HostAddr, Invite, RouteToken};
use lerri_core::protocol::{ALPN, Auth, Frame, Refusal, Role, RoomMode};
use lerri_core::{MemberId, RoomId, unix_now};
use lerri_crypto::Identity;
use lerri_net::framing::{read_frame, write_frame};
use lerri_net::iroh::Endpoint;
use lerri_net::iroh::endpoint::{Connection, SendStream};
use lerri_net::{EndpointOpts, keys};
use tokio::sync::{mpsc, oneshot};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Online { member: MemberId, first_time: bool },
    Offline { member: MemberId },
    /// A conexão com o host caiu (ou foi substituída por outra).
    Disconnected(String),
}

/// Caminho em uso até o host.
#[derive(Debug, Clone)]
pub struct Route {
    pub direct: bool,
    pub remote: String,
    pub rtt: Duration,
}

pub struct Session {
    endpoint: Endpoint,
    conn: Connection,
    send: tokio::sync::Mutex<SendStream>,
    pending: Arc<Mutex<Option<oneshot::Sender<Frame>>>>,
    events: mpsc::UnboundedReceiver<Event>,
    pub me: MemberId,
    pub room: RoomId,
    pub mode: RoomMode,
    pub role: Role,
    /// Membros online quando entrou (sem contar você).
    pub online: Vec<MemberId>,
}

impl Session {
    /// Conecta ao host da sala e se apresenta. Uma recusa do relay volta como
    /// erro que contém um [`Refusal`] (use `downcast_ref`).
    pub async fn connect(identity: &Identity, host: &HostAddr, auth: Auth) -> anyhow::Result<Self> {
        let endpoint = lerri_net::bind(EndpointOpts {
            secret: keys::ephemeral(),
            port: 0,
            meet: host.meet.clone(),
            accept: false,
            portmap: false,
        })
        .await?;
        let handshake = async {
            let addr = lerri_net::endpoint_addr(host).await?;
            let conn = tokio::time::timeout(CONNECT_TIMEOUT, endpoint.connect(addr, ALPN))
                .await
                .context("tempo esgotado tentando alcançar o host da sala")?
                .context("não foi possível conectar ao host da sala")?;
            let (mut send, mut recv) = conn.open_bi().await?;
            let proof = identity.prove(&host.key, endpoint.id().as_bytes());
            write_frame(&mut send, &Frame::Hello { auth, proof }).await?;
            let reply =
                tokio::time::timeout(REPLY_TIMEOUT, read_frame(&mut recv)).await.context("host não respondeu")??;
            match reply {
                Some(Frame::Welcome { room, mode, role, online }) => Ok((conn, send, recv, room, mode, role, online)),
                Some(Frame::Refused(r)) => Err(anyhow::Error::new(r)),
                other => bail!("resposta inesperada do host: {other:?}"),
            }
        };
        let (conn, send, mut recv, room, mode, role, online) = match handshake.await {
            Ok(parts) => parts,
            Err(e) => {
                endpoint.close().await;
                return Err(e);
            }
        };

        let (tx, events) = mpsc::unbounded_channel();
        let pending: Arc<Mutex<Option<oneshot::Sender<Frame>>>> = Arc::default();
        let p = pending.clone();
        let c = conn.clone();
        tokio::spawn(async move {
            let reason = loop {
                match read_frame(&mut recv).await {
                    Ok(Some(Frame::MemberOnline { member, first_time })) => {
                        let _ = tx.send(Event::Online { member, first_time });
                    }
                    Ok(Some(Frame::MemberOffline { member })) => {
                        let _ = tx.send(Event::Offline { member });
                    }
                    Ok(Some(reply @ (Frame::InviteRegistered { .. } | Frame::Refused(_)))) => {
                        if let Some(waiter) = p.lock().unwrap().take() {
                            let _ = waiter.send(reply);
                        }
                    }
                    Ok(Some(other)) => tracing::warn!("frame inesperado do host: {other:?}"),
                    Ok(None) => break "o host encerrou a sessão".to_string(),
                    Err(e) => break c.close_reason().map_or_else(|| format!("{e:#}"), |r| r.to_string()),
                }
            };
            let _ = tx.send(Event::Disconnected(reason));
        });

        Ok(Self {
            endpoint,
            conn,
            send: tokio::sync::Mutex::new(send),
            pending,
            events,
            me: identity.member_id(),
            room,
            mode,
            role,
            online,
        })
    }

    /// Próximo evento da sala. `None` depois de `Disconnected`.
    pub async fn next_event(&mut self) -> Option<Event> {
        self.events.recv().await
    }

    /// Gera `t` e `s`, registra só o hash de `t` no relay e devolve o convite
    /// apontando para `public_host` (SPEC-02 §3.3). Só o dono pode.
    pub async fn issue_invite(&self, public_host: &HostAddr, ttl_secs: u64) -> anyhow::Result<Invite> {
        let token = RouteToken(lerri_crypto::random());
        let expires_at = unix_now() + ttl_secs;
        let (tx, rx) = oneshot::channel();
        *self.pending.lock().unwrap() = Some(tx);
        write_frame(&mut *self.send.lock().await, &Frame::IssueInvite { token_hash: token.hash(), expires_at }).await?;
        match tokio::time::timeout(REPLY_TIMEOUT, rx).await.context("relay não confirmou o convite")? {
            Ok(Frame::InviteRegistered { token_hash }) if token_hash == token.hash() => {}
            Ok(Frame::Refused(r)) => return Err(anyhow::Error::new(r)),
            other => bail!("resposta inesperada ao registrar convite: {other:?}"),
        }
        Ok(Invite {
            room: self.room,
            host: public_host.clone(),
            token,
            secret: AdmissionSecret(lerri_crypto::random()),
            inviter: self.me,
            expires_at,
        })
    }

    /// Caminho que a conexão está usando agora (direto ou pelo ponto de encontro).
    pub fn route(&self) -> Option<Route> {
        self.conn.paths().iter().find(|p| p.is_selected()).map(|p| Route {
            direct: p.is_ip(),
            remote: format!("{:?}", p.remote_addr()),
            rtt: p.rtt(),
        })
    }

    pub async fn close(self) {
        self.conn.close(0u32.into(), b"tchau");
        self.endpoint.close().await;
    }
}

/// Atalho para o `Refusal` dentro de um erro de [`Session`].
pub fn refusal(err: &anyhow::Error) -> Option<Refusal> {
    err.downcast_ref::<Refusal>().copied()
}
