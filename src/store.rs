//! On-disk persistence for the project list.
//!
//! Single JSON file at `$XDG_DATA_HOME/agora/projects.json` (default
//! `~/.local/share/agora/projects.json`). Atomic write via tmp + rename.
//!
//! The on-disk format is versioned. Bump `STORE_VERSION` whenever the
//! `Project` shape changes incompatibly and add a migration in `load()`.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::model::Project;

pub const STORE_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct Envelope {
    version: u32,
    projects: Vec<Project>,
}

pub fn store_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => {
            let home = std::env::var_os("HOME").context("HOME not set")?;
            PathBuf::from(home).join(".local/share")
        }
    };
    Ok(base.join("agora/projects.json"))
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

    // Try the versioned envelope first.
    if let Ok(env) = serde_json::from_str::<Envelope>(&buf) {
        if env.version > STORE_VERSION {
            bail!(
                "{}: store version {} is newer than this binary supports (max {}); \
                 upgrade agorad or restore an older backup",
                path.display(),
                env.version,
                STORE_VERSION,
            );
        }
        return Ok(env.projects);
    }

    // Fall back to legacy v0: bare array of Project. Migrated on next save().
    let projects: Vec<Project> = serde_json::from_str(&buf)
        .with_context(|| format!("parse {} (tried envelope and legacy array)", path.display()))?;
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
