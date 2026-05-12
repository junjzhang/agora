//! Daemon-level agent operations: dispatch hook events to the right backend,
//! aggregate listings, and route focus requests.
//!
//! Backends own the per-host agent state; this module is the thin layer that
//! decides which backend a request belongs to.

use anyhow::{bail, Result};

use agora::model::{AgentCli, AgentSession, RemoteHost};

use crate::backend::ctx::EnrichCtx;
use crate::backend::{AgentBackend, NotifyRequest, RemoteBackend};
use crate::{Inner, State};

pub(crate) fn apply_hook(state: &State, cli: &str, event: &str, payload: &serde_json::Value) {
    let cli_kind = match cli {
        "claude" => AgentCli::Claude,
        "codex" => AgentCli::Codex,
        other => {
            tracing::warn!(cli = %other, "unknown CLI source in hook event; ignoring");
            return;
        }
    };

    // Inject cli kind into payload so the backend's apply_hook can record it.
    let mut payload = payload.clone();
    if let Some(obj) = payload.as_object_mut() {
        let kind = match cli_kind {
            AgentCli::Claude => "claude",
            AgentCli::Codex => "codex",
        };
        obj.insert(
            "agora_cli_kind".into(),
            serde_json::Value::String(kind.into()),
        );
    }

    let agora_host = payload
        .get("agora_host")
        .and_then(|v| v.as_str())
        .map(String::from);
    let local_hostname = read_local_hostname();
    let is_local = match (agora_host.as_deref(), local_hostname.as_deref()) {
        (None, _) => true,
        (Some(a), Some(l)) if a == l => true,
        _ => false,
    };

    let notify = {
        let mut inner = state.lock().unwrap();
        if is_local {
            inner.local.apply_hook(event, &payload)
        } else {
            let host = agora_host.clone().unwrap();
            let remote = inner.remotes.entry(host.clone()).or_insert_with(|| {
                tracing::info!(%host, "tracking ephemeral remote backend (not in remotes store)");
                RemoteBackend::new(RemoteHost {
                    host: host.clone(),
                    remote_socket: String::new(),
                    remote_agora_path: None,
                    auto_connect: false,
                })
            });
            remote.apply_hook(event, &payload)
        }
    };

    let notify_enabled = state.lock().unwrap().config.notify;
    if notify_enabled {
        if let Some(req) = notify {
            spawn_notification(req);
        }
    }

    if let Err(e) = write_agents_cache(state) {
        tracing::warn!(error = %e, "agents cache write failed");
    }
}

pub(crate) fn list(state: &State) -> Vec<AgentSession> {
    // Prune dead local sessions before listing.
    {
        let mut inner = state.lock().unwrap();
        inner.local.prune();
        for remote in inner.remotes.values_mut() {
            remote.prune();
        }
    }

    let inner = state.lock().unwrap();
    let ctx = EnrichCtx {
        projects: &inner.projects,
        claims: &inner.claims,
        workspaces: &inner.workspaces,
    };
    let mut out = inner.local.list(&ctx);
    for remote in inner.remotes.values() {
        out.extend(remote.list(&ctx));
    }
    out.sort_by(|a, b| {
        let pa = phase_priority(a.phase);
        let pb = phase_priority(b.phase);
        pb.cmp(&pa).then(b.last_change.cmp(&a.last_change))
    });
    out
}

pub(crate) fn focus(state: &State, session_id: &str) -> Result<()> {
    let inner = state.lock().unwrap();
    let ctx = EnrichCtx {
        projects: &inner.projects,
        claims: &inner.claims,
        workspaces: &inner.workspaces,
    };
    if inner.local.focus(session_id, &ctx)? {
        return Ok(());
    }
    for remote in inner.remotes.values() {
        if remote.focus(session_id, &ctx)? {
            return Ok(());
        }
    }
    bail!("no agent session '{session_id}' found");
}

/// Caller holds the lock on `Inner`. Avoids re-locking from inside callers
/// in `actions.rs` that already have the guard.
pub(crate) fn project_has_agent(inner: &Inner, project_id: &str, cli: Option<AgentCli>) -> bool {
    let ctx = EnrichCtx {
        projects: &inner.projects,
        claims: &inner.claims,
        workspaces: &inner.workspaces,
    };
    if inner.local.has_agent(project_id, cli, &ctx) {
        return true;
    }
    inner
        .remotes
        .values()
        .any(|r| r.has_agent(project_id, cli, &ctx))
}

fn phase_priority(phase: agora::model::AgentPhase) -> u8 {
    use agora::model::AgentPhase::*;
    match phase {
        WaitingPermission => 3,
        WaitingInput => 2,
        Running => 1,
        Idle => 0,
    }
}

pub(crate) fn read_local_hostname() -> Option<String> {
    static CACHE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .clone()
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
    use anyhow::Context;
    let Some(path) = agents_cache_path() else {
        return Ok(());
    };
    let snapshot = list(state);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("mkdir -p {}", parent.display()))?;
    }
    let buf = serde_json::to_vec(&snapshot).context("serialize agents")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &buf).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn spawn_notification(req: NotifyRequest) {
    let agora_bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("agora")))
        .unwrap_or_else(|| std::path::PathBuf::from("agora"));
    std::thread::spawn(move || {
        let script = format!(
            "ACTION=$(notify-send -a agora -A focus=Focus '{}' '{}') && [ \"$ACTION\" = focus ] && '{}' focus-agent '{}'",
            req.title.replace('\'', "'\\''"),
            req.body.replace('\'', "'\\''"),
            agora_bin.display(),
            req.session_id,
        );
        let _ = std::process::Command::new("sh")
            .args(["-c", &script])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    });
}
