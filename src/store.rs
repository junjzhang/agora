//! On-disk persistence for the project list.
//!
//! Single JSON file at `$XDG_DATA_HOME/agora/projects.json` (default
//! `~/.local/share/agora/projects.json`). Atomic write via tmp + rename.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::model::Project;

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
    serde_json::from_str(&buf).with_context(|| format!("parse {}", path.display()))
}

pub fn save(projects: &[Project]) -> Result<()> {
    let path = store_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir -p {}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let buf = serde_json::to_vec_pretty(projects).context("serialize projects")?;
    fs::write(&tmp, &buf).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}
