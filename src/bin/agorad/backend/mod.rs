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

use agora::model::{AgentCli, AgentSession};
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

pub(crate) trait AgentBackend {
    /// Update internal state from a hook event. Returns a notification
    /// request if the state transition warrants user attention.
    fn apply_hook(&mut self, event: &str, payload: &Value) -> Option<NotifyRequest>;

    /// All sessions owned by this backend, enriched with project + workspace
    /// metadata derived from the provided context.
    fn list(&self, ctx: &EnrichCtx) -> Vec<AgentSession>;

    /// Drop sessions that no longer exist (force-killed, crashed, etc).
    fn prune(&mut self);

    /// Focus the niri window for `session_id` if this backend owns it.
    /// Ok(true) = focused; Ok(false) = not owned; Err = backend owns it but
    /// focusing failed (e.g. window gone).
    fn focus(&self, session_id: &str, ctx: &EnrichCtx) -> Result<bool>;

    /// True if this backend tracks any session matching `project_id`
    /// (optionally filtered by CLI).
    fn has_agent(&self, project_id: &str, cli: Option<AgentCli>, ctx: &EnrichCtx) -> bool;
}
