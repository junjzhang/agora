use anyhow::{bail, Context, Result};

use agora::ipc::{ActionSummary, ActionTarget};
use agora::model::AgentCli;

use crate::hooks::project_has_agent;
use crate::launcher::{
    launcher_available_for_root, launcher_argv_with_args, niri_spawn, shell_quote,
};
use crate::project::{attach, forget, open};
use crate::State;

#[derive(Debug)]
struct OrderedAction {
    group_order: i32,
    order: i32,
    summary: ActionSummary,
}

pub(crate) fn actions_for_target(
    state: &State,
    target: ActionTarget,
) -> Result<Vec<ActionSummary>> {
    match target {
        ActionTarget::Project { id } => project_actions(state, &id),
    }
}

fn project_actions(state: &State, project_id: &str) -> Result<Vec<ActionSummary>> {
    let (project, registry, has_claude_agent, has_codex_agent, has_any_agent) = {
        let inner = state.lock().unwrap();
        let project = inner
            .projects
            .iter()
            .find(|p| p.id == project_id)
            .cloned()
            .with_context(|| format!("unknown project '{project_id}'"))?;
        (
            project,
            inner.launcher_registry.clone(),
            project_has_agent(&inner, project_id, Some(AgentCli::Claude)),
            project_has_agent(&inner, project_id, Some(AgentCli::Codex)),
            project_has_agent(&inner, project_id, None),
        )
    };
    let root = project
        .roots
        .get(project.default_root)
        .with_context(|| format!("project '{}' default_root index out of range", project.id))?;

    let mut out = vec![
        ordered_action(0, 0, "builtin:open", "OPEN", "Open workspace", Some("↵")),
        ordered_action(
            2,
            0,
            "builtin:edit",
            "EDIT",
            "Edit project spec",
            Some("⌥E"),
        ),
        ordered_action(2, 10, "builtin:rename", "EDIT", "Rename project", None),
        ordered_action(
            3,
            0,
            "builtin:attach",
            "MANAGE",
            "Attach to current workspace",
            None,
        ),
        ordered_action(
            3,
            10,
            "builtin:copy_path",
            "MANAGE",
            "Copy path",
            Some("⌥C"),
        ),
        ordered_action(
            3,
            20,
            "builtin:forget",
            "MANAGE",
            "Forget project",
            Some("⌥⌫"),
        ),
    ];

    for (launcher_id, launcher) in &registry {
        if launcher.disabled || !launcher_available_for_root(launcher, root) {
            continue;
        }
        for (action_id, action) in &launcher.actions {
            if action.disabled
                || !action_condition_matches(
                    action.when.as_deref(),
                    root,
                    has_claude_agent,
                    has_codex_agent,
                    has_any_agent,
                )
            {
                continue;
            }
            let group = action
                .group
                .as_deref()
                .or(launcher.group.as_deref())
                .unwrap_or("TOOLS");
            let label = action
                .label
                .as_deref()
                .or(launcher.label.as_deref())
                .unwrap_or(action_id);
            out.push(ordered_action(
                group_order(group),
                action.order,
                &format!("launcher:{launcher_id}:{action_id}"),
                group,
                label,
                action.key.as_deref(),
            ));
        }
    }

    out.sort_by(|a, b| {
        a.group_order
            .cmp(&b.group_order)
            .then(a.order.cmp(&b.order))
            .then(a.summary.label.cmp(&b.summary.label))
    });
    Ok(out.into_iter().map(|a| a.summary).collect())
}

pub(crate) fn run_action(state: &State, target: ActionTarget, action_id: &str) -> Result<()> {
    match target {
        ActionTarget::Project { id } => run_project_action(state, &id, action_id),
    }
}

fn run_project_action(state: &State, project_id: &str, action_id: &str) -> Result<()> {
    match action_id {
        "builtin:open" => {
            open(state, project_id.to_string())?;
            return Ok(());
        }
        "builtin:attach" => {
            attach(state, project_id.to_string(), false)?;
            return Ok(());
        }
        "builtin:forget" => {
            forget(state, project_id.to_string())?;
            return Ok(());
        }
        _ => {}
    }

    let (project, registry, has_claude_agent, has_codex_agent, has_any_agent) = {
        let inner = state.lock().unwrap();
        let project = inner
            .projects
            .iter()
            .find(|p| p.id == project_id)
            .cloned()
            .with_context(|| format!("unknown project '{project_id}'"))?;
        (
            project,
            inner.launcher_registry.clone(),
            project_has_agent(&inner, project_id, Some(AgentCli::Claude)),
            project_has_agent(&inner, project_id, Some(AgentCli::Codex)),
            project_has_agent(&inner, project_id, None),
        )
    };
    let root = project
        .roots
        .get(project.default_root)
        .with_context(|| format!("project '{}' default_root index out of range", project.id))?;

    match action_id {
        "builtin:copy_path" => {
            return niri_spawn(
                vec!["dms".into(), "cl".into(), "copy".into(), root.path.clone()],
                "builtin:copy_path",
                root,
            );
        }
        "builtin:edit" => {
            let cmd = format!("agora edit {}; exec zsh", shell_quote(project_id));
            return niri_spawn(
                vec!["zsh".into(), "-ic".into(), cmd],
                "builtin:edit",
                root,
            );
        }
        "builtin:rename" => {
            let cmd = format!(
                "echo {}; exec zsh",
                shell_quote(&format!("agora rename {project_id} <new-name>"))
            );
            return niri_spawn(
                vec!["zsh".into(), "-ic".into(), cmd],
                "builtin:rename",
                root,
            );
        }
        _ => {}
    }

    let Some((launcher_id, launcher_action_id)) = parse_launcher_action_id(action_id) else {
        bail!("unknown action '{action_id}'");
    };
    let launcher = registry
        .get(launcher_id)
        .with_context(|| format!("unknown launcher '{launcher_id}'"))?;
    if launcher.disabled {
        bail!("launcher '{launcher_id}' is disabled");
    }
    if !launcher_available_for_root(launcher, root) {
        bail!("launcher '{launcher_id}' is not available for this root");
    }
    let action = launcher.actions.get(launcher_action_id).with_context(|| {
        format!("unknown action '{launcher_action_id}' for launcher '{launcher_id}'")
    })?;
    if action.disabled
        || !action_condition_matches(
            action.when.as_deref(),
            root,
            has_claude_agent,
            has_codex_agent,
            has_any_agent,
        )
    {
        bail!("action '{action_id}' is not available");
    }
    let argv = launcher_argv_with_args(launcher_id, root, &registry, &action.args)
        .with_context(|| format!("could not build command for action '{action_id}'"))?;
    niri_spawn(argv, action_id, root)
}

fn ordered_action(
    group_order: i32,
    order: i32,
    id: &str,
    group: &str,
    label: &str,
    key: Option<&str>,
) -> OrderedAction {
    OrderedAction {
        group_order,
        order,
        summary: ActionSummary {
            id: id.into(),
            label: label.into(),
            group: group.into(),
            key: key.map(str::to_string),
        },
    }
}

fn parse_launcher_action_id(action_id: &str) -> Option<(&str, &str)> {
    let rest = action_id.strip_prefix("launcher:")?;
    rest.split_once(':')
}

use agora::model::Root;

fn action_condition_matches(
    when: Option<&str>,
    root: &Root,
    has_claude_agent: bool,
    has_codex_agent: bool,
    has_any_agent: bool,
) -> bool {
    let Some(when) = when else {
        return true;
    };
    match when {
        "local" => root.host.is_none(),
        "remote" => root.host.is_some(),
        "has_agent" => has_any_agent,
        "has_agent:claude" => has_claude_agent,
        "has_agent:codex" => has_codex_agent,
        "never" => false,
        other => {
            tracing::warn!(condition = other, "unknown action condition; hiding action");
            false
        }
    }
}

fn group_order(group: &str) -> i32 {
    match group {
        "OPEN" => 0,
        "AGENT" => 1,
        "EDIT" => 2,
        "MANAGE" => 3,
        _ => 10,
    }
}
