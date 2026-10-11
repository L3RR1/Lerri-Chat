//! Interface gráfica (SPEC-05, D-02: Slint com renderer software).
//!
//! A janela roda na thread principal; rede e salas rodam num runtime tokio à
//! parte. A sala ([`lerri_client::room::run`]) manda eventos para a janela
//! com `upgrade_in_event_loop` e recebe comandos por canal.

mod clipboard;
mod clock;
mod qr;
mod video;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use lerri_client::room::{self, ChatLine, Command, Exit, MemberView, OpenHow, PublishSource, RoomCore, RoomEvent, WatchTarget};
use lerri_core::media::Video;
use lerri_client::rooms::{self, SavedRoom};
use lerri_client::settings::{Settings, clean_nick};
use lerri_client::{ReachReport, Session};
use lerri_core::invite::{DEFAULT_INVITE_TTL_SECS, HostAddr, Invite, Link};
use lerri_core::protocol::{Role, RoomMode};
use lerri_core::{MemberId, unix_now};
use lerri_crypto::Identity;
use lerri_host::X1Host;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};
use tokio::sync::{mpsc, oneshot};

slint::include_modules!();

/// Linhas mantidas no registro de atividade.
const LOG_LINES: usize = 200;

enum Start {
    X1 { via: Option<String> },
    Join(String),
    CreateServer(String),
    Open(SavedRoom),
}

struct ActiveRoom {
    cmds: mpsc::UnboundedSender<Command>,
    done: oneshot::Receiver<()>,
}

struct App {
    rt: tokio::runtime::Handle,
    data_dir: PathBuf,
    identity: Arc<Identity>,
    ui: Weak<AppWindow>,
    room: RefCell<Option<ActiveRoom>>,
    saved: RefCell<Vec<SavedRoom>>,
    /// Membros mostrados agora (para o botão "Remover" achar o id).
    members: Arc<Mutex<Vec<MemberView>>>,
    /// O que estamos assistindo (para os botões "Fechar" acharem o membro).
    watching: Arc<Mutex<Vec<(MemberId, Video)>>>,
    /// Microfones e saídas mostrados no painel "Voz e áudio" (sem o "Padrão").
    audio: Arc<Mutex<(Vec<String>, Vec<String>)>>,
}

/// Abre a janela e só volta quando ela for fechada. `initial_link`: convite
/// recebido pelo handler `lerrichat://`, para entrar direto.
pub fn run(data_dir: PathBuf, initial_link: Option<String>) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let (identity, _) = Identity::load_or_create(&data_dir)?;
    let ui = AppWindow::new()?;
    ui.set_my_id(identity.member_id().short().into());
    ui.set_version(env!("CARGO_PKG_VERSION").into());
    let settings = Settings::load(&data_dir);
    ui.set_nickname(settings.nickname.into());
    ui.set_keep_history(settings.keep_history);
    ui.set_voice_strong_ns(settings.strong_ns);

    let app = Rc::new(App {
        rt: rt.handle().clone(),
        data_dir,
        identity: Arc::new(identity),
        ui: ui.as_weak(),
        room: RefCell::new(None),
        saved: RefCell::new(Vec::new()),
        members: Arc::default(),
        watching: Arc::default(),
        audio: Arc::default(),
    });
    app.refresh_saved();

    let a = app.clone();
    ui.on_join(move || {
        let link = a.ui().get_join_link().trim().to_string();
        a.start(Start::Join(link), "Conectando…");
    });
    let a = app.clone();
    ui.on_create_x1(move || {
        let via = a.ui().get_via_link().trim().to_string();
        a.start(Start::X1 { via: (!via.is_empty()).then_some(via) }, "Procurando endereços (roteador, IPv6)…");
    });
    let a = app.clone();
    ui.on_create_server(move || {
        let link = a.ui().get_admin_link().trim().to_string();
        a.start(Start::CreateServer(link), "Criando a sala no servidor…");
    });
    let a = app.clone();
    ui.on_open_room(move |i| {
        let saved = a.saved.borrow().get(i as usize).cloned();
        if let Some(room) = saved {
            a.start(Start::Open(room), "Conectando…");
        }
    });
    let a = app.clone();
    ui.on_save_settings(move || a.save_settings());
    let a = app.clone();
    ui.on_new_invite(move || a.send(Command::NewInvite));
    let a = app.clone();
    ui.on_send_chat(move || {
        let ui = a.ui();
        let text = ui.get_chat_draft().to_string();
        if !text.trim().is_empty() {
            a.send(Command::Send(text));
            ui.set_chat_draft("".into());
        }
    });
    let a = app.clone();
    ui.on_remove_member(move |i| {
        let id = a.members.lock().unwrap().get(i as usize).map(|m| m.id);
        if let Some(id) = id {
            a.send(Command::Remove(id));
        }
    });
    let a = app.clone();
    ui.on_disconnect(move || a.send(Command::Close));
    let a = app.clone();
    ui.on_leave_room(move || a.send(Command::LeaveRoom));
    let a = app.clone();
    ui.on_join_voice(move || a.send(Command::JoinVoice { devices: true }));
    let a = app.clone();
    ui.on_leave_voice(move || a.send(Command::LeaveVoice));
    let a = app.clone();
    ui.on_toggle_mute(move || {
        let muted = a.ui().get_voice_muted();
        a.send(Command::Mute(!muted));
    });
    let a = app.clone();
    ui.on_toggle_deafen(move || {
        let deaf = a.ui().get_voice_deafened();
        a.send(Command::Deafen(!deaf));
    });
    let a = app.clone();
    ui.on_strong_ns_changed(move || {
        let on = a.ui().get_voice_strong_ns();
        a.send(Command::StrongNoiseSuppression(on));
        a.save_settings();
    });
    let a = app.clone();
    ui.on_share_menu_opened(move || refresh_share_targets(&a.ui()));
    let a = app.clone();
    ui.on_audio_settings_opened(move || a.refresh_audio_devices());
    let a = app.clone();
    ui.on_input_changed(move |_| a.send_audio_choice());
    let a = app.clone();
    ui.on_output_changed(move |_| a.send_audio_choice());
    let a = app.clone();
    ui.on_ptt_mode(move |on| {
        if !on {
            a.send(Command::PushToTalk(None));
        } else if let Some(key) = Settings::load(&a.data_dir).push_to_talk_key {
            a.send(Command::PushToTalk(Some(key)));
        } else {
            a.capture_ptt_key();
        }
    });
    let a = app.clone();
    ui.on_change_ptt_key(move || a.capture_ptt_key());
    let a = app.clone();
    ui.on_share_screen(move || {
        let source = match video::share_target(a.ui().get_monitor_index().max(0) as usize) {
            video::ShareTarget::Monitor(monitor) => PublishSource::Screen { monitor },
            video::ShareTarget::Window(hwnd) => PublishSource::Window { hwnd },
        };
        a.send(Command::Publish { source, gpu: true });
    });
    let a = app.clone();
    ui.on_stop_screen(move || a.send(Command::Unpublish(Video::Screen)));
    let a = app.clone();
    ui.on_start_camera(move || {
        let device = video::camera(a.ui().get_camera_index().max(0) as usize);
        a.send(Command::Publish { source: PublishSource::Camera { device }, gpu: true });
    });
    let a = app.clone();
    ui.on_stop_camera(move || a.send(Command::Unpublish(Video::Camera)));
    let a = app.clone();
    ui.on_watch_member(move |i| a.watch(i as usize, Video::Screen));
    let a = app.clone();
    ui.on_watch_camera(move |i| a.watch(i as usize, Video::Camera));
    let a = app.clone();
    ui.on_unwatch(move || a.unwatch(Video::Screen));
    let a = app.clone();
    ui.on_unwatch_camera(move || a.unwatch(Video::Camera));
    // As janelas-filhas de vídeo acompanham suas áreas na tela da sala.
    let a = app.clone();
    let follow = slint::Timer::default();
    follow.start(slint::TimerMode::Repeated, Duration::from_millis(150), move || {
        let ui = a.ui();
        // Medido aqui e não no .slint: lá a largura entra no próprio layout (loop).
        let window = ui.window();
        ui.set_narrow((window.size().width as f32 / window.scale_factor()) < 1000.0);
        ui.set_reduce_motion(!animations_enabled());
        for change in video::follow(&ui) {
            match change {
                video::Change::Resized { window, size } => a.send(Command::VideoResize { window, size }),
                video::Change::Minimized(min) => a.send(Command::PauseVideo(min)),
            }
        }
    });
    let a = app.clone();
    ui.on_copy_invite(move || {
        let ui = a.ui();
        let (status, ok) = match clipboard::set_text(&ui.get_invite_link()) {
            Ok(()) => ("Convite copiado".into(), true),
            Err(e) => (format!("Não deu para copiar: {e}"), false),
        };
        ui.set_copy_status(status.into());
        ui.set_copy_ok(ok);
    });

    if let Some(link) = initial_link {
        ui.set_join_link(link.clone().into());
        app.start(Start::Join(link), "Conectando…");
    }

    ui.run()?;

    // Janela fechada: desconecta com educação antes de derrubar o runtime.
    if let Some(room) = app.room.borrow_mut().take() {
        let _ = room.cmds.send(Command::Close);
        let _ = rt.block_on(async { tokio::time::timeout(Duration::from_secs(4), room.done).await });
    }
    // Os callbacks da janela seguram o `App`, então o runtime não seria
    // derrubado sozinho; com threads de rede vivas, a saída do processo
    // quebrava na limpeza das threads.
    rt.shutdown_timeout(Duration::from_secs(2));
    Ok(())
}

impl App {
    fn ui(&self) -> AppWindow {
        self.ui.upgrade().expect("janela existe enquanto o event loop roda")
    }

    /// Assiste à tela ou à câmera do membro `i` da lista: mostra a área no
    /// painel, espera o layout e põe a janela-filha em cima dela.
    fn watch(self: &Rc<Self>, i: usize, video: Video) {
        let Some(m) = self.members.lock().unwrap().get(i).cloned() else { return };
        // Se já mostrava outro do mesmo tipo, a sala troca e devolve a janela
        // antiga (VideoClosed), que não é mais a registrada para o tipo.
        set_watching_label(&self.ui(), video, m.label());
        let app = self.clone();
        slint::Timer::single_shot(Duration::from_millis(80), move || {
            let ui = app.ui();
            match video::open(&ui, video) {
                Ok((hwnd, size)) => {
                    app.send(Command::Watch { member: m.id, video, target: WatchTarget::Window { hwnd, size } })
                }
                Err(e) => {
                    set_watching_label(&ui, video, String::new());
                    ui.set_error_text(format!("não deu para abrir o vídeo: {e:#}").into());
                }
            }
        });
    }

    /// Lista microfones e saídas fora da thread da interface (o WASAPI leva
    /// até ~1 s) e marca os escolhidos.
    fn refresh_audio_devices(&self) {
        let (weak, audio, data_dir) = (self.ui.clone(), self.audio.clone(), self.data_dir.clone());
        std::thread::spawn(move || {
            let (inputs, outputs) = lerri_media::device::list();
            let settings = Settings::load(&data_dir);
            let index = |list: &[String], want: &Option<String>| {
                want.as_ref().and_then(|w| list.iter().position(|n| n == w)).map_or(0, |i| i as i32 + 1)
            };
            let (ii, oi) = (index(&inputs, &settings.input_device), index(&outputs, &settings.output_device));
            let with_default = |list: &[String]| {
                let mut v: Vec<SharedString> = vec!["Padrão do Windows".into()];
                v.extend(list.iter().map(SharedString::from));
                v
            };
            let (im, om) = (with_default(&inputs), with_default(&outputs));
            *audio.lock().unwrap() = (inputs, outputs);
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_audio_inputs(ModelRc::new(VecModel::from(im)));
                ui.set_audio_outputs(ModelRc::new(VecModel::from(om)));
                ui.set_input_index(ii);
                ui.set_output_index(oi);
            });
        });
    }

    fn send_audio_choice(&self) {
        let ui = self.ui();
        let (inputs, outputs) = self.audio.lock().unwrap().clone();
        let pick = |list: &[String], i: i32| (i > 0).then(|| list.get(i as usize - 1).cloned()).flatten();
        self.send(Command::AudioDevices(lerri_media::device::DeviceChoice {
            input: pick(&inputs, ui.get_input_index()),
            output: pick(&outputs, ui.get_output_index()),
        }));
    }

    /// Grava a próxima tecla apertada (até 10 s; Esc cancela) como a de
    /// apertar para falar.
    fn capture_ptt_key(&self) {
        let Some(cmds) = self.room.borrow().as_ref().map(|r| r.cmds.clone()) else { return };
        let weak = self.ui.clone();
        self.ui().set_ptt_capturing(true);
        std::thread::spawn(move || {
            let key = lerri_media::hotkey::capture(Duration::from_secs(10));
            if let Some(key) = key {
                let _ = cmds.send(Command::PushToTalk(Some(key)));
            }
            let _ = weak.upgrade_in_event_loop(|ui| ui.set_ptt_capturing(false));
        });
    }

    fn unwatch(&self, video: Video) {
        let member = self.watching.lock().unwrap().iter().find(|(_, v)| *v == video).map(|(m, _)| *m);
        if let Some(member) = member {
            self.send(Command::Unwatch { member, video });
        }
    }

    fn send(&self, cmd: Command) {
        if let Some(room) = self.room.borrow().as_ref() {
            let _ = room.cmds.send(cmd);
        }
    }

    fn save_settings(&self) {
        let ui = self.ui();
        let nick = clean_nick(&ui.get_nickname());
        ui.set_nickname(nick.clone().into());
        // Lê antes: a sala grava microfone, saída e tecla no mesmo arquivo.
        let mut settings = Settings::load(&self.data_dir);
        settings.nickname = nick;
        settings.keep_history = ui.get_keep_history();
        settings.strong_ns = ui.get_voice_strong_ns();
        if let Err(e) = settings.save(&self.data_dir) {
            ui.set_error_text(format!("não deu para salvar as preferências: {e:#}").into());
        }
    }

    fn refresh_saved(&self) {
        let saved = rooms::load(&self.data_dir).unwrap_or_else(|e| {
            self.ui().set_error_text(format!("não deu para ler as salas salvas: {e:#}").into());
            Vec::new()
        });
        let rows: Vec<SavedRoomRow> = saved
            .iter()
            .map(|r| SavedRoomRow {
                id: r.room.to_string()[..12].into(),
                label: if r.role == Role::Owner { "dono".into() } else { "membro".into() },
            })
            .collect();
        self.ui().set_saved_rooms(ModelRc::new(VecModel::from(rows)));
        *self.saved.borrow_mut() = saved;
    }

    fn start(self: &Rc<Self>, start: Start, busy: &str) {
        if self.room.borrow().is_some() {
            return;
        }
        self.save_settings();
        let ui = self.ui();
        ui.set_error_text("".into());
        ui.set_busy_text(busy.into());
        ui.set_screen("busy".into());

        let (cmds, rx) = mpsc::unbounded_channel();
        let (done_tx, done) = oneshot::channel();
        *self.room.borrow_mut() = Some(ActiveRoom { cmds, done });

        let (weak, identity, data_dir, members, watching) = (
            self.ui.clone(),
            self.identity.clone(),
            self.data_dir.clone(),
            self.members.clone(),
            self.watching.clone(),
        );
        self.rt.spawn(async move {
            let error = match open_room(start, &identity, &data_dir, &weak).await {
                Ok(ctx) => match run_room(ctx, rx, &weak, &data_dir, members, watching).await {
                    Exit::Closed | Exit::Left => None,
                    Exit::Removed => Some("Você foi removido da sala pelo dono.".to_string()),
                    Exit::Disconnected(reason) => Some(disconnect_reason(&reason)),
                },
                Err(e) => Some(explain(&e)),
            };
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_screen("home".into());
                ui.set_voice_active(false);
                ui.set_voice_devices("".into());
                ui.set_screen_sharing(false);
                ui.set_screen_info("".into());
                ui.set_watching_label("".into());
                ui.set_camera_on(false);
                ui.set_camera_info("".into());
                ui.set_watching_cam_label("".into());
                if let Some(e) = error {
                    ui.set_error_text(e.into());
                }
            });
            let _ = done_tx.send(());
        });

        // Quando a tarefa terminar, libera o slot e atualiza a lista de salas.
        let app = self.clone();
        slint::Timer::single_shot(Duration::from_millis(200), move || app.poll_finished());
    }

    fn poll_finished(self: Rc<Self>) {
        let finished = match self.room.borrow_mut().as_mut() {
            Some(room) => !matches!(room.done.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
            None => return,
        };
        if finished {
            self.room.borrow_mut().take();
            self.refresh_saved();
        } else {
            slint::Timer::single_shot(Duration::from_millis(200), move || self.poll_finished());
        }
    }
}

fn disconnect_reason(reason: &str) -> String {
    if reason.contains("closed by peer") || reason.contains("encerrou") {
        "Você saiu da sala: o host encerrou a conexão (numa sala x1, quem criou fechou o app).".into()
    } else {
        format!("Você saiu da sala: conexão perdida ({reason}).")
    }
}

struct RoomCtx {
    session: Session,
    core: RoomCore,
    x1: Option<X1Host>,
    /// Endereço que vai nos convites (só o dono emite).
    invite_host: Option<HostAddr>,
    first_invite: Option<Invite>,
    reach: Option<ReachReport>,
}

async fn open_room(start: Start, identity: &Identity, data_dir: &std::path::Path, ui: &Weak<AppWindow>) -> anyhow::Result<RoomCtx> {
    let busy = |text: &'static str| {
        let _ = ui.upgrade_in_event_loop(move |ui| ui.set_busy_text(text.into()));
    };
    let mut ctx = match start {
        Start::Join(link) => {
            let Link::Join(invite) = Link::parse(&link).context("link inválido")? else {
                bail!("este link não é um convite de sala")
            };
            if invite.expires_at <= unix_now() {
                bail!("este convite expirou; peça outro a quem convidou");
            }
            let how = OpenHow::Join { room: invite.room, token: invite.token, secret: invite.secret.clone() };
            let (session, core) = room::open(identity, data_dir, &invite.host, how).await?;
            if session.mode == RoomMode::Server {
                rooms::remember(data_dir, saved(&session, &invite.host))?;
            }
            RoomCtx { session, core, x1: None, invite_host: None, first_invite: None, reach: None }
        }
        Start::Open(room) => {
            let how = OpenHow::Rejoin { room: room.room, mode: room.mode };
            let (session, core) = room::open(identity, data_dir, &room.host, how).await?;
            let invite_host = (session.role == Role::Owner).then_some(room.host);
            RoomCtx { session, core, x1: None, invite_host, first_invite: None, reach: None }
        }
        Start::CreateServer(link) => {
            let Link::Relay(relay) = Link::parse(&link).context("link inválido")? else {
                bail!("use o link de administração do relay")
            };
            let admin = relay.admin.context("este é o link público do relay; criar sala exige o link de administração")?;
            let how = OpenHow::Create { admin, mode: RoomMode::Server };
            let (session, core) = room::open(identity, data_dir, &relay.host, how).await?;
            rooms::remember(data_dir, saved(&session, &relay.host))?;
            RoomCtx { session, core, x1: None, invite_host: Some(relay.host), first_invite: None, reach: None }
        }
        Start::X1 { via } => {
            let meet = match via {
                Some(link) => {
                    let Link::Relay(relay) = Link::parse(&link).context("link do relay inválido")? else {
                        bail!("o ponto de encontro espera o link público de um relay (lerrichat://relay…)")
                    };
                    Some(relay.host.meet.context("esse relay não tem ponto de encontro")?)
                }
                None => None,
            };
            let assisted = meet.is_some();
            let host = X1Host::start(meet, 0).await?;
            let (public, classified) = host.host_addr().await;
            busy("Criando a sala…");
            let how = OpenHow::Create { admin: host.admin(), mode: RoomMode::X1 };
            let (session, core) = room::open(identity, data_dir, &host.loopback_addr(), how).await?;
            RoomCtx {
                session,
                core,
                x1: Some(host),
                invite_host: Some(public),
                first_invite: None,
                reach: Some(lerri_client::reach_report(&classified, assisted)),
            }
        }
    };
    // Quem cria a sala já sai com um convite na tela.
    if ctx.core.is_owner()
        && ctx.core.epoch() == 0
        && let Some(host) = &ctx.invite_host
    {
        let inv = ctx.session.issue_invite(host, DEFAULT_INVITE_TTL_SECS).await?;
        ctx.core.share_invite(&inv);
        ctx.first_invite = Some(inv);
    }
    Ok(ctx)
}

fn saved(session: &Session, host: &HostAddr) -> SavedRoom {
    SavedRoom { room: session.room, host: host.clone(), mode: session.mode, role: session.role }
}

/// O que a janela mostra da sala, montado a partir dos eventos.
#[derive(Default)]
struct View {
    me: Option<MemberId>,
    members: Vec<MemberView>,
    /// Telas e câmeras assistidas agora.
    watching: Vec<(MemberId, Video)>,
    chat: Vec<ChatLine>,
    log: Vec<SharedString>,
}

impl View {
    fn name(&self, id: MemberId) -> String {
        if Some(id) == self.me {
            return "você".into();
        }
        self.members.iter().find(|m| m.id == id).map_or_else(|| format!("{}…", &id.short()[..8]), MemberView::label)
    }

    /// Inicial para o avatar (a do apelido, ou do id).
    fn initial(&self, id: MemberId) -> String {
        let label = self.members.iter().find(|m| m.id == id).map_or_else(|| id.short(), MemberView::label);
        initial(&label)
    }
}

fn initial(label: &str) -> String {
    label.chars().find(|c| c.is_alphanumeric()).map_or_else(|| "?".into(), |c| c.to_uppercase().collect())
}

/// Mensagens seguidas da mesma pessoa, com menos de 5 min entre elas,
/// aparecem juntas, sob um só nome.
const CHAT_GROUP_SECS: u64 = 300;

async fn run_room(
    ctx: RoomCtx,
    cmds: mpsc::UnboundedReceiver<Command>,
    ui: &Weak<AppWindow>,
    data_dir: &std::path::Path,
    shared_members: Arc<Mutex<Vec<MemberView>>>,
    shared_watching: Arc<Mutex<Vec<(MemberId, Video)>>>,
) -> Exit {
    let RoomCtx { session, core, x1, invite_host, first_invite, reach } = ctx;
    {
        let room_id = session.room.to_string();
        let is_x1 = session.mode == RoomMode::X1;
        let kind = if is_x1 { "Sala x1" } else { "Sala no servidor" };
        let is_owner = core.is_owner();
        let info = if is_owner { "Você é o dono" } else { "Você é membro" };
        let has_invite_host = invite_host.is_some();
        let (reach_text, reach_note, reach_warning) = match &reach {
            Some(r) => (r.lines.join("\n"), r.note.clone().unwrap_or_default(), r.warning),
            None => (String::new(), String::new(), false),
        };
        let show_invite = first_invite.is_some();
        let _ = ui.upgrade_in_event_loop(move |ui| {
            ui.set_room_id(room_id[..12].into());
            ui.set_room_kind(kind.into());
            ui.set_room_info(info.into());
            ui.set_is_x1(is_x1);
            ui.set_is_owner(is_owner && has_invite_host);
            ui.set_reach_text(reach_text.into());
            ui.set_reach_note(reach_note.into());
            ui.set_reach_warning(reach_warning);
            ui.set_invite_link("".into());
            ui.set_copy_status("".into());
            ui.set_show_invite(show_invite);
            ui.set_room_status("".into());
            ui.set_join_link("".into());
            ui.set_chat(ModelRc::new(VecModel::from(Vec::<ChatRow>::new())));
            ui.set_screen("room".into());
        });
    }
    if let Some(inv) = &first_invite {
        show_invite(ui, inv);
    }

    let view = Arc::new(Mutex::new(View { me: Some(core.me), log: vec!["Você entrou na sala.".into()], ..View::default() }));
    let (w1, w2, v) = (ui.clone(), ui.clone(), view.clone());
    let emit = move |ev: RoomEvent| {
        let mut v = v.lock().unwrap();
        match ev {
            RoomEvent::Status(s) => {
                let text = s.unwrap_or_default();
                let _ = w1.upgrade_in_event_loop(move |ui| ui.set_room_status(text.into()));
                return;
            }
            RoomEvent::Invite(inv) => return show_invite(&w1, &inv),
            RoomEvent::Voice(voice) => {
                let _ = w1.upgrade_in_event_loop(move |ui| {
                    ui.set_voice_active(voice.active);
                    ui.set_voice_muted(voice.muted);
                    ui.set_voice_deafened(voice.deafened);
                    ui.set_voice_strong_ns(voice.strong_ns);
                    ui.set_voice_devices(voice.devices.unwrap_or_default().into());
                    ui.set_ptt_key(voice.push_to_talk.unwrap_or_default().into());
                });
                return;
            }
            // Só para testes (mídia sem dispositivos).
            RoomEvent::VoiceHandle(_) | RoomEvent::OutgoingHandle { .. } | RoomEvent::KeyframeRequested { .. } => return,
            RoomEvent::Video(info) => {
                let _ = w1.upgrade_in_event_loop(move |ui| {
                    video::set_devices(
                        info.monitors.iter().map(|(h, _)| *h).collect(),
                        info.cameras.iter().map(|(id, _)| id.clone()).collect(),
                    );
                    let names: Vec<SharedString> = info.monitors.iter().map(|(_, n)| format!("Tela inteira: {n}").into()).collect();
                    ui.set_monitor_count(names.len() as i32);
                    ui.set_monitors(ModelRc::new(VecModel::from(names)));
                    ui.set_monitor_index(0);
                    video::set_windows(Vec::new());
                    let cams: Vec<SharedString> = info.cameras.iter().map(|(_, n)| n.clone().into()).collect();
                    ui.set_cameras(ModelRc::new(VecModel::from(cams)));
                    ui.set_camera_index(0);
                    ui.set_can_share(info.can_encode && !info.monitors.is_empty());
                    ui.set_watch_problem(info.watch_problem.unwrap_or_default().into());
                });
                return;
            }
            RoomEvent::VideoClosed { window } => {
                let _ = w1.upgrade_in_event_loop(move |ui| {
                    if let Some(video) = video::closed(window) {
                        set_watching_label(&ui, video, String::new());
                    }
                });
                return;
            }
            RoomEvent::Stream { video, view } => {
                let on = view.sharing.is_some();
                let info = view.sharing.unwrap_or_default();
                let watchers = view.watchers as i32;
                let _ = w1.upgrade_in_event_loop(move |ui| match video {
                    Video::Screen => {
                        ui.set_screen_sharing(on);
                        ui.set_screen_info(info.into());
                        ui.set_screen_watchers(watchers);
                    }
                    Video::Camera => {
                        ui.set_camera_on(on);
                        ui.set_camera_info(info.into());
                        ui.set_camera_watchers(watchers);
                    }
                });
                return;
            }
            RoomEvent::Watching(list) => {
                *shared_watching.lock().unwrap() = list.clone();
                v.watching = list;
            }
            RoomEvent::Members(m) => {
                *shared_members.lock().unwrap() = m.clone();
                v.members = m;
            }
            RoomEvent::Chat(line) => v.chat.push(line),
            RoomEvent::History(lines) => v.chat = lines,
            RoomEvent::Notice(text) => {
                v.log.push(text.into());
                if v.log.len() > LOG_LINES {
                    v.log.remove(0);
                }
            }
        }
        push_view(&w1, &v);
    };
    let route = move |r: Option<lerri_client::Route>| {
        let (text, quality) = match r {
            Some(r) => {
                let kind = if r.direct { "direto" } else { "pelo ponto de encontro" };
                (format!("Caminho {kind} · {:.0} ms", r.rtt.as_secs_f64() * 1000.0), route_quality(&r))
            }
            None => (String::new(), 0),
        };
        let _ = w2.upgrade_in_event_loop(move |ui| {
            ui.set_route_text(text.into());
            ui.set_route_quality(quality);
        });
    };
    let exit = room::run(session, core, data_dir.to_path_buf(), invite_host, cmds, emit, route).await;
    if let Some(host) = x1 {
        host.shutdown().await;
    }
    exit
}

/// Cor do indicador de rede (SPEC-05 §5): 0 bom, 1 atenção, 2 ruim. Voz
/// passa de 150 ms boca-a-ouvido com ~300 ms de ida e volta.
fn route_quality(r: &lerri_client::Route) -> i32 {
    let ms = r.rtt.as_millis();
    if ms >= 300 {
        2
    } else if ms >= 120 || !r.direct {
        1
    } else {
        0
    }
}

fn push_view(ui: &Weak<AppWindow>, v: &View) {
    let offset = clock::utc_offset_secs();
    let chat: Vec<ChatRow> = v
        .chat
        .iter()
        .enumerate()
        .map(|(i, l)| ChatRow {
            who: v.name(l.from).into(),
            initial: v.initial(l.from).into(),
            text: l.text.clone().into(),
            time: clock::hh_mm(l.at, offset).into(),
            mine: Some(l.from) == v.me,
            first: i.checked_sub(1).map(|p| &v.chat[p]).is_none_or(|p| p.from != l.from || l.at.saturating_sub(p.at) > CHAT_GROUP_SECS),
        })
        .collect();
    let members: Vec<MemberRow> = v
        .members
        .iter()
        .map(|m| MemberRow {
            label: if m.me { format!("{} (você)", m.label()).into() } else { m.label().into() },
            initial: initial(&m.label()).into(),
            online: m.online,
            owner: m.owner,
            me: m.me,
            waiting: m.waiting,
            in_voice: m.in_voice,
            speaking: m.speaking,
            sharing: m.sharing,
            watching: v.watching.contains(&(m.id, Video::Screen)),
            camera: m.camera,
            watching_camera: v.watching.contains(&(m.id, Video::Camera)),
            latency: m.latency_ms.map_or(-1, |ms| ms.min(9999) as i32),
        })
        .collect();
    // Mais recente primeiro: o quadro de atividade é pequeno.
    let log: Vec<SharedString> = v.log.iter().rev().cloned().collect();
    let _ = ui.upgrade_in_event_loop(move |ui| {
        let grew = ui.get_chat().row_count() != chat.len();
        ui.set_chat(ModelRc::new(VecModel::from(chat)));
        ui.set_anyone_speaking(members.iter().any(|m| m.speaking));
        ui.set_me_speaking(members.iter().any(|m| m.me && m.speaking));
        ui.set_members(ModelRc::new(VecModel::from(members)));
        ui.set_log(ModelRc::new(VecModel::from(log)));
        if grew {
            // Rola para o fim depois do layout novo.
            let weak = ui.as_weak();
            slint::Timer::single_shot(Duration::from_millis(30), move || {
                if let Some(ui) = weak.upgrade() {
                    ui.invoke_scroll_chat_to_end();
                }
            });
        }
    });
}

/// Janelas abertas no menu "O que compartilhar", depois das telas inteiras.
/// Refeito a cada abertura do menu (janelas abrem e fecham); a escolha fica
/// se a janela ainda existe.
fn refresh_share_targets(ui: &AppWindow) {
    /// Mais que isso não cabe no menu, que abre para cima a partir da barra.
    const MAX_WINDOWS: usize = 12;
    let monitors = ui.get_monitor_count().max(0) as usize;
    let mut names: Vec<SharedString> = ui.get_monitors().iter().take(monitors).collect();
    let previous = match video::share_target(ui.get_monitor_index().max(0) as usize) {
        video::ShareTarget::Window(h) => Some(h),
        video::ShareTarget::Monitor(_) => None,
    };
    let windows: Vec<_> = lerri_media::video::capture::windows().into_iter().take(MAX_WINDOWS).collect();
    names.extend(windows.iter().map(|w| SharedString::from(format!("Janela: {}", w.title))));
    let keep = previous.and_then(|h| windows.iter().position(|w| w.handle == h));
    video::set_windows(windows.iter().map(|w| w.handle).collect());
    ui.set_monitors(ModelRc::new(VecModel::from(names)));
    match keep {
        Some(i) => ui.set_monitor_index((monitors + i) as i32),
        None if previous.is_some() => ui.set_monitor_index(0),
        None => {}
    }
}

/// Nome no topo da área de vídeo do tipo; vazio esconde a área.
fn set_watching_label(ui: &AppWindow, video: Video, label: String) {
    match video {
        Video::Screen => ui.set_watching_label(label.into()),
        Video::Camera => ui.set_watching_cam_label(label.into()),
    }
}

fn show_invite(ui: &Weak<AppWindow>, invite: &Invite) {
    let link = invite.to_link();
    let pixels = qr::pixels(&link);
    let _ = ui.upgrade_in_event_loop(move |ui| {
        ui.set_invite_link(link.into());
        ui.set_copy_status("".into());
        ui.set_show_invite(true);
        if let Some(px) = pixels {
            ui.set_invite_qr(slint::Image::from_rgb8(px));
        }
    });
}

/// Mensagem de erro para o usuário: recusas do relay em português claro.
fn explain(e: &anyhow::Error) -> String {
    match lerri_client::refusal(e) {
        Some(r) => format!("O host recusou: {r}."),
        None => format!("{e:#}"),
    }
}

use slint::Model as _;

/// "Efeitos de animação" do Windows (Acessibilidade > Efeitos visuais).
#[cfg(windows)]
fn animations_enabled() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SPI_GETCLIENTAREAANIMATION, SystemParametersInfoW};
    let mut on: i32 = 1;
    // SAFETY: SPI_GETCLIENTAREAANIMATION escreve um BOOL no ponteiro dado.
    let ok = unsafe { SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, (&raw mut on).cast(), 0) };
    ok == 0 || on != 0
}

#[cfg(not(windows))]
fn animations_enabled() -> bool {
    true
}
