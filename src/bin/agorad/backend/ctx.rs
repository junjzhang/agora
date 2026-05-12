//! Read-only context passed to backends for enrichment and focus operations.
//!
//! Backends never own niri claims/workspaces (those are populated by the
//! niri event thread into `Inner`). Instead, the daemon hands a borrowed
//! view to each backend method that needs it.

use std::collections::HashMap;

use agora::model::Project;

use crate::{Claim, WorkspaceInfo};

pub(crate) struct EnrichCtx<'a> {
    pub projects: &'a [Project],
    pub claims: &'a HashMap<u64, Claim>,
    pub workspaces: &'a HashMap<u64, WorkspaceInfo>,
}
