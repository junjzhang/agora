use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use niri_ipc::{
    Action as NiriAction, Request as NiriRequest, Response as NiriResponse, WorkspaceReferenceArg,
};

use agora::model::{AgentCli, AgentPhase, AgentSession, Project};

use crate::launcher::truncate_str;
use crate::niri::niri_call;
use crate::{Claim, Inner, State};

pub(crate) fn agents(state: &State) -> Vec<AgentSession> {
    let inner = state.lock().unwrap();
    let local_host = read_local_hostname();
    let mut out: Vec<AgentSession> = inner
        .agents
        .values()
        .map(|a| {
            let mut a = a.clone();
            let agent_host = match a.host.as_deref() {
                None => None,
                Some(h) if Some(h) == local_host.as_deref() => None,
                Some(h) => Some(h.to_string()),
            };
            a.project = a
                .cwd
                .as_deref()
                .and_then(|c| match_cwd_to_project(c, agent_host.as_deref(), &inner.projects));
            if agent_host.is_none() {
                if let Some(pid) = a.pid {
                    if let Some(wid) = find_window_for_pid(&inner.claims, pid) {
                        let ws_id = inner.claims.get(&wid).and_then(|c| c.workspace_id);
                        if let Some(ws_id) = ws_id {
                            a.workspace = inner.workspaces.get(&ws_id).and_then(|w| w.name.clone());
                        }
                    }
                }
            } else if let Some(ref proj_id) = a.project {
                a.workspace = inner
                    .projects
                    .iter()
                    .find(|p| p.id == *proj_id)
                    .map(|p| p.workspace_name.clone());
            }
            a
        })
        .collect();
    out.sort_by(|a, b| {
        let pa = priority(a.phase);
        let pb = priority(b.phase);
        pb.cmp(&pa).then(b.last_change.cmp(&a.last_change))
    });
    out
}

pub(crate) fn apply_hook(state: &State, cli_str: &str, event: &str, payload: &serde_json::Value) {
    apply_hook_inner(state, cli_str, event, payload);
    if let Err(e) = write_agents_cache(state) {
        tracing::warn!(error = %e, "agents cache write failed");
    }
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
    let Some(path) = agents_cache_path() else {
        return Ok(());
    };
    let snapshot = agents(state);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir -p {}", parent.display()))?;
    }
    let buf = serde_json::to_vec(&snapshot).context("serialize agents")?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &buf).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn apply_hook_inner(state: &State, cli_str: &str, event: &str, payload: &serde_json::Value) {
    let cli = match cli_str {
        "claude" => AgentCli::Claude,
        "codex" => AgentCli::Codex,
        other => {
            tracing::warn!(cli = %other, "unknown CLI source in hook event; ignoring");
            return;
        }
    };
    let Some(session_id) = payload
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(String::from)
    else {
        tracing::warn!(event = %event, "hook payload missing session_id; ignoring");
        return;
    };
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
    let mut inner = state.lock().unwrap();

    if event == "SessionEnd" {
        if inner.agents.remove(&session_id).is_some() {
            tracing::info!(session = %session_id, "agent session ended");
        }
        return;
    }

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

    let entry = inner.agents.entry(session_id.clone()).or_insert_with(|| {
        tracing::info!(session = %session_id, %cli_str, "agent session registered");
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

    let should_notify = inner.config.notify
        && old_phase != new_phase
        && old_phase == AgentPhase::Running
        && matches!(
            new_phase,
            AgentPhase::WaitingInput | AgentPhase::WaitingPermission | AgentPhase::Idle
        );
    if !should_notify {
        return;
    }

    let entry = inner.agents.get(&session_id).unwrap();
    let proj = entry.cwd.as_deref().and_then(|c| {
        let host = match entry.host.as_deref() {
            None => None,
            Some(h) if Some(h) == read_local_hostname().as_deref() => None,
            Some(h) => Some(h),
        };
        match_cwd_to_project(c, host, &inner.projects)
    });
    let name = proj
        .as_deref()
        .or(entry.slug.as_deref())
        .unwrap_or(&session_id[..8.min(session_id.len())]);
    let cli_label = match entry.cli {
        AgentCli::Claude => "claude",
        AgentCli::Codex => "codex",
    };
    let title = format!("agora: {name} ({cli_label})");
    let body: String = match new_phase {
        AgentPhase::WaitingInput => entry
            .last_message
            .clone()
            .unwrap_or_else(|| "Needs your input".into()),
        AgentPhase::WaitingPermission => "Needs permission to proceed".into(),
        AgentPhase::Idle => "Turn complete".into(),
        _ => return,
    };
    let sid = session_id.clone();
    let agora_bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("agora")))
        .unwrap_or_else(|| std::path::PathBuf::from("agora"));
    std::thread::spawn(move || {
        let script = format!(
            "ACTION=$(notify-send -a agora -A focus=Focus '{}' '{}') && [ \"$ACTION\" = focus ] && '{}' focus-agent '{}'",
            title.replace('\'', "'\\''"),
            body.replace('\'', "'\\''"),
            agora_bin.display(),
            sid,
        );
        let _ = std::process::Command::new("sh")
            .args(["-c", &script])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    });
}

pub(crate) fn focus_agent(state: &State, session_id: &str) -> Result<()> {
    let (agent_pid, agent_host) = {
        let inner = state.lock().unwrap();
        let agent = inner
            .agents
            .get(session_id)
            .ok_or_else(|| anyhow::anyhow!("no agent session '{session_id}'"))?;
        (agent.pid, agent.host.clone())
    };

    let local_host = read_local_hostname();
    let is_remote = match (agent_host.as_deref(), local_host.as_deref()) {
        (None, _) => false,
        (Some(a), Some(l)) if a == l => false,
        _ => true,
    };

    if is_remote {
        let ws_name = {
            let inner = state.lock().unwrap();
            let agent = inner.agents.get(session_id);
            let project_id = agent.and_then(|a| a.project.clone()).or_else(|| {
                let a = agent?;
                match_cwd_to_project(a.cwd.as_deref()?, a.host.as_deref(), &inner.projects)
            });
            project_id.and_then(|pid| {
                inner
                    .projects
                    .iter()
                    .find(|p| p.id == pid)
                    .map(|p| p.workspace_name.clone())
            })
        };
        if let Some(name) = ws_name {
            let action = NiriAction::FocusWorkspace {
                reference: WorkspaceReferenceArg::Name(name),
            };
            match niri_call(NiriRequest::Action(action))? {
                NiriResponse::Handled => return Ok(()),
                other => bail!("unexpected niri response: {other:?}"),
            }
        }
        let host_str = agent_host.as_deref().unwrap_or("");
        let window_id = {
            let inner = state.lock().unwrap();
            find_window_with_ssh_to(&inner.claims, host_str)
        };
        if let Some(wid) = window_id {
            let action = NiriAction::FocusWindow { id: wid };
            match niri_call(NiriRequest::Action(action))? {
                NiriResponse::Handled => return Ok(()),
                other => bail!("unexpected niri response: {other:?}"),
            }
        }
        bail!(
            "remote agent (host={}); no workspace or terminal found",
            host_str
        );
    }

    let pid = agent_pid.ok_or_else(|| {
        anyhow::anyhow!("agent '{session_id}' has no PID; started before hooks were installed?")
    })?;

    let window_id = {
        let inner = state.lock().unwrap();
        find_window_for_pid(&inner.claims, pid)
    };
    let window_id = window_id.ok_or_else(|| {
        anyhow::anyhow!("no niri window found for agent pid {pid} (session '{session_id}')")
    })?;

    let action = NiriAction::FocusWindow { id: window_id };
    match niri_call(NiriRequest::Action(action))? {
        NiriResponse::Handled => {}
        other => bail!("unexpected niri response to FocusWindow: {other:?}"),
    }
    Ok(())
}

pub(crate) fn find_window_for_pid(claims: &HashMap<u64, Claim>, start_pid: i32) -> Option<u64> {
    let mut pid = start_pid;
    for _ in 0..30 {
        for (wid, claim) in claims {
            if claim.pid == Some(pid) {
                return Some(*wid);
            }
        }
        let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
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

pub(crate) fn find_leaf_child(mut pid: i32) -> i32 {
    for _ in 0..20 {
        let Ok(children_str) = fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        else {
            break;
        };
        let children: Vec<i32> = children_str
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        if children.len() != 1 {
            break;
        }
        pid = children[0];
    }
    pid
}

pub(crate) fn find_ssh_host_in_children(pid: i32) -> Option<String> {
    find_ssh_host_recursive(pid, 0)
}

fn find_ssh_host_recursive(pid: i32, depth: u32) -> Option<String> {
    if depth > 10 {
        return None;
    }
    let children_str = fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).ok()?;
    for child_str in children_str.split_whitespace() {
        let Some(child) = child_str.parse::<i32>().ok() else {
            continue;
        };
        let Ok(cmdline) = fs::read_to_string(format!("/proc/{child}/cmdline")) else {
            continue;
        };
        let args: Vec<&str> = cmdline.split('\0').filter(|s| !s.is_empty()).collect();
        let bin = args.first().copied().unwrap_or("");
        let is_ssh = bin.ends_with("ssh")
            || (bin.ends_with("kitten") && args.get(1).copied() == Some("ssh"));
        if is_ssh {
            return args
                .iter()
                .rev()
                .find(|a| !a.starts_with('-') && !a.contains(':'))
                .map(|s| s.to_string());
        }
        if let Some(host) = find_ssh_host_recursive(child, depth + 1) {
            return Some(host);
        }
    }
    None
}

pub(crate) fn find_window_with_ssh_to(claims: &HashMap<u64, Claim>, host: &str) -> Option<u64> {
    for (wid, claim) in claims {
        if claim.app_id.as_deref() != Some("kitty") {
            continue;
        }
        let Some(pid) = claim.pid else { continue };
        if descendant_has_ssh_to(pid, host, 0) {
            return Some(*wid);
        }
    }
    None
}

fn descendant_has_ssh_to(pid: i32, host: &str, depth: u32) -> bool {
    if depth > 10 {
        return false;
    }
    let Ok(children_str) = fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")) else {
        return false;
    };
    for child_str in children_str.split_whitespace() {
        let Ok(child) = child_str.parse::<i32>() else {
            continue;
        };
        if let Ok(cmdline) = fs::read_to_string(format!("/proc/{child}/cmdline")) {
            let args = cmdline.replace('\0', " ");
            if (args.contains("ssh") || args.contains("kitten")) && args.contains(host) {
                return true;
            }
        }
        if descendant_has_ssh_to(child, host, depth + 1) {
            return true;
        }
    }
    false
}

pub(crate) fn priority(phase: AgentPhase) -> u8 {
    match phase {
        AgentPhase::WaitingPermission => 3,
        AgentPhase::WaitingInput => 2,
        AgentPhase::Running => 1,
        AgentPhase::Idle => 0,
    }
}

pub(crate) fn match_cwd_to_project(
    cwd: &str,
    agent_host: Option<&str>,
    projects: &[Project],
) -> Option<String> {
    let cwd_path = Path::new(cwd);
    let mut best: Option<(usize, &Project)> = None;
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

pub(crate) fn project_has_agent(inner: &Inner, project_id: &str, cli: Option<AgentCli>) -> bool {
    let local_host = read_local_hostname();
    inner.agents.values().any(|agent| {
        if cli.is_some_and(|cli| agent.cli != cli) {
            return false;
        }
        let Some(cwd) = agent.cwd.as_deref() else {
            return false;
        };
        let agent_host = match agent.host.as_deref() {
            None => None,
            Some(h) if Some(h) == local_host.as_deref() => None,
            Some(h) => Some(h.to_string()),
        };
        match_cwd_to_project(cwd, agent_host.as_deref(), &inner.projects).as_deref()
            == Some(project_id)
    })
}
