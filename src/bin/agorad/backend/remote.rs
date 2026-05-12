//! Remote agent backend — tracks sessions running on an SSH host.
//!
//! Owns the host config, the SSH reverse-forward tunnel state, and the
//! agent sessions reported via that tunnel. Hook events arrive at the
//! daemon's local socket through the tunnel; the dispatcher routes them
//! here based on the `agora_host` field in the payload.

use std::collections::HashMap;

use anyhow::{bail, Result};
use niri_ipc::{
    Action as NiriAction, Request as NiriRequest, Response as NiriResponse, WorkspaceReferenceArg,
};
use serde_json::Value;

use agora::ipc::RemoteStatus;
use agora::model::{AgentCli, AgentSession, RemoteHost};

use crate::backend::ctx::EnrichCtx;
use crate::backend::local::{apply_hook, match_cwd_to_project};
use crate::backend::{AgentBackend, NotifyRequest};
use crate::niri::niri_call;
use crate::Claim;

pub(crate) struct RemoteBackend {
    pub config: RemoteHost,
    pub status: RemoteStatus,
    pub last_error: Option<String>,
    pub shutdown_tx: Option<std::sync::mpsc::Sender<()>>,
    pub agents: HashMap<String, AgentSession>,
}

impl RemoteBackend {
    pub fn new(config: RemoteHost) -> Self {
        Self {
            config,
            status: RemoteStatus::Disconnected,
            last_error: None,
            shutdown_tx: None,
            agents: HashMap::new(),
        }
    }
}

impl AgentBackend for RemoteBackend {
    fn apply_hook(
        &mut self,
        cli: AgentCli,
        event: &str,
        payload: &Value,
    ) -> Option<NotifyRequest> {
        apply_hook(&mut self.agents, cli, event, payload)
    }

    fn list(&self, ctx: &EnrichCtx) -> Vec<AgentSession> {
        let host = self.config.host.clone();
        self.agents
            .values()
            .map(|a| enrich(a, &host, ctx))
            .collect()
    }

    fn prune(&mut self) {
        // Remote PID liveness can't be checked from here without SSH'ing.
        // Sessions are cleaned up by their SessionEnd hook, or stay until
        // the next remote scan (TODO).
    }

    fn focus(&self, session_id: &str, ctx: &EnrichCtx) -> Result<bool> {
        if !self.agents.contains_key(session_id) {
            return Ok(false);
        }
        let agent = &self.agents[session_id];
        let host = self.config.host.clone();
        // Focus the workspace bound to the project that owns this cwd.
        let project_id = agent.project.clone().or_else(|| {
            agent
                .cwd
                .as_deref()
                .and_then(|c| match_cwd_to_project(c, Some(&host), ctx.projects))
        });
        let ws_name = project_id.as_deref().and_then(|pid| {
            ctx.projects
                .iter()
                .find(|p| p.id == pid)
                .map(|p| p.workspace_name.clone())
        });
        if let Some(name) = ws_name {
            let _ = niri_call(NiriRequest::Action(NiriAction::FocusWorkspace {
                reference: WorkspaceReferenceArg::Name(name),
            }));
        }
        let window_id = find_window_with_ssh_to(ctx.claims, &host);
        if let Some(wid) = window_id {
            match niri_call(NiriRequest::Action(NiriAction::FocusWindow { id: wid }))? {
                NiriResponse::Handled => return Ok(true),
                other => bail!("unexpected niri response: {other:?}"),
            }
        }
        bail!("remote agent (host={host}); no workspace or terminal found")
    }

    fn has_agent(&self, project_id: &str, cli: Option<AgentCli>, ctx: &EnrichCtx) -> bool {
        let host = self.config.host.clone();
        self.agents.values().any(|a| {
            if cli.is_some_and(|c| a.cli != c) {
                return false;
            }
            let Some(cwd) = a.cwd.as_deref() else {
                return false;
            };
            match_cwd_to_project(cwd, Some(&host), ctx.projects).as_deref() == Some(project_id)
        })
    }
}

fn enrich(a: &AgentSession, host: &str, ctx: &EnrichCtx) -> AgentSession {
    let mut a = a.clone();
    a.project = a
        .cwd
        .as_deref()
        .and_then(|c| match_cwd_to_project(c, Some(host), ctx.projects));
    if let Some(ref proj_id) = a.project {
        a.workspace = ctx
            .projects
            .iter()
            .find(|p| p.id == *proj_id)
            .map(|p| p.workspace_name.clone());
    }
    a
}

/// Find a local kitty terminal window that has an SSH descendant connected
/// to `host`. Prefers windows whose title looks like a Claude Code agent.
pub(crate) fn find_window_with_ssh_to(claims: &HashMap<u64, Claim>, host: &str) -> Option<u64> {
    let mut fallback = None;
    for (wid, claim) in claims {
        if claim.app_id.as_deref() != Some("kitty") {
            continue;
        }
        let Some(pid) = claim.pid else { continue };
        if !descendant_has_ssh_to(pid, host, 0) {
            continue;
        }
        if title_looks_like_agent(claim.title.as_deref()) {
            return Some(*wid);
        }
        if fallback.is_none() {
            fallback = Some(*wid);
        }
    }
    fallback
}

fn title_looks_like_agent(title: Option<&str>) -> bool {
    let Some(t) = title else { return false };
    t.starts_with('⠐')
        || t.starts_with('⠂')
        || t.starts_with('⠄')
        || t.starts_with('⡀')
        || t.starts_with('⢀')
        || t.starts_with('⠠')
        || t.starts_with('⠁')
        || t.starts_with('✳')
        || t.contains("Claude Code")
}

fn descendant_has_ssh_to(pid: i32, host: &str, depth: u32) -> bool {
    if depth > 10 {
        return false;
    }
    let Ok(children_str) = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
    else {
        return false;
    };
    for child_str in children_str.split_whitespace() {
        let Ok(child) = child_str.parse::<i32>() else {
            continue;
        };
        if let Ok(cmdline) = std::fs::read_to_string(format!("/proc/{child}/cmdline")) {
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
