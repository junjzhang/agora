//! agorad — niri workspace manager daemon
//!
//! - Main thread: serves the agora unix socket (CLI requests).
//! - Background thread: subscribes to niri events; tracks window→project claims.

mod actions;
mod agents;
mod backend;
mod config;
mod launcher;
mod liveness;
mod niri;
mod procutil;
mod project;
mod tunnel;
mod watcher_sock;

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};

use agora::ipc::{self, Payload, Request, Response};
use agora::store;

use backend::{LocalBackend, RemoteBackend};
use config::{AgoraConfig, LauncherRegistry};
use liveness::Liveness;

#[derive(Default)]
pub(crate) struct Inner {
    pub projects: Vec<agora::model::Project>,
    pub claims: HashMap<u64, Claim>,
    pub workspaces: HashMap<u64, WorkspaceInfo>,
    pub local: LocalBackend,
    pub remotes: HashMap<String, RemoteBackend>,
    pub launcher_registry: LauncherRegistry,
    pub config: AgoraConfig,
    pub liveness: Option<Liveness>,
}

#[derive(Debug, Clone)]
pub(crate) struct WorkspaceInfo {
    pub idx: u8,
    pub name: Option<String>,
    pub output: Option<String>,
    pub is_active: bool,
    pub is_focused: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct Claim {
    pub app_id: Option<String>,
    pub title: Option<String>,
    pub workspace_id: Option<u64>,
    pub column: Option<usize>,
    pub pid: Option<i32>,
}

pub(crate) type State = Arc<Mutex<Inner>>;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--watcher-only") {
        return main_watcher_only();
    }

    tracing::info!("agorad v{} starting", env!("CARGO_PKG_VERSION"));

    let projects = store::load().context("load project store")?;
    tracing::info!(
        count = projects.len(),
        path = %store::store_path()?.display(),
        "loaded projects",
    );
    let remotes = store::load_remotes().context("load remotes store")?;
    tracing::info!(count = remotes.len(), "loaded remotes");
    let mut cfg = config::load_config().context("load config")?;
    if let Ok(v) = std::env::var("AGORA_CLEANUP_ALL_WS") {
        cfg.cleanup_all_workspaces = v == "1" || v.eq_ignore_ascii_case("true");
    }
    let (launcher_registry, user_launcher_overrides) =
        config::load_launcher_registry(&cfg).context("load launcher registry")?;
    tracing::info!(
        user_launcher_overrides,
        cleanup_all_workspaces = cfg.cleanup_all_workspaces,
        "loaded agora config",
    );
    let mut remote_backends: HashMap<String, RemoteBackend> = HashMap::new();
    for r in &remotes {
        remote_backends.insert(r.host.clone(), RemoteBackend::new(r.clone()));
    }
    let (liveness, exit_rx) = liveness::spawn();
    tracing::info!("liveness watcher started");
    let state: State = Arc::new(Mutex::new(Inner {
        projects,
        claims: HashMap::new(),
        workspaces: HashMap::new(),
        local: LocalBackend::new(),
        remotes: remote_backends,
        launcher_registry,
        config: cfg,
        liveness: Some(liveness),
    }));

    for r in &remotes {
        if r.auto_connect {
            tunnel::start_tunnel(&state, r.clone());
        }
    }

    {
        let state = state.clone();
        std::thread::Builder::new()
            .name("liveness-exit".into())
            .spawn(move || {
                while let Ok(session_id) = exit_rx.recv() {
                    agents::on_local_session_exit(&state, &session_id);
                }
            })
            .context("spawn liveness-exit thread")?;
    }

    {
        let state = state.clone();
        std::thread::Builder::new()
            .name("niri-events".into())
            .spawn(move || {
                if let Err(e) = niri::run_niri_loop(state) {
                    tracing::error!(error = %e, "niri event loop ended");
                }
            })
            .context("spawn niri-events thread")?;
    }

    {
        let liveness = state.lock().unwrap().liveness.clone();
        let watches = Arc::new(Mutex::new(
            HashMap::<String, watcher_sock::WatchEntry>::new(),
        ));
        let watcher_path = ipc::watcher_socket_path()?;
        let listener = watcher_sock::bind_socket(&watcher_path)?;
        tracing::info!(path = %watcher_path.display(), "watcher socket listening");
        if let Some(liveness) = liveness {
            std::thread::Builder::new()
                .name("watcher-sock".into())
                .spawn(move || watcher_sock::serve(listener, liveness, watches))
                .context("spawn watcher-sock thread")?;
        }
    }

    let path = ipc::socket_path()?;
    let listener = bind_socket(&path)?;
    tracing::info!(path = %path.display(), "socket listening");

    serve_socket(listener, state)
}

/// Slim daemon mode used on remote hosts: just liveness + watcher socket.
/// No project store, no niri loop, no daemon socket — just listens for
/// "watch this PID" commands and, on PID exit, shells out to
/// `agora hook event SessionEnd` (which reaches the central daemon via the
/// SSH reverse-forward tunnel).
fn main_watcher_only() -> Result<()> {
    tracing::info!(
        "agorad v{} starting (watcher-only)",
        env!("CARGO_PKG_VERSION")
    );
    let (liveness, exit_rx) = liveness::spawn();
    let watches: Arc<Mutex<HashMap<String, watcher_sock::WatchEntry>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let watcher_path = ipc::watcher_socket_path()?;
    let listener = watcher_sock::bind_socket(&watcher_path)?;
    tracing::info!(path = %watcher_path.display(), "watcher socket listening");

    {
        let watches = watches.clone();
        std::thread::Builder::new()
            .name("liveness-exit".into())
            .spawn(move || {
                while let Ok(session_id) = exit_rx.recv() {
                    let entry = watches.lock().unwrap().remove(&session_id);
                    let Some(entry) = entry else {
                        continue;
                    };
                    tracing::info!(session = %session_id, cli = %entry.cli, "pidfd exit; firing SessionEnd");
                    fire_remote_session_end(&session_id, &entry.cli, entry.host.as_deref());
                }
            })
            .context("spawn liveness-exit thread")?;
    }

    watcher_sock::serve(listener, liveness, watches);
    Ok(())
}

/// Shell out to `agora hook event --cli X SessionEnd` to deliver a
/// synthetic SessionEnd. Used by watcher-only mode where we have no
/// in-process state.
fn fire_remote_session_end(session_id: &str, cli: &str, host: Option<&str>) {
    let payload = serde_json::json!({ "session_id": session_id });
    // Sibling `agora` next to this binary. The watcher-only mode typically
    // runs without an interactive shell environment, so $PATH may not include
    // the install dir.
    let agora_bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("agora")))
        .unwrap_or_else(|| std::path::PathBuf::from("agora"));
    let mut cmd = std::process::Command::new(&agora_bin);
    cmd.args(["hook", "event", "--cli", cli, "SessionEnd"]);
    if let Some(h) = host {
        cmd.env("AGORA_HOST", h);
    }
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    match cmd.spawn() {
        Ok(mut child) => {
            if let Some(stdin) = child.stdin.take() {
                use std::io::Write;
                let mut stdin = stdin;
                let _ = stdin.write_all(payload.to_string().as_bytes());
            }
            // Wait briefly so we know the child started successfully.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => tracing::warn!(error = %e, "spawn `agora hook event SessionEnd` failed"),
    }
}

fn bind_socket(path: &Path) -> Result<UnixListener> {
    match UnixListener::bind(path) {
        Ok(l) => Ok(l),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => match UnixStream::connect(path) {
            Ok(_) => bail!("another agorad is already running on {}", path.display()),
            Err(_) => {
                fs::remove_file(path)
                    .with_context(|| format!("remove stale {}", path.display()))?;
                UnixListener::bind(path).with_context(|| format!("rebind {}", path.display()))
            }
        },
        Err(e) => Err(e).with_context(|| format!("bind {}", path.display())),
    }
}

fn serve_socket(listener: UnixListener, state: State) -> Result<()> {
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let state = state.clone();
                std::thread::spawn(move || {
                    if let Err(e) = handle_client(stream, state) {
                        tracing::warn!(error = %e, "client handler failed");
                    }
                });
            }
            Err(e) => tracing::warn!(error = %e, "accept failed"),
        }
    }
    Ok(())
}

fn handle_client(stream: UnixStream, state: State) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone().context("clone stream")?);
    let mut writer = stream;

    let req: Request = ipc::read_line(&mut reader)?;
    let resp = match dispatch(req, &state) {
        Ok(p) => Response::Ok(p),
        Err(e) => Response::Err(format!("{e:#}")),
    };
    ipc::write_line(&mut writer, &resp)?;
    Ok(())
}

fn dispatch(req: Request, state: &State) -> Result<Payload> {
    match req {
        Request::Add {
            name,
            root_path,
            host,
            launchers,
        } => Ok(Payload::Project(project::add(
            state, name, root_path, host, launchers,
        )?)),
        Request::List => {
            let projects = state.lock().unwrap().projects.clone();
            Ok(Payload::Projects(projects))
        }
        Request::Open { name } => project::open(state, name),
        Request::Status => Ok(niri::status(state)),
        Request::Forget { name } => Ok(Payload::Project(project::forget(state, name)?)),
        Request::Rename { from, to } => project::rename(state, from, to),
        Request::Get { name } => Ok(Payload::Project(project::get(state, name)?)),
        Request::Update { name, spec } => Ok(Payload::Project(project::update(state, name, spec)?)),
        Request::Promote {
            name,
            root_path,
            host,
            launchers,
            rename_ws,
        } => Ok(Payload::Project(project::promote(
            state, name, root_path, host, launchers, rename_ws,
        )?)),
        Request::Attach { name, rename_ws } => {
            Ok(Payload::Project(project::attach(state, name, rename_ws)?))
        }
        Request::Hook {
            cli,
            event,
            payload,
        } => {
            agents::apply_hook(state, &cli, &event, &payload);
            Ok(Payload::Ack)
        }
        Request::Agents => Ok(Payload::Agents(agents::list(state))),
        Request::RemoteAdd { host, remote_uid } => Ok(Payload::Remote(tunnel::remote_add(
            state, host, remote_uid,
        )?)),
        Request::RemoteRemove { host } => {
            tunnel::remote_remove(state, host)?;
            Ok(Payload::Ack)
        }
        Request::RemoteList => Ok(Payload::Remotes(tunnel::remote_list(state))),
        Request::FocusAgent { session_id } => {
            agents::focus(state, &session_id)?;
            Ok(Payload::Ack)
        }
        Request::Actions { target } => Ok(Payload::Actions(actions::actions_for_target(
            state, target,
        )?)),
        Request::RunAction { target, action_id } => {
            actions::run_action(state, target, &action_id)?;
            Ok(Payload::Ack)
        }
        Request::PickerState => Ok(picker_state(state)),
        Request::WorkspaceContext { ws_id } => {
            Ok(Payload::WorkspaceContext(workspace_context(state, ws_id)))
        }
    }
}

/// Best-effort promote-wizard seed for a niri workspace id. The picker
/// surfaces the returned values as editable defaults — empty fields just
/// mean we couldn't infer anything.
fn workspace_context(state: &State, ws_id: u64) -> agora::ipc::WorkspaceContext {
    use crate::backend::local::find_window_for_pid;
    let inner = state.lock().unwrap();
    let ws_name = inner.workspaces.get(&ws_id).and_then(|w| w.name.clone());

    // Walk agents first: their reported cwd is more trustworthy than reading
    // /proc/{pid}/cwd of an arbitrary terminal (which is the shell's cwd, not
    // necessarily the project root).
    let mut agent_cwd: Option<String> = None;
    let mut agent_host: Option<String> = None;
    'outer: for a in inner.local.agents.values() {
        let Some(pid) = a.pid else { continue };
        let Some(wid) = find_window_for_pid(&inner.claims, pid) else {
            continue;
        };
        if inner.claims.get(&wid).and_then(|c| c.workspace_id) != Some(ws_id) {
            continue;
        }
        if let Some(cwd) = a.cwd.as_deref() {
            if !cwd.is_empty() {
                agent_cwd = Some(cwd.to_string());
                break 'outer;
            }
        }
    }
    // Remote agents don't have local PIDs we can walk. Find the SSH terminal
    // window for the remote host and check if it lives on this workspace.
    for (host, remote) in &inner.remotes {
        if remote.agents.is_empty() {
            continue;
        }
        let Some(wid) = crate::backend::remote::find_window_with_ssh_to(&inner.claims, host) else {
            continue;
        };
        if inner.claims.get(&wid).and_then(|c| c.workspace_id) != Some(ws_id) {
            continue;
        }
        for a in remote.agents.values() {
            if let Some(cwd) = a.cwd.as_deref() {
                if !cwd.is_empty() {
                    agent_cwd.get_or_insert_with(|| cwd.to_string());
                }
            }
        }
        agent_host.get_or_insert_with(|| host.clone());
    }

    // Terminals (kitty etc.) keep their own cwd at the launch dir; the
    // useful cwd is the shell descendant's cwd.
    let pid_cwd: Option<String> = if agent_cwd.is_some() {
        None
    } else {
        inner
            .claims
            .values()
            .filter(|c| c.workspace_id == Some(ws_id))
            .find_map(|c| crate::procutil::find_shell_cwd(c.pid?))
    };

    let path = agent_cwd.or(pid_cwd).unwrap_or_default();
    let name = ws_name.unwrap_or_else(|| {
        std::path::Path::new(&path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    });

    agora::ipc::WorkspaceContext {
        ws_id,
        suggested_name: name,
        suggested_path: path,
        host: agent_host.unwrap_or_default(),
        available_launchers: inner.launcher_registry.keys().cloned().collect(),
    }
}

fn picker_state(state: &State) -> Payload {
    let projects = state.lock().unwrap().projects.clone();
    let agents = agents::list(state);
    let inner = state.lock().unwrap();
    let mut counts: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
    for c in inner.claims.values() {
        if let Some(ws_id) = c.workspace_id {
            *counts.entry(ws_id).or_insert(0) += 1;
        }
    }
    let workspaces: Vec<agora::ipc::WorkspaceSummary> = inner
        .workspaces
        .iter()
        .map(|(&id, w)| agora::ipc::WorkspaceSummary {
            id,
            name: w.name.clone(),
            is_active: w.is_active,
            is_focused: w.is_focused,
            idx: w.idx,
            window_count: counts.get(&id).copied().unwrap_or(0),
        })
        .collect();
    drop(inner);
    Payload::PickerState {
        projects,
        agents,
        workspaces,
    }
}
