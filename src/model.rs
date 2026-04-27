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
