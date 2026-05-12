use std::collections::HashMap;
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use niri_ipc::socket::Socket;
use niri_ipc::{
    Action as NiriAction, Event, Reply, Request as NiriRequest, Response as NiriResponse, Window,
    WorkspaceReferenceArg,
};

use agora::ipc::{Payload, WindowSummary};
use agora::model::AgentSession;

use crate::{Claim, State, WorkspaceInfo};

pub(crate) fn run_niri_loop(state: State) -> Result<()> {
    let mut socket = Socket::connect().context("connecting to niri IPC socket")?;
    let reply: Reply = socket
        .send(NiriRequest::EventStream)
        .context("requesting event stream")?;

    match reply {
        Ok(NiriResponse::Handled) => {}
        Ok(other) => bail!("unexpected response from niri: {other:?}"),
        Err(msg) => bail!("niri rejected event stream request: {msg}"),
    }

    tracing::info!("subscribed to niri event stream");
    let mut read_event = socket.read_events();

    loop {
        let event = read_event().context("reading niri event")?;
        apply_event(&state, event);
    }
}

fn apply_event(state: &State, event: Event) {
    match event {
        Event::WorkspacesChanged { workspaces } => {
            let mut inner = state.lock().unwrap();
            inner.workspaces = workspaces
                .iter()
                .map(|w| {
                    (
                        w.id,
                        WorkspaceInfo {
                            idx: w.idx,
                            name: w.name.clone(),
                            output: w.output.clone(),
                            is_active: w.is_active,
                            is_focused: w.is_focused,
                        },
                    )
                })
                .collect();
        }
        Event::WorkspaceActivated { id, focused } => {
            let mut inner = state.lock().unwrap();
            let output = inner.workspaces.get(&id).and_then(|w| w.output.clone());
            for (&ws_id, ws) in inner.workspaces.iter_mut() {
                let got_activated = ws_id == id;
                if ws.output == output {
                    ws.is_active = got_activated;
                }
                if focused {
                    ws.is_focused = got_activated;
                }
            }
        }
        Event::WindowsChanged { windows } => {
            let mut new_claims: HashMap<u64, Claim> = HashMap::with_capacity(windows.len());
            for w in &windows {
                new_claims.insert(w.id, build_claim(w));
            }
            let mut inner = state.lock().unwrap();
            inner.claims = new_claims;
            tracing::info!(count = windows.len(), "claims rebuilt from snapshot");
        }
        Event::WindowOpenedOrChanged { window } => {
            let prev_workspace = state
                .lock()
                .unwrap()
                .claims
                .get(&window.id)
                .and_then(|c| c.workspace_id);
            let claim = build_claim(&window);
            let new_workspace = claim.workspace_id;
            state.lock().unwrap().claims.insert(window.id, claim);
            if let Some(old) = prev_workspace {
                if Some(old) != new_workspace {
                    cleanup_workspace_if_empty(state, old);
                }
            }
        }
        Event::WindowClosed { id } => {
            tracing::info!(window = %id, "window closed");
            let prev_workspace = state
                .lock()
                .unwrap()
                .claims
                .remove(&id)
                .and_then(|c| c.workspace_id);
            if let Some(ws_id) = prev_workspace {
                tracing::info!(window = %id, workspace = %ws_id, "checking cleanup for workspace");
                cleanup_workspace_if_empty(state, ws_id);
            }
        }
        _ => {}
    }
}

fn cleanup_workspace_if_empty(state: &State, ws_id: u64) {
    let name = {
        let inner = state.lock().unwrap();
        let Some(name) = inner.workspaces.get(&ws_id).and_then(|w| w.name.clone()) else {
            return;
        };
        if !inner.config.cleanup_all_workspaces {
            let is_project_ws = inner.projects.iter().any(|p| p.workspace_name == name);
            if !is_project_ws {
                return;
            }
        }
        let still_has_claims = inner.claims.values().any(|c| c.workspace_id == Some(ws_id));
        if still_has_claims {
            return;
        }
        name
    };

    let action = NiriAction::UnsetWorkspaceName {
        reference: Some(WorkspaceReferenceArg::Name(name.clone())),
    };
    match niri_call(NiriRequest::Action(action)) {
        Ok(NiriResponse::Handled) => {
            tracing::info!(workspace = %name, "unset name on empty project workspace");
        }
        Ok(other) => {
            tracing::warn!(workspace = %name, response = ?other, "unexpected niri response to UnsetWorkspaceName");
        }
        Err(e) => {
            tracing::warn!(workspace = %name, error = %e, "UnsetWorkspaceName failed");
        }
    }
}

fn build_claim(w: &Window) -> Claim {
    Claim {
        app_id: w.app_id.clone(),
        title: w.title.clone(),
        workspace_id: w.workspace_id,
        column: w.layout.pos_in_scrolling_layout.map(|(c, _)| c),
        pid: w.pid,
    }
}

pub(crate) fn status(state: &State) -> Payload {
    let inner = state.lock().unwrap();
    let mut windows: Vec<WindowSummary> = inner
        .claims
        .iter()
        .map(|(id, c)| {
            let ws_info = c
                .workspace_id
                .and_then(|ws_id| inner.workspaces.get(&ws_id));
            let project = ws_info.and_then(|w| w.name.as_deref()).and_then(|name| {
                inner
                    .projects
                    .iter()
                    .find(|p| p.workspace_name == name)
                    .map(|p| p.id.clone())
            });
            WindowSummary {
                window_id: *id,
                project,
                app_id: c.app_id.clone(),
                title: c.title.clone(),
                workspace_id: c.workspace_id,
                workspace_idx: ws_info.map(|w| w.idx),
                workspace_name: ws_info.and_then(|w| w.name.clone()),
                column: c.column,
                pid: c.pid,
            }
        })
        .collect();
    windows.sort_by_key(|w| (w.workspace_id, w.column, w.window_id));
    let mut agents: Vec<AgentSession> = inner
        .local
        .agents
        .values()
        .cloned()
        .chain(
            inner
                .remotes
                .values()
                .flat_map(|r| r.agents.values().cloned()),
        )
        .collect();
    agents.sort_by_key(|a| std::cmp::Reverse(a.last_change));
    Payload::Status {
        project_count: inner.projects.len(),
        windows,
        agents,
    }
}

pub(crate) fn niri_call(req: NiriRequest) -> Result<NiriResponse> {
    let mut socket = Socket::connect().context("connect to niri socket")?;
    let reply: Reply = socket.send(req).context("send request to niri")?;
    reply.map_err(|msg| anyhow::anyhow!("niri error: {msg}"))
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
