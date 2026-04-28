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
