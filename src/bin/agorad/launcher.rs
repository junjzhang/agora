use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use agora::model::Root;

use crate::config::{LauncherRegistry, LauncherTemplate};

pub(crate) fn launcher_command(
    launcher: &str,
    root: &Root,
    registry: &LauncherRegistry,
) -> Option<Command> {
    launcher_command_with_args(launcher, root, registry, &[])
}

pub(crate) fn launcher_command_with_args(
    launcher: &str,
    root: &Root,
    registry: &LauncherRegistry,
    action_args: &[String],
) -> Option<Command> {
    let Some(template) = registry.get(launcher) else {
        tracing::warn!(launcher, "unknown launcher; skipping");
        return None;
    };
    let mut args = template.default_args.clone();
    args.extend(action_args.iter().cloned());
    let argv = match root.host.as_deref() {
        Some(host) => {
            let Some(remote) = template.remote.as_ref() else {
                tracing::warn!(
                    launcher,
                    host,
                    path = %root.path,
                    "launcher has no remote template; skipping"
                );
                return None;
            };
            expand_launcher_template(remote, &root.path, Some(host), &args)
        }
        None => {
            let Some(local) = template.local.as_ref() else {
                tracing::warn!(
                    launcher,
                    path = %root.path,
                    "launcher has no local template; skipping"
                );
                return None;
            };
            expand_launcher_template(local, &root.path, None, &args)
        }
    };

    let mut iter = argv.into_iter();
    let Some(program) = iter.next() else {
        tracing::warn!(
            launcher,
            "launcher template expanded to an empty command; skipping"
        );
        return None;
    };
    let mut cmd = Command::new(program);
    cmd.args(iter);
    Some(cmd)
}

pub(crate) fn spawn_detached_command(mut cmd: Command, label: &str, root: &Root) -> Result<()> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = cmd
        .spawn()
        .with_context(|| format!("spawn action '{label}'"))?;
    let pid = child.id();
    tracing::info!(
        action = label,
        pid,
        host = root.host.as_deref().unwrap_or("local"),
        path = %root.path,
        "spawned action",
    );
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok(())
}

pub(crate) fn expand_launcher_template(
    argv: &[String],
    path: &str,
    host: Option<&str>,
    args: &[String],
) -> Vec<String> {
    let args_shell = shell_join(args);
    let mut out = Vec::new();
    for arg in argv {
        if arg == "{args}" {
            out.extend(args.iter().cloned());
            continue;
        }
        let mut expanded = arg.replace("{path}", path);
        if let Some(host) = host {
            expanded = expanded.replace("{host}", host);
        }
        expanded = expanded.replace("{args}", &args_shell);
        out.push(expanded);
    }
    out
}

pub(crate) fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '=' | '+'))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(crate) fn truncate_str(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => format!("{}…", &s[..byte_idx]),
        None => s.to_string(),
    }
}

pub(crate) fn launcher_available_for_root(launcher: &LauncherTemplate, root: &Root) -> bool {
    match root.host {
        Some(_) => launcher.remote.is_some(),
        None => launcher.local.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_args_as_argv_or_shell_string() {
        let argv = vec![
            "tool".to_string(),
            "{args}".to_string(),
            "wrapped {args}".to_string(),
        ];
        let args = vec!["--flag".to_string(), "two words".to_string()];

        assert_eq!(
            expand_launcher_template(&argv, "/tmp/project", None, &args),
            vec![
                "tool".to_string(),
                "--flag".to_string(),
                "two words".to_string(),
                "wrapped --flag 'two words'".to_string(),
            ],
        );
    }
}
