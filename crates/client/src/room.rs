//! A sala do ponto de vista de um membro (SPEC-02 §3–4, Fase 2): grupo MLS,
//! admissão de quem chega com convite, chat cifrado, remoção e saída.
//!
//! [`RoomCore`] é síncrono: recebe eventos do relay e comandos da interface e
//! acumula frames para mandar. [`run`] liga isso a uma [`Session`].
//!
//! Regras de commit (o relay aceita um por epoch):
//! - no máximo um commit nosso em voo; o resto espera numa fila;
//! - se outro commit ganha o epoch, descartamos o nosso, aplicamos o dele e
//!   tentamos de novo se ainda fizer sentido;
//! - mensagens de chat esperam o commit em voo terminar.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use lerri_core::invite::{AdmissionSecret, HostAddr, Invite, RouteToken, TokenHash};
use lerri_core::protocol::{Frame, Refusal, Role, RoomMode};
use lerri_core::{MemberId, RoomId, unix_now};
use lerri_crypto::group::{GroupState, Incoming, OutgoingCommit, admission_mac, check_admission_mac};
use lerri_crypto::{Identity, secret_store};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::settings::{Settings, clean_nick};
use crate::{Event, Route, Session};

pub const TEXT_MAX_CHARS: usize = 4000;
const HISTORY_MAX: usize = 500;
/// Renovação periódica da própria chave no grupo (SPEC-02 §4.1).
const UPDATE_EVERY: Duration = Duration::from_secs(12 * 3600);
const PURPOSE: &str = "lerrichat/group/v1";

/// O que vai dentro das mensagens de aplicação MLS.
#[derive(Debug, Clone, Serialize, Deserialize)]
enum Payload {
    Text { body: String, at: u64 },
    Profile { nick: String },
    /// O dono compartilha o `s` de um convite para qualquer membro online poder
    /// admitir o convidado (SPEC-02 §3.3).
    Invite { token_hash: TokenHash, secret: [u8; 32], expires_at: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatLine {
    pub from: MemberId,
    pub text: String,
    pub at: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    mls: Option<Vec<u8>>,
    invites: Vec<(TokenHash, [u8; 32], u64)>,
    nicks: Vec<(MemberId, String)>,
    history: Vec<ChatLine>,
    /// Convite usado para entrar, até ser admitido (para pedir de novo).
    pending_join: Option<(TokenHash, [u8; 32])>,
}

pub enum Start {
    /// Acabou de criar a sala: cria o grupo MLS.
    Created,
    /// Entrou com convite: pede admissão ao grupo.
    Joined { token: RouteToken, secret: AdmissionSecret },
    /// Voltou: o estado veio do disco.
    Rejoined,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberView {
    pub id: MemberId,
    pub nick: Option<String>,
    pub online: bool,
    pub owner: bool,
    pub me: bool,
    /// Conectado, mas ainda não admitido no grupo MLS.
    pub waiting: bool,
}

impl MemberView {
    /// "apelido · 1a2b3c4d" ou só o começo do id.
    pub fn label(&self) -> String {
        let short = &self.id.short()[..8];
        match &self.nick {
            Some(n) => format!("{n} · {short}"),
            None => format!("{short}…"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomEvent {
    /// Estado que bloqueia o chat (ex.: aguardando admissão). `None`: normal.
    Status(Option<String>),
    Chat(ChatLine),
    /// Linha de atividade (entrou, saiu, removido...).
    Notice(String),
    Members(Vec<MemberView>),
    Invite(Invite),
    History(Vec<ChatLine>),
}

pub enum Command {
    Send(String),
    NewInvite,
    Remove(MemberId),
    /// Sai da sala de vez (o grupo faz o commit de remoção).
    LeaveRoom,
    /// Só desconecta; a sala continua.
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Add(MemberId),
    Remove(MemberId),
    Update,
}

/// Como a sala terminou para este cliente.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    Closed,
    Left,
    Removed,
    Disconnected(String),
}

pub struct RoomCore {
    pub me: MemberId,
    pub room: RoomId,
    pub mode: RoomMode,
    pub owner: MemberId,
    group: GroupState,
    path: Option<PathBuf>,
    settings: Settings,
    invites: HashMap<TokenHash, ([u8; 32], u64)>,
    nicks: HashMap<MemberId, String>,
    history: Vec<ChatLine>,
    pending_join: Option<(TokenHash, [u8; 32])>,
    online: BTreeSet<MemberId>,
    /// Saíram e esperam o commit de remoção.
    left: BTreeSet<MemberId>,
    /// KeyPackages validados, esperando o commit de Add.
    key_packages: HashMap<MemberId, Vec<u8>>,
    join_tokens: HashMap<MemberId, TokenHash>,
    inflight: Option<Op>,
    queue: VecDeque<Op>,
    chat_outbox: VecDeque<Vec<u8>>,
    out: Vec<Frame>,
    events: Vec<RoomEvent>,
    exit: Option<Exit>,
}

fn group_file(data_dir: &Path, room: RoomId) -> PathBuf {
    data_dir.join("groups").join(format!("{room}.bin"))
}

impl RoomCore {
    /// Carrega o estado local da sala. `persist`: salas de servidor ficam em
    /// disco (cifradas); salas x1 só em memória.
    pub fn load(identity: &Identity, data_dir: &Path, room: RoomId, mode: RoomMode, persist: bool) -> anyhow::Result<Self> {
        let path = persist.then(|| group_file(data_dir, room));
        let saved: Saved = match &path {
            Some(p) => match secret_store::load(p, PURPOSE)? {
                Some(bytes) => postcard::from_bytes(&bytes).unwrap_or_default(),
                None => Saved::default(),
            },
            None => Saved::default(),
        };
        let group = match &saved.mls {
            Some(snap) => GroupState::restore(identity, room, snap).unwrap_or_else(|e| {
                tracing::warn!("estado MLS local inválido, recomeçando: {e}");
                GroupState::new(identity, room)
            }),
            None => GroupState::new(identity, room),
        };
        let settings = Settings::load(data_dir);
        Ok(Self {
            me: identity.member_id(),
            room,
            mode,
            owner: identity.member_id(),
            group,
            path,
            history: if settings.keep_history { saved.history } else { Vec::new() },
            settings,
            invites: saved.invites.into_iter().map(|(t, s, e)| (t, (s, e))).collect(),
            nicks: saved.nicks.into_iter().collect(),
            pending_join: saved.pending_join,
            online: BTreeSet::new(),
            left: BTreeSet::new(),
            key_packages: HashMap::new(),
            join_tokens: HashMap::new(),
            inflight: None,
            queue: VecDeque::new(),
            chat_outbox: VecDeque::new(),
            out: Vec::new(),
            events: Vec::new(),
            exit: None,
        })
    }

    /// Epoch MLS local (vai no `Rejoin` para o relay mandar o que falta).
    pub fn epoch(&self) -> u64 {
        self.group.epoch()
    }

    pub fn in_group(&self) -> bool {
        self.group.in_group()
    }

    pub fn is_owner(&self) -> bool {
        self.owner == self.me
    }

    /// Liga o estado à sessão recém-aberta.
    pub fn begin(&mut self, session: &Session, start: Start) {
        self.owner = session.owner;
        self.online = session.online.iter().copied().collect();
        match start {
            Start::Created => {
                if let Err(e) = self.group.create() {
                    self.notice(format!("Falha ao criar o grupo cifrado: {e}"));
                }
            }
            Start::Joined { token, secret } => {
                self.pending_join = Some((token.hash(), secret.0));
            }
            Start::Rejoined => {}
        }
        if !self.history.is_empty() {
            self.events.push(RoomEvent::History(self.history.clone()));
        }
        if self.in_group() {
            self.send_profile();
        } else {
            self.request_admission();
        }
        self.refresh();
        self.save();
    }

    fn request_admission(&mut self) {
        let key_package = match self.group.key_package() {
            Ok(kp) => kp,
            Err(e) => return self.notice(format!("Falha ao gerar o pedido de entrada: {e}")),
        };
        let (token_hash, mac) = match self.pending_join {
            Some((th, s)) => (Some(th), admission_mac(&s, &key_package)),
            // Sem convite: membro que perdeu o estado pede readição.
            None => (None, [0; 32]),
        };
        self.out.push(Frame::JoinRequest { token_hash, key_package, mac });
        self.save();
    }

    /// Eventos para a interface acumulados desde a última chamada.
    pub fn take_events(&mut self) -> Vec<RoomEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn take_out(&mut self) -> Vec<Frame> {
        std::mem::take(&mut self.out)
    }

    pub fn exit(&self) -> Option<&Exit> {
        self.exit.as_ref()
    }

    fn notice(&mut self, text: String) {
        self.events.push(RoomEvent::Notice(text));
    }

    fn name(&self, m: MemberId) -> String {
        if m == self.me {
            return "você".into();
        }
        let short = &m.short()[..8];
        match self.nicks.get(&m) {
            Some(n) => format!("{n} ({short})"),
            None => format!("{short}…"),
        }
    }

    /// Lista de membros para a interface.
    fn refresh(&mut self) {
        let in_group: BTreeSet<MemberId> = self.group.members().into_iter().collect();
        let mut all: BTreeSet<MemberId> = in_group.clone();
        all.extend(self.online.iter().copied());
        all.insert(self.me);
        let mut views: Vec<MemberView> = all
            .into_iter()
            .map(|id| MemberView {
                id,
                nick: if id == self.me {
                    (!self.settings.nickname.is_empty()).then(|| self.settings.nickname.clone())
                } else {
                    self.nicks.get(&id).cloned()
                },
                online: id == self.me || self.online.contains(&id),
                owner: id == self.owner,
                me: id == self.me,
                waiting: !in_group.contains(&id),
            })
            .collect();
        views.sort_by_key(|v| (!v.me, !v.online, v.waiting, v.nick.clone().unwrap_or_default()));
        self.events.push(RoomEvent::Members(views));
        let status = if self.in_group() {
            None
        } else if self.exit.is_none() {
            Some("Aguardando um membro online para admitir você no grupo cifrado…".to_string())
        } else {
            None
        };
        self.events.push(RoomEvent::Status(status));
    }

    fn save(&mut self) {
        let Some(path) = &self.path else { return };
        let saved = Saved {
            mls: Some(self.group.snapshot()),
            invites: self.invites.iter().map(|(t, (s, e))| (*t, *s, *e)).collect(),
            nicks: self.nicks.iter().map(|(m, n)| (*m, n.clone())).collect(),
            history: if self.settings.keep_history { self.history.clone() } else { Vec::new() },
            pending_join: self.pending_join,
        };
        let result = (|| {
            std::fs::create_dir_all(path.parent().expect("arquivo dentro de groups/"))?;
            secret_store::save(path, PURPOSE, &postcard::to_allocvec(&saved)?)?;
            anyhow::Ok(())
        })();
        if let Err(e) = result {
            tracing::warn!("não foi possível salvar o estado da sala: {e:#}");
        }
    }

    /// Apaga o estado local (saiu ou foi removido).
    fn forget(&mut self, data_dir: Option<&Path>) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(path);
        }
        if let Some(dir) = data_dir {
            let _ = crate::rooms::forget(dir, self.room);
        }
    }

    // ---- comandos da interface ----

    pub fn send_text(&mut self, text: &str) {
        let body: String = text.trim().chars().take(TEXT_MAX_CHARS).collect();
        if body.is_empty() {
            return;
        }
        let line = ChatLine { from: self.me, text: body.clone(), at: unix_now() };
        self.remember_line(line.clone());
        self.events.push(RoomEvent::Chat(line));
        self.send_payload(&Payload::Text { body, at: unix_now() });
    }

    /// Guarda o `s` do convite e o compartilha com o grupo.
    pub fn share_invite(&mut self, invite: &Invite) {
        let th = invite.token.hash();
        self.invites.insert(th, (invite.secret.0, invite.expires_at));
        self.send_payload(&Payload::Invite { token_hash: th, secret: invite.secret.0, expires_at: invite.expires_at });
        self.save();
    }

    pub fn remove_member(&mut self, member: MemberId) {
        if !self.is_owner() {
            return self.notice("Só o dono remove membros.".into());
        }
        if member == self.me {
            return;
        }
        self.enqueue(Op::Remove(member));
    }

    pub fn leave(&mut self, data_dir: &Path) {
        if self.is_owner() {
            return self.notice("O dono não pode sair da própria sala (transferir a posse vem depois).".into());
        }
        self.out.push(Frame::Leave);
        self.forget(Some(data_dir));
        self.exit = Some(Exit::Left);
    }

    pub fn periodic_update(&mut self) {
        if self.in_group() {
            self.enqueue(Op::Update);
        }
    }

    fn send_profile(&mut self) {
        let nick = clean_nick(&self.settings.nickname);
        if !nick.is_empty() {
            self.send_payload(&Payload::Profile { nick });
        }
    }

    fn send_payload(&mut self, p: &Payload) {
        let bytes = postcard::to_allocvec(p).expect("payload serializa");
        self.chat_outbox.push_back(bytes);
        self.flush_chat();
    }

    fn flush_chat(&mut self) {
        if !self.in_group() || self.group.has_pending_commit() {
            return;
        }
        while let Some(p) = self.chat_outbox.pop_front() {
            match self.group.encrypt(&p) {
                Ok(data) => self.out.push(Frame::App { data }),
                Err(e) => {
                    self.notice(format!("Mensagem não enviada: {e}"));
                    break;
                }
            }
        }
        self.save();
    }

    fn remember_line(&mut self, line: ChatLine) {
        if self.settings.keep_history {
            self.history.push(line);
            if self.history.len() > HISTORY_MAX {
                let excess = self.history.len() - HISTORY_MAX;
                self.history.drain(..excess);
            }
        }
    }

    // ---- fila de commits ----

    fn enqueue(&mut self, op: Op) {
        if self.inflight != Some(op) && !self.queue.contains(&op) {
            self.queue.push_back(op);
        }
        self.next_op();
    }

    fn next_op(&mut self) {
        if self.inflight.is_some() || !self.in_group() || self.group.has_pending_commit() {
            return;
        }
        while let Some(op) = self.queue.pop_front() {
            let members = self.group.members();
            let commit = match op {
                Op::Add(m) => {
                    let Some(kp) = self.key_packages.get(&m) else { continue };
                    // Readição troca o leaf; entrada nova não pode já estar no grupo.
                    if members.contains(&m) && self.join_tokens.contains_key(&m) {
                        continue;
                    }
                    self.group.add(&kp.clone())
                }
                Op::Remove(m) => {
                    if !members.contains(&m) {
                        self.left.remove(&m);
                        continue;
                    }
                    self.group.remove(m)
                }
                Op::Update => self.group.self_update(),
            };
            match commit {
                Ok(c) => {
                    self.send_commit(c);
                    self.inflight = Some(op);
                    return;
                }
                Err(e) => self.notice(format!("Commit não pôde ser criado: {e}")),
            }
        }
        self.flush_chat();
    }

    fn send_commit(&mut self, c: OutgoingCommit) {
        self.out.push(Frame::Commit { epoch: c.epoch, data: c.data, welcome: c.welcome, adds: c.adds, removes: c.removes });
    }

    /// Nosso commit não vale mais: descarta e devolve a operação à fila.
    fn abandon_inflight(&mut self) {
        if self.group.has_pending_commit() {
            let _ = self.group.discard_pending();
        }
        if let Some(op) = self.inflight.take() {
            self.queue.push_front(op);
        }
    }

    /// Algo deu errado com o estado MLS: pede para ser readicionado.
    fn desync(&mut self, why: String) {
        self.notice(format!("Estado do grupo dessincronizado ({why}); pedindo para ser readicionado."));
        self.inflight = None;
        self.queue.clear();
        self.request_admission();
    }

    // ---- eventos do relay ----

    pub fn on_event(&mut self, event: Event, data_dir: &Path) {
        match event {
            Event::Online { member, first_time } => {
                self.online.insert(member);
                if first_time {
                    self.notice(format!("{} entrou pelo convite e aguarda admissão.", self.name(member)));
                } else {
                    self.notice(format!("{} ficou online.", self.name(member)));
                }
                self.refresh();
            }
            Event::Offline { member } => {
                self.online.remove(&member);
                self.notice(format!("{} saiu.", self.name(member)));
                self.refresh();
            }
            Event::Disconnected(reason) => {
                if self.exit.is_none() {
                    self.exit = Some(Exit::Disconnected(reason));
                }
            }
            Event::Relay(frame) => self.on_frame(frame, data_dir),
        }
    }

    fn on_frame(&mut self, frame: Frame, data_dir: &Path) {
        match frame {
            Frame::CommitFrom { from, epoch, data } => self.on_commit(from, epoch, &data),
            Frame::CommitAccepted { .. } => {
                if self.group.has_pending_commit()
                    && let Err(e) = self.group.confirm_pending()
                {
                    return self.desync(e.to_string());
                }
                if let Some(op) = self.inflight.take() {
                    self.after_op(op);
                }
                self.save();
                self.refresh();
                self.next_op();
            }
            Frame::CommitRejected { .. } => {
                // O commit vencedor já chegou antes (o relay manda em ordem).
                self.abandon_inflight();
                self.next_op();
            }
            Frame::Refused(Refusal::NotOwner) => {
                let _ = self.group.discard_pending();
                self.inflight = None;
                self.notice("O relay recusou: só o dono pode remover membros.".into());
                self.next_op();
            }
            Frame::Refused(r) => self.notice(format!("O relay recusou: {r}.")),
            Frame::GroupWelcome { data } => self.on_welcome(&data),
            Frame::JoinRequested { member, token_hash, key_package, mac } => {
                self.on_join_request(member, token_hash, &key_package, &mac)
            }
            Frame::AppFrom { from, data } => self.on_app(from, &data),
            Frame::MemberRemoved { member, left } => {
                self.online.remove(&member);
                if left {
                    self.left.insert(member);
                    self.notice(format!("{} saiu da sala.", self.name(member)));
                    self.enqueue(Op::Remove(member));
                } else {
                    self.notice(format!("{} foi removido pelo dono.", self.name(member)));
                }
                self.refresh();
            }
            Frame::Removed => {
                self.notice("Você foi removido da sala pelo dono.".into());
                self.forget(Some(data_dir));
                self.exit = Some(Exit::Removed);
            }
            other => tracing::debug!("frame ignorado na sala: {other:?}"),
        }
    }

    fn after_op(&mut self, op: Op) {
        match op {
            Op::Add(m) => {
                self.key_packages.remove(&m);
                if let Some(th) = self.join_tokens.remove(&m) {
                    self.invites.remove(&th);
                }
                self.notice(format!("Você admitiu {} no grupo.", self.name(m)));
                // O novato aprende os apelidos de quem já está.
                self.send_profile();
            }
            Op::Remove(m) => {
                self.left.remove(&m);
                self.notice(format!("{} não faz mais parte do grupo cifrado (chaves renovadas).", self.name(m)));
            }
            Op::Update => {}
        }
    }

    fn on_commit(&mut self, from: MemberId, epoch: u64, data: &[u8]) {
        if !self.in_group() {
            return;
        }
        let mine = self.epoch();
        if epoch < mine {
            return;
        }
        if epoch > mine {
            return self.desync(format!("faltam commits {mine}..{epoch}"));
        }
        if from != self.me && self.group.has_pending_commit() {
            // Outro commit ganhou este epoch.
            self.abandon_inflight();
        }
        match self.group.process(data) {
            Ok(Incoming::Commit { committer, added, removed, self_removed }) => {
                for m in &added {
                    self.key_packages.remove(m);
                    if let Some(th) = self.join_tokens.remove(m) {
                        self.invites.remove(&th);
                    }
                    self.notice(format!("{} foi admitido no grupo por {}.", self.name(*m), self.name(committer)));
                }
                for m in &removed {
                    let was_leaving = self.left.remove(m);
                    if committer != self.owner && !was_leaving && *m != committer {
                        self.notice(format!("Atenção: {} removeu {} sem ser o dono.", self.name(committer), self.name(*m)));
                    }
                    self.queue.retain(|op| *op != Op::Remove(*m));
                    self.notice(format!("{} não faz mais parte do grupo cifrado (chaves renovadas).", self.name(*m)));
                }
                if !added.is_empty() {
                    self.send_profile();
                }
                if self_removed {
                    self.notice("Você não faz mais parte do grupo.".into());
                }
            }
            Ok(Incoming::OwnCommit) => {
                if let Some(op) = self.inflight.take() {
                    self.after_op(op);
                }
            }
            Ok(_) => {}
            Err(e) => return self.desync(e.to_string()),
        }
        self.save();
        self.refresh();
        self.next_op();
    }

    fn on_welcome(&mut self, data: &[u8]) {
        if self.in_group() {
            return;
        }
        match self.group.join(data) {
            Ok(by) => {
                self.pending_join = None;
                self.notice(format!("Você foi admitido no grupo cifrado por {}.", self.name(by)));
                self.send_profile();
                self.save();
                self.refresh();
                self.next_op();
            }
            Err(e) => self.notice(format!("Welcome inválido: {e}")),
        }
    }

    fn on_join_request(&mut self, member: MemberId, token_hash: Option<TokenHash>, kp: &[u8], mac: &[u8; 32]) {
        if !self.in_group() || member == self.me {
            return;
        }
        let in_group = self.group.members().contains(&member);
        let valid = match token_hash {
            Some(th) => match self.invites.get(&th) {
                // Convite que este membro conhece: confere a prova sobre `s`.
                Some((secret, expires)) if *expires > unix_now() => {
                    if !check_admission_mac(secret, kp, mac) {
                        Err("prova do convite não confere".to_string())
                    } else if in_group {
                        Err("já está no grupo".into())
                    } else {
                        Ok(())
                    }
                }
                Some(_) => Err("convite expirado".into()),
                None => Err("convite desconhecido para este membro".into()),
            },
            // Readição: só de quem já está no grupo (troca de leaf).
            None if in_group => Ok(()),
            None => Err("pedido sem convite de quem não é membro".into()),
        };
        let valid = valid.and_then(|()| match self.group.check_key_package(kp) {
            Ok((_, owner)) if owner == member => Ok(()),
            Ok(_) => Err("KeyPackage de outra pessoa".into()),
            Err(e) => Err(e.to_string()),
        });
        match valid {
            Ok(()) => {
                self.key_packages.insert(member, kp.to_vec());
                if let Some(th) = token_hash {
                    self.join_tokens.insert(member, th);
                }
                self.enqueue(Op::Add(member));
            }
            // Convite que não conhecemos pode ser de outro membro: só registra.
            Err(why) if why.contains("desconhecido") => tracing::debug!("pedido de {}: {why}", member.short()),
            Err(why) => self.notice(format!("Pedido de entrada de {} recusado: {why}.", self.name(member))),
        }
    }

    fn on_app(&mut self, from: MemberId, data: &[u8]) {
        if !self.in_group() {
            return;
        }
        let (sender, bytes) = match self.group.process(data) {
            Ok(Incoming::App { sender, data }) => (sender, data),
            Ok(_) => return,
            Err(e) => return tracing::debug!("mensagem de {} não decifrou: {e}", from.short()),
        };
        if sender != from {
            return self.notice(format!("Mensagem com remetente inconsistente descartada ({}).", from.short()));
        }
        let Ok(payload) = postcard::from_bytes::<Payload>(&bytes) else { return };
        match payload {
            Payload::Text { body, at } => {
                let line = ChatLine { from: sender, text: body.chars().take(TEXT_MAX_CHARS).collect(), at };
                self.remember_line(line.clone());
                self.events.push(RoomEvent::Chat(line));
            }
            Payload::Profile { nick } => {
                let nick = clean_nick(&nick);
                if !nick.is_empty() && self.nicks.get(&sender) != Some(&nick) {
                    self.nicks.insert(sender, nick);
                    self.refresh();
                }
            }
            Payload::Invite { token_hash, secret, expires_at } => {
                // Só o dono emite convites (SPEC-02 §2).
                if sender == self.owner {
                    self.invites.insert(token_hash, (secret, expires_at));
                }
            }
        }
        self.save();
    }
}

/// Liga [`RoomCore`] à [`Session`] até a sala terminar. `emit` recebe os
/// eventos para a interface; `route` o caminho da conexão a cada 2 s.
pub async fn run(
    mut session: Session,
    mut core: RoomCore,
    data_dir: PathBuf,
    invite_host: Option<HostAddr>,
    mut cmds: mpsc::UnboundedReceiver<Command>,
    emit: impl Fn(RoomEvent) + Send,
    route: impl Fn(Option<Route>) + Send,
) -> Exit {
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    let mut update = tokio::time::interval_at(tokio::time::Instant::now() + UPDATE_EVERY, UPDATE_EVERY);
    loop {
        for f in core.take_out() {
            if let Err(e) = session.send(&f).await {
                core.on_event(Event::Disconnected(format!("{e:#}")), &data_dir);
            }
        }
        for ev in core.take_events() {
            emit(ev);
        }
        if let Some(exit) = core.exit().cloned() {
            session.close().await;
            return exit;
        }
        tokio::select! {
            ev = session.next_event() => match ev {
                Some(ev) => core.on_event(ev, &data_dir),
                None => core.on_event(Event::Disconnected("conexão encerrada".into()), &data_dir),
            },
            cmd = cmds.recv() => match cmd {
                Some(Command::Send(text)) => core.send_text(&text),
                Some(Command::NewInvite) => match &invite_host {
                    Some(host) => match session.issue_invite(host, lerri_core::invite::DEFAULT_INVITE_TTL_SECS).await {
                        Ok(inv) => {
                            core.share_invite(&inv);
                            emit(RoomEvent::Invite(inv));
                        }
                        Err(e) => emit(RoomEvent::Notice(format!("Não deu para criar o convite: {e:#}"))),
                    },
                    None => emit(RoomEvent::Notice("Só o dono emite convites.".into())),
                },
                Some(Command::Remove(m)) => core.remove_member(m),
                Some(Command::LeaveRoom) => core.leave(&data_dir),
                Some(Command::Close) | None => core.exit = Some(Exit::Closed),
            },
            _ = tick.tick() => route(session.route()),
            _ = update.tick() => core.periodic_update(),
        }
    }
}

/// Abre a sessão da sala e o estado local juntos, na ordem certa.
pub async fn open(
    identity: &Identity,
    data_dir: &Path,
    host: &HostAddr,
    how: OpenHow,
) -> anyhow::Result<(Session, RoomCore)> {
    use lerri_core::protocol::Auth;
    match how {
        OpenHow::Create { admin, mode } => {
            let session = Session::connect(identity, host, Auth::CreateRoom { admin, mode }).await?;
            let mut core = RoomCore::load(identity, data_dir, session.room, mode, mode == RoomMode::Server)?;
            core.begin(&session, Start::Created);
            Ok((session, core))
        }
        OpenHow::Join { room, token, secret } => {
            let session = Session::connect(identity, host, Auth::Join { room, token }).await?;
            let persist = session.mode == RoomMode::Server;
            let mut core = RoomCore::load(identity, data_dir, room, session.mode, persist)?;
            core.begin(&session, Start::Joined { token, secret });
            Ok((session, core))
        }
        OpenHow::Rejoin { room, mode } => {
            let core = RoomCore::load(identity, data_dir, room, mode, mode == RoomMode::Server)?;
            let session = Session::connect(identity, host, Auth::Rejoin { room, epoch: core.epoch() }).await?;
            let mut core = core;
            core.begin(&session, Start::Rejoined);
            Ok((session, core))
        }
    }
}

pub enum OpenHow {
    Create { admin: lerri_core::invite::AdminKey, mode: RoomMode },
    Join { room: RoomId, token: RouteToken, secret: AdmissionSecret },
    Rejoin { room: RoomId, mode: RoomMode },
}

/// Papel na sala para exibir.
pub fn role_of(core: &RoomCore) -> Role {
    if core.is_owner() { Role::Owner } else { Role::Member }
}
