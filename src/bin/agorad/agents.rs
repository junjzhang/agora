//! Daemon-level agent operations: dispatch hook events to the right backend,
//! aggregate listings, and route focus requests.
//!
//! Backends own the per-host agent state; this module is the thin layer that
//! decides which backend a request belongs to.

use anyhow::{bail, Result};
use niri_ipc::{
    Action as NiriAction, Request as NiriRequest, Response as NiriResponse, WorkspaceReferenceArg,
};

use agora::model::{AgentCli, AgentSession, RemoteHost};

use crate::backend::ctx::EnrichCtx;
use crate::backend::{AgentBackend, FocusPlan, NotifyRequest, RemoteBackend};
use crate::niri::niri_call;
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

    let (notify, notify_enabled, liveness_action) = {
        let mut inner = state.lock().unwrap();
        let notify = if is_local {
            inner.local.apply_hook(cli_kind, event, payload)
        } else {
            let host = agora_host.as_deref().unwrap();
            let remote = inner.remotes.entry(host.to_string()).or_insert_with(|| {
                tracing::info!(%host, "tracking ephemeral remote backend (not in remotes store)");
                RemoteBackend::new(RemoteHost {
                    host: host.to_string(),
                    remote_socket: String::new(),
                    remote_agora_path: None,
                    auto_connect: false,
                })
            });
            let n = remote.apply_hook(cli_kind, event, payload);
            if remote.is_ephemeral() && remote.is_empty() {
                inner.remotes.remove(host);
                tracing::info!(%host, "dropped empty ephemeral remote backend");
            }
            n
        };
        // Liveness watch is local-only — remote PIDs are on a different host.
        let liveness_action = if is_local {
            liveness_action_for(&inner, event, payload)
        } else {
            None
        };
        (notify, inner.config.notify, liveness_action)
    };

    if let (Some(liveness), Some(action)) = (
        state.lock().unwrap().liveness.clone(),
        liveness_action,
    ) {
        match action {
            LivenessAction::Watch { session_id, pid } => liveness.watch(session_id, pid),
            LivenessAction::Forget { session_id } => liveness.forget(session_id),
        }
    }

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
    let mut inner = state.lock().unwrap();
    inner.local.prune();
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
    // Decide what to focus under the lock, then release before any niri
    // IPC. A slow niri call must not stall hook delivery or picker reads.
    let plan: Option<FocusPlan> = {
        let inner = state.lock().unwrap();
        let ctx = EnrichCtx {
            projects: &inner.projects,
            claims: &inner.claims,
            workspaces: &inner.workspaces,
        };
        if let Some(p) = inner.local.focus_plan(session_id, &ctx)? {
            Some(p)
        } else {
            let mut found: Option<FocusPlan> = None;
            for remote in inner.remotes.values() {
                if let Some(p) = remote.focus_plan(session_id, &ctx)? {
                    found = Some(p);
                    break;
                }
            }
            found
        }
    };

    let Some(plan) = plan else {
        bail!("no agent session '{session_id}' found");
    };

    if let Some(ws_name) = plan.workspace {
        if let Err(e) = niri_call(NiriRequest::Action(NiriAction::FocusWorkspace {
            reference: WorkspaceReferenceArg::Name(ws_name.clone()),
        })) {
            tracing::warn!(workspace = %ws_name, error = %e, "FocusWorkspace failed");
        }
    }
    match niri_call(NiriRequest::Action(NiriAction::FocusWindow { id: plan.window }))? {
        NiriResponse::Handled => Ok(()),
        other => bail!("unexpected niri response to FocusWindow: {other:?}"),
    }
}

/// Synthesize a SessionEnd for a local session whose PID just exited.
/// Called by the liveness-exit thread. Uses the same dispatch path as a
/// real SessionEnd hook would, so the agent is dropped and the cache is
/// rewritten consistently.
pub(crate) fn on_local_session_exit(state: &State, session_id: &str) {
    // Skip if the session is already gone (e.g. real SessionEnd already
    // ran before the pidfd reactor woke up).
    {
        let inner = state.lock().unwrap();
        if !inner.local.agents.contains_key(session_id) {
            return;
        }
    }
    tracing::info!(session = %session_id, "pidfd reported exit; synthesizing SessionEnd");
    let payload = serde_json::json!({ "session_id": session_id });
    // We don't know the original cli kind here without looking it up, but
    // SessionEnd handling doesn't care — it removes by session_id.
    apply_hook(state, "claude", "SessionEnd", &payload);
}

#[derive(Debug)]
enum LivenessAction {
    Watch { session_id: String, pid: i32 },
    Forget { session_id: String },
}

/// Decide whether this hook event should add/remove a pidfd watch.
fn liveness_action_for(
    _inner: &Inner,
    event: &str,
    payload: &serde_json::Value,
) -> Option<LivenessAction> {
    let session_id = payload.get("session_id").and_then(|v| v.as_str())?.to_string();
    match event {
        "SessionStart" => {
            let pid = payload
                .get("agora_pid")
                .and_then(|v| v.as_i64())
                .and_then(|v| i32::try_from(v).ok())?;
            Some(LivenessAction::Watch { session_id, pid })
        }
        "SessionEnd" => Some(LivenessAction::Forget { session_id }),
        _ => None,
    }
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
