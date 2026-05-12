//! Local agent backend — tracks sessions running on this machine.

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

use agora::model::{AgentCli, AgentPhase, AgentSession};

use crate::backend::ctx::EnrichCtx;
use crate::backend::{has_matching_agent, AgentBackend, FocusPlan, NotifyRequest};
use crate::launcher::truncate_str;
use crate::Claim;

#[derive(Default)]
pub(crate) struct LocalBackend {
    pub agents: HashMap<String, AgentSession>,
}

impl LocalBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop sessions whose PID no longer exists in /proc. Unique to the
    /// local backend — remote PIDs live on the other host and can't be
    /// checked from here.
    pub fn prune(&mut self) {
        let dead: Vec<String> = self
            .agents
            .values()
            .filter_map(|a| {
                let pid = a.pid?;
                if Path::new(&format!("/proc/{pid}")).exists() {
                    None
                } else {
                    Some(a.session_id.clone())
                }
            })
            .collect();
        for sid in dead {
            if self.agents.remove(&sid).is_some() {
                tracing::info!(session = %sid, "pruned dead local session");
            }
        }
    }
}

impl AgentBackend for LocalBackend {
    fn apply_hook(
        &mut self,
        cli: AgentCli,
        event: &str,
        payload: &Value,
    ) -> Option<NotifyRequest> {
        apply_hook(&mut self.agents, cli, event, payload)
    }

    fn list(&self, ctx: &EnrichCtx) -> Vec<AgentSession> {
        self.agents.values().map(|a| enrich(a, ctx)).collect()
    }

    fn focus_plan(&self, session_id: &str, ctx: &EnrichCtx) -> anyhow::Result<Option<FocusPlan>> {
        let Some(agent) = self.agents.get(session_id) else {
            return Ok(None);
        };
        let pid = agent.pid.ok_or_else(|| {
            anyhow::anyhow!("local agent '{session_id}' has no PID; hooks installed after start?")
        })?;
        let window = find_window_for_pid(ctx.claims, pid).ok_or_else(|| {
            anyhow::anyhow!(
                "no niri window found for local agent pid {pid} (session '{session_id}')"
            )
        })?;
        Ok(Some(FocusPlan {
            workspace: None,
            window,
        }))
    }

    fn has_agent(&self, project_id: &str, cli: Option<AgentCli>, ctx: &EnrichCtx) -> bool {
        has_matching_agent(&self.agents, project_id, cli, None, ctx.projects)
    }

    fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }
}

fn enrich(a: &AgentSession, ctx: &EnrichCtx) -> AgentSession {
    let mut a = a.clone();
    a.project = a
        .cwd
        .as_deref()
        .and_then(|c| match_cwd_to_project(c, None, ctx.projects));
    if let Some(pid) = a.pid {
        if let Some(wid) = find_window_for_pid(ctx.claims, pid) {
            let ws_id = ctx.claims.get(&wid).and_then(|c| c.workspace_id);
            if let Some(ws_id) = ws_id {
                a.workspace = ctx.workspaces.get(&ws_id).and_then(|w| w.name.clone());
            }
        }
    }
    a
}

/// Shared hook-event application logic. Owns no state — operates on a
/// passed-in agents map. Used by both LocalBackend and RemoteBackend.
pub(crate) fn apply_hook(
    agents: &mut HashMap<String, AgentSession>,
    cli: AgentCli,
    event: &str,
    payload: &Value,
) -> Option<NotifyRequest> {
    let session_id = payload
        .get("session_id")
        .and_then(|v| v.as_str())?
        .to_string();
    let cwd = payload
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(String::from);
    let host = payload
        .get("agora_host")
        .and_then(|v| v.as_str())
        .map(String::from);
    let pid = payload
        .get("agora_pid")
        .and_then(|v| v.as_i64())
        .and_then(|v| i32::try_from(v).ok());
    let slug = payload
        .get("agora_slug")
        .and_then(|v| v.as_str())
        .map(String::from);
    let model = payload
        .get("agora_model")
        .or_else(|| payload.get("model"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let effort = payload
        .get("agora_effort")
        .and_then(|v| v.as_str())
        .map(String::from);
    let tool_name = payload
        .get("tool_name")
        .and_then(|v| v.as_str())
        .map(String::from);

    let message = if event == "Notification" {
        payload
            .get("message")
            .and_then(|v| v.as_str())
            .map(|s| truncate_str(s.trim(), 200))
    } else {
        None
    };

    let prompt = if event == "UserPromptSubmit" {
        payload
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(|s| truncate_str(s.trim(), 120))
    } else {
        None
    };

    let now = crate::niri::unix_now();

    if event == "SessionEnd" {
        if agents.remove(&session_id).is_some() {
            tracing::info!(session = %session_id, "agent session ended");
        }
        return None;
    }

    let entry = agents.entry(session_id.clone()).or_insert_with(|| {
        let cli_label = match cli {
            AgentCli::Claude => "claude",
            AgentCli::Codex => "codex",
        };
        tracing::info!(session = %session_id, cli = %cli_label, "agent session registered");
        AgentSession {
            session_id: session_id.clone(),
            cli,
            phase: AgentPhase::Idle,
            cwd: cwd.clone(),
            last_event: None,
            last_message: None,
            last_prompt: None,
            slug: slug.clone(),
            workspace: None,
            last_change: now,
            project: None,
            host: host.clone(),
            pid,
            model: None,
            started_at: Some(now),
            turn_count: 0,
            current_tool: None,
            effort: None,
        }
    });

    if cwd.is_some() {
        entry.cwd = cwd;
    }
    if host.is_some() {
        entry.host = host;
    }
    if pid.is_some() {
        entry.pid = pid;
    }
    if slug.is_some() {
        entry.slug = slug;
    }
    if model.is_some() {
        entry.model = model;
    }
    if effort.is_some() {
        entry.effort = effort;
    }
    if event == "SessionStart" && entry.started_at.is_none() {
        entry.started_at = Some(now);
    }
    if event == "UserPromptSubmit" {
        entry.turn_count += 1;
    }
    match event {
        "PreToolUse" => entry.current_tool = tool_name,
        "PostToolUse" | "Stop" | "SubagentStop" => entry.current_tool = None,
        _ => {}
    }
    entry.last_event = Some(event.to_string());

    let new_phase = match event {
        "SessionStart" => AgentPhase::Idle,
        "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "PreCompact" => AgentPhase::Running,
        "Notification" => AgentPhase::WaitingInput,
        "PermissionRequest" => AgentPhase::WaitingPermission,
        "Stop" | "SubagentStop" => AgentPhase::Idle,
        _ => entry.phase,
    };

    if let Some(msg) = message {
        entry.last_message = Some(msg);
    } else if event == "UserPromptSubmit" {
        entry.last_message = None;
    }
    if let Some(p) = prompt {
        entry.last_prompt = Some(p);
    }

    let old_phase = entry.phase;
    if old_phase != new_phase {
        tracing::info!(
            session = %session_id,
            from = ?old_phase,
            to = ?new_phase,
            event = %event,
            "agent phase change",
        );
        entry.phase = new_phase;
        entry.last_change = now;
    }

    let attention = old_phase != new_phase
        && old_phase == AgentPhase::Running
        && matches!(
            new_phase,
            AgentPhase::WaitingInput | AgentPhase::WaitingPermission
        );
    if !attention {
        return None;
    }

    let entry = agents.get(&session_id)?;
    let name = entry
        .slug
        .as_deref()
        .unwrap_or(&session_id[..8.min(session_id.len())]);
    let cli_label = match entry.cli {
        AgentCli::Claude => "claude",
        AgentCli::Codex => "codex",
    };
    let title = format!("agora: {name} ({cli_label})");
    let body = match new_phase {
        AgentPhase::WaitingInput => entry
            .last_message
            .clone()
            .unwrap_or_else(|| "Needs your input".into()),
        AgentPhase::WaitingPermission => "Needs permission to proceed".into(),
        _ => return None,
    };
    Some(NotifyRequest {
        title,
        body,
        session_id: session_id.clone(),
    })
}

pub(crate) fn match_cwd_to_project(
    cwd: &str,
    agent_host: Option<&str>,
    projects: &[agora::model::Project],
) -> Option<String> {
    let cwd_path = Path::new(cwd);
    let mut best: Option<(usize, &agora::model::Project)> = None;
    for p in projects {
        for r in &p.roots {
            let host_matches = match (agent_host, r.host.as_deref()) {
                (None, None) => true,
                (Some(a), Some(r)) => a == r,
                _ => false,
            };
            if !host_matches {
                continue;
            }
            let root = Path::new(&r.path);
            if cwd_path.starts_with(root) {
                let len = root.as_os_str().len();
                if best.is_none_or(|(l, _)| len > l) {
                    best = Some((len, p));
                }
            }
        }
    }
    best.map(|(_, p)| p.id.clone())
}

pub(crate) fn find_window_for_pid(claims: &HashMap<u64, Claim>, start_pid: i32) -> Option<u64> {
    let mut pid = start_pid;
    for _ in 0..30 {
        for (wid, claim) in claims {
            if claim.pid == Some(pid) {
                return Some(*wid);
            }
        }
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let ppid: i32 = status
            .lines()
            .find(|l| l.starts_with("PPid:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())?;
        if ppid <= 1 {
            break;
        }
        pid = ppid;
    }
    None
}
