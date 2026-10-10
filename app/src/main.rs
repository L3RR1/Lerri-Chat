//! `lerrichat.exe` — cliente e relay no mesmo binário (SPEC-01 §2).
//!
//! Sem argumentos (ou abrindo um link `lerrichat://`) abre a janela. Os
//! comandos de terminal continuam para relay, testes e automação.

mod protocol_handler;

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use lerri_client::rooms::{self, SavedRoom};
use lerri_client::{Event, Session};
use lerri_core::invite::{DEFAULT_INVITE_TTL_SECS, HostAddr, Invite, Link};
use lerri_core::protocol::{Auth, Role, RoomMode};
use lerri_core::unix_now;
use lerri_crypto::Identity;
use lerri_host::{RelayConfig, X1Host};
use lerri_net::reach;

const SCHEME: &str = lerri_core::invite::SCHEME;

const USAGE: &str = "\
Lerri Chat (Fase 1)

Uso:
  lerrichat                          abre a janela
  lerrichat LINK                     abre a janela e entra pelo convite (o mesmo que --join)
  lerrichat --cli --join LINK        entra pelo convite no terminal
  lerrichat --id                     mostra sua identidade
  lerrichat --x1 [--via LINK] [--port N]
                                     cria uma sala x1 neste PC. --via usa o link
                                     público de um relay seu como ponto de encontro
  lerrichat --create LINK_ADMIN      cria uma sala num relay (modo servidor)
  lerrichat --rooms                  lista as salas salvas
  lerrichat --open ID                volta a uma sala salva (começo do id basta)
  lerrichat --host [--port N] [--public HOST]... [--no-meet]
                                     roda um relay (modo servidor)
  lerrichat --register               faz os links lerrichat:// abrirem este programa
  lerrichat --unregister

Dentro de uma sala: convite, membros, rota, sair.

Opções comuns:
  --data-dir PASTA    pasta de dados (padrão: %APPDATA%\\LerriChat)
  --version, --help";

enum Command {
    Gui(Option<String>),
    Identity,
    Host,
    Join(String),
    X1 { via: Option<String> },
    Create(String),
    Rooms,
    Open(String),
    Register,
    Unregister,
}

struct Args {
    command: Command,
    cli: bool,
    port: Option<u16>,
    public: Vec<String>,
    meet: bool,
    data_dir: Option<PathBuf>,
}

fn parse_args() -> anyhow::Result<Option<Args>> {
    let mut args = Args { command: Command::Gui(None), cli: false, port: None, public: Vec::new(), meet: true, data_dir: None };
    let mut via = None;
    let mut it = std::env::args().skip(1);
    let set = |cmd: Command, current: &mut Command| {
        if !matches!(current, Command::Gui(None)) {
            bail!("escolha um comando só\n\n{USAGE}");
        }
        *current = cmd;
        Ok(())
    };
    while let Some(flag) = it.next() {
        let mut value = || it.next().with_context(|| format!("{flag} precisa de um valor"));
        match flag.as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(None);
            }
            "--version" | "-V" => {
                println!("lerrichat {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--id" => set(Command::Identity, &mut args.command)?,
            "--cli" => args.cli = true,
            "--host" => set(Command::Host, &mut args.command)?,
            "--join" => set(Command::Join(value()?), &mut args.command)?,
            "--x1" => set(Command::X1 { via: None }, &mut args.command)?,
            "--via" => via = Some(value()?),
            "--create" => set(Command::Create(value()?), &mut args.command)?,
            "--rooms" => set(Command::Rooms, &mut args.command)?,
            "--open" => set(Command::Open(value()?), &mut args.command)?,
            "--register" => set(Command::Register, &mut args.command)?,
            "--unregister" => set(Command::Unregister, &mut args.command)?,
            "--port" => args.port = Some(value()?.parse().context("--port inválido")?),
            "--public" => args.public.push(value()?),
            "--no-meet" => args.meet = false,
            "--data-dir" => args.data_dir = Some(value()?.into()),
            link if link.starts_with(&format!("{SCHEME}:")) => set(Command::Join(link.into()), &mut args.command)?,
            other => bail!("opção desconhecida: {other}\n\n{USAGE}"),
        }
    }
    match (&mut args.command, via) {
        (Command::X1 { via: v }, Some(link)) => *v = Some(link),
        (_, Some(_)) => bail!("--via só vale com --x1"),
        _ => {}
    }
    // Convite abre a janela, a menos que peça o terminal.
    if let Command::Join(link) = &args.command
        && !args.cli
    {
        args.command = Command::Gui(Some(link.clone()));
    }
    Ok(Some(args))
}

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn,lerri_host=info,iroh_relay::server=error".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let result = (|| {
        let Some(args) = parse_args()? else { return Ok(()) };
        let data_dir = match args.data_dir.clone() {
            Some(d) => d,
            None => lerri_core::paths::default_data_dir().context("pasta de dados não encontrada")?,
        };
        if let Command::Gui(link) = args.command {
            return gui(data_dir, link);
        }
        tokio::runtime::Runtime::new()?.block_on(run(args, data_dir))
    })();

    let code = match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("erro: {e:#}");
            std::process::ExitCode::FAILURE
        }
    };
    pause_if_own_console();
    code
}

async fn run(args: Args, data_dir: PathBuf) -> anyhow::Result<()> {
    match args.command {
        Command::Host => {
            firewall_notice();
            let cfg = RelayConfig { data_dir, port: args.port, public: args.public, meet: args.meet };
            lerri_host::run_foreground(&cfg).await
        }
        Command::Register => {
            let exe = std::env::current_exe()?;
            protocol_handler::register(SCHEME, &exe)?;
            println!("Links {SCHEME}:// agora abrem {}", exe.display());
            Ok(())
        }
        Command::Unregister => {
            protocol_handler::unregister(SCHEME)?;
            println!("Links {SCHEME}:// não abrem mais o Lerri Chat");
            Ok(())
        }
        Command::Rooms => list_rooms(&data_dir),
        Command::Gui(_) => unreachable!("a janela não roda dentro do runtime"),
        Command::Identity => {
            load_identity(&data_dir)?;
            match protocol_handler::registered_command(SCHEME) {
                Some(cmd) => println!("Links {SCHEME}:// abrem: {cmd}"),
                None if cfg!(windows) => println!("Links {SCHEME}:// ainda não abrem este programa (use --register)"),
                None => {}
            }
            println!("\n{USAGE}");
            Ok(())
        }
        Command::Join(link) => join(&data_dir, &link).await,
        Command::Open(prefix) => open(&data_dir, &prefix).await,
        Command::Create(link) => create(&data_dir, &link).await,
        Command::X1 { via } => x1(&data_dir, via.as_deref(), args.port.unwrap_or(0)).await,
    }
}

fn load_identity(data_dir: &Path) -> anyhow::Result<Identity> {
    let (identity, created) = Identity::load_or_create(data_dir)?;
    if created {
        println!("Identidade nova criada em {}", data_dir.display());
    }
    println!("Seu MemberId: {}", identity.member_id());
    Ok(identity)
}

async fn join(data_dir: &Path, link: &str) -> anyhow::Result<()> {
    let Link::Join(invite) = Link::parse(link)? else { bail!("este link não é um convite de sala") };
    if invite.expires_at <= unix_now() {
        bail!("este convite expirou; peça outro a quem convidou");
    }
    let identity = load_identity(data_dir)?;
    println!("Convidado por {}…  Conectando…", invite.inviter.short());
    let session = Session::connect(&identity, &invite.host, Auth::Join { room: invite.room, token: invite.token })
        .await
        .map_err(explain)?;
    if session.mode == RoomMode::Server {
        rooms::remember(
            data_dir,
            SavedRoom { room: session.room, host: invite.host.clone(), mode: session.mode, role: session.role },
        )?;
    }
    room_loop(session, None).await
}

async fn open(data_dir: &Path, prefix: &str) -> anyhow::Result<()> {
    let saved = rooms::find(data_dir, prefix)?;
    let identity = load_identity(data_dir)?;
    println!("Conectando…");
    let session = Session::connect(&identity, &saved.host, Auth::Rejoin { room: saved.room }).await.map_err(explain)?;
    let invite_host = (session.role == Role::Owner).then(|| saved.host.clone());
    room_loop(session, invite_host).await
}

async fn create(data_dir: &Path, link: &str) -> anyhow::Result<()> {
    let Link::Relay(relay) = Link::parse(link)? else { bail!("use o link de administração do relay") };
    let admin = relay.admin.context("este é o link público do relay; criar sala exige o link de administração")?;
    let identity = load_identity(data_dir)?;
    let session = Session::connect(&identity, &relay.host, Auth::CreateRoom { admin, mode: RoomMode::Server })
        .await
        .map_err(explain)?;
    rooms::remember(
        data_dir,
        SavedRoom { room: session.room, host: relay.host.clone(), mode: session.mode, role: session.role },
    )?;
    println!("Sala criada e salva. Para voltar depois: lerrichat --open {}", &session.room.to_string()[..8]);
    print_invite(&session.issue_invite(&relay.host, DEFAULT_INVITE_TTL_SECS).await?);
    room_loop(session, Some(relay.host)).await
}

async fn x1(data_dir: &Path, via: Option<&str>, port: u16) -> anyhow::Result<()> {
    let meet = match via {
        Some(link) => {
            let Link::Relay(relay) = Link::parse(link)? else { bail!("--via espera o link público de um relay") };
            Some(relay.host.meet.context("esse relay não tem ponto de encontro (rodando com --no-meet?)")?)
        }
        None => None,
    };
    let identity = load_identity(data_dir)?;
    firewall_notice();
    let assisted = meet.is_some();
    let host = X1Host::start(meet, port).await?;
    println!("Procurando endereços (roteador, IPv6{})…", if assisted { ", ponto de encontro" } else { "" });
    let (public, classified) = host.host_addr().await;
    print_reachability(&classified, assisted);

    let session =
        Session::connect(&identity, &host.loopback_addr(), Auth::CreateRoom { admin: host.admin(), mode: RoomMode::X1 })
            .await?;
    println!("Sala x1 criada. Ela existe enquanto este programa estiver aberto.");
    print_invite(&session.issue_invite(&public, DEFAULT_INVITE_TTL_SECS).await?);
    let result = room_loop(session, Some(public)).await;
    host.shutdown().await;
    result
}

fn print_reachability(cands: &[reach::Candidate], assisted: bool) {
    let report = lerri_client::reach_report(cands, assisted);
    println!("Endereços no convite:");
    for line in &report.lines {
        println!("  {line}");
    }
    if let Some(note) = report.note {
        println!("{}{note}", if report.warning { "\nAVISO: " } else { "" });
    }
}

fn print_invite(invite: &Invite) {
    let link = invite.to_link();
    println!("\nConvite (uso único, válido por 24 h). Envie por um canal confiável:\n{link}\n");
    match qrcode::QrCode::new(link.as_bytes()) {
        Ok(code) => {
            use qrcode::render::unicode::Dense1x2;
            // Invertido de propósito: em terminal escuro, o bloco cheio é o claro.
            let art = code.render::<Dense1x2>().dark_color(Dense1x2::Light).light_color(Dense1x2::Dark).quiet_zone(true).build();
            println!("{art}");
        }
        Err(e) => println!("(QR indisponível: {e})"),
    }
}

async fn room_loop(mut session: Session, invite_host: Option<HostAddr>) -> anyhow::Result<()> {
    let role = if session.role == Role::Owner { "dono" } else { "membro" };
    let mode = if session.mode == RoomMode::X1 { "x1" } else { "servidor" };
    println!("Na sala {} ({mode}, você é {role}).", session.room);
    let mut online: Vec<_> = session.online.clone();
    print_online(&online);
    if let Some(route) = session.route() {
        println!("Caminho: {}", describe_route(&route));
    }

    let (tx, mut lines) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    loop {
        tokio::select! {
            ev = session.next_event() => match ev {
                Some(Event::Online { member, first_time }) => {
                    online.retain(|m| *m != member);
                    online.push(member);
                    let what = if first_time { "entrou na sala pela primeira vez" } else { "voltou" };
                    println!("+ {}… {what}", member.short());
                }
                Some(Event::Offline { member }) => {
                    online.retain(|m| *m != member);
                    println!("- {}… saiu", member.short());
                }
                Some(Event::Disconnected(reason)) => {
                    println!("Desconectado: {reason}");
                    return Ok(());
                }
                None => return Ok(()),
            },
            line = lines.recv() => match line.as_deref().map(str::trim) {
                Some("convite" | "c") => match &invite_host {
                    Some(host) => match session.issue_invite(host, DEFAULT_INVITE_TTL_SECS).await {
                        Ok(inv) => print_invite(&inv),
                        Err(e) => println!("Não deu para criar o convite: {e:#}"),
                    },
                    None => println!("Só o dono da sala emite convites."),
                },
                Some("membros" | "m") => print_online(&online),
                Some("rota" | "r") => match session.route() {
                    Some(route) => println!("Caminho: {}", describe_route(&route)),
                    None => println!("Sem caminho ativo."),
                },
                Some("sair" | "q") | None => break,
                Some("") => {}
                Some(other) => println!("Comando desconhecido: {other}. Use convite, membros, rota ou sair."),
            },
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    session.close().await;
    Ok(())
}

fn print_online(online: &[lerri_core::MemberId]) {
    if online.is_empty() {
        println!("Ninguém mais online.");
    } else {
        let list: Vec<String> = online.iter().map(|m| format!("{}…", m.short())).collect();
        println!("Online: {}", list.join(", "));
    }
}

fn describe_route(route: &lerri_client::Route) -> String {
    let kind = if route.direct { "direto" } else { "pelo ponto de encontro" };
    format!("{kind}, RTT {:.1} ms ({})", route.rtt.as_secs_f64() * 1000.0, route.remote)
}

fn list_rooms(data_dir: &Path) -> anyhow::Result<()> {
    let all = rooms::load(data_dir)?;
    if all.is_empty() {
        println!("Nenhuma sala salva. Salas x1 não ficam salvas: elas acabam quando o criador fecha o app.");
    }
    for r in all {
        let role = if r.role == Role::Owner { "dono" } else { "membro" };
        println!("{}  ({role})", r.room);
    }
    Ok(())
}

/// Traduz recusas do relay para algo que o usuário entenda.
fn explain(e: anyhow::Error) -> anyhow::Error {
    match lerri_client::refusal(&e) {
        Some(r) => anyhow::anyhow!("o host recusou: {r}"),
        None => e,
    }
}

fn firewall_notice() {
    if cfg!(windows) {
        println!(
            "Este PC vai receber conexões. Se o Windows perguntar se o Lerri Chat pode usar a rede, permita; \
             sem isso ninguém de fora consegue entrar."
        );
    }
}

fn gui(data_dir: PathBuf, link: Option<String>) -> anyhow::Result<()> {
    hide_own_console();
    // Sem console, um panic sumiria sem rastro: grava em crash.log.
    let crash_log = data_dir.join("crash.log");
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let bt = std::backtrace::Backtrace::force_capture();
        let line = format!("[{}] {info}\n{bt}\n", lerri_core::unix_now());
        let _ = std::fs::OpenOptions::new().create(true).append(true).open(&crash_log).and_then(|mut f| {
            use std::io::Write;
            f.write_all(line.as_bytes())
        });
        default_hook(info);
    }));
    // Na primeira vez, faz os links lerrichat:// abrirem o app (RF-04). Não
    // mexe se já apontam para outro executável.
    if protocol_handler::registered_command(SCHEME).is_none()
        && let Ok(exe) = std::env::current_exe()
        && let Err(e) = protocol_handler::register(SCHEME, &exe)
    {
        tracing::warn!("não deu para registrar {SCHEME}://: {e:#}");
    }
    let result = lerri_ui::run(data_dir, link);
    if let Err(e) = &result {
        show_fatal(&format!("{e:#}"));
    }
    result
}

/// Aberto pelo Explorer ou por um link, o Windows cria um console só para
/// nós; na janela ele é só ruído. Aberto de um terminal, o console é
/// compartilhado e fica (logs continuam aparecendo).
fn hide_own_console() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{
            FreeConsole, GetConsoleProcessList, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
        };
        let mut pids = [0u32; 4];
        // SAFETY: buffer com o tamanho informado.
        if unsafe { GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) } == 1 {
            // SAFETY: sem pré-condições. Sem console, os handles padrão ficam
            // inválidos e escrever neles (logs) derruba o processo; nulo faz a
            // biblioteca padrão descartar a escrita.
            unsafe {
                FreeConsole();
                for h in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                    SetStdHandle(h, std::ptr::null_mut());
                }
            }
        }
    }
}

/// Erro que impede a janela de abrir: sem console, só uma caixa de mensagem.
fn show_fatal(msg: &str) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
        let text: Vec<u16> = msg.encode_utf16().chain(Some(0)).collect();
        let title: Vec<u16> = "Lerri Chat".encode_utf16().chain(Some(0)).collect();
        // SAFETY: strings UTF-16 terminadas em zero.
        unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR) };
    }
    #[cfg(not(windows))]
    eprintln!("{msg}");
}

/// Aberto por um link (o Windows cria um console só para nós), a janela
/// fecharia antes de dar para ler a mensagem final.
fn pause_if_own_console() {
    #[cfg(windows)]
    {
        let mut pids = [0u32; 4];
        // SAFETY: buffer com o tamanho informado.
        let n = unsafe { windows_sys::Win32::System::Console::GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) };
        if n == 1 {
            println!("\nPressione Enter para fechar.");
            let _ = std::io::stdin().read_line(&mut String::new());
        }
    }
}
