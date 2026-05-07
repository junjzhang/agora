use std::fs;

use anyhow::{bail, Context, Result};
use niri_ipc::{
    Action as NiriAction, Request as NiriRequest, Response as NiriResponse, Workspace,
    WorkspaceReferenceArg,
};

use agora::ipc::Payload;
use agora::model::{Project, ProjectSpec, Root};
use agora::store;

use crate::hooks::{find_leaf_child, find_ssh_host_in_children};
use crate::launcher::{launcher_command, spawn_detached_command};
use crate::niri::{niri_call, unix_now};
use crate::State;

pub(crate) fn add(
    state: &State,
    name: String,
    root_path: String,
    host: Option<String>,
    launchers: Vec<String>,
) -> Result<Project> {
    if name.is_empty() {
        bail!("project name must not be empty");
    }

    let path = match host.as_deref() {
        Some("") => bail!("--host is empty; omit it for local"),
        Some(_) => {
            if root_path.is_empty() {
                bail!("root path must not be empty");
            }
            root_path
        }
        None => {
            let resolved = fs::canonicalize(&root_path)
                .with_context(|| format!("resolve root path {root_path}"))?;
            let meta =
                fs::metadata(&resolved).with_context(|| format!("stat {}", resolved.display()))?;
            if !meta.is_dir() {
                bail!("root path is not a directory: {}", resolved.display());
            }
            resolved.to_string_lossy().into_owned()
        }
    };

    let mut inner = state.lock().unwrap();
    if inner.projects.iter().any(|p| p.id == name) {
        bail!("project '{name}' already exists");
    }

    let now = unix_now();
    let project = Project {
        id: name.clone(),
        workspace_name: name.clone(),
        name,
        roots: vec![Root {
            path,
            host,
            label: None,
            launchers,
        }],
        default_root: 0,
        pinned: false,
        ts_created: now,
        ts_last_active: now,
        archived_at: None,
    };
    inner.projects.push(project.clone());
    store::save(&inner.projects).context("save project store")?;
    Ok(project)
}

pub(crate) fn forget(state: &State, name: String) -> Result<Project> {
    let mut inner = state.lock().unwrap();
    let idx = inner
        .projects
        .iter()
        .position(|p| p.id == name)
        .ok_or_else(|| anyhow::anyhow!("no project named '{name}'"))?;
    let removed = inner.projects.remove(idx);
    store::save(&inner.projects).context("save project store")?;
    Ok(removed)
}

pub(crate) fn rename(state: &State, from: String, to: String) -> Result<Payload> {
    if from == to {
        bail!("'from' and 'to' are the same");
    }
    if to.is_empty() {
        bail!("new project name must not be empty");
    }

    let old_workspace_name = {
        let inner = state.lock().unwrap();
        if inner.projects.iter().any(|p| p.id == to) {
            bail!("project '{to}' already exists");
        }
        let p = inner
            .projects
            .iter()
            .find(|p| p.id == from)
            .ok_or_else(|| anyhow::anyhow!("no project named '{from}'"))?;
        p.workspace_name.clone()
    };

    let workspaces = match niri_call(NiriRequest::Workspaces)? {
        NiriResponse::Workspaces(ws) => ws,
        other => bail!("unexpected niri response to Workspaces: {other:?}"),
    };
    let niri_ws_renamed = workspaces
        .iter()
        .any(|w| w.name.as_deref() == Some(old_workspace_name.as_str()));

    if niri_ws_renamed {
        let action = NiriAction::SetWorkspaceName {
            name: to.clone(),
            workspace: Some(WorkspaceReferenceArg::Name(old_workspace_name.clone())),
        };
        match niri_call(NiriRequest::Action(action))? {
            NiriResponse::Handled => {}
            other => bail!("unexpected niri response to Action: {other:?}"),
        }
    }

    let mut inner = state.lock().unwrap();
    let p = inner
        .projects
        .iter_mut()
        .find(|p| p.id == from)
        .ok_or_else(|| anyhow::anyhow!("project '{from}' disappeared during rename"))?;
    p.id = to.clone();
    p.name = to.clone();
    if p.workspace_name == from {
        p.workspace_name = to.clone();
    }
    p.ts_last_active = unix_now();
    let project = p.clone();
    store::save(&inner.projects).context("save project store")?;

    Ok(Payload::Renamed {
        project,
        niri_ws_renamed,
    })
}

pub(crate) fn get(state: &State, name: String) -> Result<Project> {
    let inner = state.lock().unwrap();
    inner
        .projects
        .iter()
        .find(|p| p.id == name)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no project named '{name}'"))
}

pub(crate) fn update(state: &State, name: String, spec: ProjectSpec) -> Result<Project> {
    let old_workspace_name = {
        let inner = state.lock().unwrap();
        let p = inner
            .projects
            .iter()
            .find(|p| p.id == name)
            .ok_or_else(|| anyhow::anyhow!("project '{name}' no longer exists (forgotten?)"))?;
        p.workspace_name.clone()
    };

    let normalized_spec = validate_spec(state, &name, spec)?;

    let workspace_name_changed = old_workspace_name != normalized_spec.workspace_name;
    if workspace_name_changed {
        let workspaces = match niri_call(NiriRequest::Workspaces)? {
            NiriResponse::Workspaces(ws) => ws,
            other => bail!("unexpected niri response to Workspaces: {other:?}"),
        };
        let in_use = workspaces
            .iter()
            .any(|w| w.name.as_deref() == Some(old_workspace_name.as_str()));
        if in_use {
            let action = NiriAction::SetWorkspaceName {
                name: normalized_spec.workspace_name.clone(),
                workspace: Some(WorkspaceReferenceArg::Name(old_workspace_name.clone())),
            };
            match niri_call(NiriRequest::Action(action))? {
                NiriResponse::Handled => {}
                other => bail!("unexpected niri response to SetWorkspaceName: {other:?}"),
            }
        }
    }

    let mut inner = state.lock().unwrap();
    let p = inner
        .projects
        .iter_mut()
        .find(|p| p.id == name)
        .ok_or_else(|| anyhow::anyhow!("project '{name}' disappeared during update"))?;
    p.apply_spec(normalized_spec);
    p.ts_last_active = unix_now();
    let updated = p.clone();
    store::save(&inner.projects).context("save project store")?;
    Ok(updated)
}

pub(crate) fn promote(
    state: &State,
    name_arg: Option<String>,
    root_path: String,
    host: Option<String>,
    launchers: Vec<String>,
    rename_ws: bool,
) -> Result<Project> {
    let workspaces = match niri_call(NiriRequest::Workspaces)? {
        NiriResponse::Workspaces(ws) => ws,
        other => bail!("unexpected niri response to Workspaces: {other:?}"),
    };
    let focused = workspaces
        .iter()
        .find(|w| w.is_focused)
        .ok_or_else(|| anyhow::anyhow!("no focused niri workspace"))?;

    let (effective_root, detected_host) = if root_path.is_empty() {
        let inner = state.lock().unwrap();
        let focused_ws_id = focused.id;
        let active_id = focused.active_window_id;
        let ws_claims: Vec<_> = inner
            .claims
            .iter()
            .filter(|(_, c)| c.workspace_id == Some(focused_ws_id))
            .collect();
        let term_pid = active_id
            .and_then(|aid| ws_claims.iter().find(|(id, _)| **id == aid))
            .filter(|(_, c)| c.app_id.as_deref() == Some("kitty"))
            .and_then(|(_, c)| c.pid)
            .or_else(|| {
                ws_claims
                    .iter()
                    .find(|(_, c)| c.app_id.as_deref() == Some("kitty"))
                    .and_then(|(_, c)| c.pid)
            });
        drop(inner);

        if let Some(pid) = term_pid {
            if let Some(ssh_host) = find_ssh_host_in_children(pid) {
                let remote_cwd = {
                    let inner = state.lock().unwrap();
                    inner
                        .agents
                        .values()
                        .filter(|a| a.host.as_deref() == Some(&ssh_host))
                        .filter_map(|a| a.cwd.as_deref())
                        .max_by_key(|cwd| cwd.len())
                        .map(String::from)
                };
                match remote_cwd {
                    Some(cwd) if !cwd.is_empty() => (cwd, Some(ssh_host)),
                    _ => bail!(
                        "detected SSH to {ssh_host} but no agent session with a cwd on that host; \
                         start a claude session there first, or pass a root path explicitly"
                    ),
                }
            } else {
                let leaf = find_leaf_child(pid);
                let cwd = std::fs::read_link(format!("/proc/{leaf}/cwd"))
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
                    .ok_or_else(|| anyhow::anyhow!("cannot read cwd of pid {leaf}"))?;
                (cwd, None)
            }
        } else {
            let fallback_pid = {
                let inner = state.lock().unwrap();
                inner
                    .claims
                    .values()
                    .filter(|c| c.workspace_id == Some(focused_ws_id))
                    .find_map(|c| c.pid)
            };
            let cwd = fallback_pid
                .and_then(|p| {
                    let leaf = find_leaf_child(p);
                    std::fs::read_link(format!("/proc/{leaf}/cwd"))
                        .ok()
                        .map(|p| p.to_string_lossy().into_owned())
                })
                .ok_or_else(|| anyhow::anyhow!("no window cwd found on focused workspace"))?;
            (cwd, None)
        }
    } else {
        (root_path.clone(), None)
    };
    let effective_host = host.or(detected_host);
    let resolved_path = match effective_host.as_deref() {
        Some("") => bail!("--host is empty; omit it for local"),
        Some(_) => {
            if effective_root.is_empty() {
                bail!("root path must not be empty");
            }
            effective_root
        }
        None => {
            let resolved = fs::canonicalize(&effective_root)
                .with_context(|| format!("resolve root path {effective_root}"))?;
            let meta =
                fs::metadata(&resolved).with_context(|| format!("stat {}", resolved.display()))?;
            if !meta.is_dir() {
                bail!("root path is not a directory: {}", resolved.display());
            }
            resolved.to_string_lossy().into_owned()
        }
    };

    let derived = name_arg
        .clone()
        .or_else(|| focused.name.clone())
        .or_else(|| {
            std::path::Path::new(&resolved_path)
                .file_name()
                .and_then(|s| s.to_str())
                .map(String::from)
        })
        .ok_or_else(|| anyhow::anyhow!("could not derive a name; pass --name"))?;
    if derived.is_empty() {
        bail!("derived name is empty; pass --name");
    }

    let ws_rename_needed = match (focused.name.as_deref(), name_arg.as_deref()) {
        (Some(ws_name), Some(arg_name)) if ws_name != arg_name => {
            if !rename_ws {
                bail!(
                    "current workspace is named '{ws_name}'; \
                     pass --rename-ws to rename it to '{arg_name}', \
                     or use --name {ws_name} to keep it"
                );
            }
            true
        }
        _ => false,
    };

    {
        let inner = state.lock().unwrap();
        if inner.projects.iter().any(|p| p.id == derived) {
            bail!("project '{derived}' already exists");
        }
        if let Some(other) = inner.projects.iter().find(|p| p.workspace_name == derived) {
            bail!(
                "workspace_name '{derived}' already used by project '{}'",
                other.id
            );
        }
    }

    let needs_set_name = focused.name.is_none() || ws_rename_needed;
    if needs_set_name {
        let action = NiriAction::SetWorkspaceName {
            name: derived.clone(),
            workspace: Some(WorkspaceReferenceArg::Id(focused.id)),
        };
        match niri_call(NiriRequest::Action(action))? {
            NiriResponse::Handled => {}
            other => bail!("unexpected niri response to SetWorkspaceName: {other:?}"),
        }
    }

    let mut inner = state.lock().unwrap();
    if inner.projects.iter().any(|p| p.id == derived) {
        bail!("project '{derived}' was created concurrently");
    }
    let now = unix_now();
    let project = Project {
        id: derived.clone(),
        name: derived.clone(),
        workspace_name: derived.clone(),
        roots: vec![Root {
            path: resolved_path,
            host: effective_host,
            label: None,
            launchers,
        }],
        default_root: 0,
        pinned: false,
        ts_created: now,
        ts_last_active: now,
        archived_at: None,
    };
    inner.projects.push(project.clone());
    store::save(&inner.projects).context("save project store")?;
    Ok(project)
}

pub(crate) fn attach(state: &State, project_name: String, rename_ws: bool) -> Result<Project> {
    let current_workspace_name = {
        let inner = state.lock().unwrap();
        inner
            .projects
            .iter()
            .find(|p| p.id == project_name)
            .map(|p| p.workspace_name.clone())
            .ok_or_else(|| anyhow::anyhow!("no project named '{project_name}'"))?
    };

    let workspaces = match niri_call(NiriRequest::Workspaces)? {
        NiriResponse::Workspaces(ws) => ws,
        other => bail!("unexpected niri response to Workspaces: {other:?}"),
    };
    let focused = workspaces
        .iter()
        .find(|w| w.is_focused)
        .ok_or_else(|| anyhow::anyhow!("no focused niri workspace"))?;

    let new_workspace_name = if rename_ws {
        if focused.name.as_deref() != Some(current_workspace_name.as_str()) {
            let action = NiriAction::SetWorkspaceName {
                name: current_workspace_name.clone(),
                workspace: Some(WorkspaceReferenceArg::Id(focused.id)),
            };
            match niri_call(NiriRequest::Action(action))? {
                NiriResponse::Handled => {}
                other => bail!("unexpected niri response to SetWorkspaceName: {other:?}"),
            }
        }
        current_workspace_name.clone()
    } else {
        let ws_name = focused.name.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "focused workspace is unnamed; pass --rename-ws to claim it as '{current_workspace_name}'"
            )
        })?;
        let inner = state.lock().unwrap();
        if let Some(other) = inner
            .projects
            .iter()
            .find(|p| p.id != project_name && p.workspace_name == ws_name)
        {
            bail!(
                "workspace_name '{}' already used by project '{}'",
                ws_name,
                other.id
            );
        }
        ws_name
    };

    let mut inner = state.lock().unwrap();
    let p = inner
        .projects
        .iter_mut()
        .find(|p| p.id == project_name)
        .ok_or_else(|| anyhow::anyhow!("project '{project_name}' disappeared during attach"))?;
    p.workspace_name = new_workspace_name;
    p.ts_last_active = unix_now();
    let updated = p.clone();
    store::save(&inner.projects).context("save project store")?;
    Ok(updated)
}

fn validate_spec(state: &State, self_name: &str, mut spec: ProjectSpec) -> Result<ProjectSpec> {
    if spec.workspace_name.is_empty() {
        bail!("workspace_name must not be empty");
    }
    if spec.roots.is_empty() {
        bail!("at least one root is required");
    }
    if spec.default_root >= spec.roots.len() {
        bail!(
            "default_root {} out of range (roots has {} entries)",
            spec.default_root,
            spec.roots.len()
        );
    }

    for (i, root) in spec.roots.iter_mut().enumerate() {
        if root.path.is_empty() {
            bail!("roots[{i}].path must not be empty");
        }
        match root.host.as_deref() {
            Some("") => bail!("roots[{i}].host is empty; use null for local"),
            Some(_) => {}
            None => {
                let resolved = fs::canonicalize(&root.path)
                    .with_context(|| format!("roots[{i}].path: resolve {} failed", root.path))?;
                let meta = fs::metadata(&resolved).with_context(|| {
                    format!("roots[{i}].path: stat {} failed", resolved.display())
                })?;
                if !meta.is_dir() {
                    bail!("roots[{i}].path is not a directory: {}", resolved.display());
                }
                root.path = resolved.to_string_lossy().into_owned();
            }
        }
    }

    let inner = state.lock().unwrap();
    if let Some(other) = inner
        .projects
        .iter()
        .find(|p| p.id != self_name && p.workspace_name == spec.workspace_name)
    {
        bail!(
            "workspace_name '{}' already used by project '{}'",
            spec.workspace_name,
            other.id
        );
    }
    Ok(spec)
}

pub(crate) fn open(state: &State, name: String) -> Result<Payload> {
    let project = {
        let inner = state.lock().unwrap();
        inner
            .projects
            .iter()
            .find(|p| p.id == name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no project named '{name}'"))?
    };

    let workspaces = match niri_call(NiriRequest::Workspaces)? {
        NiriResponse::Workspaces(ws) => ws,
        other => bail!("unexpected niri response to Workspaces: {other:?}"),
    };

    if let Some(ws) = workspaces
        .iter()
        .find(|w| w.name.as_deref() == Some(project.workspace_name.as_str()))
    {
        let action = NiriAction::FocusWorkspace {
            reference: WorkspaceReferenceArg::Name(project.workspace_name.clone()),
        };
        match niri_call(NiriRequest::Action(action))? {
            NiriResponse::Handled => {}
            other => bail!("unexpected niri response to Action: {other:?}"),
        }

        let ws_empty = {
            let inner = state.lock().unwrap();
            !inner.claims.values().any(|c| c.workspace_id == Some(ws.id))
        };
        if ws_empty {
            spawn_launchers(state, &project);
        }

        bump_last_active(state, &name)?;
        return Ok(Payload::Opened {
            project,
            claimed_current: false,
        });
    }

    let target = pick_target_workspace(&workspaces)?;

    let focused_id = workspaces.iter().find(|w| w.is_focused).map(|w| w.id);
    if Some(target.id) != focused_id {
        let action = NiriAction::FocusWorkspace {
            reference: WorkspaceReferenceArg::Id(target.id),
        };
        match niri_call(NiriRequest::Action(action))? {
            NiriResponse::Handled => {}
            other => bail!("unexpected niri response to FocusWorkspace: {other:?}"),
        }
    }

    let action = NiriAction::SetWorkspaceName {
        name: project.workspace_name.clone(),
        workspace: None,
    };
    match niri_call(NiriRequest::Action(action))? {
        NiriResponse::Handled => {}
        other => bail!("unexpected niri response to SetWorkspaceName: {other:?}"),
    }

    spawn_launchers(state, &project);
    bump_last_active(state, &name)?;

    Ok(Payload::Opened {
        project,
        claimed_current: true,
    })
}

fn pick_target_workspace(workspaces: &[Workspace]) -> Result<&Workspace> {
    let focused = workspaces
        .iter()
        .find(|w| w.is_focused)
        .ok_or_else(|| anyhow::anyhow!("niri reports no focused workspace"))?;

    if focused.name.is_none() && focused.active_window_id.is_none() {
        return Ok(focused);
    }

    workspaces
        .iter()
        .filter(|w| w.output == focused.output && w.name.is_none() && w.active_window_id.is_none())
        .max_by_key(|w| w.idx)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no empty unnamed workspace on output {:?}; \
                 niri usually keeps a trailing empty one — check `niri msg workspaces`",
                focused.output
            )
        })
}

fn bump_last_active(state: &State, name: &str) -> Result<()> {
    let mut inner = state.lock().unwrap();
    if let Some(p) = inner.projects.iter_mut().find(|p| p.id == name) {
        p.ts_last_active = unix_now();
    }
    store::save(&inner.projects).context("save project store")
}

fn spawn_launchers(state: &State, project: &Project) {
    let Some(root) = project.roots.get(project.default_root) else {
        tracing::warn!(project = %project.id, "default_root index out of range");
        return;
    };
    let registry = { state.lock().unwrap().launcher_registry.clone() };
    if root.launchers.is_empty() {
        spawn_one("terminal", root, &registry);
        return;
    }
    for launcher in &root.launchers {
        spawn_one(launcher, root, &registry);
    }
}

fn spawn_one(launcher: &str, root: &Root, registry: &crate::config::LauncherRegistry) {
    let Some(mut cmd) = launcher_command(launcher, root, registry) else {
        return;
    };
    if root.host.is_none() {
        cmd.current_dir(&root.path);
    }
    if let Err(e) = spawn_detached_command(cmd, launcher, root) {
        tracing::warn!(
            launcher,
            error = %e,
            "spawn failed",
        );
    }
}
