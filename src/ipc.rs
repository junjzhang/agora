//! Wire protocol between `agora` (CLI) and `agorad` (daemon).
//!
//! JSON-line protocol over a unix socket: client writes one `Request` JSON
//! per line, daemon writes one `Response` JSON per line. Connection closes
//! after each round-trip — no multi-request sessions in MVP.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::model::{AgentSession, Launcher, Project, ProjectSpec, RemoteHost};

pub fn socket_path() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR") {
        if !d.is_empty() {
            return Ok(PathBuf::from(d).join("agora.sock"));
        }
    }
    // Fallback when XDG_RUNTIME_DIR isn't set — common in ssh non-login
    // shells. systemd-logind still creates /run/user/$UID on most systems.
    let uid = unsafe { libc::getuid() };
    let path = PathBuf::from(format!("/run/user/{uid}/agora.sock"));
    let parent_exists = path.parent().map(|p| p.exists()).unwrap_or(false);
    if parent_exists {
        return Ok(path);
    }
    anyhow::bail!(
        "could not locate agora socket: XDG_RUNTIME_DIR unset and {} missing",
        path.parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    );
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Add {
        name: String,
        root_path: String,
        /// Some(host) marks this as a remote root reachable via ssh; daemon
        /// skips local canonicalization in that case.
        #[serde(default)]
        host: Option<String>,
        #[serde(default)]
        launchers: Vec<Launcher>,
    },
    List,
    Open {
        name: String,
    },
    Status,
    Forget {
        name: String,
    },
    Rename {
        from: String,
        to: String,
    },
    /// Read a single project's full record (spec + status).
    Get {
        name: String,
    },
    /// Replace a project's user-editable spec. Daemon validates and applies
    /// niri lock-step rename if `workspace_name` changed.
    Update {
        name: String,
        spec: ProjectSpec,
    },
    /// Promote the currently focused niri workspace into an agora project.
    /// Derives id from `name` arg, then ws name, then basename(root_path).
    Promote {
        name: Option<String>,
        root_path: String,
        host: Option<String>,
        #[serde(default)]
        launchers: Vec<Launcher>,
        /// If the focused ws is already named differently from the resolved id,
        /// rename the ws to match instead of erroring.
        #[serde(default)]
        rename_ws: bool,
    },
    /// Bind the focused niri workspace to an existing project.
    ///
    /// Default direction: `project.workspace_name` is updated to match the
    /// focused ws's current name (project follows ws). With `rename_ws`, the
    /// focused ws is renamed/claimed to the project's `workspace_name`
    /// instead (ws follows project) — same semantics as `agora promote`'s
    /// flag.
    Attach {
        name: String,
        #[serde(default)]
        rename_ws: bool,
    },
    /// Hook event from a CLI agent (Claude Code / Codex). Daemon updates
    /// the per-session AgentState. `payload` is the raw JSON the hook gave
    /// us on stdin — daemon picks out session_id, cwd, event-specific bits.
    Hook {
        cli: String,
        event: String,
        payload: serde_json::Value,
    },
    /// Active agent sessions, with project membership derived per-call.
    Agents,
    /// Register a remote host. Daemon brings up an SSH reverse-forward
    /// tunnel and persists the entry. `remote_uid` is needed to construct the
    /// remote socket path; CLI queries it via `ssh host id -u` before sending.
    RemoteAdd {
        host: String,
        remote_uid: u32,
    },
    RemoteRemove {
        host: String,
    },
    RemoteList,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Ok(Payload),
    Err(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Payload {
    /// Generic acknowledgement for fire-and-forget operations.
    Ack,
    Project(Project),
    Projects(Vec<Project>),
    Opened {
        project: Project,
        /// True if we just named the focused workspace (vs. focusing one that already had the name).
        claimed_current: bool,
    },
    Status {
        project_count: usize,
        windows: Vec<WindowSummary>,
        #[serde(default)]
        agents: Vec<AgentSession>,
    },
    Renamed {
        project: Project,
        /// True if a niri workspace carrying the old name was renamed in lock-step.
        niri_ws_renamed: bool,
    },
    Agents(Vec<AgentSession>),
    Remotes(Vec<RemoteSummary>),
    Remote(RemoteSummary),
}

/// One remote, with both stored config and live tunnel status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteSummary {
    pub host: RemoteHost,
    pub status: RemoteStatus,
    /// Last error string from the tunnel manager, if any (for display).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteStatus {
    Disconnected,
    Connecting,
    Connected,
    Failed,
}

/// One window's view as the daemon sees it. Sent in `Status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowSummary {
    pub window_id: u64,
    pub project: Option<String>,
    pub app_id: Option<String>,
    pub title: Option<String>,
    /// niri's stable workspace id (does not shift on workspace move).
    pub workspace_id: Option<u64>,
    /// niri's per-output 1-based workspace position (shifts on move).
    #[serde(default)]
    pub workspace_idx: Option<u8>,
    /// niri workspace name, if any. Independent from `project` (a named ws
    /// without a matching agora project still has a name).
    #[serde(default)]
    pub workspace_name: Option<String>,
    pub column: Option<usize>,
    pub pid: Option<i32>,
    pub cwd: Option<String>,
}

impl Response {
    pub fn into_result(self) -> Result<Payload> {
        match self {
            Response::Ok(p) => Ok(p),
            Response::Err(msg) => Err(anyhow::anyhow!("daemon: {msg}")),
        }
    }
}

pub fn write_line<W: std::io::Write, T: Serialize>(w: &mut W, value: &T) -> Result<()> {
    let mut buf = serde_json::to_vec(value).context("serialize")?;
    buf.push(b'\n');
    w.write_all(&buf).context("write line")?;
    w.flush().context("flush")?;
    Ok(())
}

pub fn read_line<R: std::io::BufRead, T: for<'de> Deserialize<'de>>(r: &mut R) -> Result<T> {
    let mut buf = String::new();
    let n = r.read_line(&mut buf).context("read line")?;
    if n == 0 {
        anyhow::bail!("connection closed");
    }
    serde_json::from_str(buf.trim_end()).context("parse line")
}
