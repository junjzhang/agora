//! agorad — niri workspace manager daemon
//!
//! - Main thread: serves the agora unix socket (CLI requests).
//! - Background thread: subscribes to niri events; tracks window→project claims.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use niri_ipc::socket::Socket;
use niri_ipc::{
    Action as NiriAction, Event, Reply, Request as NiriRequest, Response as NiriResponse, Window,
    Workspace, WorkspaceReferenceArg,
};

use agora::ipc::{self, Payload, Request, Response, WindowSummary};
use agora::model::{Launcher, Project, ProjectSpec, Root};
use agora::store;

#[derive(Default)]
struct Inner {
    projects: Vec<Project>,
    claims: HashMap<u64, Claim>,
    /// niri workspace id → its current (idx, name). Driven by WorkspacesChanged.
    /// idx is per-output, 1-based, and shifts when workspaces are moved.
    workspaces: HashMap<u64, WorkspaceInfo>,
}

#[derive(Debug, Clone)]
struct WorkspaceInfo {
    idx: u8,
    name: Option<String>,
}

#[derive(Debug, Clone)]
struct Claim {
    app_id: Option<String>,
    title: Option<String>,
    workspace_id: Option<u64>,
    column: Option<usize>,
    pid: Option<i32>,
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
        workspaces: HashMap::new(),
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
            host,
            launchers,
        } => Ok(Payload::Project(add(
            state, name, root_path, host, launchers,
        )?)),
        Request::List => {
            let projects = state.lock().unwrap().projects.clone();
            Ok(Payload::Projects(projects))
        }
        Request::Open { name } => open(state, name),
        Request::Status => Ok(status(state)),
        Request::Forget { name } => Ok(Payload::Project(forget(state, name)?)),
        Request::Rename { from, to } => rename(state, from, to),
        Request::Get { name } => Ok(Payload::Project(get(state, name)?)),
        Request::Update { name, spec } => Ok(Payload::Project(update(state, name, spec)?)),
    }
}

fn add(
    state: &State,
    name: String,
    root_path: String,
    host: Option<String>,
    launchers: Vec<Launcher>,
) -> Result<Project> {
    if name.is_empty() {
        bail!("project name must not be empty");
    }

    // Local: canonicalize + must be a directory.
    // Remote: trust the path; we can't stat across ssh from the daemon.
    let path = match host.as_deref() {
        Some("") => bail!("--host is empty; omit it for local"),
        Some(_) => {
            if root_path.is_empty() {
                bail!("root path must not be empty");
            }
            root_path
        }
        None => {
            let resolved = fs::canonicalize(&root_path)
                .with_context(|| format!("resolve root path {root_path}"))?;
            let meta = fs::metadata(&resolved)
                .with_context(|| format!("stat {}", resolved.display()))?;
            if !meta.is_dir() {
                bail!("root path is not a directory: {}", resolved.display());
            }
            resolved.to_string_lossy().into_owned()
        }
    };

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
            host,
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
    Ok(project)
}

fn forget(state: &State, name: String) -> Result<Project> {
    let mut inner = state.lock().unwrap();
    let idx = inner
        .projects
        .iter()
        .position(|p| p.id == name)
        .ok_or_else(|| anyhow::anyhow!("no project named '{name}'"))?;
    let removed = inner.projects.remove(idx);
    store::save(&inner.projects).context("save project store")?;
    Ok(removed)
}

fn rename(state: &State, from: String, to: String) -> Result<Payload> {
    if from == to {
        bail!("'from' and 'to' are the same");
    }
    if to.is_empty() {
        bail!("new project name must not be empty");
    }

    let old_workspace_name = {
        let inner = state.lock().unwrap();
        if inner.projects.iter().any(|p| p.id == to) {
            bail!("project '{to}' already exists");
        }
        let p = inner
            .projects
            .iter()
            .find(|p| p.id == from)
            .ok_or_else(|| anyhow::anyhow!("no project named '{from}'"))?;
        p.workspace_name.clone()
    };

    let workspaces = match niri_call(NiriRequest::Workspaces)? {
        NiriResponse::Workspaces(ws) => ws,
        other => bail!("unexpected niri response to Workspaces: {other:?}"),
    };
    let niri_ws_renamed = workspaces
        .iter()
        .any(|w| w.name.as_deref() == Some(old_workspace_name.as_str()));

    if niri_ws_renamed {
        let action = NiriAction::SetWorkspaceName {
            name: to.clone(),
            workspace: Some(WorkspaceReferenceArg::Name(old_workspace_name.clone())),
        };
        match niri_call(NiriRequest::Action(action))? {
            NiriResponse::Handled => {}
            other => bail!("unexpected niri response to Action: {other:?}"),
        }
    }

    let mut inner = state.lock().unwrap();
    let p = inner
        .projects
        .iter_mut()
        .find(|p| p.id == from)
        .ok_or_else(|| anyhow::anyhow!("project '{from}' disappeared during rename"))?;
    p.id = to.clone();
    p.name = to.clone();
    if p.workspace_name == from {
        p.workspace_name = to.clone();
    }
    p.ts_last_active = unix_now();
    let project = p.clone();
    store::save(&inner.projects).context("save project store")?;

    Ok(Payload::Renamed {
        project,
        niri_ws_renamed,
    })
}

fn get(state: &State, name: String) -> Result<Project> {
    let inner = state.lock().unwrap();
    inner
        .projects
        .iter()
        .find(|p| p.id == name)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no project named '{name}'"))
}

fn update(state: &State, name: String, spec: ProjectSpec) -> Result<Project> {
    // 1. Existence check (LWW: project may have been forgotten while user was editing).
    let old_workspace_name = {
        let inner = state.lock().unwrap();
        let p = inner
            .projects
            .iter()
            .find(|p| p.id == name)
            .ok_or_else(|| anyhow::anyhow!("project '{name}' no longer exists (forgotten?)"))?;
        p.workspace_name.clone()
    };

    // 2. Validate spec.
    let normalized_spec = validate_spec(state, &name, spec)?;

    // 3. niri lock-step rename if workspace_name changed and old name is in use.
    let workspace_name_changed = old_workspace_name != normalized_spec.workspace_name;
    if workspace_name_changed {
        let workspaces = match niri_call(NiriRequest::Workspaces)? {
            NiriResponse::Workspaces(ws) => ws,
            other => bail!("unexpected niri response to Workspaces: {other:?}"),
        };
        let in_use = workspaces
            .iter()
            .any(|w| w.name.as_deref() == Some(old_workspace_name.as_str()));
        if in_use {
            let action = NiriAction::SetWorkspaceName {
                name: normalized_spec.workspace_name.clone(),
                workspace: Some(WorkspaceReferenceArg::Name(old_workspace_name.clone())),
            };
            match niri_call(NiriRequest::Action(action))? {
                NiriResponse::Handled => {}
                other => bail!("unexpected niri response to SetWorkspaceName: {other:?}"),
            }
        }
    }

    // 4. Apply.
    let mut inner = state.lock().unwrap();
    let p = inner
        .projects
        .iter_mut()
        .find(|p| p.id == name)
        .ok_or_else(|| anyhow::anyhow!("project '{name}' disappeared during update"))?;
    p.apply_spec(normalized_spec);
    p.ts_last_active = unix_now();
    let updated = p.clone();
    store::save(&inner.projects).context("save project store")?;
    Ok(updated)
}

/// Validate a spec against the current project list. Returns a normalized copy
/// (local root paths canonicalized; remote root paths left as-is).
fn validate_spec(state: &State, self_name: &str, mut spec: ProjectSpec) -> Result<ProjectSpec> {
    if spec.workspace_name.is_empty() {
        bail!("workspace_name must not be empty");
    }
    if spec.roots.is_empty() {
        bail!("at least one root is required");
    }
    if spec.default_root >= spec.roots.len() {
        bail!(
            "default_root {} out of range (roots has {} entries)",
            spec.default_root,
            spec.roots.len()
        );
    }

    // Normalize and validate each root.
    for (i, root) in spec.roots.iter_mut().enumerate() {
        if root.path.is_empty() {
            bail!("roots[{i}].path must not be empty");
        }
        match root.host.as_deref() {
            Some("") => bail!("roots[{i}].host is empty; use null for local"),
            Some(_) => {
                // Remote: don't touch the path; we can't stat remote filesystems.
            }
            None => {
                // Local: canonicalize + must be a directory.
                let resolved = fs::canonicalize(&root.path).with_context(|| {
                    format!("roots[{i}].path: resolve {} failed", root.path)
                })?;
                let meta = fs::metadata(&resolved)
                    .with_context(|| format!("roots[{i}].path: stat {} failed", resolved.display()))?;
                if !meta.is_dir() {
                    bail!(
                        "roots[{i}].path is not a directory: {}",
                        resolved.display()
                    );
                }
                root.path = resolved.to_string_lossy().into_owned();
            }
        }
    }

    // workspace_name uniqueness across other projects.
    let inner = state.lock().unwrap();
    if let Some(other) = inner
        .projects
        .iter()
        .find(|p| p.id != self_name && p.workspace_name == spec.workspace_name)
    {
        bail!(
            "workspace_name '{}' already used by project '{}'",
            spec.workspace_name,
            other.id
        );
    }
    Ok(spec)
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

    // Existing project workspace? Just focus it.
    if workspaces
        .iter()
        .any(|w| w.name.as_deref() == Some(project.workspace_name.as_str()))
    {
        let action = NiriAction::FocusWorkspace {
            reference: WorkspaceReferenceArg::Name(project.workspace_name.clone()),
        };
        match niri_call(NiriRequest::Action(action))? {
            NiriResponse::Handled => {}
            other => bail!("unexpected niri response to Action: {other:?}"),
        }

        bump_last_active(state, &name)?;
        return Ok(Payload::Opened {
            project,
            claimed_current: false,
        });
    }

    // Need to claim a fresh workspace. niri keeps a trailing empty unnamed
    // workspace at the bottom of every output — that's our new-workspace slot.
    let target = pick_target_workspace(&workspaces)?;

    // Move focus to it (no-op if we're already there).
    let focused_id = workspaces.iter().find(|w| w.is_focused).map(|w| w.id);
    if Some(target.id) != focused_id {
        let action = NiriAction::FocusWorkspace {
            reference: WorkspaceReferenceArg::Id(target.id),
        };
        match niri_call(NiriRequest::Action(action))? {
            NiriResponse::Handled => {}
            other => bail!("unexpected niri response to FocusWorkspace: {other:?}"),
        }
    }

    // Name the now-focused workspace.
    let action = NiriAction::SetWorkspaceName {
        name: project.workspace_name.clone(),
        workspace: None,
    };
    match niri_call(NiriRequest::Action(action))? {
        NiriResponse::Handled => {}
        other => bail!("unexpected niri response to SetWorkspaceName: {other:?}"),
    }

    spawn_launchers(&project);
    bump_last_active(state, &name)?;

    Ok(Payload::Opened {
        project,
        claimed_current: true,
    })
}

/// Pick the workspace to claim for a new project: prefer the focused workspace
/// if it is itself empty + unnamed; otherwise the bottom-most empty unnamed one
/// on the focused output (niri's auto-managed trailing slot).
fn pick_target_workspace(workspaces: &[Workspace]) -> Result<&Workspace> {
    let focused = workspaces
        .iter()
        .find(|w| w.is_focused)
        .ok_or_else(|| anyhow::anyhow!("niri reports no focused workspace"))?;

    if focused.name.is_none() && focused.active_window_id.is_none() {
        return Ok(focused);
    }

    workspaces
        .iter()
        .filter(|w| {
            w.output == focused.output && w.name.is_none() && w.active_window_id.is_none()
        })
        .max_by_key(|w| w.idx)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no empty unnamed workspace on output {:?}; \
                 niri usually keeps a trailing empty one — check `niri msg workspaces`",
                focused.output
            )
        })
}

fn bump_last_active(state: &State, name: &str) -> Result<()> {
    let mut inner = state.lock().unwrap();
    if let Some(p) = inner.projects.iter_mut().find(|p| p.id == name) {
        p.ts_last_active = unix_now();
    }
    store::save(&inner.projects).context("save project store")
}

fn spawn_launchers(project: &Project) {
    let Some(root) = project.roots.get(project.default_root) else {
        tracing::warn!(project = %project.id, "default_root index out of range");
        return;
    };
    if root.launchers.is_empty() {
        return;
    }
    for launcher in &root.launchers {
        spawn_one(launcher, root);
    }
}

fn spawn_one(launcher: &Launcher, root: &Root) {
    let Some(mut cmd) = launcher_command(launcher, root) else {
        return; // a warn was already logged
    };
    // For local roots, set cwd so the spawned process inherits it.
    // For remote roots, root.path is the remote path; we never chdir locally.
    if root.host.is_none() {
        cmd.current_dir(&root.path);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match cmd.spawn() {
        Ok(child) => {
            let pid = child.id();
            tracing::info!(
                kind = launcher.kind(),
                pid,
                host = root.host.as_deref().unwrap_or("local"),
                path = %root.path,
                "spawned launcher",
            );
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

fn launcher_command(launcher: &Launcher, root: &Root) -> Option<Command> {
    let host = root.host.as_deref();
    let path = root.path.as_str();
    match launcher {
        Launcher::Vscode => {
            let mut c = Command::new("code");
            match host {
                Some(h) => {
                    c.arg("--folder-uri")
                        .arg(format!("vscode-remote://ssh-remote+{h}{path}"));
                }
                None => {
                    c.arg(path);
                }
            }
            Some(c)
        }
        Launcher::Zed => {
            if host.is_some() {
                tracing::warn!(
                    host = host.unwrap_or(""),
                    "zed remote not supported; skipping"
                );
                return None;
            }
            let mut c = Command::new("zed");
            c.arg(path);
            Some(c)
        }
        Launcher::Kitty { run } => Some(kitty_command(host, path, run.as_deref())),
        Launcher::Claude => Some(kitty_command(host, path, Some("claude"))),
        Launcher::Codex => Some(kitty_command(host, path, Some("codex"))),
        Launcher::Browser { url } => {
            let mut c = Command::new("xdg-open");
            c.arg(url);
            Some(c)
        }
        Launcher::Custom { argv } => {
            let mut iter = argv.iter();
            let head = iter.next().map(String::as_str).unwrap_or("true");
            let mut c = Command::new(head);
            c.args(iter);
            Some(c)
        }
    }
}

/// Build a `kitty` command for opening a terminal at `path`, optionally running
/// `run` first. For remote roots we wrap with the kitty ssh kitten, mirroring
/// the user's shell helper `sshp` (kitten ssh -R 7897:127.0.0.1:7890).
fn kitty_command(host: Option<&str>, path: &str, run: Option<&str>) -> Command {
    let mut c = Command::new("kitty");
    match host {
        Some(h) => {
            // sshp-equivalent: forward Clash proxy port to remote.
            const SSHP_PORT_FORWARD: &str = "7897:127.0.0.1:7890";
            let remote_cmd = match run {
                Some(r) => format!("cd {}; {}; exec $SHELL", shell_quote(path), r),
                None => format!("cd {}; exec $SHELL", shell_quote(path)),
            };
            c.arg("+kitten")
                .arg("ssh")
                .arg("-R")
                .arg(SSHP_PORT_FORWARD)
                .arg(h)
                .arg("-t")
                .arg(remote_cmd);
        }
        None => {
            c.arg("--directory").arg(path);
            if let Some(r) = run {
                // Use the user's interactive zsh so .zshrc is sourced — the daemon
                // runs as a systemd user service whose PATH lacks ~/.local/bin etc.
                // After `r` exits, drop into a fresh interactive zsh.
                c.arg("--")
                    .arg("zsh")
                    .arg("-ic")
                    .arg(format!("{r}; exec zsh"));
            }
        }
    }
    c
}

/// Single-quote a path for safe inclusion in a remote shell command.
fn shell_quote(s: &str) -> String {
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

fn status(state: &State) -> Payload {
    let inner = state.lock().unwrap();
    let mut windows: Vec<WindowSummary> = inner
        .claims
        .iter()
        .map(|(id, c)| {
            // project is whichever project's workspace_name matches this
            // window's niri workspace name. Single source of truth: niri.
            let ws_info = c.workspace_id.and_then(|ws_id| inner.workspaces.get(&ws_id));
            let project = ws_info
                .and_then(|w| w.name.as_deref())
                .and_then(|name| {
                    inner
                        .projects
                        .iter()
                        .find(|p| p.workspace_name == name)
                        .map(|p| p.id.clone())
                });
            WindowSummary {
                window_id: *id,
                project,
                app_id: c.app_id.clone(),
                title: c.title.clone(),
                workspace_id: c.workspace_id,
                workspace_idx: ws_info.map(|w| w.idx),
                workspace_name: ws_info.and_then(|w| w.name.clone()),
                column: c.column,
                pid: c.pid,
                cwd: None,
            }
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
        Event::WorkspacesChanged { workspaces } => {
            let mut inner = state.lock().unwrap();
            inner.workspaces = workspaces
                .iter()
                .map(|w| {
                    (
                        w.id,
                        WorkspaceInfo {
                            idx: w.idx,
                            name: w.name.clone(),
                        },
                    )
                })
                .collect();
        }
        Event::WindowsChanged { windows } => {
            let mut new_claims: HashMap<u64, Claim> = HashMap::with_capacity(windows.len());
            for w in &windows {
                new_claims.insert(w.id, build_claim(w));
            }
            let mut inner = state.lock().unwrap();
            inner.claims = new_claims;
            tracing::info!(count = windows.len(), "claims rebuilt from snapshot");
        }
        Event::WindowOpenedOrChanged { window } => {
            let prev_workspace = state
                .lock()
                .unwrap()
                .claims
                .get(&window.id)
                .and_then(|c| c.workspace_id);
            let claim = build_claim(&window);
            let new_workspace = claim.workspace_id;
            state.lock().unwrap().claims.insert(window.id, claim);
            // If the window moved to a different workspace, the old one might be empty now.
            if let Some(old) = prev_workspace {
                if Some(old) != new_workspace {
                    cleanup_workspace_if_empty(state, old);
                }
            }
        }
        Event::WindowClosed { id } => {
            let prev_workspace = state
                .lock()
                .unwrap()
                .claims
                .remove(&id)
                .and_then(|c| c.workspace_id);
            if let Some(ws_id) = prev_workspace {
                cleanup_workspace_if_empty(state, ws_id);
            }
        }
        _ => {
            // Ignore for MVP; later: focus tracking, urgency, etc.
        }
    }
}

/// If `ws_id` carries a project workspace_name and now has zero claims, ask niri
/// to unset its name. Frees the anchor for reuse and keeps niri tidy.
fn cleanup_workspace_if_empty(state: &State, ws_id: u64) {
    let name = {
        let inner = state.lock().unwrap();
        let Some(name) = inner.workspaces.get(&ws_id).and_then(|w| w.name.clone()) else {
            return;
        };
        let is_project_ws = inner.projects.iter().any(|p| p.workspace_name == name);
        if !is_project_ws {
            return;
        }
        let still_has_claims = inner
            .claims
            .values()
            .any(|c| c.workspace_id == Some(ws_id));
        if still_has_claims {
            return;
        }
        name
    };

    let action = NiriAction::UnsetWorkspaceName {
        reference: Some(WorkspaceReferenceArg::Name(name.clone())),
    };
    match niri_call(NiriRequest::Action(action)) {
        Ok(NiriResponse::Handled) => {
            tracing::info!(workspace = %name, "unset name on empty project workspace");
        }
        Ok(other) => {
            tracing::warn!(workspace = %name, response = ?other, "unexpected niri response to UnsetWorkspaceName");
        }
        Err(e) => {
            tracing::warn!(workspace = %name, error = %e, "UnsetWorkspaceName failed");
        }
    }
}

fn build_claim(w: &Window) -> Claim {
    Claim {
        app_id: w.app_id.clone(),
        title: w.title.clone(),
        workspace_id: w.workspace_id,
        column: w.layout.pos_in_scrolling_layout.map(|(c, _)| c),
        pid: w.pid,
    }
}
