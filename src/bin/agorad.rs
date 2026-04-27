//! agorad — niri workspace manager daemon
//!
//! - Main thread: serves the agora unix socket (CLI requests).
//! - Background thread: subscribes to niri events (best-effort; logs only in MVP).

use std::fs;
use std::io::{self, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use niri_ipc::socket::Socket;
use niri_ipc::{Event, Reply, Request as NiriRequest, Response as NiriResponse};

use agora::ipc::{self, Payload, Request, Response};
use agora::model::{Project, Root};
use agora::store;

type State = Arc<Mutex<Vec<Project>>>;

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
    let state: State = Arc::new(Mutex::new(projects));

    std::thread::Builder::new()
        .name("niri-events".into())
        .spawn(|| {
            if let Err(e) = run_niri_loop() {
                tracing::error!(error = %e, "niri event loop ended");
            }
        })
        .context("spawn niri-events thread")?;

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
        Request::Add { name, root_path } => Ok(Payload::Project(add(state, name, root_path)?)),
        Request::List => {
            let projects = state.lock().unwrap().clone();
            Ok(Payload::Projects(projects))
        }
    }
}

fn add(state: &State, name: String, root_path: String) -> Result<Project> {
    if name.is_empty() {
        bail!("project name must not be empty");
    }

    let resolved =
        fs::canonicalize(&root_path).with_context(|| format!("resolve root path {root_path}"))?;
    let meta = fs::metadata(&resolved).with_context(|| format!("stat {}", resolved.display()))?;
    if !meta.is_dir() {
        bail!("root path is not a directory: {}", resolved.display());
    }
    let path = resolved.to_string_lossy().into_owned();

    let mut projects = state.lock().unwrap();
    if projects.iter().any(|p| p.id == name) {
        bail!("project '{name}' already exists");
    }

    let now = unix_now();
    let project = Project {
        id: name.clone(),
        workspace_name: name.clone(),
        name,
        roots: vec![Root {
            path,
            host: None,
            label: None,
            launchers: Vec::new(),
        }],
        default_root: 0,
        pinned: false,
        ts_created: now,
        ts_last_active: now,
        archived_at: None,
    };
    projects.push(project.clone());
    store::save(&projects).context("save project store")?;
    Ok(project)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn run_niri_loop() -> Result<()> {
    let mut socket = Socket::connect().context("connecting to niri IPC socket")?;
    let reply: Reply = socket
        .send(NiriRequest::EventStream)
        .context("requesting event stream")?;

    match reply {
        Ok(NiriResponse::Handled) => {}
        Ok(other) => bail!("unexpected response from niri: {other:?}"),
        Err(msg) => bail!("niri rejected event stream request: {msg}"),
    }

    tracing::info!("subscribed to niri event stream");
    let mut read_event = socket.read_events();

    loop {
        let event = read_event().context("reading niri event")?;
        log_niri_event(&event);
    }
}

fn log_niri_event(event: &Event) {
    match serde_json::to_string(event) {
        Ok(json) => tracing::info!(target: "niri", "{json}"),
        Err(e) => tracing::warn!(target: "niri", error = %e, "serialize failed"),
    }
}
