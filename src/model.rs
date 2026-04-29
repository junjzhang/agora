//! Core data model.
//!
//! Mirrors the structures defined in VISION.html §3.3.

use serde::{Deserialize, Serialize};

pub type ProjectId = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    pub roots: Vec<Root>,
    pub default_root: usize,
    pub workspace_name: String,
    #[serde(default)]
    pub pinned: bool,
    pub ts_created: u64,
    pub ts_last_active: u64,
    #[serde(default)]
    pub archived_at: Option<u64>,
}

/// User-editable view of a Project.
///
/// `Project` carries both the user's intent (this struct) and runtime metadata
/// the daemon owns (id, ts_*, archived_at). When a client reads/writes via
/// `agora edit` or a future GUI, it deals only with `ProjectSpec` — runtime
/// fields can't be tampered with, and the editor view stays focused.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSpec {
    pub workspace_name: String,
    pub roots: Vec<Root>,
    pub default_root: usize,
    #[serde(default)]
    pub pinned: bool,
}

impl Project {
    pub fn spec(&self) -> ProjectSpec {
        ProjectSpec {
            workspace_name: self.workspace_name.clone(),
            roots: self.roots.clone(),
            default_root: self.default_root,
            pinned: self.pinned,
        }
    }

    pub fn apply_spec(&mut self, spec: ProjectSpec) {
        self.workspace_name = spec.workspace_name;
        self.roots = spec.roots;
        self.default_root = spec.default_root;
        self.pinned = spec.pinned;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Root {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub launchers: Vec<Launcher>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Launcher {
    Vscode,
    Zed,
    Kitty {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run: Option<String>,
    },
    Claude,
    Codex,
    Browser {
        url: String,
    },
    Custom {
        argv: Vec<String>,
    },
}

impl Launcher {
    pub fn kind(&self) -> &'static str {
        match self {
            Launcher::Vscode => "vscode",
            Launcher::Zed => "zed",
            Launcher::Kitty { .. } => "kitty",
            Launcher::Claude => "claude",
            Launcher::Codex => "codex",
            Launcher::Browser { .. } => "browser",
            Launcher::Custom { .. } => "custom",
        }
    }
}

/// A host the daemon keeps an SSH reverse-forward tunnel to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteHost {
    /// SSH alias / hostname. Resolved via the user's ~/.ssh/config.
    pub host: String,
    /// Remote-side socket path. Filled at `remote add` time by `ssh host id -u`.
    pub remote_socket: String,
    /// Optional override of the remote agora binary path. Defaults to
    /// `~/.local/bin/agora` resolved via the remote shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_agora_path: Option<String>,
    /// Whether daemon should auto-bring-up the tunnel on startup.
    #[serde(default = "default_true")]
    pub auto_connect: bool,
}

fn default_true() -> bool {
    true
}

/// Which CLI agent this session belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentCli {
    Claude,
    Codex,
}

/// Coarse-grained agent state. v1 derives this from Claude Code hook events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPhase {
    /// Session known to exist; no recent user prompt or finished turn.
    Idle,
    /// User prompt submitted, agent processing, no `Stop` yet.
    Running,
    /// Agent emitted a Notification — almost always means it wants attention.
    WaitingInput,
}

/// One agent session as the daemon tracks it. Keyed by `session_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    pub session_id: String,
    pub cli: AgentCli,
    pub phase: AgentPhase,
    /// `cwd` reported by the hook payload. Used to derive project membership.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Last hook event seen, for debugging.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event: Option<String>,
    /// Most recent Notification message (truncated upstream).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message: Option<String>,
    /// Unix timestamp of last state change.
    pub last_change: u64,
    /// Project id derived at IPC-response time from cwd vs project roots.
    /// Always None inside the daemon's in-memory map; only filled for clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Hostname reported by the hook adapter. Local sessions report the local
    /// hostname; remote sessions (via SSH reverse forward) report their own.
    /// Empty / None when adapter didn't include it (older clients).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}
