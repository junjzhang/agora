//! agorad — niri workspace manager daemon
//!
//! - Main thread: serves the agora unix socket (CLI requests).
//! - Background thread: subscribes to niri events; tracks window→project claims.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use niri_ipc::socket::Socket;
use niri_ipc::{
    Action as NiriAction, Event, Reply, Request as NiriRequest, Response as NiriResponse, Window,
    WorkspaceReferenceArg,
};

use agora::ipc::{self, Payload, Request, Response, WindowSummary};
use agora::model::{Launcher, Project, Root};
use agora::store;

#[derive(Default)]
struct Inner {
    projects: Vec<Project>,
    claims: HashMap<u64, Claim>,
}

#[derive(Debug, Clone)]
struct Claim {
    project: Option<String>,
    app_id: Option<String>,
    title: Option<String>,
    workspace_id: Option<u64>,
    column: Option<usize>,
    pid: Option<i32>,
    cwd: Option<PathBuf>,
}

type State = Arc<Mutex<Inner>>;

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
    let state: State = Arc::new(Mutex::new(Inner {
        projects,
        claims: HashMap::new(),
    }));

    {
        let state = state.clone();
        std::thread::Builder::new()
            .name("niri-events".into())
            .spawn(move || {
                if let Err(e) = run_niri_loop(state) {
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
            launchers,
        } => Ok(Payload::Project(add(state, name, root_path, launchers)?)),
        Request::List => {
            let projects = state.lock().unwrap().projects.clone();
            Ok(Payload::Projects(projects))
        }
        Request::Open { name } => open(state, name),
        Request::Status => Ok(status(state)),
    }
}

fn add(
    state: &State,
    name: String,
    root_path: String,
    launchers: Vec<Launcher>,
) -> Result<Project> {
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

    let mut inner = state.lock().unwrap();
    if inner.projects.iter().any(|p| p.id == name) {
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
            launchers,
        }],
        default_root: 0,
        pinned: false,
        ts_created: now,
        ts_last_active: now,
        archived_at: None,
    };
    inner.projects.push(project.clone());
    store::save(&inner.projects).context("save project store")?;
    rematch_claims(&mut inner);
    Ok(project)
}

fn rematch_claims(inner: &mut Inner) {
    for claim in inner.claims.values_mut() {
        claim.project = claim
            .cwd
            .as_deref()
            .and_then(|c| match_project(c, &inner.projects));
    }
}

fn open(state: &State, name: String) -> Result<Payload> {
    let project = {
        let inner = state.lock().unwrap();
        inner
            .projects
            .iter()
            .find(|p| p.id == name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no project named '{name}'"))?
    };

    let workspaces = match niri_call(NiriRequest::Workspaces)? {
        NiriResponse::Workspaces(ws) => ws,
        other => bail!("unexpected niri response to Workspaces: {other:?}"),
    };
    let exists = workspaces
        .iter()
        .any(|w| w.name.as_deref() == Some(project.workspace_name.as_str()));

    let action = if exists {
        NiriAction::FocusWorkspace {
            reference: WorkspaceReferenceArg::Name(project.workspace_name.clone()),
        }
    } else {
        NiriAction::SetWorkspaceName {
            name: project.workspace_name.clone(),
            workspace: None,
        }
    };

    match niri_call(NiriRequest::Action(action))? {
        NiriResponse::Handled => {}
        other => bail!("unexpected niri response to Action: {other:?}"),
    }

    if !exists {
        spawn_launchers(&project);
    }

    {
        let mut inner = state.lock().unwrap();
        if let Some(p) = inner.projects.iter_mut().find(|p| p.id == name) {
            p.ts_last_active = unix_now();
        }
        store::save(&inner.projects).context("save project store")?;
    }

    Ok(Payload::Opened {
        project,
        claimed_current: !exists,
    })
}

fn spawn_launchers(project: &Project) {
    let Some(root) = project.roots.get(project.default_root) else {
        tracing::warn!(project = %project.id, "default_root index out of range");
        return;
    };
    if root.host.is_some() {
        tracing::warn!(project = %project.id, "remote launcher not yet supported");
        return;
    }
    if root.launchers.is_empty() {
        return;
    }
    for launcher in &root.launchers {
        spawn_one(launcher, &root.path);
    }
}

fn spawn_one(launcher: &Launcher, cwd: &str) {
    let mut cmd = launcher_command(launcher, cwd);
    cmd.current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match cmd.spawn() {
        Ok(child) => {
            let pid = child.id();
            tracing::info!(
                kind = launcher.kind(),
                pid,
                cwd = %cwd,
                "spawned launcher",
            );
            // Reap when the child exits so it doesn't linger as a zombie.
            std::thread::spawn(move || {
                let mut child = child;
                let _ = child.wait();
            });
        }
        Err(e) => {
            tracing::warn!(
                kind = launcher.kind(),
                error = %e,
                "spawn failed",
            );
        }
    }
}

fn launcher_command(launcher: &Launcher, cwd: &str) -> Command {
    match launcher {
        Launcher::Vscode => {
            let mut c = Command::new("code");
            c.arg(cwd);
            c
        }
        Launcher::Zed => {
            let mut c = Command::new("zed");
            c.arg(cwd);
            c
        }
        Launcher::Kitty { run: None } => Command::new("kitty"),
        Launcher::Kitty { run: Some(run) } => {
            let mut c = Command::new("kitty");
            c.arg("--").arg("sh").arg("-lc").arg(run);
            c
        }
        Launcher::Browser { url } => {
            let mut c = Command::new("xdg-open");
            c.arg(url);
            c
        }
        Launcher::Custom { argv } => {
            let mut iter = argv.iter();
            let head = iter.next().map(String::as_str).unwrap_or("true");
            let mut c = Command::new(head);
            c.args(iter);
            c
        }
    }
}

fn status(state: &State) -> Payload {
    let inner = state.lock().unwrap();
    let mut windows: Vec<WindowSummary> = inner
        .claims
        .iter()
        .map(|(id, c)| WindowSummary {
            window_id: *id,
            project: c.project.clone(),
            app_id: c.app_id.clone(),
            title: c.title.clone(),
            workspace_id: c.workspace_id,
            column: c.column,
            pid: c.pid,
            cwd: c.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
        })
        .collect();
    windows.sort_by_key(|w| (w.workspace_id, w.column, w.window_id));
    Payload::Status {
        project_count: inner.projects.len(),
        windows,
    }
}

fn niri_call(req: NiriRequest) -> Result<NiriResponse> {
    let mut socket = Socket::connect().context("connect to niri socket")?;
    let reply: Reply = socket.send(req).context("send request to niri")?;
    reply.map_err(|msg| anyhow::anyhow!("niri error: {msg}"))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn run_niri_loop(state: State) -> Result<()> {
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
        apply_event(&state, event);
    }
}

fn apply_event(state: &State, event: Event) {
    match event {
        Event::WindowsChanged { windows } => {
            // Full snapshot — replace claims.
            let projects = state.lock().unwrap().projects.clone();
            let mut new_claims: HashMap<u64, Claim> = HashMap::with_capacity(windows.len());
            for w in &windows {
                new_claims.insert(w.id, build_claim(w, &projects));
            }
            let mut inner = state.lock().unwrap();
            inner.claims = new_claims;
            tracing::info!(count = windows.len(), "claims rebuilt from snapshot");
        }
        Event::WindowOpenedOrChanged { window } => {
            let projects = state.lock().unwrap().projects.clone();
            let claim = build_claim(&window, &projects);
            let project = claim.project.clone();
            state.lock().unwrap().claims.insert(window.id, claim);
            if let Some(p) = project {
                tracing::info!(window_id = window.id, project = %p, app_id = ?window.app_id, "window claimed");
            }
        }
        Event::WindowClosed { id } => {
            state.lock().unwrap().claims.remove(&id);
        }
        _ => {
            // Ignore for MVP; later: workspace name diffs, focus tracking, etc.
        }
    }
}

fn build_claim(w: &Window, projects: &[Project]) -> Claim {
    let cwd = w.pid.and_then(read_proc_cwd);
    let project = cwd.as_deref().and_then(|c| match_project(c, projects));
    Claim {
        project,
        app_id: w.app_id.clone(),
        title: w.title.clone(),
        workspace_id: w.workspace_id,
        column: w.layout.pos_in_scrolling_layout.map(|(c, _)| c),
        pid: w.pid,
        cwd,
    }
}

fn read_proc_cwd(pid: i32) -> Option<PathBuf> {
    if pid <= 0 {
        return None;
    }
    fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

fn match_project(cwd: &Path, projects: &[Project]) -> Option<String> {
    let mut best: Option<(usize, &Project)> = None;
    for p in projects {
        for r in &p.roots {
            let root = Path::new(&r.path);
            if cwd.starts_with(root) {
                let len = root.as_os_str().len();
                if best.is_none_or(|(l, _)| len > l) {
                    best = Some((len, p));
                }
            }
        }
    }
    best.map(|(_, p)| p.id.clone())
}
