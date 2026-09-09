mod configured;
mod destination;
mod environment;
mod matching;
mod provider_config;
mod providers;
mod registry;

use crate::config::NetworkProxyConfig;
use crate::policy::normalize_host;
use environment::update_brokered_credentials_marker;
use rama_http::HeaderMap;
use registry::ActiveCredentialSource;
use registry::BrokeredCredentialProvider;
use registry::active_credential_sources;
use registry::prioritized_credentials;
use registry::select_credentials;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;
use url::Url;

pub use environment::brokered_credential_binding_env_keys;
pub use environment::brokered_credential_dummy_env_keys;
pub use environment::brokered_credential_env_keys;
pub use environment::brokered_credential_marker_env_keys;
pub use environment::brokered_credential_value_env_keys;
pub use environment::credential_broker_provider_context_env_keys;
pub use environment::credential_broker_provider_sources_allowed;
pub use environment::is_credential_broker_provider_env_key;
pub use provider_config::CredentialAuthMethod;
pub use provider_config::CredentialProviderConfig;

pub const CREDENTIAL_BROKER_ACTIVE_ENV_KEY: &str = "CODEX_NETWORK_PROXY_CREDENTIAL_BROKER_ACTIVE";
pub(crate) const BROKERED_CREDENTIALS_ENV_KEY: &str = "CODEX_NETWORK_PROXY_BROKERED_CREDENTIALS";
const MIN_EMBEDDED_CREDENTIAL_LENGTH: usize = 16;
const BROKERED_CREDENTIAL_ALIAS_MARKER_PREFIX: &str = "@alias:";

#[derive(Clone)]
pub(crate) struct CredentialBroker {
    state: Arc<RwLock<CredentialBrokerState>>,
}

#[derive(Default)]
struct CredentialBrokerState {
    config_revision: u64,
    enabled: bool,
    allow_local_binding: bool,
    openai_api_host: Option<String>,
    configured_provider_configs: BTreeMap<String, CredentialProviderConfig>,
    configured_providers: Vec<Arc<configured::ConfiguredCredentialProvider>>,
    credentials: Vec<CredentialRecord>,
    credential_owners: Vec<CredentialOwner>,
    credential_aliases: Vec<CredentialAlias>,
}

struct CredentialOwner {
    env_var: String,
    real_value: String,
}

struct CredentialRecord {
    env_var: String,
    provider: BrokeredCredentialProvider,
    host_binding: providers::CredentialHostBinding,
    additional_host_bindings: Vec<providers::CredentialHostBinding>,
    environment_id: Option<String>,
    real_value: String,
    dummy_value: String,
}

impl CredentialRecord {
    fn should_reconcile_host_binding(&self, env: &HashMap<String, String>) -> bool {
        env_contains_credential_value(env, &self.dummy_value)
            || self.invalidates_host_binding(env)
                && env.iter().any(|(key, value)| {
                    !env_key_matches(key, CREDENTIAL_BROKER_ACTIVE_ENV_KEY)
                        && !env_key_matches(key, BROKERED_CREDENTIALS_ENV_KEY)
                        && !crate::is_managed_proxy_env_var(key, value)
                        && (value == &self.real_value
                            || self
                                .provider
                                .contains_embedded_value(value, &self.real_value))
                })
    }

    fn invalidates_host_binding(&self, env: &HashMap<String, String>) -> bool {
        match &self.provider {
            BrokeredCredentialProvider::Builtin(provider) => {
                provider.sources().iter().any(|source| {
                    source
                        .env_vars
                        .iter()
                        .any(|key| env_key_matches(key, &self.env_var))
                        && (source.invalidates_host_binding)(env)
                })
            }
            BrokeredCredentialProvider::Configured(provider) => {
                provider
                    .config
                    .url_prefix_from_env
                    .as_deref()
                    .is_some_and(|key| env_value(env, key).is_some())
                    && provider.dynamic_destination(env).is_none()
            }
        }
    }

    fn observe_host_binding(
        &mut self,
        host_binding: providers::CredentialHostBinding,
        env: &HashMap<String, String>,
    ) {
        if self.invalidates_host_binding(env) {
            self.additional_host_bindings.clear();
            self.host_binding = host_binding;
            return;
        }
        if self.host_binding == host_binding {
            return;
        }
        // Track the latest source separately, but keep prior destinations usable by
        // concurrent commands sharing this dummy in the same environment.
        self.additional_host_bindings
            .retain(|binding| binding != &host_binding);
        self.additional_host_bindings
            .push(std::mem::replace(&mut self.host_binding, host_binding));
    }

    fn host_bindings(&self) -> impl Iterator<Item = &providers::CredentialHostBinding> {
        std::iter::once(&self.host_binding).chain(&self.additional_host_bindings)
    }

    fn belongs_to_environment(&self, environment_id: Option<&str>) -> bool {
        self.environment_id.as_deref() == environment_id
    }
}

fn source_accepts_credential(
    source: &ActiveCredentialSource,
    credential: &CredentialRecord,
) -> bool {
    credential.provider.same_provider(&source.provider)
        && source
            .env_vars
            .iter()
            .any(|env_var| env_key_matches(env_var, &credential.env_var))
}

fn source_tracks_credential(
    source: &ActiveCredentialSource,
    credential: &CredentialRecord,
    env: &HashMap<String, String>,
) -> bool {
    if !source_accepts_credential(source, credential) {
        return false;
    }
    let mut source_present = false;
    let contains_credential = source
        .env_vars
        .iter()
        .filter_map(|env_var| env_value(env, env_var))
        .any(|value| {
            source_present = true;
            let value = match &source.provider {
                BrokeredCredentialProvider::Builtin(_) => value.trim(),
                BrokeredCredentialProvider::Configured(_) => value,
            };
            value == credential.dummy_value || value == credential.real_value
        });
    !source_present || contains_credential
}

struct CredentialAlias {
    env_var: String,
    dummy_value: String,
}

enum CredentialEnvironment {
    Child,
    Snapshot,
}

fn env_key_matches(candidate: &str, expected: &str) -> bool {
    if cfg!(windows) {
        candidate.eq_ignore_ascii_case(expected)
    } else {
        candidate == expected
    }
}

fn env_entry<'a>(env: &'a HashMap<String, String>, key: &str) -> Option<(&'a str, &'a str)> {
    env.iter()
        .find(|(candidate, _)| env_key_matches(candidate, key))
        .map(|(key, value)| (key.as_str(), value.as_str()))
}

pub(super) fn env_value<'a>(env: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    env_entry(env, key).map(|(_, value)| value)
}

fn set_env_value(env: &mut HashMap<String, String>, key: &str, value: String) {
    if cfg!(windows) {
        env.retain(|candidate, _| !env_key_matches(candidate, key));
    }
    env.insert(key.to_string(), value);
}

fn remove_env_value(env: &mut HashMap<String, String>, key: &str) {
    if cfg!(windows) {
        env.retain(|candidate, _| !env_key_matches(candidate, key));
    } else {
        env.remove(key);
    }
}

fn env_contains_credential_value(env: &HashMap<String, String>, credential: &str) -> bool {
    env.iter().any(|(key, value)| {
        !env_key_matches(key, CREDENTIAL_BROKER_ACTIVE_ENV_KEY)
            && !env_key_matches(key, BROKERED_CREDENTIALS_ENV_KEY)
            && value.contains(credential)
            && !crate::is_managed_proxy_env_var(key, value)
    })
}

impl CredentialBroker {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            state: Arc::new(RwLock::new(CredentialBrokerState {
                enabled,
                ..CredentialBrokerState::default()
            })),
        }
    }

    pub(crate) fn configure(&self, config: &NetworkProxyConfig) {
        let mut state = self.write_state();
        if state.enabled != config.credential_broker
            || state.openai_api_host != config.credential_broker_openai_host
            || state.allow_local_binding != config.allow_local_binding
            || state.configured_provider_configs != config.credential_providers
        {
            state.config_revision += 1;
        }
        state.allow_local_binding = config.allow_local_binding;
        if state.enabled != config.credential_broker {
            state.enabled = config.credential_broker;
            state.credentials.clear();
            state.credential_owners.clear();
            state.credential_aliases.clear();
        }
        if state.openai_api_host != config.credential_broker_openai_host {
            state
                .openai_api_host
                .clone_from(&config.credential_broker_openai_host);
            state.credentials.retain(|credential| {
                !matches!(
                    &credential.provider,
                    BrokeredCredentialProvider::Builtin(provider)
                        if provider.reset_on_configuration_change
                )
            });
        }
        if state.configured_provider_configs != config.credential_providers {
            let mut configured_providers: Vec<Arc<configured::ConfiguredCredentialProvider>> =
                Vec::new();
            let mut provider_configs = config.credential_providers.iter().collect::<Vec<_>>();
            provider_configs.sort_by_key(|(id, provider_config)| {
                std::cmp::Reverse(
                    state.configured_provider_configs.get(id.as_str()) == Some(*provider_config)
                        && state
                            .configured_providers
                            .iter()
                            .any(|provider| provider.id == id.as_str()),
                )
            });
            for (id, provider_config) in provider_configs {
                if configured_providers.iter().any(|existing| {
                    existing.config.env.iter().any(|existing_key| {
                        provider_config
                            .env
                            .iter()
                            .any(|key| env_key_matches(existing_key, key))
                    })
                }) {
                    tracing::warn!(
                        provider = %id,
                        "ignoring credential provider with an overlapping environment source"
                    );
                    continue;
                }

                let existing = (state.configured_provider_configs.get(id) == Some(provider_config))
                    .then(|| {
                        state
                            .configured_providers
                            .iter()
                            .find(|provider| provider.id == *id)
                            .cloned()
                    })
                    .flatten();
                match existing {
                    Some(provider) => configured_providers.push(provider),
                    None => {
                        match configured::ConfiguredCredentialProvider::compile(id, provider_config)
                        {
                            Ok(provider) => configured_providers.push(Arc::new(provider)),
                            Err(error) => {
                                tracing::warn!(provider = %id, %error, "ignoring invalid credential provider");
                            }
                        }
                    }
                }
            }
            state
                .credentials
                .retain(|credential| match &credential.provider {
                    BrokeredCredentialProvider::Builtin(_) => true,
                    BrokeredCredentialProvider::Configured(provider) => configured_providers
                        .iter()
                        .any(|current| Arc::ptr_eq(current, provider)),
                });
            state.configured_providers = configured_providers;
            state
                .configured_provider_configs
                .clone_from(&config.credential_providers);
        }
    }

    pub(crate) fn config_revision(&self) -> u64 {
        self.read_state().config_revision
    }

    #[cfg(test)]
    pub(crate) fn discover_parent_credentials(
        &self,
        parent_env: &HashMap<String, String>,
        child_env: &HashMap<String, String>,
    ) {
        self.discover_parent_credentials_for_environment(
            parent_env, child_env, /*environment_id*/ None,
        );
    }

    pub(crate) fn discover_parent_credentials_for_environment(
        &self,
        parent_env: &HashMap<String, String>,
        child_env: &HashMap<String, String>,
        environment_id: Option<&str>,
    ) {
        let mut state = self.write_state();
        if !state.enabled {
            return;
        }
        state.observe_credential_owners(parent_env);

        for source in active_credential_sources(&state, child_env) {
            for env_var in source.env_vars {
                let Some(real_value) =
                    brokerable_credential_value(parent_env, &state, &env_var, &source.provider)
                        .map(str::to_string)
                else {
                    continue;
                };
                if env_value(child_env, &env_var) != Some(real_value.as_str())
                    && child_env.values().any(|value| {
                        value == &real_value
                            || source.provider.contains_embedded_value(value, &real_value)
                    })
                {
                    let _ = state.register(
                        &env_var,
                        source.provider.clone(),
                        source.host_binding.clone(),
                        environment_id,
                        &real_value,
                        parent_env,
                    );
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn virtualize_child_env(&self, env: &mut HashMap<String, String>) {
        self.virtualize_child_env_for_environment(env, /*environment_id*/ None);
    }

    pub(crate) fn virtualize_child_env_for_environment(
        &self,
        env: &mut HashMap<String, String>,
        environment_id: Option<&str>,
    ) {
        self.virtualize_env(env, environment_id, CredentialEnvironment::Child);
    }

    pub(crate) fn virtualize_snapshot_env(
        &self,
        env: &mut HashMap<String, String>,
        environment_id: Option<&str>,
    ) {
        self.virtualize_env(env, environment_id, CredentialEnvironment::Snapshot);
    }

    fn virtualize_env(
        &self,
        env: &mut HashMap<String, String>,
        environment_id: Option<&str>,
        destination: CredentialEnvironment,
    ) {
        let mut state = self.write_state();
        if !state.enabled {
            remove_env_value(env, CREDENTIAL_BROKER_ACTIVE_ENV_KEY);
            remove_env_value(env, BROKERED_CREDENTIALS_ENV_KEY);
            return;
        }
        set_env_value(env, CREDENTIAL_BROKER_ACTIVE_ENV_KEY, "1".to_string());
        state.observe_credential_owners(env);

        let active_sources = active_credential_sources(&state, env);
        for credential in state.credentials.iter_mut().filter(|credential| {
            credential.belongs_to_environment(environment_id)
                && credential.should_reconcile_host_binding(env)
        }) {
            if let Some(source) = active_sources
                .iter()
                .find(|source| source_tracks_credential(source, credential, env))
            {
                credential.observe_host_binding(source.host_binding.clone(), env);
            }
        }
        let stale_credentials = state
            .credentials
            .iter()
            .filter(|credential| {
                credential.belongs_to_environment(environment_id)
                    && credential.should_reconcile_host_binding(env)
                    && !active_sources
                        .iter()
                        .any(|source| source_tracks_credential(source, credential, env))
            })
            .map(|credential| {
                (
                    credential.dummy_value.clone(),
                    credential.real_value.clone(),
                )
            })
            .collect::<Vec<_>>();
        for (dummy_value, real_value) in &stale_credentials {
            for value in env.values_mut() {
                if value.contains(dummy_value) {
                    *value = value.replace(dummy_value, real_value);
                }
            }
        }
        state.credentials.retain(|credential| {
            !credential.belongs_to_environment(environment_id)
                || !stale_credentials
                    .iter()
                    .any(|(dummy_value, _)| credential.dummy_value == *dummy_value)
        });

        let unbound_inherited_credentials = state
            .credentials
            .iter()
            .filter(|credential| {
                !credential.belongs_to_environment(environment_id)
                    && env_contains_credential_value(env, &credential.dummy_value)
                    && !active_sources
                        .iter()
                        .any(|source| source_accepts_credential(source, credential))
            })
            .map(|credential| {
                (
                    credential.dummy_value.clone(),
                    credential.real_value.clone(),
                )
            })
            .collect::<Vec<_>>();
        for (dummy_value, real_value) in unbound_inherited_credentials {
            for value in env.values_mut() {
                if value.contains(&dummy_value) {
                    *value = value.replace(&dummy_value, &real_value);
                }
            }
        }

        let inherited_credentials = active_sources
            .iter()
            .flat_map(|source| {
                state
                    .credentials
                    .iter()
                    .filter(|&credential| {
                        !credential.belongs_to_environment(environment_id)
                            && source_accepts_credential(source, credential)
                            && env_contains_credential_value(env, &credential.dummy_value)
                    })
                    .map(|credential| {
                        (
                            credential.env_var.clone(),
                            source.provider.clone(),
                            source.host_binding.clone(),
                            credential.real_value.clone(),
                            credential.dummy_value.clone(),
                        )
                    })
            })
            .collect::<Vec<_>>();
        for (env_var, provider, host_binding, real_value, inherited_dummy) in inherited_credentials
        {
            let current_dummy = if let Some(credential) =
                state.credentials.iter_mut().find(|credential| {
                    credential.belongs_to_environment(environment_id)
                        && credential.provider.same_provider(&provider)
                        && env_key_matches(&credential.env_var, &env_var)
                        && credential.real_value == real_value
                }) {
                credential.observe_host_binding(host_binding, env);
                credential.dummy_value.clone()
            } else {
                state.credentials.push(CredentialRecord {
                    env_var,
                    provider,
                    host_binding,
                    additional_host_bindings: Vec::new(),
                    environment_id: environment_id.map(str::to_string),
                    real_value,
                    dummy_value: inherited_dummy.clone(),
                });
                inherited_dummy.clone()
            };
            if current_dummy != inherited_dummy {
                for value in env.values_mut() {
                    if value.contains(&inherited_dummy) {
                        *value = value.replace(&inherited_dummy, &current_dummy);
                    }
                }
            }
        }

        for source in &active_sources {
            for env_var in &source.env_vars {
                virtualize_env_var(
                    env,
                    &mut state,
                    env_var,
                    source.provider.clone(),
                    source.host_binding.clone(),
                    environment_id,
                );
            }
        }
        let provider_context_keys = credential_broker_provider_context_env_keys()
            .map(str::to_string)
            .chain(
                state
                    .configured_providers
                    .iter()
                    .filter_map(|provider| provider.config.url_prefix_from_env.as_ref().cloned()),
            )
            .collect::<Vec<_>>();
        let discoverable_values = env
            .iter()
            .filter(|(key, value)| {
                !key.eq_ignore_ascii_case("PATH")
                    && !key.to_ascii_uppercase().ends_with("_PATH")
                    && !crate::is_managed_proxy_env_var(key, value)
                    && !provider_context_keys
                        .iter()
                        .any(|context_key| env_key_matches(key, context_key))
            })
            .map(|(_, value)| value)
            .collect::<Vec<_>>();
        for provider in providers::credential_providers() {
            let brokered_provider = BrokeredCredentialProvider::Builtin(provider);
            let Some((source, host_binding)) = provider.sources().iter().rev().find_map(|source| {
                (source.host_binding)(env, state.openai_api_host.as_deref())
                    .map(|binding| (source, binding))
            }) else {
                continue;
            };
            for value in &discoverable_values {
                for prefix in provider.credential_prefixes {
                    for (start, _) in value.match_indices(prefix) {
                        let credential =
                            matching::builtin_credential_candidate(provider, value, start);
                        if credential.len() < provider.minimum_credential_len
                            || state
                                .configured_providers
                                .iter()
                                .any(|configured| configured.matches_value(credential))
                            || matching::is_operational_path_match(
                                value,
                                start,
                                start + credential.len(),
                            )
                            || provider.ignored_credential_prefixes.iter().any(|ignored| {
                                credential.starts_with(ignored)
                                    && provider
                                        .credential_watermark
                                        .is_none_or(|watermark| !credential.contains(watermark))
                            })
                            || state.is_dummy_value(credential)
                            || state.credentials.iter().any(|existing| {
                                existing.belongs_to_environment(environment_id)
                                    && existing.provider.same_provider(&brokered_provider)
                                    && existing.host_binding == host_binding
                                    && existing.real_value == credential
                            })
                            || source.binding_env_vars.is_empty()
                                && state.credential_owners.iter().any(|existing| {
                                    !source
                                        .env_vars
                                        .iter()
                                        .any(|key| env_key_matches(key, &existing.env_var))
                                        && existing.real_value == credential
                                })
                            || provider.sources().iter().any(|candidate| {
                                candidate
                                    .env_vars
                                    .iter()
                                    .any(|key| env_value(env, key) == Some(credential))
                                    && (candidate.host_binding)(
                                        env,
                                        state.openai_api_host.as_deref(),
                                    )
                                    .is_none()
                            })
                            || provider.request_header_value(credential).is_none()
                        {
                            continue;
                        }
                        let _ = state.register(
                            source.env_vars[0],
                            brokered_provider.clone(),
                            host_binding.clone(),
                            environment_id,
                            credential,
                            env,
                        );
                    }
                }
            }
        }
        let credentials = prioritized_credentials(&state, env)
            .into_iter()
            .filter(|credential| {
                credential.belongs_to_environment(environment_id)
                    && active_sources.iter().any(|source| {
                        credential.provider.same_provider(&source.provider)
                            && credential.host_binding == source.host_binding
                    })
            })
            .collect::<Vec<_>>();
        let mut credential_aliases = Vec::new();
        for (key, value) in env.iter_mut() {
            if crate::is_managed_proxy_env_var(key, value) {
                continue;
            }
            let mut virtualized = false;
            for credential in &credentials {
                if value == &credential.real_value {
                    value.clone_from(&credential.dummy_value);
                    virtualized = true;
                } else if credential
                    .provider
                    .contains_embedded_value(value, &credential.real_value)
                {
                    *value = credential.provider.replace_embedded_value(
                        value,
                        &credential.real_value,
                        &credential.dummy_value,
                    );
                    virtualized = true;
                } else if value.contains(&credential.dummy_value) {
                    virtualized = true;
                }
            }
            if virtualized {
                credential_aliases.push(CredentialAlias {
                    env_var: key.clone(),
                    dummy_value: value.clone(),
                });
            }
        }
        for alias in credential_aliases {
            if !state.credential_aliases.iter().any(|existing| {
                env_key_matches(&existing.env_var, &alias.env_var)
                    && existing.dummy_value == alias.dummy_value
            }) {
                state.credential_aliases.push(alias);
            }
        }
        if state.allow_local_binding && matches!(destination, CredentialEnvironment::Child) {
            // Clients bypass the proxy for local destinations, so their credentials must stay real.
            state.restore_child_env(env, |credential| {
                credential.belongs_to_environment(environment_id)
                    && credential
                        .host_bindings()
                        .any(providers::CredentialHostBinding::bypasses_proxy_with_local_binding)
            });
        }
        update_brokered_credentials_marker(&state, env);
    }

    pub(crate) fn restore_child_env(
        &self,
        env: &mut HashMap<String, String>,
        _command: &mut [String],
    ) {
        let state = self.read_state();
        if !state.enabled || env_value(env, CREDENTIAL_BROKER_ACTIVE_ENV_KEY) != Some("1") {
            return;
        }
        state.restore_child_env(env, |_| true);
    }

    pub(crate) fn restore_and_disable_child_env(
        &self,
        env: &mut HashMap<String, String>,
        command: &mut [String],
    ) {
        self.restore_child_env(env, command);
        remove_env_value(env, CREDENTIAL_BROKER_ACTIVE_ENV_KEY);
        remove_env_value(env, BROKERED_CREDENTIALS_ENV_KEY);
    }

    pub(crate) fn child_alias_matches(
        &self,
        key: &str,
        value: &str,
        snapshot_value: &str,
        environment_id: Option<&str>,
    ) -> bool {
        let state = self.read_state();
        if !state.enabled {
            return false;
        }
        let mut expected = HashMap::from([(key.to_string(), snapshot_value.to_string())]);
        state.restore_child_env(&mut expected, |_| true);
        let referenced_credentials = |text: &str| {
            state
                .credentials
                .iter()
                .filter(|credential| {
                    text == credential.dummy_value
                        || credential
                            .provider
                            .contains_embedded_value(text, &credential.dummy_value)
                })
                .collect::<Vec<_>>()
        };
        let expected_credentials = referenced_credentials(snapshot_value);
        if expected_credentials.is_empty() {
            return false;
        }
        state.credential_aliases.iter().any(|alias| {
            if !env_key_matches(key, &alias.env_var) {
                return false;
            }
            let alias_credentials = referenced_credentials(&alias.dummy_value);
            let same_owner = |left: &&CredentialRecord, right: &&CredentialRecord| {
                left.provider.same_provider(&right.provider)
                    && env_key_matches(&left.env_var, &right.env_var)
                    && left.real_value == right.real_value
            };
            if !expected_credentials.iter().all(|expected| {
                alias_credentials
                    .iter()
                    .any(|actual| same_owner(expected, actual))
            }) || !alias_credentials.iter().all(|actual| {
                expected_credentials
                    .iter()
                    .any(|expected| same_owner(expected, actual))
            }) {
                return false;
            }
            let mut candidate = HashMap::from([(key.to_string(), alias.dummy_value.clone())]);
            let mut identity = candidate.clone();
            if state.allow_local_binding {
                state.restore_child_env(&mut candidate, |credential| {
                    credential.belongs_to_environment(environment_id)
                        && credential.host_bindings().any(
                            providers::CredentialHostBinding::bypasses_proxy_with_local_binding,
                        )
                });
            }
            if env_value(&candidate, key) != Some(value) {
                return false;
            }
            state.restore_child_env(&mut identity, |_| true);
            identity == expected
        })
    }

    #[cfg(test)]
    pub(crate) fn host_requires_mitm(&self, host: &str, port: u16) -> bool {
        self.host_requires_mitm_for_environment(host, port, /*environment_id*/ None)
    }

    pub(crate) fn host_requires_mitm_for_environment(
        &self,
        host: &str,
        port: u16,
        environment_id: Option<&str>,
    ) -> bool {
        let normalized_host = normalize_host(host);
        let state = self.read_state();
        state.enabled
            && state.credentials.iter().any(|credential| {
                credential.belongs_to_environment(environment_id)
                    && credential
                        .host_bindings()
                        .any(|binding| binding.requires_mitm(&normalized_host, port))
            })
    }

    pub(crate) fn virtualize_text(&self, text: &mut String, env: &HashMap<String, String>) -> bool {
        let state = self.read_state();
        matching::virtualize_text(&state, text, env)
    }

    pub(crate) fn restore_text(&self, text: &mut String) -> bool {
        let state = self.read_state();
        if !state.enabled {
            return false;
        }

        let mut credentials = state.credentials.iter().collect::<Vec<_>>();
        credentials
            .sort_unstable_by_key(|credential| std::cmp::Reverse(credential.dummy_value.len()));
        let mut restored = false;
        for credential in credentials {
            if text.as_str() == credential.dummy_value {
                text.clone_from(&credential.real_value);
                restored = true;
            } else if credential
                .provider
                .contains_embedded_value(text, &credential.dummy_value)
            {
                *text = credential.provider.replace_embedded_value(
                    text,
                    &credential.dummy_value,
                    &credential.real_value,
                );
                restored = true;
            }
        }
        restored
    }

    #[cfg(test)]
    pub(crate) fn inject_request_headers(&self, destination: &str, headers: &mut HeaderMap) {
        self.inject_request_headers_for_environment(
            destination,
            headers,
            /*environment_id*/ None,
        );
    }

    pub(crate) fn inject_request_headers_for_environment(
        &self,
        destination: &str,
        headers: &mut HeaderMap,
        environment_id: Option<&str>,
    ) {
        let request = if destination.contains("://") {
            let Ok(request) = Url::parse(destination) else {
                return;
            };
            Some(request)
        } else {
            None
        };
        let normalized_host = normalize_host(
            request
                .as_ref()
                .and_then(Url::host_str)
                .unwrap_or(destination),
        );
        let state = self.read_state();
        if !state.enabled {
            return;
        }

        let credentials = select_credentials(
            headers,
            &normalized_host,
            request.as_ref(),
            &state.credentials,
            environment_id,
        );
        if credentials.iter().any(|(credential, _, _)| {
            matches!(
                &credential.provider,
                BrokeredCredentialProvider::Configured(_)
            )
        }) && let Some(request) = request.as_ref()
        {
            let Ok(raw_request) = destination.parse::<rama_http::Uri>() else {
                return;
            };
            let raw_path = raw_request.path();
            if raw_path != request.path()
                || !crate::authorization_path::is_safe_for_authorization(raw_path)
            {
                return;
            }
        }
        for (credential, header_name, header_value) in credentials {
            credential
                .provider
                .insert_request_header(headers, header_name, header_value);
        }
    }

    fn read_state(&self) -> std::sync::RwLockReadGuard<'_, CredentialBrokerState> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write_state(&self) -> std::sync::RwLockWriteGuard<'_, CredentialBrokerState> {
        self.state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn virtualize_env_var(
    env: &mut HashMap<String, String>,
    state: &mut CredentialBrokerState,
    env_var: &str,
    provider: BrokeredCredentialProvider,
    host_binding: providers::CredentialHostBinding,
    environment_id: Option<&str>,
) {
    let previous_dummy = env_value(env, env_var)
        .filter(|value| state.is_dummy_value(value))
        .map(str::to_string);
    let Some(real_value) = brokerable_credential_value(env, state, env_var, &provider)
        .map(str::to_string)
        .or_else(|| {
            let dummy = previous_dummy.as_deref()?;
            state
                .credentials
                .iter()
                .find(|credential| {
                    credential.dummy_value == dummy
                        && credential.provider.same_provider(&provider)
                        && env_key_matches(&credential.env_var, env_var)
                })
                .map(|credential| credential.real_value.clone())
        })
    else {
        return;
    };

    if let Some(dummy_value) = state.register(
        env_var,
        provider,
        host_binding,
        environment_id,
        &real_value,
        env,
    ) {
        if let Some(previous_dummy) = previous_dummy
            && previous_dummy != dummy_value
        {
            for value in env.values_mut() {
                if value.contains(&previous_dummy) {
                    *value = value.replace(&previous_dummy, &dummy_value);
                }
            }
        }
        set_env_value(env, env_var, dummy_value);
    }
}

fn brokerable_credential_value<'a>(
    env: &'a HashMap<String, String>,
    state: &CredentialBrokerState,
    env_var: &str,
    provider: &BrokeredCredentialProvider,
) -> Option<&'a str> {
    let real_value = env_value(env, env_var)?;
    let real_value = match provider {
        BrokeredCredentialProvider::Builtin(_) => real_value.trim(),
        BrokeredCredentialProvider::Configured(_) => real_value,
    };
    (!real_value.is_empty()
        && !state.is_dummy_value(real_value)
        && provider.request_header_value(real_value).is_some())
    .then_some(real_value)
}

impl CredentialBrokerState {
    fn restore_child_env(
        &self,
        env: &mut HashMap<String, String>,
        should_restore: impl Fn(&CredentialRecord) -> bool,
    ) {
        let credentials = self
            .credentials
            .iter()
            .filter(|credential| should_restore(credential))
            .filter(|credential| {
                env.iter().any(|(key, value)| {
                    !env_key_matches(key, CREDENTIAL_BROKER_ACTIVE_ENV_KEY)
                        && !env_key_matches(key, BROKERED_CREDENTIALS_ENV_KEY)
                        && (value == &credential.dummy_value
                            || credential
                                .provider
                                .contains_embedded_value(value, &credential.dummy_value))
                })
            })
            .collect::<Vec<_>>();
        for (key, value) in env.iter_mut() {
            if env_key_matches(key, CREDENTIAL_BROKER_ACTIVE_ENV_KEY)
                || env_key_matches(key, BROKERED_CREDENTIALS_ENV_KEY)
            {
                continue;
            }
            let canonical_credential = self
                .credentials
                .iter()
                .any(|credential| env_key_matches(key, &credential.env_var));
            if !canonical_credential
                && !self.credential_aliases.iter().any(|alias| {
                    env_key_matches(key, &alias.env_var) && value == &alias.dummy_value
                })
            {
                continue;
            }
            for credential in &credentials {
                if canonical_credential
                    && !self.credentials.iter().any(|candidate| {
                        env_key_matches(key, &candidate.env_var)
                            && candidate.provider.same_provider(&credential.provider)
                            && candidate.host_binding == credential.host_binding
                            && candidate.real_value == credential.real_value
                    })
                {
                    continue;
                }
                if value == &credential.dummy_value {
                    value.clone_from(&credential.real_value);
                } else if credential
                    .provider
                    .contains_embedded_value(value, &credential.dummy_value)
                {
                    *value = credential.provider.replace_embedded_value(
                        value,
                        &credential.dummy_value,
                        &credential.real_value,
                    );
                }
            }
        }
    }

    fn observe_credential_owners(&mut self, env: &HashMap<String, String>) {
        // Ownership is known even when a destination is missing or invalid.
        let sources = providers::credential_providers()
            .flat_map(|provider| {
                provider.sources().iter().flat_map(move |source| {
                    source
                        .env_vars
                        .iter()
                        .map(move |key| (*key, BrokeredCredentialProvider::Builtin(provider)))
                })
            })
            .chain(self.configured_providers.iter().flat_map(|provider| {
                provider.config.env.iter().map(move |key| {
                    (
                        key.as_str(),
                        BrokeredCredentialProvider::Configured(Arc::clone(provider)),
                    )
                })
            }));
        let owners = sources
            .filter_map(|(key, provider)| {
                brokerable_credential_value(env, self, key, &provider)
                    .map(|value| (key.to_string(), value.to_string()))
            })
            .collect::<Vec<_>>();
        for (key, value) in owners {
            self.remember_credential_owner(&key, &value);
        }
    }

    fn remember_credential_owner(&mut self, env_var: &str, real_value: &str) {
        if !self
            .credential_owners
            .iter()
            .any(|owner| env_key_matches(&owner.env_var, env_var) && owner.real_value == real_value)
        {
            self.credential_owners.push(CredentialOwner {
                env_var: env_var.to_string(),
                real_value: real_value.to_string(),
            });
        }
    }

    fn register(
        &mut self,
        env_var: &str,
        provider: BrokeredCredentialProvider,
        host_binding: providers::CredentialHostBinding,
        environment_id: Option<&str>,
        real_value: &str,
        existing_env: &HashMap<String, String>,
    ) -> Option<String> {
        self.remember_credential_owner(env_var, real_value);
        if let Some(existing) = self.credentials.iter_mut().find(|credential| {
            env_key_matches(&credential.env_var, env_var)
                && credential.provider.same_provider(&provider)
                && credential.belongs_to_environment(environment_id)
                && credential.real_value == real_value
        }) {
            existing.observe_host_binding(host_binding, existing_env);
            return Some(existing.dummy_value.clone());
        }
        let Some(dummy_value) = (0..64).find_map(|_| {
            let candidate = provider.dummy_value(real_value)?;
            (candidate != real_value
                && !existing_env
                    .values()
                    .any(|value| value.contains(&candidate))
                && !self.credentials.iter().any(|credential| {
                    credential.dummy_value == candidate || credential.real_value == candidate
                }))
            .then_some(candidate)
        }) else {
            tracing::warn!(
                env_var,
                "credential brokerage skipped: unable to generate a unique dummy credential"
            );
            return None;
        };
        self.credentials.push(CredentialRecord {
            env_var: env_var.to_string(),
            provider,
            host_binding,
            additional_host_bindings: Vec::new(),
            environment_id: environment_id.map(str::to_string),
            real_value: real_value.to_string(),
            dummy_value: dummy_value.clone(),
        });
        Some(dummy_value)
    }

    fn is_dummy_value(&self, value: &str) -> bool {
        self.credentials
            .iter()
            .any(|credential| credential.dummy_value == value)
    }
}

#[cfg(test)]
#[path = "credential_broker_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "credential_broker/configured_tests.rs"]
mod configured_tests;
