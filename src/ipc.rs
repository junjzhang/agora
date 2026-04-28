//! Wire protocol between `agora` (CLI) and `agorad` (daemon).
//!
//! JSON-line protocol over a unix socket: client writes one `Request` JSON
//! per line, daemon writes one `Response` JSON per line. Connection closes
//! after each round-trip — no multi-request sessions in MVP.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::model::{AgentSession, Launcher, Project, ProjectSpec};

pub fn socket_path() -> Result<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR not set")?;
    if dir.is_empty() {
        anyhow::bail!("XDG_RUNTIME_DIR is empty");
    }
    Ok(PathBuf::from(dir).join("agora.sock"))
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
