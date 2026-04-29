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

use agora::ipc::{self, Payload, Request, RemoteStatus, RemoteSummary, Response, WindowSummary};
use agora::model::{
    AgentCli, AgentPhase, AgentSession, Launcher, Project, ProjectSpec, RemoteHost, Root,
};
use agora::store;

#[derive(Default)]
struct Inner {
    projects: Vec<Project>,
    claims: HashMap<u64, Claim>,
    /// niri workspace id → its current (idx, name). Driven by WorkspacesChanged.
    /// idx is per-output, 1-based, and shifts when workspaces are moved.
    workspaces: HashMap<u64, WorkspaceInfo>,
    /// Agent sessions keyed by session_id. Driven by `Request::Hook`.
    agents: HashMap<String, AgentSession>,
    /// Configured remote hosts (persistent).
    remotes: Vec<RemoteHost>,
    /// Live tunnel state per host (in-memory only).
    tunnels: HashMap<String, TunnelState>,
}

#[derive(Debug, Clone)]
struct TunnelState {
    status: ipc::RemoteStatus,
    last_error: Option<String>,
    /// Set when a tunnel manager thread is active. We send () to ask it to
    /// shut down; otherwise it auto-restarts on ssh exit.
    shutdown_tx: Option<std::sync::mpsc::Sender<()>>,
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
    let remotes = store::load_remotes().context("load remotes store")?;
    tracing::info!(count = remotes.len(), "loaded remotes");
    let state: State = Arc::new(Mutex::new(Inner {
        projects,
        claims: HashMap::new(),
        workspaces: HashMap::new(),
        agents: HashMap::new(),
        remotes: remotes.clone(),
        tunnels: HashMap::new(),
    }));

    // Bring up tunnels for any remote with auto_connect = true.
    for r in &remotes {
        if r.auto_connect {
            start_tunnel(&state, r.clone());
        }
    }

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
        Request::Promote {
            name,
            root_path,
            host,
            launchers,
            rename_ws,
        } => Ok(Payload::Project(promote(
            state, name, root_path, host, launchers, rename_ws,
        )?)),
        Request::Attach { name, rename_ws } => {
            Ok(Payload::Project(attach(state, name, rename_ws)?))
        }
        Request::Hook { cli, event, payload } => {
            apply_hook(state, &cli, &event, &payload);
            Ok(Payload::Ack)
        }
        Request::Agents => Ok(Payload::Agents(agents(state))),
        Request::RemoteAdd { host, remote_uid } => {
            Ok(Payload::Remote(remote_add(state, host, remote_uid)?))
        }
        Request::RemoteRemove { host } => {
            remote_remove(state, host)?;
            Ok(Payload::Ack)
        }
        Request::RemoteList => Ok(Payload::Remotes(remote_list(state))),
        Request::FocusAgent { session_id } => {
            focus_agent(state, &session_id)?;
            Ok(Payload::Ack)
        }
    }
}

fn agents(state: &State) -> Vec<AgentSession> {
    let inner = state.lock().unwrap();
    let local_host = read_local_hostname();
    let mut out: Vec<AgentSession> = inner
        .agents
        .values()
        .map(|a| {
            let mut a = a.clone();
            // An agent is "local" when its reported host matches ours (or is
            // unset, for backwards compat with older clients). Otherwise treat
            // it as remote and match against same-host remote roots only.
            let agent_host = match a.host.as_deref() {
                None => None,
                Some(h) if Some(h) == local_host.as_deref() => None,
                Some(h) => Some(h.to_string()),
            };
            a.project = a
                .cwd
                .as_deref()
                .and_then(|c| match_cwd_to_project(c, agent_host.as_deref(), &inner.projects));
            a
        })
        .collect();
    // Priority: WaitingInput > Running > Idle. Within each, MRU.
    out.sort_by(|a, b| {
        let pa = priority(a.phase);
        let pb = priority(b.phase);
        pb.cmp(&pa).then(b.last_change.cmp(&a.last_change))
    });
    out
}

fn priority(phase: AgentPhase) -> u8 {
    match phase {
        AgentPhase::WaitingPermission => 3,
        AgentPhase::WaitingInput => 2,
        AgentPhase::Running => 1,
        AgentPhase::Idle => 0,
    }
}

/// Longest-prefix match: pick the project whose root contains `cwd`.
///
/// `agent_host` selects which roots are eligible:
/// - `None` (local agent): only roots with `host == None` are matched.
/// - `Some(h)` (remote agent): only roots with `host == Some(h)` are matched.
fn match_cwd_to_project(
    cwd: &str,
    agent_host: Option<&str>,
    projects: &[Project],
) -> Option<String> {
    let cwd_path = Path::new(cwd);
    let mut best: Option<(usize, &Project)> = None;
    for p in projects {
        for r in &p.roots {
            let host_matches = match (agent_host, r.host.as_deref()) {
                (None, None) => true,
                (Some(a), Some(r)) => a == r,
                _ => false,
            };
            if !host_matches {
                continue;
            }
            let root = Path::new(&r.path);
            if cwd_path.starts_with(root) {
                let len = root.as_os_str().len();
                if best.is_none_or(|(l, _)| len > l) {
                    best = Some((len, p));
                }
            }
        }
    }
    best.map(|(_, p)| p.id.clone())
}

fn read_local_hostname() -> Option<String> {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn remote_add(state: &State, host_name: String, remote_uid: u32) -> Result<RemoteSummary> {
    if host_name.is_empty() {
        bail!("remote host must not be empty");
    }
    let new_host = RemoteHost {
        host: host_name.clone(),
        remote_socket: format!("/run/user/{remote_uid}/agora.sock"),
        remote_agora_path: None,
        auto_connect: true,
    };
    {
        let mut inner = state.lock().unwrap();
        if let Some(existing) = inner.remotes.iter_mut().find(|r| r.host == host_name) {
            *existing = new_host.clone();
        } else {
            inner.remotes.push(new_host.clone());
        }
        store::save_remotes(&inner.remotes).context("save remotes store")?;
    }
    start_tunnel(state, new_host.clone());
    Ok(remote_summary(state, &host_name))
}

fn remote_remove(state: &State, host_name: String) -> Result<()> {
    let shutdown_tx = {
        let mut inner = state.lock().unwrap();
        let idx = inner
            .remotes
            .iter()
            .position(|r| r.host == host_name)
            .ok_or_else(|| anyhow::anyhow!("no remote named '{host_name}'"))?;
        inner.remotes.remove(idx);
        store::save_remotes(&inner.remotes).context("save remotes store")?;
        inner.tunnels.remove(&host_name).and_then(|t| t.shutdown_tx)
    };
    if let Some(tx) = shutdown_tx {
        let _ = tx.send(());
    }
    Ok(())
}

fn remote_list(state: &State) -> Vec<RemoteSummary> {
    let inner = state.lock().unwrap();
    inner
        .remotes
        .iter()
        .map(|r| {
            let (status, last_error) = inner
                .tunnels
                .get(&r.host)
                .map(|t| (t.status, t.last_error.clone()))
                .unwrap_or((RemoteStatus::Disconnected, None));
            RemoteSummary {
                host: r.clone(),
                status,
                last_error,
            }
        })
        .collect()
}

fn remote_summary(state: &State, host_name: &str) -> RemoteSummary {
    let inner = state.lock().unwrap();
    let host = inner
        .remotes
        .iter()
        .find(|r| r.host == host_name)
        .cloned()
        .unwrap_or_else(|| RemoteHost {
            host: host_name.to_string(),
            remote_socket: String::new(),
            remote_agora_path: None,
            auto_connect: false,
        });
    let (status, last_error) = inner
        .tunnels
        .get(host_name)
        .map(|t| (t.status, t.last_error.clone()))
        .unwrap_or((RemoteStatus::Disconnected, None));
    RemoteSummary {
        host,
        status,
        last_error,
    }
}

/// Spawn a thread that keeps `ssh -N -R` alive for a remote, with exponential
/// backoff on failure. Replaces any existing tunnel for the same host.
fn focus_agent(state: &State, session_id: &str) -> Result<()> {
    let (agent_pid, agent_host) = {
        let inner = state.lock().unwrap();
        let agent = inner
            .agents
            .get(session_id)
            .ok_or_else(|| anyhow::anyhow!("no agent session '{session_id}'"))?;
        (agent.pid, agent.host.clone())
    };

    let local_host = read_local_hostname();
    let is_remote = match (agent_host.as_deref(), local_host.as_deref()) {
        (None, _) => false,
        (Some(a), Some(l)) if a == l => false,
        _ => true,
    };

    if is_remote {
        let host_str = agent_host.as_deref().unwrap_or("");
        // Find the local kitty window whose process tree contains an ssh
        // session to the agent's host — that's the terminal the user
        // interacts with.
        let window_id = {
            let inner = state.lock().unwrap();
            find_window_with_ssh_to(&inner.claims, host_str)
        };
        if let Some(wid) = window_id {
            let action = NiriAction::FocusWindow { id: wid };
            match niri_call(NiriRequest::Action(action))? {
                NiriResponse::Handled => return Ok(()),
                other => bail!("unexpected niri response: {other:?}"),
            }
        }
        bail!(
            "remote agent (host={}); no local terminal with SSH to that host found",
            host_str
        );
    }

    // Local agent: walk PID tree to find the terminal window.
    let pid = agent_pid.ok_or_else(|| {
        anyhow::anyhow!("agent '{session_id}' has no PID; started before hooks were installed?")
    })?;

    let window_id = {
        let inner = state.lock().unwrap();
        find_window_for_pid(&inner.claims, pid)
    };
    let window_id = window_id.ok_or_else(|| {
        anyhow::anyhow!("no niri window found for agent pid {pid} (session '{session_id}')")
    })?;

    let action = NiriAction::FocusWindow { id: window_id };
    match niri_call(NiriRequest::Action(action))? {
        NiriResponse::Handled => {}
        other => bail!("unexpected niri response to FocusWindow: {other:?}"),
    }
    Ok(())
}

/// Walk up the process tree from `start_pid`, checking each PID against
/// known niri window PIDs. Returns the window_id of the first match.
fn find_window_for_pid(claims: &HashMap<u64, Claim>, start_pid: i32) -> Option<u64> {
    let mut pid = start_pid;
    for _ in 0..30 {
        for (wid, claim) in claims {
            if claim.pid == Some(pid) {
                return Some(*wid);
            }
        }
        // Read PPid from /proc
        let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let ppid: i32 = status
            .lines()
            .find(|l| l.starts_with("PPid:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())?;
        if ppid <= 1 {
            break;
        }
        pid = ppid;
    }
    None
}

/// Find a kitty window whose process tree contains an ssh session to `host`.
fn find_window_with_ssh_to(claims: &HashMap<u64, Claim>, host: &str) -> Option<u64> {
    for (wid, claim) in claims {
        if claim.app_id.as_deref() != Some("kitty") {
            continue;
        }
        let Some(pid) = claim.pid else { continue };
        if descendant_has_ssh_to(pid, host, 0) {
            return Some(*wid);
        }
    }
    None
}

fn descendant_has_ssh_to(pid: i32, host: &str, depth: u32) -> bool {
    if depth > 10 {
        return false;
    }
    let Ok(children_str) = fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")) else {
        return false;
    };
    for child_str in children_str.split_whitespace() {
        let Ok(child) = child_str.parse::<i32>() else {
            continue;
        };
        // Check if this child is an ssh process connecting to the host.
        if let Ok(cmdline) = fs::read_to_string(format!("/proc/{child}/cmdline")) {
            let args = cmdline.replace('\0', " ");
            if (args.contains("ssh") || args.contains("kitten")) && args.contains(host) {
                return true;
            }
        }
        if descendant_has_ssh_to(child, host, depth + 1) {
            return true;
        }
    }
    false
}

fn start_tunnel(state: &State, host: RemoteHost) {
    // Tear down any prior tunnel for this host.
    let prev = state.lock().unwrap().tunnels.remove(&host.host);
    if let Some(t) = prev {
        if let Some(tx) = t.shutdown_tx {
            let _ = tx.send(());
        }
    }

    let (tx, rx) = std::sync::mpsc::channel::<()>();
    state.lock().unwrap().tunnels.insert(
        host.host.clone(),
        TunnelState {
            status: RemoteStatus::Connecting,
            last_error: None,
            shutdown_tx: Some(tx),
        },
    );

    let state = state.clone();
    let host_clone = host.clone();
    std::thread::Builder::new()
        .name(format!("ssh-{}", host.host))
        .spawn(move || tunnel_loop(state, host_clone, rx))
        .expect("spawn ssh tunnel thread");
}

fn tunnel_loop(
    state: State,
    host: RemoteHost,
    shutdown_rx: std::sync::mpsc::Receiver<()>,
) {
    let local_socket = match ipc::socket_path() {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => {
            tracing::error!(host = %host.host, error = %e, "cannot resolve local socket");
            set_tunnel_status(&state, &host.host, RemoteStatus::Failed, Some(e.to_string()));
            return;
        }
    };

    let mut backoff_secs: u64 = 1;
    loop {
        // Check for shutdown before each connect attempt.
        if shutdown_rx.try_recv().is_ok() {
            tracing::info!(host = %host.host, "tunnel shutting down");
            set_tunnel_status(&state, &host.host, RemoteStatus::Disconnected, None);
            return;
        }

        let args = build_ssh_args(&host, &local_socket);
        tracing::info!(host = %host.host, ?args, "starting ssh tunnel");
        set_tunnel_status(&state, &host.host, RemoteStatus::Connecting, None);

        let mut child = match Command::new("ssh")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(host = %host.host, error = %e, "ssh spawn failed");
                set_tunnel_status(
                    &state,
                    &host.host,
                    RemoteStatus::Failed,
                    Some(format!("spawn: {e}")),
                );
                if !sleep_or_shutdown(&shutdown_rx, backoff_secs) {
                    return;
                }
                backoff_secs = (backoff_secs * 2).min(60);
                continue;
            }
        };

        // Mark connected after a brief delay (ssh -R doesn't signal readiness).
        // Capture stderr in background so we can include it in last_error.
        let stderr = child.stderr.take();
        let host_for_log = host.host.clone();
        let stderr_thread = stderr.map(|s| {
            std::thread::spawn(move || {
                use std::io::{BufRead, BufReader};
                let mut last = String::new();
                for line in BufReader::new(s).lines().map_while(|l| l.ok()) {
                    tracing::debug!(host = %host_for_log, "ssh stderr: {line}");
                    last = line;
                }
                last
            })
        });

        // Optimistic: assume connected after 1.5s of running ssh.
        std::thread::sleep(std::time::Duration::from_millis(1500));
        if child.try_wait().ok().flatten().is_none() {
            set_tunnel_status(&state, &host.host, RemoteStatus::Connected, None);
            backoff_secs = 1; // reset backoff once we got past handshake
        }

        // Wait for ssh to exit (or external kill).
        let exit = child.wait();
        let last_stderr = stderr_thread.and_then(|h| h.join().ok()).filter(|s| !s.is_empty());

        if shutdown_rx.try_recv().is_ok() {
            // External shutdown: kill ssh just in case it didn't exit.
            let _ = child.kill();
            set_tunnel_status(&state, &host.host, RemoteStatus::Disconnected, None);
            return;
        }

        let err_summary = match exit {
            Ok(s) => format!("ssh exited: {s}"),
            Err(e) => format!("wait failed: {e}"),
        };
        let display_err = last_stderr.clone().unwrap_or_else(|| err_summary.clone());
        tracing::warn!(host = %host.host, "{err_summary}; stderr={:?}", last_stderr);
        set_tunnel_status(
            &state,
            &host.host,
            RemoteStatus::Failed,
            Some(display_err),
        );

        if !sleep_or_shutdown(&shutdown_rx, backoff_secs) {
            return;
        }
        backoff_secs = (backoff_secs * 2).min(60);
    }
}

fn build_ssh_args(host: &RemoteHost, local_socket: &str) -> Vec<String> {
    // The tunnel is a long-lived dedicated connection. Don't use
    // ControlMaster=auto — it would multiplex onto an existing master
    // (e.g. from `agora remote install`) and exit immediately once
    // the -R forward is delegated.
    vec![
        "-N".into(),
        "-T".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ServerAliveInterval=10".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
        "-o".into(),
        "StreamLocalBindUnlink=yes".into(),
        "-o".into(),
        "StreamLocalBindMask=0077".into(),
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        "-o".into(),
        "ControlMaster=no".into(),
        "-R".into(),
        format!("{}:{}", host.remote_socket, local_socket),
        host.host.clone(),
    ]
}

fn set_tunnel_status(
    state: &State,
    host: &str,
    status: RemoteStatus,
    last_error: Option<String>,
) {
    let mut inner = state.lock().unwrap();
    if let Some(t) = inner.tunnels.get_mut(host) {
        t.status = status;
        t.last_error = last_error;
    }
}

/// Sleep for `secs`, returning false if we received a shutdown signal during.
fn sleep_or_shutdown(rx: &std::sync::mpsc::Receiver<()>, secs: u64) -> bool {
    match rx.recv_timeout(std::time::Duration::from_secs(secs)) {
        Ok(()) => false, // shutdown
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => true,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => false,
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

fn promote(
    state: &State,
    name_arg: Option<String>,
    root_path: String,
    host: Option<String>,
    launchers: Vec<Launcher>,
    rename_ws: bool,
) -> Result<Project> {
    // 1. Find the focused niri workspace.
    let workspaces = match niri_call(NiriRequest::Workspaces)? {
        NiriResponse::Workspaces(ws) => ws,
        other => bail!("unexpected niri response to Workspaces: {other:?}"),
    };
    let focused = workspaces
        .iter()
        .find(|w| w.is_focused)
        .ok_or_else(|| anyhow::anyhow!("no focused niri workspace"))?;

    // 2. Resolve the root path (local: stat; remote: pass-through).
    let resolved_path = match host.as_deref() {
        Some("") => bail!("--host is empty; omit it for local"),
        Some(_) => {
            if root_path.is_empty() {
                bail!("root path must not be empty");
            }
            root_path.clone()
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

    // 3. Derive id: --name → focused.name → basename(resolved_path).
    let derived = name_arg
        .clone()
        .or_else(|| focused.name.clone())
        .or_else(|| {
            Path::new(&resolved_path)
                .file_name()
                .and_then(|s| s.to_str())
                .map(String::from)
        })
        .ok_or_else(|| anyhow::anyhow!("could not derive a name; pass --name"))?;
    if derived.is_empty() {
        bail!("derived name is empty; pass --name");
    }

    // 4. Detect ws name conflict (only when --name explicit AND ws already named differently).
    let ws_rename_needed = match (focused.name.as_deref(), name_arg.as_deref()) {
        (Some(ws_name), Some(arg_name)) if ws_name != arg_name => {
            if !rename_ws {
                bail!(
                    "current workspace is named '{ws_name}'; \
                     pass --rename-ws to rename it to '{arg_name}', \
                     or use --name {ws_name} to keep it"
                );
            }
            true
        }
        _ => false,
    };

    // 5. Uniqueness checks (against existing projects).
    {
        let inner = state.lock().unwrap();
        if inner.projects.iter().any(|p| p.id == derived) {
            bail!("project '{derived}' already exists");
        }
        if let Some(other) = inner
            .projects
            .iter()
            .find(|p| p.workspace_name == derived)
        {
            bail!(
                "workspace_name '{derived}' already used by project '{}'",
                other.id
            );
        }
    }

    // 6. Touch niri: claim if unnamed, or rename if --rename-ws.
    let needs_set_name = focused.name.is_none() || ws_rename_needed;
    if needs_set_name {
        let action = NiriAction::SetWorkspaceName {
            name: derived.clone(),
            workspace: Some(WorkspaceReferenceArg::Id(focused.id)),
        };
        match niri_call(NiriRequest::Action(action))? {
            NiriResponse::Handled => {}
            other => bail!("unexpected niri response to SetWorkspaceName: {other:?}"),
        }
    }

    // 7. Persist the new project.
    let mut inner = state.lock().unwrap();
    if inner.projects.iter().any(|p| p.id == derived) {
        bail!("project '{derived}' was created concurrently");
    }
    let now = unix_now();
    let project = Project {
        id: derived.clone(),
        name: derived.clone(),
        workspace_name: derived.clone(),
        roots: vec![Root {
            path: resolved_path,
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

fn attach(state: &State, project_name: String, rename_ws: bool) -> Result<Project> {
    // 1. Project must exist.
    let current_workspace_name = {
        let inner = state.lock().unwrap();
        inner
            .projects
            .iter()
            .find(|p| p.id == project_name)
            .map(|p| p.workspace_name.clone())
            .ok_or_else(|| anyhow::anyhow!("no project named '{project_name}'"))?
    };

    // 2. Find the focused niri workspace.
    let workspaces = match niri_call(NiriRequest::Workspaces)? {
        NiriResponse::Workspaces(ws) => ws,
        other => bail!("unexpected niri response to Workspaces: {other:?}"),
    };
    let focused = workspaces
        .iter()
        .find(|w| w.is_focused)
        .ok_or_else(|| anyhow::anyhow!("no focused niri workspace"))?;

    // 3. Pick direction.
    let new_workspace_name = if rename_ws {
        // ws follows project: SetWorkspaceName(focused, project.workspace_name).
        // No-op if focused already named that. Niri rejects if another ws holds the name.
        if focused.name.as_deref() != Some(current_workspace_name.as_str()) {
            let action = NiriAction::SetWorkspaceName {
                name: current_workspace_name.clone(),
                workspace: Some(WorkspaceReferenceArg::Id(focused.id)),
            };
            match niri_call(NiriRequest::Action(action))? {
                NiriResponse::Handled => {}
                other => bail!("unexpected niri response to SetWorkspaceName: {other:?}"),
            }
        }
        current_workspace_name.clone()
    } else {
        // project follows ws: project.workspace_name = focused.name.
        let ws_name = focused.name.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "focused workspace is unnamed; pass --rename-ws to claim it as '{current_workspace_name}'"
            )
        })?;
        // Reject if another project already uses this workspace_name.
        let inner = state.lock().unwrap();
        if let Some(other) = inner
            .projects
            .iter()
            .find(|p| p.id != project_name && p.workspace_name == ws_name)
        {
            bail!(
                "workspace_name '{}' already used by project '{}'",
                ws_name,
                other.id
            );
        }
        ws_name
    };

    // 4. Persist.
    let mut inner = state.lock().unwrap();
    let p = inner
        .projects
        .iter_mut()
        .find(|p| p.id == project_name)
        .ok_or_else(|| anyhow::anyhow!("project '{project_name}' disappeared during attach"))?;
    p.workspace_name = new_workspace_name;
    p.ts_last_active = unix_now();
    let updated = p.clone();
    store::save(&inner.projects).context("save project store")?;
    Ok(updated)
}

/// Apply a hook event to the agent state machine. Fire-and-forget; we never
/// block the caller (hook scripts return immediately). On unknown event names
/// we just record `last_event` and don't change phase.
fn apply_hook(state: &State, cli_str: &str, event: &str, payload: &serde_json::Value) {
    apply_hook_inner(state, cli_str, event, payload);
    // Drive the bar widget (and any other FileView watcher) by mirroring the
    // Agents IPC payload to a cache file. Best effort.
    if let Err(e) = write_agents_cache(state) {
        tracing::warn!(error = %e, "agents cache write failed");
    }
}

fn agents_cache_path() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    let base = match std::env::var_os("XDG_CACHE_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => {
            let home = std::env::var_os("HOME")?;
            PathBuf::from(home).join(".cache")
        }
    };
    Some(base.join("agora/agents.json"))
}

fn write_agents_cache(state: &State) -> Result<()> {
    let Some(path) = agents_cache_path() else {
        return Ok(());
    };
    let snapshot = agents(state);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir -p {}", parent.display()))?;
    }
    let buf = serde_json::to_vec(&snapshot).context("serialize agents")?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &buf).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn apply_hook_inner(
    state: &State,
    cli_str: &str,
    event: &str,
    payload: &serde_json::Value,
) {
    let cli = match cli_str {
        "claude" => AgentCli::Claude,
        "codex" => AgentCli::Codex,
        other => {
            tracing::warn!(cli = %other, "unknown CLI source in hook event; ignoring");
            return;
        }
    };
    let Some(session_id) = payload
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(String::from)
    else {
        tracing::warn!(event = %event, "hook payload missing session_id; ignoring");
        return;
    };
    let cwd = payload
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(String::from);
    let host = payload
        .get("agora_host")
        .and_then(|v| v.as_str())
        .map(String::from);
    let pid = payload
        .get("agora_pid")
        .and_then(|v| v.as_i64())
        .and_then(|v| i32::try_from(v).ok());

    // Notification message: a short reason the agent wants attention.
    let message = if event == "Notification" {
        payload
            .get("message")
            .and_then(|v| v.as_str())
            .map(|s| {
                let trimmed = s.trim();
                if trimmed.len() > 200 {
                    format!("{}…", &trimmed[..200])
                } else {
                    trimmed.to_string()
                }
            })
    } else {
        None
    };

    let now = unix_now();
    let mut inner = state.lock().unwrap();

    // SessionEnd → forget the session.
    if event == "SessionEnd" {
        if inner.agents.remove(&session_id).is_some() {
            tracing::info!(session = %session_id, "agent session ended");
        }
        return;
    }

    let entry = inner.agents.entry(session_id.clone()).or_insert_with(|| {
        tracing::info!(session = %session_id, %cli_str, "agent session registered");
        AgentSession {
            session_id: session_id.clone(),
            cli,
            phase: AgentPhase::Idle,
            cwd: cwd.clone(),
            last_event: None,
            last_message: None,
            last_change: now,
            project: None,
            host: host.clone(),
            pid,
        }
    });

    if cwd.is_some() {
        entry.cwd = cwd;
    }
    if host.is_some() {
        entry.host = host;
    }
    if pid.is_some() {
        entry.pid = pid;
    }
    entry.last_event = Some(event.to_string());

    let new_phase = match event {
        "SessionStart" => AgentPhase::Idle,
        "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "PreCompact" => AgentPhase::Running,
        "Notification" => AgentPhase::WaitingInput,
        "PermissionRequest" => AgentPhase::WaitingPermission,
        "Stop" | "SubagentStop" => AgentPhase::Idle,
        _ => entry.phase,
    };

    if let Some(msg) = message {
        entry.last_message = Some(msg);
    } else if event == "UserPromptSubmit" {
        entry.last_message = None;
    }

    if entry.phase != new_phase {
        tracing::info!(
            session = %session_id,
            from = ?entry.phase,
            to = ?new_phase,
            event = %event,
            "agent phase change",
        );
        entry.phase = new_phase;
        entry.last_change = now;
    }
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
    let mut agents: Vec<AgentSession> = inner.agents.values().cloned().collect();
    agents.sort_by(|a, b| b.last_change.cmp(&a.last_change));
    Payload::Status {
        project_count: inner.projects.len(),
        windows,
        agents,
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
