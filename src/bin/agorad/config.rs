use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub(crate) type LauncherRegistry = BTreeMap<String, LauncherTemplate>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LauncherTemplate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<Vec<String>>,
    #[serde(default)]
    pub default_args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default)]
    pub actions: BTreeMap<String, LauncherAction>,
    #[serde(default)]
    pub disabled: bool,
}

impl LauncherTemplate {
    fn new(label: &str, group: &str) -> Self {
        Self {
            local: None,
            remote: None,
            default_args: Vec::new(),
            label: Some(label.into()),
            group: Some(group.into()),
            actions: BTreeMap::new(),
            disabled: false,
        }
    }

    fn local(mut self, parts: &[&str]) -> Self {
        self.local = Some(argv(parts));
        self
    }

    fn remote(mut self, parts: &[&str]) -> Self {
        self.remote = Some(argv(parts));
        self
    }

    fn action(mut self, id: &str, action: LauncherAction) -> Self {
        self.actions.insert(id.into(), action);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LauncherAction {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    #[serde(default)]
    pub order: i32,
    #[serde(default)]
    pub disabled: bool,
}

impl LauncherAction {
    fn new(label: &str, group: &str, order: i32) -> Self {
        Self {
            label: Some(label.into()),
            group: Some(group.into()),
            key: None,
            args: Vec::new(),
            when: None,
            order,
            disabled: false,
        }
    }

    fn key(mut self, k: &str) -> Self {
        self.key = Some(k.into());
        self
    }

    fn args(mut self, a: &[&str]) -> Self {
        self.args = argv(a);
        self
    }

    fn when(mut self, w: &str) -> Self {
        self.when = Some(w.into());
        self
    }
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

#[derive(Default, Deserialize)]
pub(crate) struct AgoraConfig {
    #[serde(default)]
    pub cleanup_all_workspaces: bool,
    #[serde(default = "default_true")]
    pub notify: bool,
    #[serde(default)]
    pub launcher_patches: BTreeMap<String, LauncherPatch>,
}

fn default_true() -> bool {
    true
}

#[derive(Default, Deserialize)]
struct ConfigToml {
    #[serde(default)]
    cleanup_all_workspaces: Option<bool>,
    #[serde(default)]
    notify: Option<bool>,
    #[serde(default)]
    launchers: BTreeMap<String, LauncherPatch>,
}

#[derive(Default, Deserialize)]
pub(crate) struct LauncherPatch {
    #[serde(default)]
    local: Option<Vec<String>>,
    #[serde(default)]
    remote: Option<Vec<String>>,
    #[serde(default)]
    default_args: Option<Vec<String>>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    actions: BTreeMap<String, LauncherActionPatch>,
    #[serde(default)]
    disabled: Option<bool>,
}

#[derive(Default, Deserialize)]
struct LauncherActionPatch {
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    when: Option<String>,
    #[serde(default)]
    order: Option<i32>,
    #[serde(default)]
    disabled: Option<bool>,
}

#[derive(Default, Deserialize)]
struct LegacyConfigJson {
    #[serde(default)]
    cleanup_all_workspaces: bool,
    #[serde(default)]
    claude_extra_args: Vec<String>,
}

pub(crate) fn config_dir() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("XDG_CONFIG_HOME") {
        if !d.is_empty() {
            return Ok(PathBuf::from(d).join("agora"));
        }
    }
    let home = std::env::var_os("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".config/agora"))
}

fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

fn legacy_config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.json"))
}

fn launcher_overrides_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("launchers.json"))
}

pub(crate) fn load_config() -> Result<AgoraConfig> {
    let path = config_path()?;
    if path.exists() {
        let buf = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        return match toml::from_str::<ConfigToml>(&buf) {
            Ok(config) => Ok(AgoraConfig {
                cleanup_all_workspaces: config.cleanup_all_workspaces.unwrap_or(false),
                notify: config.notify.unwrap_or(true),
                launcher_patches: config.launchers,
            }),
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "failed to parse config.toml; using defaults"
                );
                Ok(AgoraConfig::default())
            }
        };
    }

    let path = legacy_config_path()?;
    if !path.exists() {
        return Ok(AgoraConfig::default());
    }

    let buf = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    match serde_json::from_str::<LegacyConfigJson>(&buf) {
        Ok(config) => {
            let mut launcher_patches = BTreeMap::new();
            if !config.claude_extra_args.is_empty() {
                launcher_patches.insert(
                    "claude".into(),
                    LauncherPatch {
                        default_args: Some(config.claude_extra_args),
                        ..LauncherPatch::default()
                    },
                );
            }
            Ok(AgoraConfig {
                cleanup_all_workspaces: config.cleanup_all_workspaces,
                notify: true,
                launcher_patches,
            })
        }
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "failed to parse config.json; using defaults"
            );
            Ok(AgoraConfig::default())
        }
    }
}

pub(crate) fn load_launcher_registry(config: &AgoraConfig) -> Result<(LauncherRegistry, usize)> {
    let mut registry = default_launcher_registry();
    let mut count = 0;
    let path = launcher_overrides_path()?;
    if path.exists() {
        let buf = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let overrides: BTreeMap<String, LauncherPatch> = match serde_json::from_str(&buf) {
            Ok(overrides) => overrides,
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "failed to parse launchers.json; using defaults only"
                );
                BTreeMap::new()
            }
        };
        count += overrides.len();
        apply_launcher_patches(&mut registry, &overrides);
    }

    count += config.launcher_patches.len();
    apply_launcher_patches(&mut registry, &config.launcher_patches);
    Ok((registry, count))
}

pub(crate) fn apply_launcher_patches(
    registry: &mut LauncherRegistry,
    patches: &BTreeMap<String, LauncherPatch>,
) {
    for (name, patch) in patches {
        let launcher = registry
            .entry(name.clone())
            .or_insert_with(|| LauncherTemplate::new(&title_case_id(name), "TOOLS"));
        if let Some(local) = patch.local.clone() {
            launcher.local = Some(local);
        }
        if let Some(remote) = patch.remote.clone() {
            launcher.remote = Some(remote);
        }
        if let Some(default_args) = patch.default_args.clone() {
            launcher.default_args = default_args;
        }
        if let Some(label) = patch.label.clone() {
            launcher.label = Some(label);
        }
        if let Some(group) = patch.group.clone() {
            launcher.group = Some(group);
        }
        if let Some(disabled) = patch.disabled {
            launcher.disabled = disabled;
        }
        for (action_id, action_patch) in &patch.actions {
            let action = launcher
                .actions
                .entry(action_id.clone())
                .or_insert_with(|| LauncherAction {
                    label: Some(title_case_id(action_id)),
                    group: None,
                    key: None,
                    args: Vec::new(),
                    when: None,
                    order: 100,
                    disabled: false,
                });
            if let Some(label) = action_patch.label.clone() {
                action.label = Some(label);
            }
            if let Some(group) = action_patch.group.clone() {
                action.group = Some(group);
            }
            if let Some(key) = action_patch.key.clone() {
                action.key = Some(key);
            }
            if let Some(args) = action_patch.args.clone() {
                action.args = args;
            }
            if let Some(when) = action_patch.when.clone() {
                action.when = Some(when);
            }
            if let Some(order) = action_patch.order {
                action.order = order;
            }
            if let Some(disabled) = action_patch.disabled {
                action.disabled = disabled;
            }
        }
    }
}

pub(crate) fn title_case_id(id: &str) -> String {
    id.split(['-', '_', '.'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn default_launcher_registry() -> LauncherRegistry {
    let mut registry = LauncherRegistry::new();
    registry.insert(
        "terminal".into(),
        LauncherTemplate::new("Open terminal", "OPEN")
            .local(&["kitty", "--directory", "{path}"])
            .remote(&[
                "kitty",
                "--",
                "kitten",
                "ssh",
                "-R",
                "7897:127.0.0.1:7890",
                "{host}",
                "-t",
                "cd {path}; exec /usr/bin/zsh",
            ])
            .action(
                "open",
                LauncherAction::new("Open terminal", "OPEN", 10).key("⌥T"),
            ),
    );
    registry.insert(
        "claude".into(),
        LauncherTemplate::new("Claude", "AGENT")
            .local(&[
                "kitty",
                "--directory",
                "{path}",
                "--",
                "zsh",
                "-ic",
                "claude {args}; exec zsh",
            ])
            .remote(&[
                "kitty",
                "--",
                "kitten",
                "ssh",
                "-R",
                "7897:127.0.0.1:7890",
                "{host}",
                "-t",
                "cd {path}; claude {args}; exec /usr/bin/zsh",
            ])
            .action(
                "new",
                LauncherAction::new("New Claude", "AGENT", 10).key("⌥N"),
            )
            .action(
                "continue",
                LauncherAction::new("Continue Claude", "AGENT", 20)
                    .key("⌥C")
                    .args(&["--continue"])
                    .when("has_agent:claude"),
            )
            .action(
                "resume",
                LauncherAction::new("Resume Claude", "AGENT", 30)
                    .key("⌥R")
                    .args(&["--resume"])
                    .when("has_agent:claude"),
            ),
    );
    registry.insert(
        "codex".into(),
        LauncherTemplate::new("Codex", "AGENT")
            .local(&[
                "kitty",
                "--directory",
                "{path}",
                "--",
                "zsh",
                "-ic",
                "codex {args}; exec zsh",
            ])
            .remote(&[
                "kitty",
                "--",
                "kitten",
                "ssh",
                "-R",
                "7897:127.0.0.1:7890",
                "{host}",
                "-t",
                "cd {path}; codex {args}; exec /usr/bin/zsh",
            ])
            .action(
                "new",
                LauncherAction::new("New Codex", "AGENT", 40).key("⌥X"),
            )
            .action(
                "continue",
                LauncherAction::new("Continue Codex", "AGENT", 50)
                    .key("⌥⇧X")
                    .args(&["resume", "--last"])
                    .when("has_agent:codex"),
            )
            .action(
                "resume",
                LauncherAction::new("Resume Codex", "AGENT", 60)
                    .args(&["resume"])
                    .when("has_agent:codex"),
            ),
    );
    registry.insert(
        "vscode".into(),
        LauncherTemplate::new("Open VS Code", "OPEN")
            .local(&["code", "{path}"])
            .remote(&[
                "code",
                "--folder-uri",
                "vscode-remote://ssh-remote+{host}{path}",
            ])
            .action(
                "open",
                LauncherAction::new("Open VS Code", "OPEN", 20).key("⌥V"),
            ),
    );
    registry.insert(
        "zed".into(),
        LauncherTemplate::new("Open Zed", "OPEN").local(&["zed", "{path}"]),
    );
    registry.insert(
        "file-manager".into(),
        LauncherTemplate::new("Open file manager", "OPEN")
            .local(&["xdg-open", "{path}"])
            .action(
                "open",
                LauncherAction::new("Open file manager", "OPEN", 30)
                    .key("⌥F")
                    .when("local"),
            ),
    );
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_patch_preserves_default_actions() {
        let mut registry = default_launcher_registry();
        let mut patches = BTreeMap::new();
        patches.insert(
            "claude".to_string(),
            LauncherPatch {
                default_args: Some(vec!["--dangerously-skip-permissions".to_string()]),
                ..LauncherPatch::default()
            },
        );

        apply_launcher_patches(&mut registry, &patches);

        let claude = registry.get("claude").unwrap();
        assert_eq!(
            claude.default_args,
            vec!["--dangerously-skip-permissions".to_string()]
        );
        assert!(claude.actions.contains_key("new"));
        assert!(claude.actions.contains_key("continue"));
        assert!(claude.actions.contains_key("resume"));
    }

    #[test]
    fn legacy_launcher_override_preserves_default_actions() {
        let mut registry = default_launcher_registry();
        let mut patches = BTreeMap::new();
        patches.insert(
            "claude".to_string(),
            LauncherPatch {
                local: Some(vec!["custom-claude".to_string(), "{args}".to_string()]),
                ..LauncherPatch::default()
            },
        );
        apply_launcher_patches(&mut registry, &patches);

        let claude = registry.get("claude").unwrap();
        assert_eq!(
            claude.local,
            Some(vec!["custom-claude".to_string(), "{args}".to_string()])
        );
        assert!(claude.actions.contains_key("new"));
        assert!(claude.actions.contains_key("continue"));
        assert!(claude.actions.contains_key("resume"));
    }

    #[test]
    fn codex_defaults_include_resume_actions() {
        let registry = default_launcher_registry();
        let codex = registry.get("codex").unwrap();

        assert_eq!(
            codex.actions.get("continue").unwrap().args,
            vec!["resume".to_string(), "--last".to_string()]
        );
        assert_eq!(
            codex.actions.get("resume").unwrap().args,
            vec!["resume".to_string()]
        );
        assert_eq!(
            codex.actions.get("continue").unwrap().when.as_deref(),
            Some("has_agent:codex")
        );
    }
}
