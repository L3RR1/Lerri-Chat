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
use std::sync::Arc;
use std::time::Duration;

use lerri_core::invite::{AdmissionSecret, HostAddr, Invite, RouteToken, TokenHash};
use lerri_core::media::{Codec, Video};
use lerri_core::protocol::{Frame, Refusal, Role, RoomMode};
use lerri_core::{MemberId, RoomId, unix_now};
use lerri_crypto::group::{GroupState, Incoming, OutgoingCommit, admission_mac, check_admission_mac};
use lerri_crypto::sframe::MediaEpoch;
use lerri_crypto::{Identity, secret_store};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::settings::{Settings, clean_nick};
use crate::voice::VoiceSession;
use crate::{Event, Route, Session};

pub const TEXT_MAX_CHARS: usize = 4000;
const HISTORY_MAX: usize = 500;
/// Renovação periódica da própria chave no grupo (SPEC-02 §4.1).
const UPDATE_EVERY: Duration = Duration::from_secs(12 * 3600);
const PURPOSE: &str = "lerrichat/group/v1";
/// Com que frequência a lista de quem está falando é atualizada.
const SPEAKING_EVERY: Duration = Duration::from_millis(250);

/// O que vai dentro das mensagens de aplicação MLS.
#[derive(Debug, Clone, Serialize, Deserialize)]
enum Payload {
    Text { body: String, at: u64 },
    Profile { nick: String },
    /// O dono compartilha o `s` de um convite para qualquer membro online poder
    /// admitir o convidado (SPEC-02 §3.3).
    Invite { token_hash: TokenHash, secret: [u8; 32], expires_at: u64 },
    /// Codecs de vídeo que este membro decodifica (negociação, SPEC-04 §2).
    Caps { decode: Vec<Codec> },
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
    /// Está no canal de voz.
    pub in_voice: bool,
    /// Falando agora (RF-09).
    pub speaking: bool,
    /// Compartilhando a tela.
    pub sharing: bool,
    /// Com a câmera ligada.
    pub camera: bool,
    /// Ida e volta estimada até esta pessoa, em ms (para você: até o host).
    pub latency_ms: Option<u32>,
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
    /// Estado do nosso canal de voz.
    Voice(VoiceView),
    /// Voz aberta sem dispositivos ([`Command::JoinVoice`] com `devices:
    /// false`): quem pediu alimenta o microfone e puxa a saída.
    VoiceHandle(VoiceHandle),
    /// Estado de um stream nosso (tela ou câmera).
    Stream { video: Video, view: StreamView },
    /// O que estamos assistindo mudou.
    Watching(Vec<(MemberId, Video)>),
    /// Publicação sem GPU ([`Command::Publish`] com `gpu: false`): quem pediu
    /// manda os quadros já codificados.
    OutgoingHandle { video: Video, handle: OutgoingHandle },
    /// A janela de vídeo não é mais usada e pode ser destruída.
    VideoClosed { window: isize },
    /// Alguém pediu um keyframe de um stream nosso (só sem GPU; com GPU o
    /// encoder trata sozinho).
    KeyframeRequested { video: Video },
    /// O que esta máquina faz com vídeo (chega logo depois de abrir a sala).
    Video(VideoInfo),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VideoInfo {
    /// Monitores (identificador, nome), o principal primeiro.
    pub monitors: Vec<(isize, String)>,
    /// Câmeras (identificador, nome).
    pub cameras: Vec<(String, String)>,
    /// Tem encoder de vídeo por hardware.
    pub can_encode: bool,
    /// Por que não dá para assistir, se não dá.
    pub watch_problem: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamView {
    /// Estamos publicando: o que sai ("HEVC 1920×1080 · encoder").
    pub sharing: Option<String>,
    /// Quantos assistem.
    pub watchers: u32,
    /// Quantos deles recebem a camada baixa do simulcast (rede ruim).
    pub low_watchers: u32,
}

#[derive(Clone)]
pub struct OutgoingHandle(pub Arc<crate::screen::Outgoing>);

impl std::fmt::Debug for OutgoingHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OutgoingHandle")
    }
}

impl PartialEq for OutgoingHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for OutgoingHandle {}

/// O que publicar.
#[derive(Debug, Clone)]
pub enum PublishSource {
    /// O monitor (`None`: o principal).
    Screen { monitor: Option<isize> },
    /// Uma janela só; quem assiste vê como a tela.
    Window { hwnd: isize },
    /// A câmera (`None`: a primeira).
    Camera { device: Option<String> },
}

impl PublishSource {
    pub fn video(&self) -> Video {
        match self {
            PublishSource::Screen { .. } | PublishSource::Window { .. } => Video::Screen,
            PublishSource::Camera { .. } => Video::Camera,
        }
    }
}

/// Onde mostrar um stream assistido.
pub enum WatchTarget {
    /// Janela-filha criada pela interface (pixels físicos).
    Window { hwnd: isize, size: (u32, u32) },
    /// Quadros decifrados, ainda codificados (testes).
    Frames(crate::screen::Sink),
    /// Decodifica na GPU sem mostrar e relata fps e banda a cada 5 s
    /// (diagnóstico no terminal).
    Headless,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VoiceView {
    pub active: bool,
    pub muted: bool,
    pub deafened: bool,
    /// Supressão forte de ruído (RNNoise).
    pub strong_ns: bool,
    /// "microfone → saída" em uso.
    pub devices: Option<String>,
    /// Apertar para falar: o nome da tecla. `None`: detecção de voz.
    pub push_to_talk: Option<String>,
}

#[derive(Clone)]
pub struct VoiceHandle(pub Arc<lerri_media::Voice>);

impl std::fmt::Debug for VoiceHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VoiceHandle")
    }
}

impl PartialEq for VoiceHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for VoiceHandle {}

pub enum Command {
    Send(String),
    NewInvite,
    Remove(MemberId),
    /// Sai da sala de vez (o grupo faz o commit de remoção).
    LeaveRoom,
    /// Só desconecta; a sala continua.
    Close,
    /// Entra no canal de voz. `devices: false` é para testes (ver
    /// [`RoomEvent::VoiceHandle`]).
    JoinVoice { devices: bool },
    LeaveVoice,
    Mute(bool),
    /// Não ouve nem fala.
    Deafen(bool),
    StrongNoiseSuppression(bool),
    /// Troca o microfone e a saída (`None`: os padrões do Windows). Na voz,
    /// reabre o áudio sem sair dela.
    AudioDevices(lerri_media::device::DeviceChoice),
    /// Apertar para falar com a tecla dada (código virtual); `None` volta
    /// para a detecção de voz.
    PushToTalk(Option<u32>),
    /// Publica a tela ou a câmera. `gpu: false` é para testes (ver
    /// [`RoomEvent::OutgoingHandle`]).
    Publish { source: PublishSource, gpu: bool },
    Unpublish(Video),
    /// Assiste à tela ou à câmera de `member` (troca o que já mostrava do
    /// mesmo tipo).
    Watch { member: MemberId, video: Video, target: WatchTarget },
    Unwatch { member: MemberId, video: Video },
    /// Uma janela de vídeo mudou de tamanho (pixels físicos).
    VideoResize { window: isize, size: (u32, u32) },
    /// Janela principal minimizada: para de receber e decodificar o que
    /// assiste (`true`) e volta num keyframe (`false`).
    PauseVideo(bool),
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
    /// Quem está no canal de voz (nós inclusive, quando estamos).
    voice_members: BTreeSet<MemberId>,
    speaking: BTreeSet<MemberId>,
    /// Ida e volta de cada membro até o host (do relay, a cada 3 s).
    latency: HashMap<MemberId, u32>,
    /// Quem publica tela ou câmera.
    publishing: BTreeSet<(MemberId, Video)>,
    /// Codecs que cada membro decodifica (anunciados pelo MLS).
    caps: HashMap<MemberId, Vec<Codec>>,
    my_caps: Vec<Codec>,
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
            voice_members: BTreeSet::new(),
            speaking: BTreeSet::new(),
            latency: HashMap::new(),
            publishing: BTreeSet::new(),
            caps: HashMap::new(),
            my_caps: Vec::new(),
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
        self.voice_members = session.voice.iter().copied().collect();
        self.publishing = session.publishing.iter().copied().collect();
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
                in_voice: self.voice_members.contains(&id),
                speaking: self.speaking.contains(&id),
                latency_ms: self.latency_to(id),
                sharing: self.publishing.contains(&(id, Video::Screen)),
                camera: self.publishing.contains(&(id, Video::Camera)),
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

    /// Chaves de mídia do epoch atual (`None` fora do grupo).
    pub fn media_epoch(&self) -> Option<MediaEpoch> {
        self.group.media_epoch().ok()
    }

    /// Entra ou sai do canal de voz (avisa o relay).
    pub fn set_in_voice(&mut self, on: bool) {
        self.out.push(Frame::Voice { on });
        if on {
            self.voice_members.insert(self.me);
        } else {
            self.voice_members.remove(&self.me);
            self.speaking.clear();
        }
        self.refresh();
    }

    pub fn voice_members(&self) -> &BTreeSet<MemberId> {
        &self.voice_members
    }

    /// Quem está falando agora (nós inclusive).
    /// A mídia sempre passa pelo host: até alguém é a sua ida e volta até o
    /// host mais a dele (no x1 o dono é o host, então a dele é ~0).
    fn latency_to(&self, id: MemberId) -> Option<u32> {
        let mine = *self.latency.get(&self.me)?;
        if id == self.me {
            return Some(mine);
        }
        Some(mine.saturating_add(*self.latency.get(&id)?))
    }

    pub fn set_latency(&mut self, rtts: Vec<(MemberId, u32)>) {
        let latency: HashMap<MemberId, u32> = rtts.into_iter().collect();
        if latency != self.latency {
            self.latency = latency;
            self.refresh();
        }
    }

    pub fn set_speaking(&mut self, speaking: BTreeSet<MemberId>) {
        if speaking != self.speaking {
            self.speaking = speaking;
            self.refresh();
        }
    }

    pub fn strong_ns(&self) -> bool {
        self.settings.strong_ns
    }

    pub fn set_strong_ns(&mut self, on: bool, data_dir: &Path) {
        self.settings.strong_ns = on;
        self.save_settings(data_dir);
    }

    pub fn audio_choice(&self) -> lerri_media::device::DeviceChoice {
        lerri_media::device::DeviceChoice { input: self.settings.input_device.clone(), output: self.settings.output_device.clone() }
    }

    pub fn set_audio_choice(&mut self, choice: &lerri_media::device::DeviceChoice, data_dir: &Path) {
        self.settings.input_device = choice.input.clone();
        self.settings.output_device = choice.output.clone();
        self.save_settings(data_dir);
    }

    pub fn push_to_talk(&self) -> Option<u32> {
        self.settings.push_to_talk
    }

    pub fn set_push_to_talk(&mut self, key: Option<u32>, data_dir: &Path) {
        self.settings.push_to_talk = key;
        if key.is_some() {
            self.settings.push_to_talk_key = key;
        }
        self.save_settings(data_dir);
    }

    fn save_settings(&mut self, data_dir: &Path) {
        if let Err(e) = self.settings.save(data_dir) {
            self.notice(format!("Não deu para salvar a preferência: {e:#}"));
        }
    }

    pub fn periodic_update(&mut self) {
        if self.in_group() {
            self.enqueue(Op::Update);
        }
    }

    /// Apelido e codecs de vídeo que decodificamos, para quem entrou agora.
    fn send_profile(&mut self) {
        let nick = clean_nick(&self.settings.nickname);
        if !nick.is_empty() {
            self.send_payload(&Payload::Profile { nick });
        }
        if !self.my_caps.is_empty() {
            self.send_payload(&Payload::Caps { decode: self.my_caps.clone() });
        }
    }

    /// Codecs que esta máquina decodifica; anuncia ao grupo se mudou.
    pub fn set_decode_caps(&mut self, caps: Vec<Codec>) {
        if caps != self.my_caps {
            self.my_caps = caps;
            if self.in_group() && !self.my_caps.is_empty() {
                self.send_payload(&Payload::Caps { decode: self.my_caps.clone() });
            }
        }
    }

    /// Começa ou para de publicar a tela ou a câmera (avisa o relay).
    pub fn set_publishing(&mut self, video: Video, on: bool) {
        self.out.push(Frame::Publish { video, on });
        if on {
            self.publishing.insert((self.me, video));
        } else {
            self.publishing.remove(&(self.me, video));
        }
        self.refresh();
    }

    pub fn publishing(&self) -> &BTreeSet<(MemberId, Video)> {
        &self.publishing
    }

    /// Codecs que cada outro membro online decodifica (só quem anunciou
    /// algum; quem não decodifica nada não entra na negociação).
    pub fn peer_decode_caps(&self) -> Vec<Vec<Codec>> {
        self.online
            .iter()
            .filter(|m| **m != self.me)
            .filter_map(|m| self.caps.get(m))
            .filter(|c| !c.is_empty())
            .cloned()
            .collect()
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
                self.voice_members.remove(&member);
                self.speaking.remove(&member);
                self.publishing.retain(|(m, _)| *m != member);
                self.notice(format!("{} saiu.", self.name(member)));
                self.refresh();
            }
            Event::Publish { member, video, on } => {
                let changed = if on { self.publishing.insert((member, video)) } else { self.publishing.remove(&(member, video)) };
                if changed {
                    let what = match (video, on) {
                        (Video::Screen, true) => "começou a compartilhar a tela",
                        (Video::Screen, false) => "parou de compartilhar a tela",
                        (Video::Camera, true) => "ligou a câmera",
                        (Video::Camera, false) => "desligou a câmera",
                    };
                    self.notice(format!("{} {what}.", self.name(member)));
                    self.refresh();
                }
            }
            Event::Voice { member, on } => {
                let changed = if on { self.voice_members.insert(member) } else { self.voice_members.remove(&member) };
                if changed {
                    let what = if on { "entrou na voz" } else { "saiu da voz" };
                    self.notice(format!("{} {what}.", self.name(member)));
                    self.speaking.remove(&member);
                    self.refresh();
                }
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
            Payload::Caps { mut decode } => {
                decode.truncate(Codec::ALL.len());
                self.caps.insert(sender, decode);
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
    let mut fast_tick = tokio::time::interval(SPEAKING_EVERY);
    fast_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut media = Media::new(&session, &core);
    // A interface já mostra a supressão e a tecla de falar antes de entrar na voz.
    emit(RoomEvent::Voice(media.voice_view.clone()));
    // O que a GPU desta máquina faz com vídeo (alguns ms; fora do runtime).
    let mut probe = Some(tokio::task::spawn_blocking(probe_video));
    loop {
        for f in core.take_out() {
            if let Err(e) = session.send(&f).await {
                core.on_event(Event::Disconnected(format!("{e:#}")), &data_dir);
            }
        }
        media.sync_keys(&core);
        for ev in core.take_events() {
            emit(ev);
        }
        if let Some(exit) = core.exit().cloned() {
            media.close(&emit);
            session.close().await;
            return exit;
        }
        tokio::select! {
            ev = session.next_event() => match ev {
                Some(Event::Relay(Frame::KeyframeRequested { video })) => media.keyframe_requested(video, &emit),
                Some(Event::Relay(Frame::Watchers { video, count, low })) => media.set_watchers(video, count, low, &emit),
                Some(Event::Publish { member, video, on: false }) => {
                    core.on_event(Event::Publish { member, video, on: false }, &data_dir);
                    media.unwatch(&mut core, member, video, &emit);
                }
                Some(Event::Offline { member }) => {
                    core.on_event(Event::Offline { member }, &data_dir);
                    for video in Video::ALL {
                        media.unwatch(&mut core, member, video, &emit);
                    }
                }
                Some(ev) => core.on_event(ev, &data_dir),
                None => core.on_event(Event::Disconnected("conexão encerrada".into()), &data_dir),
            },
            caps = async { probe.as_mut().expect("guardado pelo if").await }, if probe.is_some() => {
                probe = None;
                let caps = caps.unwrap_or_default();
                core.set_decode_caps(caps.decode.clone());
                emit(RoomEvent::Video(video_info(&caps)));
                media.video_caps = Some(caps);
            }
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
                Some(Command::JoinVoice { devices }) => media.join_voice(&session, &mut core, devices, &emit),
                Some(Command::LeaveVoice) => media.leave_voice(&mut core, &emit, None),
                Some(Command::Mute(on)) => media.mute(on, &emit),
                Some(Command::Deafen(on)) => media.deafen(on, &emit),
                Some(Command::StrongNoiseSuppression(on)) => {
                    core.set_strong_ns(on, &data_dir);
                    media.strong_ns(on, &emit);
                }
                Some(Command::AudioDevices(choice)) => media.audio_devices(&mut core, choice, &data_dir, &emit),
                Some(Command::PushToTalk(key)) => media.push_to_talk(&mut core, key, &data_dir, &emit),
                Some(Command::Publish { source, gpu }) => media.publish(&session, &mut core, source, gpu, &emit).await,
                Some(Command::Unpublish(video)) => media.unpublish(&mut core, video, &emit),
                Some(Command::Watch { member, video, target }) => media.watch(&mut core, member, video, target, &emit),
                Some(Command::Unwatch { member, video }) => media.unwatch(&mut core, member, video, &emit),
                Some(Command::VideoResize { window, size }) => media.resize(window, size),
                Some(Command::PauseVideo(paused)) => media.pause(&mut core, paused),
            },
            _ = tick.tick() => {
                route(session.route());
                core.set_latency(session.latency());
                if let Some(v) = &mut media.voice {
                    v.measure_loss();
                }
            }
            _ = fast_tick.tick(), if media.active() => media.poll(&mut core, &emit),
            _ = update.tick() => core.periodic_update(),
        }
    }
}

/// O que esta máquina faz com vídeo.
#[cfg(windows)]
pub type VideoCaps = crate::video::Caps;

/// Sem Windows não há vídeo com GPU; o tipo existe para o resto do código ser
/// igual.
#[cfg(not(windows))]
#[derive(Debug, Clone, Default)]
pub struct VideoCaps {
    pub encode: Vec<Codec>,
    pub decode: Vec<Codec>,
}

fn device_label(d: &lerri_media::device::Devices) -> String {
    let (input, output) = d.names();
    format!("{input} → {output}")
}

fn video_info(caps: &VideoCaps) -> VideoInfo {
    #[cfg(windows)]
    return VideoInfo {
        monitors: caps
            .monitors
            .iter()
            .map(|m| (m.handle, format!("{} ({}×{}){}", m.name, m.width, m.height, if m.primary { ", principal" } else { "" })))
            .collect(),
        cameras: caps.cameras.iter().map(|c| (c.id.clone(), c.name.clone())).collect(),
        can_encode: !caps.encode.is_empty(),
        watch_problem: caps
            .decode_problem
            .clone()
            .or_else(|| caps.decode.is_empty().then(|| "a GPU não decodifica H.264, HEVC nem AV1".into())),
    };
    #[cfg(not(windows))]
    {
        let _ = caps;
        VideoInfo { watch_problem: Some("vídeo ainda só no Windows".into()), ..VideoInfo::default() }
    }
}

fn probe_video() -> VideoCaps {
    #[cfg(windows)]
    return crate::video::probe();
    #[cfg(not(windows))]
    VideoCaps::default()
}

/// Um stream nosso no ar.
struct Share {
    /// Mantém a saída viva; derrubar o `Share` para o envio.
    _out: Arc<crate::screen::Outgoing>,
    #[cfg(windows)]
    gpu: Option<crate::video::GpuShare>,
}

/// Um stream que estamos assistindo.
struct Viewer {
    #[cfg(windows)]
    gpu: Option<crate::video::GpuViewer>,
    /// Último relatório (só sem janela): quando, quadros, bytes.
    #[cfg(windows)]
    report: Option<(std::time::Instant, u64, u64)>,
}

/// Mídia da sala: chaves SFrame (voz e vídeo usam as mesmas), voz, nossos
/// streams de vídeo e os que assistimos.
struct Media {
    keys: lerri_media::screen::Keys,
    epoch: Option<u64>,
    voice: Option<VoiceSession>,
    voice_view: VoiceView,
    shares: HashMap<Video, Share>,
    views: HashMap<Video, StreamView>,
    viewers: HashMap<(MemberId, Video), Viewer>,
    watching: crate::screen::Watching,
    incoming: tokio::task::JoinHandle<()>,
    video_caps: Option<VideoCaps>,
}

impl Media {
    fn new(session: &Session, core: &RoomCore) -> Self {
        let keys: lerri_media::screen::Keys = Arc::default();
        let watching: crate::screen::Watching = Arc::default();
        let incoming = crate::screen::spawn_incoming(session.connection(), keys.clone(), watching.clone());
        Self {
            keys,
            epoch: None,
            voice: None,
            voice_view: VoiceView {
                strong_ns: core.strong_ns(),
                push_to_talk: core.push_to_talk().map(lerri_media::hotkey::key_name),
                ..VoiceView::default()
            },
            shares: HashMap::new(),
            views: HashMap::new(),
            viewers: HashMap::new(),
            watching,
            incoming,
            video_caps: None,
        }
    }

    fn active(&self) -> bool {
        self.voice.is_some() || !self.shares.is_empty() || !self.viewers.is_empty()
    }

    /// Troca as chaves de mídia quando o epoch MLS muda.
    fn sync_keys(&mut self, core: &RoomCore) {
        match core.media_epoch() {
            Some(e) if self.epoch != Some(e.epoch) => {
                self.epoch = Some(e.epoch);
                self.keys.lock().unwrap().set_epoch(e);
            }
            None if self.epoch.is_some() => {
                self.epoch = None;
                self.keys.lock().unwrap().clear();
            }
            _ => {}
        }
    }

    fn close(&mut self, emit: &impl Fn(RoomEvent)) {
        self.voice = None;
        self.shares.clear();
        self.watching.lock().unwrap().clear();
        for (_, v) in self.viewers.drain() {
            Self::drop_viewer(v, emit);
        }
        self.incoming.abort();
    }

    // ---- voz ----

    fn join_voice(&mut self, session: &Session, core: &mut RoomCore, devices: bool, emit: &impl Fn(RoomEvent)) {
        if self.voice.is_some() {
            return;
        }
        if !core.in_group() {
            return emit(RoomEvent::Notice("A voz abre depois que você for admitido no grupo cifrado.".into()));
        }
        let v = match VoiceSession::start(session.connection(), self.keys.clone(), devices, self.voice_view.strong_ns, &core.audio_choice()) {
            Ok(v) => v,
            Err(e) => return emit(RoomEvent::Notice(format!("Não deu para abrir o áudio: {e:#}"))),
        };
        v.voice().set_muted(self.voice_view.muted);
        v.voice().set_deafened(self.voice_view.deafened);
        v.voice().set_push_to_talk(core.push_to_talk());
        if !devices {
            emit(RoomEvent::VoiceHandle(VoiceHandle(v.voice().clone())));
        }
        self.voice_view.active = true;
        self.voice_view.devices = v.devices().map(device_label);
        self.voice = Some(v);
        core.set_in_voice(true);
        emit(RoomEvent::Voice(self.voice_view.clone()));
    }

    fn leave_voice(&mut self, core: &mut RoomCore, emit: &impl Fn(RoomEvent), why: Option<String>) {
        let Some(v) = self.voice.take() else { return };
        let s = v.voice().stats();
        emit(RoomEvent::Notice(match why {
            Some(why) => format!("O áudio parou ({why}); você saiu da voz."),
            None => format!(
                "Você saiu da voz ({} pacotes enviados, {} recebidos, {} descartados).",
                s.sent, s.received, s.rejected
            ),
        }));
        core.set_in_voice(false);
        self.voice_view.active = false;
        self.voice_view.devices = None;
        emit(RoomEvent::Voice(self.voice_view.clone()));
    }

    fn mute(&mut self, on: bool, emit: &impl Fn(RoomEvent)) {
        self.voice_view.muted = on;
        if let Some(v) = &self.voice {
            v.voice().set_muted(on);
        }
        emit(RoomEvent::Voice(self.voice_view.clone()));
    }

    fn deafen(&mut self, on: bool, emit: &impl Fn(RoomEvent)) {
        self.voice_view.deafened = on;
        if let Some(v) = &self.voice {
            v.voice().set_deafened(on);
        }
        emit(RoomEvent::Voice(self.voice_view.clone()));
    }

    fn strong_ns(&mut self, on: bool, emit: &impl Fn(RoomEvent)) {
        self.voice_view.strong_ns = on;
        if let Some(d) = self.voice.as_ref().and_then(VoiceSession::devices) {
            d.set_strong_ns(on);
        }
        emit(RoomEvent::Voice(self.voice_view.clone()));
    }

    fn audio_devices(&mut self, core: &mut RoomCore, choice: lerri_media::device::DeviceChoice, data_dir: &Path, emit: &impl Fn(RoomEvent)) {
        core.set_audio_choice(&choice, data_dir);
        let Some(v) = &mut self.voice else { return };
        match v.restart_devices(self.voice_view.strong_ns, &choice) {
            Ok(()) => {
                self.voice_view.devices = v.devices().map(device_label);
                emit(RoomEvent::Voice(self.voice_view.clone()));
            }
            Err(e) => self.leave_voice(core, emit, Some(format!("{e:#}"))),
        }
    }

    fn push_to_talk(&mut self, core: &mut RoomCore, key: Option<u32>, data_dir: &Path, emit: &impl Fn(RoomEvent)) {
        core.set_push_to_talk(key, data_dir);
        if let Some(v) = &self.voice {
            v.voice().set_push_to_talk(key);
        }
        self.voice_view.push_to_talk = key.map(lerri_media::hotkey::key_name);
        emit(RoomEvent::Voice(self.voice_view.clone()));
    }

    // ---- vídeo ----

    /// Melhor codec que codificamos e que todos os outros decodificam
    /// (SPEC-04 §2). Sem nenhum em comum, o mais compatível: quem não o
    /// decodifica não consegue assistir.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn negotiated_codec(&self, core: &RoomCore) -> Option<Codec> {
        let caps = self.video_caps.as_ref()?;
        let peers = core.peer_decode_caps();
        lerri_core::media::choose_codec(&caps.encode, peers.iter().map(Vec::as_slice))
            .or_else(|| caps.encode.iter().copied().find(|c| *c == Codec::H264).or(caps.encode.first().copied()))
    }

    fn emit_view(&self, video: Video, emit: &impl Fn(RoomEvent)) {
        emit(RoomEvent::Stream { video, view: self.views.get(&video).cloned().unwrap_or_default() });
    }

    async fn publish(&mut self, session: &Session, core: &mut RoomCore, source: PublishSource, gpu: bool, emit: &impl Fn(RoomEvent)) {
        let video = source.video();
        if self.shares.contains_key(&video) {
            return;
        }
        if !core.in_group() {
            return emit(RoomEvent::Notice("O vídeo abre depois que você for admitido no grupo cifrado.".into()));
        }
        let out = crate::screen::Outgoing::start(session.connection(), self.keys.clone(), video);
        let (share, description) = if gpu {
            #[cfg(windows)]
            match self.start_gpu(core, out.clone(), source).await {
                Ok(g) => {
                    let d = g.description.clone();
                    (Share { _out: out, gpu: Some(g) }, d)
                }
                Err(e) => return emit(RoomEvent::Notice(format!("Não deu para publicar a {}: {e:#}", video.name()))),
            }
            #[cfg(not(windows))]
            {
                let _ = source;
                return emit(RoomEvent::Notice("Vídeo ainda é só no Windows.".into()));
            }
        } else {
            emit(RoomEvent::OutgoingHandle { video, handle: OutgoingHandle(out.clone()) });
            let share = Share {
                _out: out,
                #[cfg(windows)]
                gpu: None,
            };
            (share, "sem GPU (teste)".to_string())
        };
        self.shares.insert(video, share);
        self.views.insert(video, StreamView { sharing: Some(description), watchers: 0, low_watchers: 0 });
        core.set_publishing(video, true);
        self.emit_view(video, emit);
    }

    #[cfg(windows)]
    async fn start_gpu(
        &self,
        core: &RoomCore,
        out: Arc<crate::screen::Outgoing>,
        source: PublishSource,
    ) -> anyhow::Result<crate::video::GpuShare> {
        let caps = self.video_caps.as_ref().ok_or_else(|| anyhow::anyhow!("ainda verificando a GPU; tente de novo"))?;
        let codec = self.negotiated_codec(core).ok_or_else(|| anyhow::anyhow!("esta GPU não tem encoder de vídeo por hardware"))?;
        let source = match source {
            PublishSource::Screen { monitor } => crate::video::Source::Screen {
                monitor: monitor.or_else(|| caps.monitors.first().map(|m| m.handle)).ok_or_else(|| anyhow::anyhow!("nenhum monitor"))?,
            },
            PublishSource::Window { hwnd } => crate::video::Source::Window { hwnd },
            PublishSource::Camera { device } => {
                anyhow::ensure!(!caps.cameras.is_empty(), "nenhuma câmera encontrada");
                crate::video::Source::Camera { device }
            }
        };
        tokio::task::spawn_blocking(move || crate::video::GpuShare::start(out, source, codec)).await?
    }

    fn unpublish(&mut self, core: &mut RoomCore, video: Video, emit: &impl Fn(RoomEvent)) {
        if self.shares.remove(&video).is_none() {
            return;
        }
        core.set_publishing(video, false);
        self.views.remove(&video);
        self.emit_view(video, emit);
    }

    fn keyframe_requested(&mut self, video: Video, emit: &impl Fn(RoomEvent)) {
        let Some(share) = self.shares.get(&video) else { return };
        #[cfg(windows)]
        if let Some(g) = &share.gpu {
            return g.request_keyframe();
        }
        let _ = share;
        emit(RoomEvent::KeyframeRequested { video });
    }

    fn set_watchers(&mut self, video: Video, count: u32, low: u32, emit: &impl Fn(RoomEvent)) {
        #[cfg(windows)]
        if let Some(g) = self.shares.get(&video).and_then(|s| s.gpu.as_ref()) {
            g.set_watchers(count);
            g.set_low_layer(low > 0);
        }
        if let Some(view) = self.views.get_mut(&video)
            && (view.watchers, view.low_watchers) != (count, low)
        {
            (view.watchers, view.low_watchers) = (count, low);
            self.emit_view(video, emit);
        }
    }

    fn emit_watching(&self, emit: &impl Fn(RoomEvent)) {
        let mut list: Vec<(MemberId, Video)> = self.viewers.keys().copied().collect();
        list.sort();
        emit(RoomEvent::Watching(list));
    }

    fn watch(&mut self, core: &mut RoomCore, member: MemberId, video: Video, target: WatchTarget, emit: &impl Fn(RoomEvent)) {
        // Um de cada tipo por vez: troca o que já mostrava.
        let same_kind: Vec<MemberId> = self.viewers.keys().filter(|(_, v)| *v == video).map(|(m, _)| *m).collect();
        for m in same_kind {
            self.unwatch(core, m, video, emit);
        }
        let sink: crate::screen::Sink = match target {
            WatchTarget::Frames(sink) => {
                self.viewers.insert(
                    (member, video),
                    Viewer {
                        #[cfg(windows)]
                        gpu: None,
                        #[cfg(windows)]
                        report: None,
                    },
                );
                sink
            }
            #[cfg(windows)]
            target @ (WatchTarget::Window { .. } | WatchTarget::Headless) => {
                let window = match target {
                    WatchTarget::Window { hwnd, size } => Some((hwnd, size)),
                    _ => None,
                };
                match crate::video::GpuViewer::start(window) {
                    Ok(g) => {
                        let sink = g.sink();
                        let report = window.is_none().then(|| (std::time::Instant::now(), 0, 0));
                        self.viewers.insert((member, video), Viewer { gpu: Some(g), report });
                        sink
                    }
                    Err(e) => {
                        emit(RoomEvent::Notice(format!("Não deu para mostrar o vídeo: {e:#}")));
                        if let Some((hwnd, _)) = window {
                            emit(RoomEvent::VideoClosed { window: hwnd });
                        }
                        return;
                    }
                }
            }
            #[cfg(not(windows))]
            target => {
                emit(RoomEvent::Notice("Assistir vídeo ainda é só no Windows.".into()));
                if let WatchTarget::Window { hwnd, .. } = target {
                    emit(RoomEvent::VideoClosed { window: hwnd });
                }
                return;
            }
        };
        self.watching.lock().unwrap().insert((member, video), sink);
        core.out.push(Frame::Watch { member, video, on: true });
        self.emit_watching(emit);
    }

    fn unwatch(&mut self, core: &mut RoomCore, member: MemberId, video: Video, emit: &impl Fn(RoomEvent)) {
        let Some(v) = self.viewers.remove(&(member, video)) else { return };
        self.watching.lock().unwrap().remove(&(member, video));
        core.out.push(Frame::Watch { member, video, on: false });
        Self::drop_viewer(v, emit);
        self.emit_watching(emit);
    }

    /// Para a thread de vídeo e libera a janela para a interface destruir.
    fn drop_viewer(v: Viewer, emit: &impl Fn(RoomEvent)) {
        #[cfg(windows)]
        if let Some(g) = v.gpu {
            let window = g.window;
            drop(g);
            if let Some(window) = window {
                emit(RoomEvent::VideoClosed { window });
            }
        }
        #[cfg(not(windows))]
        let _ = (v, emit);
    }

    /// O relay para de mandar (e volta com keyframe); as threads de vídeo
    /// ficam, paradas, sem decodificar nada.
    fn pause(&mut self, core: &mut RoomCore, paused: bool) {
        for (member, video) in self.viewers.keys() {
            core.out.push(Frame::Watch { member: *member, video: *video, on: !paused });
        }
    }

    fn resize(&mut self, window: isize, size: (u32, u32)) {
        #[cfg(windows)]
        if let Some(g) = self.viewers.values().filter_map(|v| v.gpu.as_ref()).find(|g| g.window == Some(window)) {
            g.resize(size);
        }
        #[cfg(not(windows))]
        let _ = (window, size);
    }

    /// Sem janela (terminal): fps e banda decodificados a cada 5 s.
    #[cfg(windows)]
    fn report_headless(&mut self, emit: &impl Fn(RoomEvent)) {
        for ((_, video), v) in self.viewers.iter_mut() {
            let (Some(g), Some((at, frames, bytes))) = (&v.gpu, v.report.as_mut()) else { continue };
            let secs = at.elapsed().as_secs_f64();
            if secs < 5.0 {
                continue;
            }
            let (f, b) =
                (g.stats.frames.load(std::sync::atomic::Ordering::Relaxed), g.stats.bytes.load(std::sync::atomic::Ordering::Relaxed));
            emit(RoomEvent::Notice(format!(
                "Vídeo ({}): {:.1} quadros/s decodificados, {:.0} kbps.",
                video.name(),
                (f - *frames) as f64 / secs,
                (b - *bytes) as f64 * 8.0 / secs / 1000.0
            )));
            *at = std::time::Instant::now();
            *frames = f;
            *bytes = b;
        }
    }

    /// A cada 250 ms: quem fala, falhas de dispositivo, keyframes pedidos
    /// pelo vídeo e renegociação de codec.
    fn poll(&mut self, core: &mut RoomCore, emit: &impl Fn(RoomEvent)) {
        if let Some(v) = &self.voice {
            if let Some(err) = v.devices().and_then(|d| d.error()) {
                // Microfone ou fone desconectado: sai da voz em vez de ficar mudo sem aviso.
                self.leave_voice(core, emit, Some(err));
            } else {
                if let Some(d) = v.devices().filter(|d| d.take_switched()) {
                    // O padrão do Windows mudou (ou o fone saiu): seguiu sozinho.
                    let (input, output) = d.names();
                    emit(RoomEvent::Notice(format!("Áudio agora em {input} e {output}")));
                    self.voice_view.devices = Some(device_label(d));
                    emit(RoomEvent::Voice(self.voice_view.clone()));
                }
                let members = core.voice_members().clone();
                v.voice().retain(|m| members.contains(m));
                let mut speaking: BTreeSet<MemberId> = v.voice().speaking().into_iter().collect();
                if v.voice().talking() {
                    speaking.insert(core.me);
                }
                core.set_speaking(speaking);
            }
        }
        #[cfg(windows)]
        {
            self.report_headless(emit);
            let mut failed = Vec::new();
            for ((member, video), v) in &self.viewers {
                let Some(g) = &v.gpu else { continue };
                if let Some(err) = g.error() {
                    emit(RoomEvent::Notice(format!("O vídeo parou: {err}")));
                    failed.push((*member, *video));
                } else if g.take_keyframe_request() {
                    core.out.push(Frame::KeyframeRequest { member: *member, video: *video });
                }
            }
            for (member, video) in failed {
                self.unwatch(core, member, video, emit);
            }
            let codec = self.negotiated_codec(core);
            let mut stopped = Vec::new();
            for (video, share) in &self.shares {
                let Some(g) = &share.gpu else { continue };
                if let Some(c) = codec {
                    g.set_codec(c);
                }
                if let Some(err) = g.error() {
                    emit(RoomEvent::Notice(format!("A {} parou: {err}", video.name())));
                    stopped.push(*video);
                }
            }
            for video in stopped {
                self.unpublish(core, video, emit);
            }
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
