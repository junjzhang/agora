//! agorad — niri workspace manager daemon
//!
//! - Main thread: serves the agora unix socket (CLI requests).
//! - Background thread: subscribes to niri events; tracks window→project claims.

mod actions;
mod config;
mod hooks;
mod launcher;
mod niri;
mod project;
mod tunnel;

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};

use agora::ipc::{self, Payload, Request, Response};
use agora::model::AgentSession;
use agora::store;

use config::{AgoraConfig, LauncherRegistry};
use tunnel::TunnelState;

#[derive(Default)]
pub(crate) struct Inner {
    pub projects: Vec<agora::model::Project>,
    pub claims: HashMap<u64, Claim>,
    pub workspaces: HashMap<u64, WorkspaceInfo>,
    pub agents: HashMap<String, AgentSession>,
    pub remotes: Vec<agora::model::RemoteHost>,
    pub tunnels: HashMap<String, TunnelState>,
    pub launcher_registry: LauncherRegistry,
    pub config: AgoraConfig,
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
    let state: State = Arc::new(Mutex::new(Inner {
        projects,
        claims: HashMap::new(),
        workspaces: HashMap::new(),
        agents: HashMap::new(),
        remotes: remotes.clone(),
        tunnels: HashMap::new(),
        launcher_registry,
        config: cfg,
    }));

    for r in &remotes {
        if r.auto_connect {
            tunnel::start_tunnel(&state, r.clone());
        }
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

    let path = ipc::socket_path()?;
    let listener = bind_socket(&path)?;
    tracing::info!(path = %path.display(), "socket listening");

    serve_socket(listener, state)
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
            hooks::apply_hook(state, &cli, &event, &payload);
            Ok(Payload::Ack)
        }
        Request::Agents => Ok(Payload::Agents(hooks::agents(state))),
        Request::RemoteAdd { host, remote_uid } => Ok(Payload::Remote(tunnel::remote_add(
            state, host, remote_uid,
        )?)),
        Request::RemoteRemove { host } => {
            tunnel::remote_remove(state, host)?;
            Ok(Payload::Ack)
        }
        Request::RemoteList => Ok(Payload::Remotes(tunnel::remote_list(state))),
        Request::FocusAgent { session_id } => {
            hooks::focus_agent(state, &session_id)?;
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
    }
}

fn picker_state(state: &State) -> Payload {
    let projects = state.lock().unwrap().projects.clone();
    let agents = hooks::agents(state);
    let workspaces: Vec<agora::ipc::WorkspaceSummary> = state
        .lock()
        .unwrap()
        .workspaces
        .iter()
        .map(|(&id, w)| agora::ipc::WorkspaceSummary {
            id,
            name: w.name.clone(),
            is_active: w.is_active,
            is_focused: w.is_focused,
        })
        .collect();
    Payload::PickerState {
        projects,
        agents,
        workspaces,
    }
}
