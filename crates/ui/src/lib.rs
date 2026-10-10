//! Interface gráfica (SPEC-05, D-02: Slint com renderer software).
//!
//! A janela roda na thread principal; rede e salas rodam num runtime tokio à
//! parte. A tarefa da sala manda atualizações para a janela com
//! `upgrade_in_event_loop` e recebe comandos (novo convite, sair) por canal.

mod clipboard;
mod qr;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use lerri_client::rooms::{self, SavedRoom};
use lerri_client::{Event, Session};
use lerri_core::invite::{DEFAULT_INVITE_TTL_SECS, HostAddr, Invite, Link};
use lerri_core::protocol::{Auth, Role, RoomMode};
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

enum RoomCmd {
    NewInvite,
    Leave,
}

struct ActiveRoom {
    cmds: mpsc::UnboundedSender<RoomCmd>,
    done: oneshot::Receiver<()>,
}

struct App {
    rt: tokio::runtime::Handle,
    data_dir: PathBuf,
    identity: Arc<Identity>,
    ui: Weak<AppWindow>,
    room: RefCell<Option<ActiveRoom>>,
    saved: RefCell<Vec<SavedRoom>>,
}

/// Abre a janela e só volta quando ela for fechada. `initial_link`: convite
/// recebido pelo handler `lerrichat://`, para entrar direto.
pub fn run(data_dir: PathBuf, initial_link: Option<String>) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let (identity, _) = Identity::load_or_create(&data_dir)?;
    let ui = AppWindow::new()?;
    ui.set_my_id(identity.member_id().short().into());

    let app = Rc::new(App {
        rt: rt.handle().clone(),
        data_dir,
        identity: Arc::new(identity),
        ui: ui.as_weak(),
        room: RefCell::new(None),
        saved: RefCell::new(Vec::new()),
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
    ui.on_new_invite(move || a.send(RoomCmd::NewInvite));
    let a = app.clone();
    ui.on_leave(move || a.send(RoomCmd::Leave));
    let a = app.clone();
    ui.on_copy_invite(move || {
        let ui = a.ui();
        let status = match clipboard::set_text(&ui.get_invite_link()) {
            Ok(()) => "copiado".into(),
            Err(e) => format!("não deu para copiar: {e}"),
        };
        ui.set_copy_status(status.into());
    });

    if let Some(link) = initial_link {
        ui.set_join_link(link.clone().into());
        app.start(Start::Join(link), "Conectando…");
    }

    ui.run()?;

    // Janela fechada: sai da sala com educação antes de derrubar o runtime.
    if let Some(room) = app.room.borrow_mut().take() {
        let _ = room.cmds.send(RoomCmd::Leave);
        let _ = rt.block_on(async { tokio::time::timeout(Duration::from_secs(3), room.done).await });
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

    fn send(&self, cmd: RoomCmd) {
        if let Some(room) = self.room.borrow().as_ref() {
            let _ = room.cmds.send(cmd);
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
        let ui = self.ui();
        ui.set_error_text("".into());
        ui.set_busy_text(busy.into());
        ui.set_screen("busy".into());

        let (cmds, rx) = mpsc::unbounded_channel();
        let (done_tx, done) = oneshot::channel();
        *self.room.borrow_mut() = Some(ActiveRoom { cmds, done });

        let (weak, identity, data_dir) = (self.ui.clone(), self.identity.clone(), self.data_dir.clone());
        self.rt.spawn(async move {
            let result = match open_room(start, &identity, &data_dir, &weak).await {
                Ok(ctx) => room_task(ctx, rx, &weak).await,
                Err(e) => Err(e),
            };
            let error = result.err().map(|e| explain(&e));
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_screen("home".into());
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

struct RoomCtx {
    session: Session,
    x1: Option<X1Host>,
    /// Endereço que vai nos convites (só o dono emite).
    invite_host: Option<HostAddr>,
    first_invite: Option<Invite>,
    reach: Option<lerri_client::ReachReport>,
}

async fn open_room(start: Start, identity: &Identity, data_dir: &std::path::Path, ui: &Weak<AppWindow>) -> anyhow::Result<RoomCtx> {
    let busy = |text: &'static str| {
        let _ = ui.upgrade_in_event_loop(move |ui| ui.set_busy_text(text.into()));
    };
    match start {
        Start::Join(link) => {
            let Link::Join(invite) = Link::parse(&link).context("link inválido")? else {
                bail!("este link não é um convite de sala")
            };
            if invite.expires_at <= unix_now() {
                bail!("este convite expirou; peça outro a quem convidou");
            }
            let session =
                Session::connect(identity, &invite.host, Auth::Join { room: invite.room, token: invite.token }).await?;
            if session.mode == RoomMode::Server {
                rooms::remember(data_dir, saved(&session, &invite.host))?;
            }
            Ok(RoomCtx { session, x1: None, invite_host: None, first_invite: None, reach: None })
        }
        Start::Open(room) => {
            let session = Session::connect(identity, &room.host, Auth::Rejoin { room: room.room }).await?;
            let invite_host = (session.role == Role::Owner).then_some(room.host);
            Ok(RoomCtx { session, x1: None, invite_host, first_invite: None, reach: None })
        }
        Start::CreateServer(link) => {
            let Link::Relay(relay) = Link::parse(&link).context("link inválido")? else {
                bail!("use o link de administração do relay")
            };
            let admin = relay.admin.context("este é o link público do relay; criar sala exige o link de administração")?;
            let session = Session::connect(identity, &relay.host, Auth::CreateRoom { admin, mode: RoomMode::Server }).await?;
            rooms::remember(data_dir, saved(&session, &relay.host))?;
            let first_invite = Some(session.issue_invite(&relay.host, DEFAULT_INVITE_TTL_SECS).await?);
            Ok(RoomCtx { session, x1: None, invite_host: Some(relay.host), first_invite, reach: None })
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
            let session = Session::connect(
                identity,
                &host.loopback_addr(),
                Auth::CreateRoom { admin: host.admin(), mode: RoomMode::X1 },
            )
            .await?;
            let first_invite = Some(session.issue_invite(&public, DEFAULT_INVITE_TTL_SECS).await?);
            Ok(RoomCtx {
                session,
                x1: Some(host),
                invite_host: Some(public),
                first_invite,
                reach: Some(lerri_client::reach_report(&classified, assisted)),
            })
        }
    }
}

fn saved(session: &Session, host: &HostAddr) -> SavedRoom {
    SavedRoom { room: session.room, host: host.clone(), mode: session.mode, role: session.role }
}

async fn room_task(ctx: RoomCtx, mut cmds: mpsc::UnboundedReceiver<RoomCmd>, ui: &Weak<AppWindow>) -> anyhow::Result<()> {
    let RoomCtx { mut session, x1, invite_host, first_invite, reach } = ctx;
    let me = session.me;
    let mut online: Vec<MemberId> = session.online.clone();
    let mut log: Vec<SharedString> = Vec::new();
    let push = |log: &mut Vec<SharedString>, line: String| {
        log.push(line.into());
        if log.len() > LOG_LINES {
            log.remove(0);
        }
    };
    push(&mut log, "Você entrou na sala.".into());

    {
        let room_id = session.room.to_string();
        let mode = if session.mode == RoomMode::X1 { "x1, direta entre os PCs" } else { "servidor" };
        let role = if session.role == Role::Owner { "você é o dono" } else { "você é membro" };
        let info = format!("Sala {mode} · {role}");
        let is_owner = invite_host.is_some();
        let (reach_text, reach_warning) = match &reach {
            Some(r) => {
                let mut text = r.lines.join("\n");
                if let Some(note) = &r.note {
                    text.push_str("\n\n");
                    text.push_str(note);
                }
                (text, r.warning)
            }
            None => (String::new(), false),
        };
        let _ = ui.upgrade_in_event_loop(move |ui| {
            ui.set_room_id(room_id[..12].into());
            ui.set_room_info(info.into());
            ui.set_is_owner(is_owner);
            ui.set_reach_text(reach_text.into());
            ui.set_reach_warning(reach_warning);
            ui.set_invite_link("".into());
            ui.set_copy_status("".into());
            ui.set_join_link("".into());
            ui.set_screen("room".into());
        });
    }
    if let Some(inv) = first_invite {
        show_invite(ui, &inv);
    }
    show_people(ui, me, &online, &log);
    show_route(ui, &session);

    let mut tick = tokio::time::interval(Duration::from_secs(2));
    let result = loop {
        tokio::select! {
            ev = session.next_event() => match ev {
                Some(Event::Online { member, first_time }) => {
                    online.retain(|m| *m != member);
                    online.push(member);
                    let what = if first_time { "entrou na sala pela primeira vez" } else { "voltou" };
                    push(&mut log, format!("{}… {what}", member.short()));
                    show_people(ui, me, &online, &log);
                }
                Some(Event::Offline { member }) => {
                    online.retain(|m| *m != member);
                    push(&mut log, format!("{}… saiu", member.short()));
                    show_people(ui, me, &online, &log);
                }
                Some(Event::Disconnected(reason)) => {
                    let why = if reason.contains("closed by peer") && session.mode == RoomMode::X1 {
                        "a sala acabou: quem criou fechou o app".to_string()
                    } else if reason.contains("closed by peer") {
                        "o host encerrou a conexão".to_string()
                    } else {
                        format!("conexão perdida ({reason})")
                    };
                    break Err(anyhow::anyhow!("Você saiu da sala: {why}."));
                }
                None => break Ok(()),
            },
            cmd = cmds.recv() => match cmd {
                Some(RoomCmd::NewInvite) => if let Some(host) = &invite_host {
                    match session.issue_invite(host, DEFAULT_INVITE_TTL_SECS).await {
                        Ok(inv) => {
                            push(&mut log, "Novo convite gerado.".into());
                            show_invite(ui, &inv);
                        }
                        Err(e) => push(&mut log, format!("Não deu para criar o convite: {}", explain(&e))),
                    }
                    show_people(ui, me, &online, &log);
                },
                Some(RoomCmd::Leave) | None => break Ok(()),
            },
            _ = tick.tick() => show_route(ui, &session),
        }
    };
    session.close().await;
    if let Some(host) = x1 {
        host.shutdown().await;
    }
    result
}

fn show_invite(ui: &Weak<AppWindow>, invite: &Invite) {
    let link = invite.to_link();
    let pixels = qr::pixels(&link);
    let _ = ui.upgrade_in_event_loop(move |ui| {
        ui.set_invite_link(link.into());
        ui.set_copy_status("".into());
        if let Some(px) = pixels {
            ui.set_invite_qr(slint::Image::from_rgb8(px));
        }
    });
}

fn show_people(ui: &Weak<AppWindow>, me: MemberId, online: &[MemberId], log: &[SharedString]) {
    let mut people: Vec<SharedString> = vec![format!("{}… (você)", me.short()).into()];
    people.extend(online.iter().map(|m| SharedString::from(format!("{}…", m.short()))));
    // Mais recente primeiro: o quadro de atividade é pequeno.
    let log: Vec<SharedString> = log.iter().rev().cloned().collect();
    let _ = ui.upgrade_in_event_loop(move |ui| {
        ui.set_online(ModelRc::new(VecModel::from(people)));
        ui.set_log(ModelRc::new(VecModel::from(log)));
    });
}

fn show_route(ui: &Weak<AppWindow>, session: &Session) {
    let (text, direct) = match session.route() {
        Some(r) => {
            let kind = if r.direct { "direto" } else { "pelo ponto de encontro" };
            (format!("Caminho {kind} · {:.1} ms", r.rtt.as_secs_f64() * 1000.0), r.direct)
        }
        None => (String::new(), false),
    };
    let _ = ui.upgrade_in_event_loop(move |ui| {
        ui.set_route_text(text.into());
        ui.set_route_direct(direct);
    });
}

/// Mensagem de erro para o usuário: recusas do relay em português claro.
fn explain(e: &anyhow::Error) -> String {
    match lerri_client::refusal(e) {
        Some(r) => format!("O host recusou: {r}."),
        None => format!("{e:#}"),
    }
}
