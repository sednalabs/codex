//! Applies agent-role configuration layers on top of an existing session config.
//!
//! Roles are selected at spawn time and are loaded with the same config machinery as
//! `config.toml`. This module resolves built-in and user-defined role files, inserts the role as a
//! high-precedence layer, and preserves the caller's current profile/provider unless the role
//! explicitly takes ownership of model selection. It does not decide when to spawn a sub-agent or
//! which role to use; the multi-agent tool handler owns that orchestration.

use crate::config::AgentRoleConfig;
use crate::config::Config;
use crate::config::ConfigOverrides;
use crate::config::ConfigToml;
use crate::config::agent_roles::parse_agent_role_file_contents;
use crate::config::deserialize_config_toml_with_base;
use crate::config_loader::ConfigLayerEntry;
use crate::config_loader::ConfigLayerStack;
use crate::config_loader::ConfigLayerStackOrdering;
use crate::config_loader::resolve_relative_paths_in_config_toml;
use codex_app_server_protocol::ConfigLayerSource;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::LazyLock;
use toml::Value as TomlValue;

/// The role name used when a caller omits `agent_type`.
pub const DEFAULT_ROLE_NAME: &str = "default";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RoleModelOverrideLocks {
    pub(crate) model: bool,
    pub(crate) model_reasoning_effort: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct RoleActiveProfileFieldUpdates {
    model: bool,
    model_provider: bool,
    model_reasoning_effort: bool,
    model_reasoning_summary: bool,
    model_verbosity: bool,
}

struct RoleLayerConfig {
    role_config: ConfigToml,
    role_layer_toml: TomlValue,
}

async fn load_role_layer_config(
    config: &Config,
    role_name: &str,
) -> Result<Option<RoleLayerConfig>, String> {
    let role = resolve_role_config(config, role_name)
        .ok_or_else(|| format!("unknown agent_type '{role_name}'"))?;
    let Some(config_file) = role.config_file.as_deref() else {
        return Ok(None);
    };
    let is_built_in = !config.agent_roles.contains_key(role_name);

    let (role_config_toml, role_config_base) = if is_built_in {
        let role_config_contents =
            built_in::config_file_contents(config_file).ok_or_else(|| {
                format!(
                    "agent type '{role_name}' built-in config '{}' is unavailable",
                    config_file.display()
                )
            })?;
        let role_config_toml: TomlValue = toml::from_str(role_config_contents).map_err(|err| {
            format!(
                "failed to parse built-in config for agent type '{role_name}' ({}): {err}",
                config_file.display()
            )
        })?;
        (role_config_toml, config.codex_home.as_path())
    } else {
        let role_config_contents = tokio::fs::read_to_string(config_file)
            .await
            .map_err(|err| {
                format!(
                    "failed to read config for agent type '{role_name}' ({}): {err}",
                    config_file.display()
                )
            })?;
        let role_dir = config_file.parent().ok_or_else(|| {
            format!(
                "config file for agent type '{role_name}' has no parent directory: {}",
                config_file.display()
            )
        })?;
        let role_config_toml = parse_agent_role_file_contents(
            &role_config_contents,
            config_file,
            role_dir,
            Some(role_name),
        )
        .map_err(|err| {
            format!(
                "failed to parse config for agent type '{role_name}' ({}): {err}",
                config_file.display()
            )
        })?
        .config;
        (role_config_toml, role_dir)
    };

    let role_config = deserialize_config_toml_with_base(role_config_toml.clone(), role_config_base)
        .map_err(|err| {
            format!(
                "failed to deserialize config for agent type '{role_name}' ({}): {err}",
                config_file.display()
            )
        })?;
    let role_layer_toml = resolve_relative_paths_in_config_toml(role_config_toml, role_config_base)
        .map_err(|err| {
            format!(
                "failed to resolve relative paths for agent type '{role_name}' ({}): {err}",
                config_file.display()
            )
        })?;

    Ok(Some(RoleLayerConfig {
        role_config,
        role_layer_toml,
    }))
}

fn role_profile_field_updates(
    profile_name: Option<&str>,
    role_layer_toml: &TomlValue,
) -> RoleActiveProfileFieldUpdates {
    profile_name
        .and_then(|name| {
            role_layer_toml
                .get("profiles")
                .and_then(TomlValue::as_table)
                .and_then(|profiles| profiles.get(name))
                .and_then(TomlValue::as_table)
        })
        .map(|profile_updates| RoleActiveProfileFieldUpdates {
            model: profile_updates.contains_key("model"),
            model_provider: profile_updates.contains_key("model_provider"),
            model_reasoning_effort: profile_updates.contains_key("model_reasoning_effort"),
            model_reasoning_summary: profile_updates.contains_key("model_reasoning_summary"),
            model_verbosity: profile_updates.contains_key("model_verbosity"),
        })
        .unwrap_or_default()
}

fn role_preserves_current_profile(role_config: &ConfigToml) -> bool {
    role_config.model_provider.is_none() && role_config.profile.is_none()
}

fn role_layer_stack_with_session_flags(
    config: &Config,
    role_name: &str,
    role_layer_toml: &TomlValue,
) -> Result<ConfigLayerStack, String> {
    let mut layers: Vec<ConfigLayerEntry> = config
        .config_layer_stack
        .get_layers(
            ConfigLayerStackOrdering::LowestPrecedenceFirst,
            /*include_disabled*/ true,
        )
        .into_iter()
        .cloned()
        .collect();
    let layer = ConfigLayerEntry::new(ConfigLayerSource::SessionFlags, role_layer_toml.clone());
    let insertion_index =
        layers.partition_point(|existing_layer| existing_layer.name <= layer.name);
    layers.insert(insertion_index, layer);

    ConfigLayerStack::new(
        layers,
        config.config_layer_stack.requirements().clone(),
        config.config_layer_stack.requirements_toml().clone(),
    )
    .map_err(|err| format!("failed to create layered config for agent type '{role_name}': {err}"))
}

fn effective_role_profile_after_precedence(
    config: &Config,
    role_config: &ConfigToml,
    role_name: &str,
    role_layer_toml: &TomlValue,
) -> Result<Option<String>, String> {
    let config_layer_stack =
        role_layer_stack_with_session_flags(config, role_name, role_layer_toml)?;
    let merged_toml = config_layer_stack.effective_config();
    let merged_config = deserialize_config_toml_with_base(merged_toml, &config.codex_home)
        .map_err(|err| {
            format!("failed to deserialize merged config for agent type '{role_name}': {err}")
        })?;
    let preserve_current_profile = role_preserves_current_profile(role_config);

    if preserve_current_profile {
        Ok(config.active_profile.clone().or(merged_config.profile))
    } else {
        Ok(merged_config.profile)
    }
}

/// Applies a named role layer to `config` while preserving caller-owned model selection.
///
/// The role layer is inserted at session-flag precedence so it can override persisted config, but
/// the caller's current `profile` and `model_provider` remain sticky runtime choices unless the
/// role explicitly sets `profile`, explicitly sets `model_provider`, or rewrites the active
/// profile's `model_provider` in place. Likewise, the caller's already-selected model and
/// reasoning settings remain sticky unless the role explicitly owns those fields. Rebuilding the
/// config without reapplying those runtime choices would make a spawned agent silently drift back
/// to inherited parent-profile settings, which is the bug this preservation logic avoids.
pub(crate) async fn apply_role_to_config(
    config: &mut Config,
    role_name: Option<&str>,
) -> Result<(), String> {
    let role_name = role_name.unwrap_or(DEFAULT_ROLE_NAME);
    let Some(RoleLayerConfig {
        role_config,
        role_layer_toml,
    }) = load_role_layer_config(config, role_name).await?
    else {
        return Ok(());
    };
    let role_selects_model = role_config.model.is_some();
    let role_selects_reasoning_effort = role_config.model_reasoning_effort.is_some();
    let role_selects_reasoning_summary = role_config.model_reasoning_summary.is_some();
    let role_selects_verbosity = role_config.model_verbosity.is_some();
    let active_profile_updates =
        role_profile_field_updates(config.active_profile.as_deref(), &role_layer_toml);
    // A role that does not explicitly take ownership of model selection should inherit the
    // caller's current profile/provider choices across the config reload.
    let preserve_current_profile = role_preserves_current_profile(&role_config);
    let preserve_current_provider =
        preserve_current_profile && !active_profile_updates.model_provider;
    let preserved_model = if preserve_current_profile && !active_profile_updates.model {
        if role_selects_model {
            role_config.model.clone()
        } else {
            config.model.clone()
        }
    } else {
        None
    };
    let preserved_reasoning_effort =
        if preserve_current_profile && !active_profile_updates.model_reasoning_effort {
            if role_selects_reasoning_effort {
                role_config.model_reasoning_effort
            } else {
                config.model_reasoning_effort
            }
        } else {
            None
        };
    let preserved_reasoning_summary =
        if preserve_current_profile && !active_profile_updates.model_reasoning_summary {
            if role_selects_reasoning_summary {
                role_config.model_reasoning_summary
            } else {
                config.model_reasoning_summary
            }
        } else {
            None
        };
    let preserved_verbosity = if preserve_current_profile && !active_profile_updates.model_verbosity
    {
        if role_selects_verbosity {
            role_config.model_verbosity
        } else {
            config.model_verbosity
        }
    } else {
        None
    };

    let config_layer_stack =
        role_layer_stack_with_session_flags(config, role_name, &role_layer_toml)?;

    let merged_toml = config_layer_stack.effective_config();
    let merged_config = deserialize_config_toml_with_base(merged_toml, &config.codex_home)
        .map_err(|err| {
            format!("failed to deserialize merged config for agent type '{role_name}': {err}")
        })?;
    let next_config = Config::load_config_with_layer_stack(
        merged_config,
        ConfigOverrides {
            model: preserved_model,
            model_reasoning_effort: preserved_reasoning_effort,
            model_reasoning_summary: preserved_reasoning_summary,
            model_verbosity: preserved_verbosity,
            cwd: Some(config.cwd.clone()),
            model_provider: preserve_current_provider.then(|| config.model_provider_id.clone()),
            config_profile: preserve_current_profile
                .then(|| config.active_profile.clone())
                .flatten(),
            codex_linux_sandbox_exe: config.codex_linux_sandbox_exe.clone(),
            main_execve_wrapper_exe: config.main_execve_wrapper_exe.clone(),
            js_repl_node_path: config.js_repl_node_path.clone(),
            ..Default::default()
        },
        config.codex_home.clone(),
        config_layer_stack,
    )
    .map_err(|err| format!("failed to apply merged config for agent type '{role_name}': {err}"))?;
    *config = next_config;

    Ok(())
}

pub(crate) async fn role_model_override_locks(
    config: &Config,
    role_name: Option<&str>,
) -> Result<RoleModelOverrideLocks, String> {
    let role_name = role_name.unwrap_or(DEFAULT_ROLE_NAME);
    let Some(RoleLayerConfig {
        role_config,
        role_layer_toml,
    }) = load_role_layer_config(config, role_name).await?
    else {
        return Ok(RoleModelOverrideLocks::default());
    };
    let effective_profile =
        effective_role_profile_after_precedence(config, &role_config, role_name, &role_layer_toml)?;
    let active_profile_updates =
        role_profile_field_updates(effective_profile.as_deref(), &role_layer_toml);

    Ok(RoleModelOverrideLocks {
        model: role_config.model.is_some() || active_profile_updates.model,
        model_reasoning_effort: role_config.model_reasoning_effort.is_some()
            || active_profile_updates.model_reasoning_effort,
    })
}

pub(crate) fn resolve_role_config<'a>(
    config: &'a Config,
    role_name: &str,
) -> Option<&'a AgentRoleConfig> {
    config
        .agent_roles
        .get(role_name)
        .or_else(|| built_in::configs().get(role_name))
}

pub(crate) mod spawn_tool_spec {
    use super::*;

    /// Builds the spawn-agent tool description text from built-in and configured roles.
    pub(crate) fn build(user_defined_agent_roles: &BTreeMap<String, AgentRoleConfig>) -> String {
        let built_in_roles = built_in::configs();
        build_from_configs(built_in_roles, user_defined_agent_roles)
    }

    // This function is not inlined for testing purpose.
    fn build_from_configs(
        built_in_roles: &BTreeMap<String, AgentRoleConfig>,
        user_defined_roles: &BTreeMap<String, AgentRoleConfig>,
    ) -> String {
        let mut seen = BTreeSet::new();
        let mut formatted_roles = Vec::new();
        for (name, declaration) in user_defined_roles {
            if seen.insert(name.as_str()) {
                formatted_roles.push(format_role(name, declaration));
            }
        }
        for (name, declaration) in built_in_roles {
            if seen.insert(name.as_str()) {
                formatted_roles.push(format_role(name, declaration));
            }
        }

        format!(
            "Optional type name for the new agent. If omitted, `{DEFAULT_ROLE_NAME}` is used.\nAvailable roles:\n{}",
            formatted_roles.join("\n"),
        )
    }

    fn format_role(name: &str, declaration: &AgentRoleConfig) -> String {
        if let Some(description) = &declaration.description {
            let locked_settings_note = declaration
                .config_file
                .as_ref()
                .and_then(|config_file| {
                    built_in::config_file_contents(config_file)
                        .map(str::to_owned)
                        .or_else(|| std::fs::read_to_string(config_file).ok())
                })
                .and_then(|contents| toml::from_str::<TomlValue>(&contents).ok())
                .map(|role_toml| {
                    let model = role_toml
                        .get("model")
                        .and_then(TomlValue::as_str);
                    let reasoning_effort = role_toml
                        .get("model_reasoning_effort")
                        .and_then(TomlValue::as_str);

                    match (model, reasoning_effort) {
                        (Some(model), Some(reasoning_effort)) => format!(
                            "\n- This role's model is set to `{model}` and its reasoning effort is set to `{reasoning_effort}`. These settings cannot be changed."
                        ),
                        (Some(model), None) => {
                            format!(
                                "\n- This role's model is set to `{model}` and cannot be changed."
                            )
                        }
                        (None, Some(reasoning_effort)) => {
                            format!(
                                "\n- This role's reasoning effort is set to `{reasoning_effort}` and cannot be changed."
                            )
                        }
                        (None, None) => String::new(),
                    }
                })
                .unwrap_or_default();
            format!("{name}: {{\n{description}{locked_settings_note}\n}}")
        } else {
            format!("{name}: no description")
        }
    }
}

mod built_in {
    use super::*;

    /// Returns the cached built-in role declarations defined in this module.
    pub(super) fn configs() -> &'static BTreeMap<String, AgentRoleConfig> {
        static CONFIG: LazyLock<BTreeMap<String, AgentRoleConfig>> = LazyLock::new(|| {
            BTreeMap::from([
                (
                    DEFAULT_ROLE_NAME.to_string(),
                    AgentRoleConfig {
                        description: Some("Default agent.".to_string()),
                        config_file: None,
                        nickname_candidates: None,
                    }
                ),
                (
                    "explorer".to_string(),
                    AgentRoleConfig {
                        description: Some(r#"Use `explorer` for specific codebase questions.
Explorers are fast and authoritative.
They must be used to ask specific, well-scoped questions on the codebase.
Rules:
- In order to avoid redundant work, you should avoid exploring the same problem that explorers have already covered. Typically, you should trust the explorer results without additional verification. You are still allowed to inspect the code yourself to gain the needed context!
- You are encouraged to spawn up multiple explorers in parallel when you have multiple distinct questions to ask about the codebase that can be answered independently. This allows you to get more information faster without waiting for one question to finish before asking the next. While waiting for the explorer results, you can continue working on other local tasks that do not depend on those results. This parallelism is a key advantage of delegation, so use it whenever you have multiple questions to ask.
- Reuse existing explorers for related questions."#.to_string()),
                        config_file: Some("explorer.toml".to_string().parse().unwrap_or_default()),
                        nickname_candidates: None,
                    }
                ),
                (
                    "worker".to_string(),
                    AgentRoleConfig {
                        description: Some(r#"Use for execution and production work.
Typical tasks:
- Implement part of a feature
- Fix tests or bugs
- Split large refactors into independent chunks
Rules:
- Explicitly assign **ownership** of the task (files / responsibility). When the subtask involves code changes, you should clearly specify which files or modules the worker is responsible for. This helps avoid merge conflicts and ensures accountability. For example, you can say "Worker 1 is responsible for updating the authentication module, while Worker 2 will handle the database layer." By defining clear ownership, you can delegate more effectively and reduce coordination overhead.
- Always tell workers they are **not alone in the codebase**, and they should not revert the edits made by others, and they should adjust their implementation to accommodate the changes made by others. This is important because there may be multiple workers making changes in parallel, and they need to be aware of each other's work to avoid conflicts and ensure a cohesive final product."#.to_string()),
                        config_file: None,
                        nickname_candidates: None,
                    }
                ),
                // Awaiter is temp removed
//                 (
//                     "awaiter".to_string(),
//                     AgentRoleConfig {
//                         description: Some(r#"Use an `awaiter` agent EVERY TIME you must run a command that will take some very long time.
// This includes, but not only:
// * testing
// * monitoring of a long running process
// * explicit ask to wait for something
//
// Rules:
// - When an awaiter is running, you can work on something else. If you need to wait for its completion, use the largest possible timeout.
// - Be patient with the `awaiter`.
// - Do not use an awaiter for every compilation/test if it won't take time. Only use if for long running commands.
// - Close the awaiter when you're done with it."#.to_string()),
//                         config_file: Some("awaiter.toml".to_string().parse().unwrap_or_default()),
//                     }
//                 )
            ])
        });
        &CONFIG
    }

    /// Resolves a built-in role `config_file` path to embedded content.
    pub(super) fn config_file_contents(path: &Path) -> Option<&'static str> {
        const EXPLORER: &str = include_str!("builtins/explorer.toml");
        const AWAITER: &str = include_str!("builtins/awaiter.toml");
        match path.to_str()? {
            "explorer.toml" => Some(EXPLORER),
            "awaiter.toml" => Some(AWAITER),
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "role_tests.rs"]
mod tests;
