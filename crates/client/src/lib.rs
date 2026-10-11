//! Orquestra rede, mídia e criptografia para a UI (SPEC-01 §3): a conexão com
//! o host da sala ([`Session`]) e a sala em si, com grupo MLS e chat
//! ([`room`]).

pub mod room;
pub mod rooms;
pub mod screen;
pub mod settings;
#[cfg(windows)]
pub mod video;
pub mod voice;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use lerri_core::invite::{AdmissionSecret, HostAddr, Invite, RouteToken};
use lerri_core::media::Video;
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
    /// Entrou (`on`) ou saiu do canal de voz.
    Voice { member: MemberId, on: bool },
    /// Começou (`on`) ou parou de publicar a tela ou a câmera.
    Publish { member: MemberId, video: Video, on: bool },
    /// A conexão com o host caiu (ou foi substituída por outra).
    Disconnected(String),
    /// Frames do MLS e de membros (tratados por [`room::RoomCore`]).
    Relay(Frame),
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
    latency: Arc<Mutex<Vec<(MemberId, u32)>>>,
    pub me: MemberId,
    pub room: RoomId,
    pub mode: RoomMode,
    pub role: Role,
    /// Membros online quando entrou (sem contar você).
    pub online: Vec<MemberId>,
    pub owner: MemberId,
    /// Último epoch MLS que o relay ordenou.
    pub relay_epoch: u64,
    /// Quem estava no canal de voz quando entrou.
    pub voice: Vec<MemberId>,
    /// Quem estava publicando tela ou câmera quando entrou.
    pub publishing: Vec<(MemberId, Video)>,
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
                Some(Frame::Welcome { room, mode, role, online, owner, epoch, voice, publishing }) => {
                    Ok((conn, send, recv, room, mode, role, online, owner, epoch, voice, publishing))
                }
                Some(Frame::Refused(r)) => Err(anyhow::Error::new(r)),
                other => bail!("resposta inesperada do host: {other:?}"),
            }
        };
        let (conn, send, mut recv, room, mode, role, online, owner, relay_epoch, voice, publishing) = match handshake.await {
            Ok(parts) => parts,
            Err(e) => {
                endpoint.close().await;
                return Err(e);
            }
        };

        let (tx, events) = mpsc::unbounded_channel();
        let pending: Arc<Mutex<Option<oneshot::Sender<Frame>>>> = Arc::default();
        let latency: Arc<Mutex<Vec<(MemberId, u32)>>> = Arc::default();
        let (p, l) = (pending.clone(), latency.clone());
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
                    Ok(Some(Frame::MemberVoice { member, on })) => {
                        let _ = tx.send(Event::Voice { member, on });
                    }
                    Ok(Some(Frame::MemberPublish { member, video, on })) => {
                        let _ = tx.send(Event::Publish { member, video, on });
                    }
                    Ok(Some(reply @ (Frame::InviteRegistered { .. } | Frame::Refused(_)))) => {
                        // Recusa sem ninguém esperando é de um commit: vai para a sala.
                        let waiter = p.lock().unwrap().take();
                        match waiter {
                            Some(w) => {
                                let _ = w.send(reply);
                            }
                            None => {
                                let _ = tx.send(Event::Relay(reply));
                            }
                        }
                    }
                    // Chega a cada 3 s: vira estado consultável, não evento.
                    Ok(Some(Frame::Latency { rtts })) => *l.lock().unwrap() = rtts,
                    Ok(Some(other)) => {
                        let _ = tx.send(Event::Relay(other));
                    }
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
            latency,
            me: identity.member_id(),
            room,
            mode,
            role,
            online,
            owner,
            relay_epoch,
            voice,
            publishing,
        })
    }

    /// Última ida e volta de cada membro até o host, em ms (o relay manda a
    /// cada 3 s quando há duas pessoas ou mais).
    pub fn latency(&self) -> Vec<(MemberId, u32)> {
        self.latency.lock().unwrap().clone()
    }

    /// Manda um frame ao relay.
    pub async fn send(&self, frame: &Frame) -> anyhow::Result<()> {
        write_frame(&mut *self.send.lock().await, frame).await
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

    /// A conexão QUIC com o host (datagramas de mídia).
    pub fn connection(&self) -> Connection {
        self.conn.clone()
    }

    /// Caminho que a conexão está usando agora (direto ou pelo ponto de encontro).
    pub fn route(&self) -> Option<Route> {
        self.conn.paths().iter().find(|p| p.is_selected()).map(|p| Route {
            direct: p.is_ip(),
            remote: format!("{:?}", p.remote_addr()),
            rtt: p.rtt(),
        })
    }

    /// Fecha com calma: termina o stream e espera o relay encerrar, para o
    /// que foi mandado por último (ex.: `Leave`) não se perder.
    pub async fn close(self) {
        let _ = self.send.lock().await.finish();
        let _ = tokio::time::timeout(Duration::from_secs(2), self.conn.closed()).await;
        self.conn.close(0u32.into(), b"tchau");
        self.endpoint.close().await;
    }
}

/// Atalho para o `Refusal` dentro de um erro de [`Session`].
pub fn refusal(err: &anyhow::Error) -> Option<Refusal> {
    err.downcast_ref::<Refusal>().copied()
}

/// Endereços do convite de uma sala x1 explicados para o usuário, com o aviso
/// de alcance da SPEC-03 §4.2.
pub struct ReachReport {
    /// Uma linha por endereço: "endereço  tipo".
    pub lines: Vec<String>,
    pub note: Option<String>,
    /// O aviso é sério (ninguém de fora deve conseguir entrar).
    pub warning: bool,
}

pub fn reach_report(cands: &[lerri_net::reach::Candidate], assisted: bool) -> ReachReport {
    use lerri_net::reach::{Kind, reachable_from_internet};
    let lines = cands
        .iter()
        .map(|c| {
            let kind = match &c.kind {
                Kind::Internet if c.addr.is_ipv6() => "internet (IPv6)".to_string(),
                Kind::Internet => "internet (IPv4)".to_string(),
                Kind::Lan => "rede local".to_string(),
                Kind::Vpn(name) => format!("VPN ({name})"),
                Kind::Cgnat => "CGNAT".to_string(),
            };
            format!("{:<44} {kind}", c.addr.to_string())
        })
        .collect();
    let (note, warning) = if assisted {
        (
            Some("Com ponto de encontro: quem não alcançar um desses entra pelo relay, e o caminho direto é tentado depois.".into()),
            false,
        )
    } else if !reachable_from_internet(cands) {
        (
            Some(
                "Seu PC não parece alcançável pela internet (sem IPv6 público e sem porta mapeada no roteador; talvez \
                 CGNAT). Só entra quem estiver na mesma rede local ou VPN. Para os outros, use o link público de um \
                 relay seu como ponto de encontro, ou abra uma porta UDP no roteador."
                    .into(),
            ),
            true,
        )
    } else if !cands.iter().any(|c| c.kind == Kind::Internet && c.addr.is_ipv4()) {
        (
            Some("Só há IPv6 público: quem não tem IPv6 não alcança este PC. O roteador também pode bloquear entrada em IPv6.".into()),
            false,
        )
    } else {
        (None, false)
    };
    ReachReport { lines, note, warning }
}
