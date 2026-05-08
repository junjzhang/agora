//! On-disk persistence for the project list.
//!
//! Single JSON file at `$XDG_DATA_HOME/agora/projects.json` (default
//! `~/.local/share/agora/projects.json`). Atomic write via tmp + rename.
//!
//! The on-disk format is versioned. Bump `STORE_VERSION` whenever the
//! `Project` shape changes incompatibly and add a migration in `load()`.

use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{Project, RemoteHost};

pub const STORE_VERSION: u32 = 2;
pub const REMOTES_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct Envelope {
    version: u32,
    projects: Vec<Project>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RemotesEnvelope {
    version: u32,
    remotes: Vec<RemoteHost>,
}

fn data_dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => {
            let home = std::env::var_os("HOME").context("HOME not set")?;
            PathBuf::from(home).join(".local/share")
        }
    };
    Ok(base.join("agora"))
}

pub fn store_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("projects.json"))
}

pub fn remotes_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("remotes.json"))
}

pub fn load() -> Result<Vec<Project>> {
    let path = store_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let buf = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    if buf.trim().is_empty() {
        return Ok(Vec::new());
    }

    let value: Value =
        serde_json::from_str(&buf).with_context(|| format!("parse {}", path.display()))?;

    if let Some(obj) = value.as_object() {
        if let Some(version) = obj.get("version").and_then(Value::as_u64) {
            if version > STORE_VERSION as u64 {
                bail!(
                    "{}: store version {} is newer than this binary supports (max {}); \
                     upgrade agorad or restore an older backup",
                    path.display(),
                    version,
                    STORE_VERSION,
                );
            }
            if version == STORE_VERSION as u64 {
                let env: Envelope = serde_json::from_value(Value::Object(obj.clone()))
                    .with_context(|| format!("parse {}", path.display()))?;
                return Ok(env.projects);
            }
            let projects_value = obj
                .get("projects")
                .cloned()
                .context("store envelope missing projects")?;
            let projects = migrate_projects(projects_value)?;
            tracing::info!(
                version,
                "migrated store launcher format to v{STORE_VERSION} on next write"
            );
            return Ok(projects);
        }
    }

    let projects = migrate_projects(value)?;
    tracing::info!("migrated legacy v0 store to v{STORE_VERSION} on next write");
    Ok(projects)
}

pub fn save(projects: &[Project]) -> Result<()> {
    let path = store_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir -p {}", parent.display()))?;
    }
    let envelope = Envelope {
        version: STORE_VERSION,
        projects: projects.to_vec(),
    };
    let tmp = path.with_extension("json.tmp");
    let buf = serde_json::to_vec_pretty(&envelope).context("serialize projects")?;
    fs::write(&tmp, &buf).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

pub fn load_remotes() -> Result<Vec<RemoteHost>> {
    let path = remotes_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let buf = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    if buf.trim().is_empty() {
        return Ok(Vec::new());
    }
    let env: RemotesEnvelope =
        serde_json::from_str(&buf).with_context(|| format!("parse {}", path.display()))?;
    if env.version > REMOTES_VERSION {
        bail!(
            "{}: remotes store version {} is newer than this binary supports (max {})",
            path.display(),
            env.version,
            REMOTES_VERSION,
        );
    }
    Ok(env.remotes)
}

pub fn save_remotes(remotes: &[RemoteHost]) -> Result<()> {
    let path = remotes_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir -p {}", parent.display()))?;
    }
    let envelope = RemotesEnvelope {
        version: REMOTES_VERSION,
        remotes: remotes.to_vec(),
    };
    let tmp = path.with_extension("json.tmp");
    let buf = serde_json::to_vec_pretty(&envelope).context("serialize remotes")?;
    fs::write(&tmp, &buf).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn migrate_projects(value: Value) -> Result<Vec<Project>> {
    let mut projects = value;
    let arr = projects
        .as_array_mut()
        .context("project store root must be an array")?;
    for project in arr {
        let project_id = project
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>")
            .to_string();
        let Some(roots) = project.get_mut("roots").and_then(Value::as_array_mut) else {
            continue;
        };
        for root in roots {
            let root_path = root
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>")
                .to_string();
            let Some(launchers) = root.get_mut("launchers").and_then(Value::as_array_mut) else {
                continue;
            };
            let legacy = std::mem::take(launchers);
            for launcher in legacy {
                match migrate_launcher(launcher, &project_id, &root_path) {
                    Some(name) => launchers.push(Value::String(name)),
                    None => continue,
                }
            }
        }
    }
    serde_json::from_value(projects).context("deserialize migrated projects")
}

fn migrate_launcher(value: Value, project_id: &str, root_path: &str) -> Option<String> {
    match value {
        Value::String(name) => Some(name),
        Value::Object(obj) => {
            let kind = obj.get("kind").and_then(Value::as_str)?;
            match kind {
                "vscode" | "zed" | "claude" | "codex" => Some(kind.to_string()),
                "kitty" => Some("terminal".to_string()),
                "browser" | "custom" => {
                    tracing::warn!(
                        project = project_id,
                        path = root_path,
                        kind,
                        "dropping unsupported legacy launcher during migration"
                    );
                    None
                }
                other => {
                    tracing::warn!(
                        project = project_id,
                        path = root_path,
                        kind = other,
                        "dropping unknown legacy launcher during migration"
                    );
                    None
                }
            }
        }
        other => {
            tracing::warn!(
                project = project_id,
                path = root_path,
                launcher = %other,
                "dropping malformed launcher during migration"
            );
            None
        }
    }
}
