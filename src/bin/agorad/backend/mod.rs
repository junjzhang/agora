//! Agent backends — separate code paths for local vs remote agent tracking.
//!
//! Each backend owns the subset of agent sessions it tracks and implements
//! the operations specific to its environment (e.g. local backends use
//! `/proc` for PID liveness; remote backends use an SSH reverse-forward
//! tunnel for hook delivery).
//!
//! The daemon's `Inner` holds one `LocalBackend` plus a map of
//! `RemoteBackend` keyed by SSH alias. Top-level operations (list, focus,
//! has_agent) dispatch to the right backend(s) and aggregate.

pub(crate) mod ctx;
pub(crate) mod local;
pub(crate) mod remote;

pub(crate) use ctx::EnrichCtx;
pub(crate) use local::LocalBackend;
pub(crate) use remote::RemoteBackend;

use std::collections::HashMap;

use agora::model::{AgentCli, AgentSession, Project};
use anyhow::Result;
use serde_json::Value;

/// Result of applying a hook event. Used by the daemon to decide whether to
/// fire a desktop notification.
#[derive(Debug)]
pub(crate) struct NotifyRequest {
    pub title: String,
    pub body: String,
    pub session_id: String,
}

/// What the daemon should do with niri to focus an agent. Decided under the
/// state lock; executed outside the lock so a slow niri IPC doesn't stall
/// the rest of the daemon.
#[derive(Debug)]
pub(crate) struct FocusPlan {
    /// Optional workspace name to focus before focusing the window.
    pub workspace: Option<String>,
    /// Window id to focus.
    pub window: u64,
}

pub(crate) trait AgentBackend {
    /// Update internal state from a hook event. Returns a notification
    /// request if the state transition warrants user attention.
    fn apply_hook(&mut self, cli: AgentCli, event: &str, payload: &Value) -> Option<NotifyRequest>;

    /// All sessions owned by this backend, enriched with project + workspace
    /// metadata derived from the provided context.
    fn list(&self, ctx: &EnrichCtx) -> Vec<AgentSession>;

    /// Build a focus plan for `session_id` if this backend owns it.
    /// Ok(Some(plan)) = owned, here's what to do.
    /// Ok(None) = not owned by this backend; try another.
    /// Err = owned but cannot resolve a target (no window, no PID, etc).
    fn focus_plan(&self, session_id: &str, ctx: &EnrichCtx) -> Result<Option<FocusPlan>>;

    /// True if this backend tracks any session matching `project_id`
    /// (optionally filtered by CLI).
    fn has_agent(&self, project_id: &str, cli: Option<AgentCli>, ctx: &EnrichCtx) -> bool;

    /// True iff this backend's agent map is empty. Lets the dispatcher
    /// drop ephemeral remotes whose last session just ended.
    fn is_empty(&self) -> bool;
}

/// Filter shared by both backends: does the agent map contain a session
/// whose cwd lives under one of `project_id`'s roots matching `host`?
pub(crate) fn has_matching_agent(
    agents: &HashMap<String, AgentSession>,
    project_id: &str,
    cli: Option<AgentCli>,
    host: Option<&str>,
    projects: &[Project],
) -> bool {
    agents.values().any(|a| {
        if cli.is_some_and(|c| a.cli != c) {
            return false;
        }
        let Some(cwd) = a.cwd.as_deref() else {
            return false;
        };
        local::match_cwd_to_project(cwd, host, projects).as_deref() == Some(project_id)
    })
}
