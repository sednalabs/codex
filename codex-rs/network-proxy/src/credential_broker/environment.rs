use super::BROKERED_CREDENTIAL_ALIAS_MARKER_PREFIX;
use super::BROKERED_CREDENTIALS_ENV_KEY;
use super::CREDENTIAL_BROKER_ACTIVE_ENV_KEY;
use super::CredentialBrokerState;
use super::MIN_EMBEDDED_CREDENTIAL_LENGTH;
use super::env_entry;
use super::env_key_matches;
use super::env_value;
use super::matching;
use super::providers;
use super::remove_env_value;
use super::set_env_value;
use std::collections::HashMap;

pub(super) fn update_brokered_credentials_marker(
    state: &CredentialBrokerState,
    env: &mut HashMap<String, String>,
) {
    let mut brokered = state
        .credentials
        .iter()
        .filter(|credential| {
            env.iter().any(|(key, value)| {
                !env_key_matches(key, CREDENTIAL_BROKER_ACTIVE_ENV_KEY)
                    && !env_key_matches(key, BROKERED_CREDENTIALS_ENV_KEY)
                    && (value == &credential.dummy_value
                        || credential.dummy_value.len() >= MIN_EMBEDDED_CREDENTIAL_LENGTH
                            && value.contains(&credential.dummy_value))
            })
        })
        .map(|credential| (credential.env_var.clone(), credential.dummy_value.clone()))
        .collect::<Vec<_>>();
    let brokered_dummy_values = brokered
        .iter()
        .map(|(_, dummy_value)| dummy_value.clone())
        .collect::<Vec<_>>();
    brokered.extend(state.credential_aliases.iter().filter_map(|alias| {
        let dummy_value = brokered_dummy_values.iter().find(|&dummy_value| {
            alias.dummy_value == *dummy_value
                || dummy_value.len() >= MIN_EMBEDDED_CREDENTIAL_LENGTH
                    && alias.dummy_value.contains(dummy_value)
        })?;
        Some((
            format!("{BROKERED_CREDENTIAL_ALIAS_MARKER_PREFIX}{}", alias.env_var),
            dummy_value.clone(),
        ))
    }));
    brokered.sort_unstable();
    brokered.dedup();
    match serde_json::to_string(&brokered) {
        Ok(marker) => {
            set_env_value(env, BROKERED_CREDENTIALS_ENV_KEY, marker);
        }
        Err(_) => {
            remove_env_value(env, BROKERED_CREDENTIALS_ENV_KEY);
        }
    }
}

/// Returns supported environment keys whose current values still match the child-scoped dummy
/// values recorded by the credential broker.
///
/// The broker marker is treated as untrusted: malformed metadata, unsupported keys, and values
/// replaced by the user are ignored. The environment is not mutated; callers own the decision to
/// remove the returned keys.
pub fn brokered_credential_dummy_env_keys(env: &HashMap<String, String>) -> Vec<String> {
    let mut keys = env_value(env, BROKERED_CREDENTIALS_ENV_KEY)
        .and_then(|marker| serde_json::from_str::<Vec<(String, String)>>(marker).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(key, dummy_value)| {
            providers::credential_env_keys()
                .any(|candidate| env_key_matches(&key, candidate))
                .then_some(())?;
            let (actual_key, actual_value) = env_entry(env, &key)?;
            (actual_value == dummy_value.as_str()).then(|| actual_key.to_string())
        })
        .collect::<Vec<_>>();
    let context_bound = |key: &str| {
        providers::credential_providers().any(|provider| {
            provider.sources().iter().any(|source| {
                source
                    .env_vars
                    .iter()
                    .any(|candidate| env_key_matches(key, candidate))
                    && source
                        .binding_env_vars
                        .iter()
                        .any(|context| env_value(env, context).is_some())
            })
        })
    };
    keys.sort_unstable_by(|left, right| {
        context_bound(right)
            .cmp(&context_bound(left))
            .then_with(|| left.cmp(right))
    });
    keys
}

/// Returns canonical credential and known alias keys recorded for an active brokered child,
/// including keys that are currently absent.
pub fn brokered_credential_marker_env_keys(env: &HashMap<String, String>) -> Vec<String> {
    if env_value(env, CREDENTIAL_BROKER_ACTIVE_ENV_KEY) != Some("1") {
        return Vec::new();
    }
    let entries = env_value(env, BROKERED_CREDENTIALS_ENV_KEY)
        .and_then(|marker| serde_json::from_str::<Vec<(String, String)>>(marker).ok())
        .unwrap_or_default();
    let dummy_values = entries
        .iter()
        .filter(|(key, _)| {
            providers::credential_env_keys().any(|candidate| env_key_matches(key, candidate))
        })
        .map(|(_, dummy_value)| dummy_value.clone())
        .collect::<Vec<_>>();
    let mut keys = entries
        .into_iter()
        .filter_map(|(key, value)| {
            if providers::credential_env_keys().any(|candidate| env_key_matches(&key, candidate)) {
                return Some(key);
            }
            let alias_key = key.strip_prefix(BROKERED_CREDENTIAL_ALIAS_MARKER_PREFIX)?;
            let known_alias = dummy_values.iter().any(|dummy_value| {
                value == *dummy_value
                    || dummy_value.len() >= MIN_EMBEDDED_CREDENTIAL_LENGTH
                        && value.contains(dummy_value)
            });
            known_alias.then(|| alias_key.to_string())
        })
        .collect::<Vec<_>>();
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// Returns environment keys whose current values are brokered dummies or aliases containing them,
/// including aliases retained after their canonical source variable was removed.
pub fn brokered_credential_value_env_keys(env: &HashMap<String, String>) -> Vec<String> {
    if env_value(env, CREDENTIAL_BROKER_ACTIVE_ENV_KEY) != Some("1") {
        return Vec::new();
    }
    let dummy_values = env_value(env, BROKERED_CREDENTIALS_ENV_KEY)
        .and_then(|marker| serde_json::from_str::<Vec<(String, String)>>(marker).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(key, dummy_value)| {
            providers::credential_env_keys()
                .any(|candidate| env_key_matches(&key, candidate))
                .then_some(dummy_value)
        })
        .collect::<Vec<_>>();
    let mut keys = env
        .iter()
        .filter(|(key, _)| {
            !env_key_matches(key, CREDENTIAL_BROKER_ACTIVE_ENV_KEY)
                && !env_key_matches(key, BROKERED_CREDENTIALS_ENV_KEY)
        })
        .filter(|(_, value)| {
            dummy_values.iter().any(|dummy| {
                value.as_str() == dummy.as_str()
                    || dummy.len() >= MIN_EMBEDDED_CREDENTIAL_LENGTH
                        && value.contains(dummy.as_str())
            })
        })
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    keys.sort_unstable();
    keys
}

/// Returns environment keys used to bind currently brokered credentials to hosts.
pub fn brokered_credential_binding_env_keys(
    env: &HashMap<String, String>,
) -> impl Iterator<Item = &'static str> {
    let brokered_keys = brokered_credential_dummy_env_keys(env);
    providers::credential_binding_env_keys(&brokered_keys)
        .filter(|key| env_value(env, key).is_some())
        .collect::<Vec<_>>()
        .into_iter()
}

/// Returns environment keys used to bind registered credential providers to their destinations.
pub fn credential_broker_provider_context_env_keys() -> impl Iterator<Item = &'static str> {
    providers::credential_providers().flat_map(|provider| provider.context_env_vars.iter().copied())
}

/// Checks whether each credential in a value has an allowed source environment key.
pub fn credential_broker_provider_sources_allowed(
    value: &str,
    virtualized: &str,
    source_env: &HashMap<String, String>,
    is_allowed: impl Fn(&str) -> bool,
) -> bool {
    let mut recognized = false;
    let allowed = providers::credential_providers()
        .filter(move |provider| {
            provider.credential_prefixes.iter().any(|prefix| {
                value.match_indices(*prefix).any(|(start, _)| {
                    matching::recognized_credential_match(provider, value, virtualized, start)
                        .is_some()
                })
            })
        })
        .all(|provider| {
            recognized = true;
            let actual_sources = provider
                .sources()
                .iter()
                .flat_map(|source| source.env_vars.iter().copied())
                .filter(|source| {
                    env_value(source_env, source).is_some_and(|source_value| {
                        source_value.len() >= provider.minimum_credential_len
                            && value.contains(source_value)
                    })
                })
                .collect::<Vec<_>>();
            let unattributed = provider.credential_prefixes.iter().any(|prefix| {
                value.match_indices(*prefix).any(|(start, _)| {
                    matching::recognized_credential_match(provider, value, virtualized, start)
                        .is_some_and(|credential| {
                            !actual_sources.iter().any(|source| {
                                env_value(source_env, source).is_some_and(|source_value| {
                                    credential == source_value
                                        || credential
                                            .strip_prefix(source_value)
                                            .is_some_and(|suffix| suffix.starts_with(['_', '-']))
                                })
                            })
                        })
                })
            });
            if actual_sources.is_empty() || unattributed {
                provider
                    .sources()
                    .iter()
                    .flat_map(|source| source.env_vars.iter().copied())
                    .all(&is_allowed)
            } else {
                actual_sources.iter().all(|source| {
                    actual_sources.iter().any(|equivalent| {
                        env_value(source_env, source) == env_value(source_env, equivalent)
                            && is_allowed(equivalent)
                    })
                })
            }
        });
    allowed && recognized
}

/// Returns whether an environment key belongs to a supported credential provider.
pub fn is_credential_broker_provider_env_key(key: &str) -> bool {
    providers::credential_providers().any(|provider| {
        provider
            .sources()
            .iter()
            .flat_map(|source| source.env_vars.iter().copied())
            .chain(provider.context_env_vars.iter().copied())
            .any(|candidate| env_key_matches(key, candidate))
    })
}

/// Returns credential keys plus provider context keys already present in an environment with an
/// active broker.
pub fn brokered_credential_env_keys(
    env: &HashMap<String, String>,
) -> impl Iterator<Item = &'static str> {
    let active = env_value(env, CREDENTIAL_BROKER_ACTIVE_ENV_KEY).is_some_and(|value| value == "1");
    let mut keys = Vec::new();
    if active {
        let brokered_keys = brokered_credential_dummy_env_keys(env);
        keys.extend(providers::credential_env_keys().filter(|key| {
            brokered_keys
                .iter()
                .any(|brokered_key| env_key_matches(brokered_key, key))
        }));
        keys.extend(
            providers::credential_context_env_keys(&brokered_keys)
                .filter(|key| env_value(env, key).is_some()),
        );
    }
    keys.into_iter()
}
