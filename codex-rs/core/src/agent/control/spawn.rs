use super::residency::V2ResidencySlot;
use super::residency::is_v2_resident_session_source;
use super::*;
use crate::agent::child_config::build_agent_resume_config;
use crate::agent::role::apply_role_to_config;
use crate::codex_thread::CodexThread;
use crate::config::PermissionProfileSnapshot;
use crate::environment_selection::TurnEnvironmentSnapshot;
use codex_browser_computer_use::configured_browser_dynamic_tools_for_codex_home;
use codex_extension_api::ExtensionDataInit;
use codex_protocol::config_types::MultiAgentMode;
use codex_protocol::models::ResponseItem;
use codex_thread_store::ThreadStoreError;
use tokio::time::Duration;

/// A failed shutdown must not turn a live unpublished child into an untracked runtime. Retry a
/// bounded number of times, then retain its reservation while the normal session-loop shutdown
/// path finishes instead of issuing an unbounded background stream of shutdown operations.
const UNPUBLISHED_SPAWN_CLEANUP_MAX_SHUTDOWN_ATTEMPTS: usize = 3;
const UNPUBLISHED_SPAWN_CLEANUP_RETRY_DELAY: Duration = Duration::from_millis(100);
use crate::context::ContextualUserFragment;
use crate::context::CurrentTimeReminder;
use crate::context::DeveloperInstructions;
use crate::context::GuardianContextMode;
use crate::context::ManagedDeveloperInstructions;
use crate::context::MultiAgentModeInstructions;
use crate::context::MultiAgentRoleInstructions;
use crate::context::world_state::PersistentModeState;
use crate::session::multi_agents::resolve_usage_hints;
use codex_context_fragments::set_annotated_content;
use codex_context_fragments::to_annotated_content;
use codex_extension_api::ExtensionDataInit;
use codex_history::ResponseItemEnvelope;
use codex_protocol::intersect_effective_permission_profiles;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_utils_path_uri::PathUri;

const AGENT_NAMES: &str = include_str!("../../../assets/agent/agent_names.txt");

/// Browser tools are session-scoped. Re-derive them from each child config at
/// every AgentControl lifecycle boundary instead of relying on the ambient
/// process home or persisted rollout metadata.
fn configured_browser_dynamic_tools(
    config: &Config,
) -> Vec<codex_protocol::dynamic_tools::DynamicToolSpec> {
    configured_browser_dynamic_tools_for_codex_home(config.codex_home.as_path())
}

struct SpawnAgentThreadInheritance {
    environments: Option<TurnEnvironmentSnapshot>,
    exec_policy: Option<Arc<crate::exec_policy::ExecPolicyManager>>,
}

/// Initial input delivered after a spawned agent acquires execution capacity.
///
/// V2 communication spawns keep the communication and its context paired so centralized
/// submission and lifecycle logging cannot receive one without the other. Other spawn sources
/// provide user input directly, making an uncontextualized inter-agent communication
/// unrepresentable.
#[allow(clippy::large_enum_variant)]
enum SpawnInitialInput {
    UserInput(Vec<UserInput>),
    InterAgentCommunication(InterAgentCommunication, AgentCommunicationContext),
}

fn spawn_publication_key(options: &SpawnAgentOptions) -> Option<SpawnPublicationKey> {
    Some(SpawnPublicationKey::new(
        options.parent_thread_id?,
        options.spawn_call_id.as_deref()?,
    ))
}

fn spawn_cancellation_owns_child(
    control: &AgentControl,
    publication_key: Option<&SpawnPublicationKey>,
) -> bool {
    publication_key.is_some_and(|key| {
        control.state.spawn_publication_decision(key) == SpawnPublicationDecision::CancellationOwned
    })
}

fn is_benign_unpublished_spawn_cleanup_error(error: &CodexErr) -> bool {
    matches!(
        error.details(),
        CodexErrorDetails::ThreadNotFound(_) | CodexErrorDetails::InternalAgentDied
    )
}

fn is_benign_unpublished_spawn_persistence_error(error: &std::io::Error) -> bool {
    matches!(
        error
            .get_ref()
            .and_then(|cause| cause.downcast_ref::<ThreadStoreError>()),
        Some(ThreadStoreError::ThreadNotFound { .. })
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum UnpublishedSpawnReconciliation {
    ShutdownThenVerifyTerminal,
    PreserveNaturalTerminal,
    ShutdownPreexistingInterrupted,
    ChildDied,
}

pub(super) fn unpublished_spawn_reconciliation(
    status: &AgentStatus,
) -> UnpublishedSpawnReconciliation {
    match status {
        AgentStatus::PendingInit | AgentStatus::Running => {
            UnpublishedSpawnReconciliation::ShutdownThenVerifyTerminal
        }
        AgentStatus::Interrupted => UnpublishedSpawnReconciliation::ShutdownPreexistingInterrupted,
        AgentStatus::Completed(_) | AgentStatus::Errored(_) | AgentStatus::Shutdown => {
            UnpublishedSpawnReconciliation::PreserveNaturalTerminal
        }
        AgentStatus::NotFound => UnpublishedSpawnReconciliation::ChildDied,
    }
}

enum UnpublishedSpawnCleanupAttempt {
    /// The manager still owns no live instance for this child ID. It is safe for the provisional
    /// reservation to release once this outcome has been observed.
    Terminal { cleanup_error: Option<CodexErr> },
    /// The exact provisional instance is still live. Its reservation and any pending V2
    /// residency slot must remain held by an explicit cleanup owner.
    StillLive { cleanup_error: CodexErr },
    /// The provisional instance reached terminal state, but a different manager instance now
    /// occupies its ID. The owner must never remove that replacement or release capacity/path
    /// until the replacement's manager lifetime has also ended.
    Replaced { cleanup_error: Option<CodexErr> },
}

/// Owns pre-publication capacity while a cleanup path remains in progress.
///
/// `SpawnReservation` continues to reserve both capacity and any requested agent path. A pending
/// `V2ResidencySlot` deliberately remains pending, making the live child non-evictable while it
/// has not crossed the parent-visible publication boundary. The manager continues to own the
/// actual `CodexThread`; this task is only the explicit, auditable cleanup owner.
struct RetainedUnpublishedSpawnCleanup {
    _reservation: crate::agent::registry::SpawnReservation,
    _residency_slot: Option<V2ResidencySlot>,
}

/// Restore the agent's latest durable model selection before reopening an evicted V2 runtime.
/// Role configuration is loaded first so role-local providers are available, while the caller's
/// config remains the source of current runtime policy such as permissions and cwd.
fn restore_persisted_agent_model_selection(
    config: &mut Config,
    model: Option<&str>,
    provider_id: &str,
    reasoning_effort: Option<ReasoningEffort>,
    thread_id: ThreadId,
) -> CodexResult<()> {
    let Some(model) = model else {
        return Err(CodexErr::UnsupportedOperation(format!(
            "cannot safely reload agent {thread_id}: persisted model identity is unavailable"
        )));
    };

    let provider = if config.model_provider_id == provider_id {
        // Preserve runtime-only provider overrides such as test endpoints, headers, and
        // transport selection. The provider catalog can contain the built-in descriptor for
        // this same ID, which is not necessarily the effective provider for this session.
        config.model_provider.clone()
    } else {
        config.model_providers.get(provider_id).cloned().ok_or_else(|| {
            CodexErr::UnsupportedOperation(format!(
                "cannot safely reload agent {thread_id}: persisted model provider `{provider_id}` is not configured"
            ))
        })?
    };

    config.model = Some(model.to_string());
    config.model_provider_id = provider_id.to_string();
    config.model_provider = provider;
    config.model_reasoning_effort = reasoning_effort;
    Ok(())
}

async fn terminal_idle_unload_timeout_for_resumed_role(
    config: &Config,
    role_name: Option<&str>,
    thread_id: ThreadId,
) -> u64 {
    let fallback_timeout_ms = config.multi_agent_v2.terminal_idle_unload_timeout_ms;
    let Some(role_name) = role_name else {
        return fallback_timeout_ms;
    };

    let mut role_config = config.clone();
    if let Err(err) = apply_role_to_config(&mut role_config, Some(role_name)).await {
        warn!(
            "failed to restore terminal idle unload timeout for resumed agent {thread_id} with role `{role_name}`: {err}; using caller timeout"
        );
        return fallback_timeout_ms;
    }

    role_config.multi_agent_v2.terminal_idle_unload_timeout_ms
}

fn default_agent_nickname_list() -> Vec<&'static str> {
    AGENT_NAMES
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect()
}

pub(super) fn agent_nickname_candidates(config: &Config, role_name: Option<&str>) -> Vec<String> {
    let role_name = role_name.unwrap_or(DEFAULT_ROLE_NAME);
    if let Some(candidates) =
        resolve_role_config(config, role_name).and_then(|role| role.nickname_candidates.clone())
    {
        return candidates;
    }

    default_agent_nickname_list()
        .into_iter()
        .map(ToOwned::to_owned)
        .collect()
}

fn keep_forked_rollout_item(item: &RolloutItem, preserve_reference_context_item: bool) -> bool {
    match item {
        RolloutItem::ResponseItem(envelope) => match &envelope.item {
            ResponseItem::Message { role, phase, .. } => match role.as_str() {
                "system" | "developer" | "user" => true,
                "assistant" => *phase == Some(MessagePhase::FinalAnswer),
                _ => false,
            },
            ResponseItem::FunctionCallOutput { call_id: None, .. }
            | ResponseItem::ConfigurationUpdate { .. } => true,
            ResponseItem::AdditionalTools { .. }
            | ResponseItem::AgentMessage { .. }
            | ResponseItem::Reasoning { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::FunctionCallOutput {
                call_id: Some(_), ..
            }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::CustomToolCallOutput { .. }
            | ResponseItem::ToolSearchOutput { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::CompactionTrigger { .. }
            | ResponseItem::ContextCompaction { .. }
            | ResponseItem::Other => false,
        },
        RolloutItem::RealtimeItem(_)
        | RolloutItem::InterAgentCommunication(_)
        | RolloutItem::InterAgentCommunicationMetadata { .. }
        | RolloutItem::RetainedContext(_)
        | RolloutItem::SecurityRiskScore(_) => false,
        // Full-history forks preserve the cached prompt prefix and can keep diffing
        // from the parent's durable baseline. Truncated forks drop part of that prompt,
        // so they must rebuild context on their first child turn.
        RolloutItem::TurnContext(_) | RolloutItem::WorldState(_) => preserve_reference_context_item,
        // Child threads inherit model context, not the parent's cumulative usage state.
        RolloutItem::TokenUsageRecord(_) => false,
        RolloutItem::Compacted(_) | RolloutItem::EventMsg(_) | RolloutItem::SessionMeta(_) => true,
    }
}

fn retain_forked_developer_message(
    item: &mut ResponseItem,
    usage_hint_texts: &[String],
    context_mode: GuardianContextMode,
) -> bool {
    if !matches!(item, ResponseItem::Message { role, .. } if role == "developer") {
        return true;
    }

    let Some(mut content) = to_annotated_content(item) else {
        return false;
    };
    content.retain(|content_item| {
        if context_mode == GuardianContextMode::ThreadOwned
            && content_item.kind().0 == "guardian.approved_action"
        {
            return false;
        }
        let ContentItem::InputText { text } = content_item.content() else {
            return true;
        };

        !(MultiAgentRoleInstructions::matches_text(text)
            || (context_mode == GuardianContextMode::ThreadOwned
                && text.starts_with(
                    crate::guardian::AUTO_REVIEW_DENIED_ACTION_APPROVAL_DEVELOPER_PREFIX,
                ))
            || MultiAgentModeInstructions::matches_text(text)
            || CurrentTimeReminder::matches_text(text)
            || usage_hint_texts
                .iter()
                .any(|usage_hint_text| usage_hint_text == text))
    });
    !content.is_empty() && set_annotated_content(item, content).is_some()
}

async fn load_agent_model_context(
    state: &ThreadManagerState,
    thread_id: ThreadId,
    history_mode: ThreadHistoryMode,
) -> CodexResult<Option<Vec<RolloutItem>>> {
    match history_mode {
        ThreadHistoryMode::Legacy => Ok(state
            .read_stored_thread(ReadThreadParams {
                thread_id,
                include_archived: true,
                include_history: true,
            })
            .await?
            .history
            .map(|history| history.items)),
        ThreadHistoryMode::Paginated => Ok(Some(
            state
                .load_latest_model_context(LoadThreadHistoryParams {
                    thread_id,
                    include_archived: true,
                })
                .await?
                .items,
        )),
    }
}

impl AgentControl {
    /// Restore persisted V2 agent identities without reopening their runtimes.
    pub(crate) async fn restore_v2_agent_metadata(
        &self,
        config: &Config,
        root_thread_id: ThreadId,
    ) {
        self.state.register_root_thread(root_thread_id);

        let Ok(state) = self.upgrade() else {
            return;
        };
        let Some(agent_graph_store) = state.agent_graph_store() else {
            return;
        };
        let descendants = match agent_graph_store
            .list_thread_spawn_descendants_bounded(
                root_thread_id,
                Some(codex_agent_graph_store::ThreadSpawnEdgeStatus::Open),
            )
            .await
        {
            Ok(descendant_ids) => descendant_ids,
            Err(err) => {
                warn!("failed to restore persisted V2 agent metadata for {root_thread_id}: {err}");
                return;
            }
        };

        if descendants.relation_limit_reached {
            warn!(
                "persisted V2 agent metadata restoration reached the descendant safety limit for {root_thread_id}; retaining the incomplete result instead of registering a partial agent tree"
            );
            return;
        }

        for thread_id in descendants.thread_ids {
            if self.state.agent_metadata_for_thread(thread_id).is_some() {
                continue;
            }
            let restore_result = async {
                let stored_thread = state
                    .read_stored_thread(ReadThreadParams {
                        thread_id,
                        include_archived: true,
                        include_history: false,
                    })
                    .await?;
                let stored_agent_path = stored_thread
                    .agent_path
                    .as_deref()
                    .map(AgentPath::try_from)
                    .transpose()
                    .map_err(|err| {
                        CodexErr::InvalidRequest(format!("invalid stored agent path: {err}"))
                    })?;
                let mut reservation = self.state.reserve_spawn_slot(/*max_threads*/ None)?;
                let mut metadata = self.prepare_agent_metadata(
                    &mut reservation,
                    config,
                    stored_agent_path.or_else(|| stored_thread.source.get_agent_path()),
                    stored_thread
                        .agent_role
                        .or_else(|| stored_thread.source.get_agent_role()),
                    stored_thread
                        .agent_nickname
                        .or_else(|| stored_thread.source.get_nickname()),
                )?;
                metadata.agent_id = Some(thread_id);
                reservation.commit(metadata);
                Ok::<(), CodexErr>(())
            }
            .await;
            if let Err(err) = restore_result {
                warn!("failed to restore V2 agent metadata for {thread_id}: {err}");
            }
        }
    }

    /// Spawn a new agent thread and submit the initial prompt.
    #[cfg(test)]
    pub(crate) async fn spawn_agent(
        &self,
        config: Config,
        initial_input: Vec<UserInput>,
        session_source: Option<SessionSource>,
    ) -> CodexResult<ThreadId> {
        let spawned_agent = Box::pin(self.spawn_agent_internal(
            config,
            SpawnInitialInput::UserInput(initial_input),
            session_source,
            SpawnAgentOptions::default(),
        ))
        .await?;
        Ok(spawned_agent.thread_id)
    }

    /// Spawn an agent thread with some metadata.
    pub(crate) async fn spawn_agent_with_metadata(
        &self,
        config: Config,
        initial_input: Vec<UserInput>,
        session_source: Option<SessionSource>,
        options: SpawnAgentOptions, // TODO(jif) drop with new fork.
    ) -> CodexResult<LiveAgent> {
        Box::pin(self.spawn_agent_internal(
            config,
            SpawnInitialInput::UserInput(initial_input),
            session_source,
            options,
        ))
        .await
    }

    pub(crate) async fn spawn_agent_with_communication(
        &self,
        config: Config,
        communication: InterAgentCommunication,
        context: AgentCommunicationContext,
        session_source: Option<SessionSource>,
        options: SpawnAgentOptions,
    ) -> CodexResult<LiveAgent> {
        Box::pin(self.spawn_agent_internal(
            config,
            SpawnInitialInput::InterAgentCommunication(communication, context),
            session_source,
            options,
        ))
        .await
    }

    fn validate_loaded_v2_child(
        &self,
        thread: &CodexThread,
        parent_thread_id: ThreadId,
    ) -> CodexResult<()> {
        if thread.is_running()
            && thread.multi_agent_version() == Some(MultiAgentVersion::V2)
            && thread.session_source.parent_thread_id() == Some(parent_thread_id)
            && Arc::ptr_eq(&self.state, &thread.session.services.agent_control.state)
        {
            return Ok(());
        }
        Err(CodexErr::InvalidRequest(format!(
            "multi-agent v2 child {} is not owned by its loaded parent",
            thread.session.thread_id
        )))
    }

    /// A provided parent enables owner-validated reloads; `None` preserves sender-driven reloads.
    pub(crate) async fn ensure_v2_agent_loaded(
        &self,
        config: Config,
        thread_id: ThreadId,
        parent: Option<Arc<CodexThread>>,
    ) -> CodexResult<()> {
        let state = self.upgrade()?;
        let parent = if let Some(parent) = parent {
            let parent_thread_id = parent.session.thread_id;
            let turn = parent.session.new_default_turn().await;
            config = build_agent_resume_config(&turn).map_err(|_| {
                CodexErr::InvalidRequest(format!(
                    "cannot resume multi-agent v2 child {thread_id} with the current parent settings"
                ))
            })?;
            let registered_parent = state.get_thread(parent_thread_id).await.ok();
            if !registered_parent
                .as_ref()
                .is_some_and(|registered| Arc::ptr_eq(registered, &parent))
                || !parent.is_running()
                || parent.multi_agent_version() != Some(MultiAgentVersion::V2)
                || !Arc::ptr_eq(&self.state, &parent.session.services.agent_control.state)
            {
                return Err(CodexErr::InvalidRequest(format!(
                    "cannot resume multi-agent v2 child {thread_id}: parent ownership is unavailable; resume the parent first"
                )));
            }
            Some((parent, turn.environments.clone()))
        } else {
            None
        };
        let owner_thread_id = parent.as_ref().map(|(parent, _)| parent.session.thread_id);
        if owner_thread_id.is_none() && state.get_thread(thread_id).await.is_ok() {
            self.touch_loaded_v2_residency(&state, thread_id).await;
            return Ok(());
        }
        if self.state.agent_metadata_for_thread(thread_id).is_none() {
            return Err(CodexErr::ThreadNotFound(thread_id));
        }
        let mut environment_selections = self.state.evicted_environments(thread_id);

        let stored_thread = state
            .read_stored_thread(ReadThreadParams {
                thread_id,
                include_archived: true,
                include_history: false,
            })
            .await?;
        let stored_model = stored_thread.model.clone();
        let stored_model_provider = stored_thread.model_provider.clone();
        let stored_reasoning_effort = stored_thread.reasoning_effort.clone();
        let stored_source = stored_thread.source.clone();
        let stored_parent_thread_id = stored_thread.parent_thread_id;
        let history = load_agent_model_context(state, thread_id, stored_thread.history_mode)
            .await?
            .ok_or(CodexErr::ThreadNotFound(thread_id))?;
        let persisted_approvals_reviewer = history.iter().rev().find_map(|item| match item {
            RolloutItem::TurnContext(turn_context) => turn_context.approvals_reviewer,
            RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(event)) => {
                Some(event.thread_settings.approvals_reviewer)
            }
            _ => None,
        });
        let initial_history = InitialHistory::Resumed(ResumedHistory {
            conversation_id: thread_id,
            history: Arc::new(history),
            rollout_path: stored_thread.rollout_path,
        });
        if initial_history.get_multi_agent_version() != Some(MultiAgentVersion::V2) {
            return Err(CodexErr::ThreadNotFound(thread_id));
        }
        let (session_source, _) = initial_history
            .get_resumed_session_sources()
            .unwrap_or((stored_source, None));
        if let Some(parent_thread_id) = owner_thread_id {
            if session_source.parent_thread_id() != Some(parent_thread_id)
                || initial_history
                    .get_resumed_parent_thread_id()
                    .is_some_and(|recorded_parent| recorded_parent != parent_thread_id)
                || stored_parent_thread_id
                    .is_some_and(|recorded_parent| recorded_parent != parent_thread_id)
            {
                return Err(CodexErr::InvalidRequest(format!(
                    "cannot resume multi-agent v2 child {thread_id}: recorded parent ownership is inconsistent"
                )));
            }
            if let Ok(thread) = state.get_thread(thread_id).await {
                self.validate_loaded_v2_child(&thread, parent_thread_id)?;
                self.touch_loaded_v2_residency(&state, thread_id).await;
                return Ok(());
            }
        }
        config.model_reasoning_effort = stored_reasoning_effort;
        if let Some(role_name) = session_source.get_agent_role() {
            let runtime_approval_policy = config.permissions.approval_policy.value();
            let runtime_approvals_reviewer = config.approvals_reviewer;
            let runtime_cwd = config.cwd.clone();
            let runtime_permission_profile = match config.permissions.active_permission_profile() {
                Some(active_permission_profile) => {
                    PermissionProfileSnapshot::active_with_profile_workspace_roots(
                        config.permissions.permission_profile().clone(),
                        active_permission_profile,
                        config.permissions.profile_workspace_roots().to_vec(),
                    )
                }
                None => PermissionProfileSnapshot::legacy(
                    config.permissions.permission_profile().clone(),
                ),
            };

            apply_role_to_config(&mut config, Some(&role_name))
                .await
                .map_err(CodexErr::InvalidRequest)?;
            config
                .permissions
                .approval_policy
                .set(runtime_approval_policy)
                .map_err(|err| {
                    CodexErr::InvalidRequest(format!("approval_policy is invalid: {err}"))
                })?;
            config.approvals_reviewer = runtime_approvals_reviewer;
            config.cwd = runtime_cwd;
            config
                .permissions
                .set_permission_profile_from_session_snapshot(runtime_permission_profile)
                .map_err(|err| {
                    CodexErr::InvalidRequest(format!("permission_profile is invalid: {err}"))
                })?;
        }
        config.service_tier = self.root_service_tier();
        if let Some(model) = stored_model {
            config.model = Some(model);
        }
        if config.model_provider_id != stored_model_provider {
            config.model_provider = config
                .model_providers
                .get(&stored_model_provider)
                .cloned()
                .ok_or_else(|| {
                    CodexErr::InvalidRequest(format!(
                        "Model provider `{stored_model_provider}` not found"
                    ))
                })?;
            config.model_provider_id = stored_model_provider;
        }
        let parent_thread_id = owner_thread_id
            .or_else(|| initial_history.get_resumed_parent_thread_id())
            .or(stored_parent_thread_id);
        let (inherited_environments, inherited_exec_policy, client_mcp_extensions) = if let Some(
            (parent, parent_environments),
        ) =
            parent.as_ref()
        {
            let parent_config = parent.session.get_config().await;
            if !crate::exec_policy::child_uses_parent_exec_policy(&parent_config, &config) {
                return Err(CodexErr::InvalidRequest(format!(
                    "cannot resume multi-agent v2 child {thread_id}: parent execution policy has changed; retry through the parent"
                )));
            }
            if let Some(selections) = environment_selections.as_mut() {
                for selection in selections {
                    let environment_id = &selection.environment_id;
                    let invalid_environment = |reason: &str| {
                        CodexErr::InvalidRequest(format!(
                            "cannot resume multi-agent v2 child {thread_id}: cached environment {environment_id} {reason}"
                        ))
                    };
                    // Matching the attachment also keeps startup on the captured owner executor.
                    let owner_environment = parent_environments
                        .turn_environments()
                        .find(|environment| {
                            let parent_selection = &environment.selection;
                            parent_selection.environment_id == selection.environment_id
                                && parent_selection.cwd == selection.cwd
                                && parent_selection.workspace_roots == selection.workspace_roots
                        })
                        .ok_or_else(|| {
                            invalid_environment("no longer matches a ready parent environment")
                        })?;
                    let owner_config = owner_environment.config();
                    let child_config = match &selection.config {
                        EnvironmentConfigState::FromThread => {
                            // Pin current owner authority instead of re-inferring child settings.
                            selection.config = EnvironmentConfigState::Ready(owner_config.clone());
                            continue;
                        }
                        EnvironmentConfigState::Ready(config) => config,
                        EnvironmentConfigState::Pending | EnvironmentConfigState::Failed(_) => {
                            return Err(invalid_environment("configuration is not ready"));
                        }
                    };
                    let mut bounded_config = child_config.clone();
                    bounded_config.permission_profile = owner_config.permission_profile.clone();
                    if bounded_config != *owner_config {
                        return Err(invalid_environment(
                            "configuration differs from the current parent",
                        ));
                    }
                    if child_config.permission_profile == owner_config.permission_profile {
                        continue;
                    }
                    if owner_environment.environment.is_remote() {
                        return Err(invalid_environment(
                            "permissions changed on a remote executor",
                        ));
                    }
                    let cwd = selection.cwd.to_abs_path().map_err(|_| {
                        invalid_environment("working directory is not a local absolute path")
                    })?;
                    let roots = owner_environment
                        .workspace_roots()
                        .iter()
                        .map(PathUri::to_abs_path)
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|_| {
                            invalid_environment("workspace roots are not local absolute paths")
                        })?;
                    let authority = owner_environment
                        .permission_profile()
                        .clone()
                        .materialize_project_roots_with_workspace_roots(&roots);
                    let requested = child_config
                        .permission_profile
                        .permission_profile()
                        .clone()
                        .materialize_project_roots_with_workspace_roots(&roots);
                    let permissions =
                        intersect_effective_permission_profiles(&authority, &requested, &cwd)
                            .map_err(|err| {
                                invalid_environment(&format!(
                                    "permissions cannot be intersected safely: {err}"
                                ))
                            })?;
                    bounded_config.permission_profile =
                        PermissionProfileSnapshot::legacy(permissions);
                    selection.config = EnvironmentConfigState::Ready(bounded_config);
                }
            }
            (
                Some(parent_environments.clone()),
                Some(Arc::clone(&parent.session.services.exec_policy)),
                Some(parent.client_mcp_extensions()),
            )
        } else {
            (
                self.inherited_environments_for_source(&state, Some(&session_source))
                    .await,
                self.inherited_exec_policy_for_source(&state, Some(&session_source), &config)
                    .await,
                None,
            )
        };
        let inherited_instructions = if let Some((parent, _)) = parent.as_ref() {
            Some(parent.session.inherited_instructions().await)
        } else if let Some(parent_thread_id) = parent_thread_id
            && let Ok(parent) = state.get_thread(parent_thread_id).await
        {
            Some(parent.session.inherited_instructions().await)
        } else {
            None
        };
        // Reserving a slot can evict an idle nested parent. Capture its instructions
        // alongside its authority so the child does not depend on a later live lookup.
        let residency_slot = self
            .reserve_v2_residency_slot(&state, &config, Some(thread_id))
            .await?;

        match state
            .resume_thread_with_history_with_source(ResumeThreadWithHistoryOptions {
                dynamic_tools: configured_browser_dynamic_tools(&config),
                config,
                initial_history,
                agent_control: self.clone(),
                session_source,
                parent_thread_id,
                environment_selections,
                inherited_environments,
                inherited_instructions,
                inherited_exec_policy,
                client_mcp_extensions,
            })
            .await
        {
            Ok(reloaded_thread) => {
                if let Some(parent_thread_id) = owner_thread_id {
                    self.validate_loaded_v2_child(&reloaded_thread.thread, parent_thread_id)?;
                }
                self.state.clear_evicted_environments(thread_id);
                residency_slot.commit(reloaded_thread.thread_id);
                self.start_terminal_idle_unload_watcher_under_lifecycle(
                    Arc::clone(&reloaded_thread.thread),
                    metadata.clone(),
                    terminal_idle_unload_timeout_ms,
                    lifecycle,
                );
                state.notify_thread_created(reloaded_thread.thread_id);
                self.restore_cold_mail_to_loaded_thread(state, thread_id, lifecycle)
                    .await
            }
            Err(err) => {
                if let Ok(thread) = state.get_thread(thread_id).await {
                    if let Some(parent_thread_id) = owner_thread_id {
                        self.validate_loaded_v2_child(&thread, parent_thread_id)?;
                    }
                    self.state.clear_evicted_environments(thread_id);
                    drop(residency_slot);
                    self.touch_loaded_v2_residency(state, thread_id).await;
                    return self
                        .restore_cold_mail_to_loaded_thread(state, thread_id, lifecycle)
                        .await;
                }
                Err(err)
            }
        }
    }

    async fn spawn_agent_internal(
        &self,
        config: Config,
        initial_input: SpawnInitialInput,
        session_source: Option<SessionSource>,
        options: SpawnAgentOptions,
    ) -> CodexResult<LiveAgent> {
        let publication_key = spawn_publication_key(&options);
        if spawn_cancellation_owns_child(self, publication_key.as_ref()) {
            return Err(CodexErr::TurnAborted);
        }
        let state = self.upgrade()?;
        if spawn_cancellation_owns_child(self, publication_key.as_ref()) {
            return Err(CodexErr::TurnAborted);
        }
        let multi_agent_version = state
            .effective_multi_agent_version_for_spawn(
                &InitialHistory::New,
                session_source.as_ref(),
                options.parent_thread_id,
                /*forked_from_thread_id*/ None,
                &config,
            )
            .await;
        if let Some(session_source) = session_source.as_ref() {
            self.ensure_execution_capacity(multi_agent_version, session_source)?;
        }
        let agent_max_threads = config.effective_agent_max_threads(multi_agent_version);
        let spawn_uses_v2_residency = multi_agent_version == MultiAgentVersion::V2
            && session_source
                .as_ref()
                .is_some_and(is_v2_resident_session_source);
        let terminal_idle_unload_timeout_ms = config.multi_agent_v2.terminal_idle_unload_timeout_ms;
        let mut residency_slot = if spawn_uses_v2_residency {
            Some(
                self.reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
                    .await?,
            )
        } else {
            None
        };
        let reservation_max_threads = if spawn_uses_v2_residency {
            None
        } else {
            agent_max_threads
        };
        let mut reservation = Some(self.state.reserve_spawn_slot(reservation_max_threads)?);
        let inheritance = SpawnAgentThreadInheritance {
            environments: self
                .inherited_environments_for_source(&state, session_source.as_ref())
                .await,
            exec_policy: self
                .inherited_exec_policy_for_source(&state, session_source.as_ref(), &config)
                .await,
        };
        let (session_source, mut agent_metadata) = match session_source {
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth,
                agent_path,
                agent_role,
                ..
            })) => {
                let Some(reservation) = reservation.as_mut() else {
                    return Err(CodexErr::Fatal(
                        "spawn reservation missing before publication".to_string(),
                    ));
                };
                let (session_source, agent_metadata) = self.prepare_thread_spawn(
                    reservation,
                    &config,
                    parent_thread_id,
                    depth,
                    agent_path,
                    agent_role,
                    /*preferred_agent_nickname*/ None,
                )?;
                (Some(session_source), agent_metadata)
            }
            other => (other, AgentMetadata::default()),
        };
        let notification_source = session_source.clone();

        // The same `AgentControl` is sent to spawn the thread.
        let new_thread = match (session_source, options.fork_mode.as_ref(), inheritance) {
            (Some(session_source), Some(_), inheritance) => {
                Box::pin(self.spawn_forked_thread(
                    &state,
                    config,
                    session_source,
                    &options,
                    inheritance,
                    multi_agent_version,
                ))
                .await?
            }
            (Some(session_source), None, inheritance) => {
                let history_mode = if let Some(parent_thread_id) = options.parent_thread_id
                    && let Ok(parent_thread) = state.get_thread(parent_thread_id).await
                {
                    matches!(
                        parent_thread.config_snapshot().await.history_mode,
                        ThreadHistoryMode::Paginated
                    )
                    .then_some(ThreadHistoryMode::Paginated)
                } else {
                    None
                };
                Box::pin(state.spawn_new_thread_with_source_and_dynamic_tools(
                    config.clone(),
                    self.clone(),
                    session_source,
                    history_mode,
                    options.parent_thread_id,
                    /*forked_from_thread_id*/ None,
                    /*thread_source*/ Some(ThreadSource::Subagent),
                    /*metrics_service_name*/ None,
                    configured_browser_dynamic_tools(&config),
                    inheritance.environments,
                    inheritance.exec_policy,
                    options.environments.clone(),
                ))
                .await?
            }
            (None, _, _) => Box::pin(state.spawn_new_thread(config.clone(), self.clone())).await?,
        };
        #[cfg(test)]
        self.await_after_new_thread_test_hook(new_thread.thread_id)
            .await;
        if spawn_cancellation_owns_child(self, publication_key.as_ref()) {
            if let Err(cleanup_error) = self
                .reconcile_unpublished_spawn(
                    &state,
                    new_thread.thread_id,
                    &new_thread.thread,
                    &mut reservation,
                    &mut residency_slot,
                )
                .await
            {
                tracing::error!(
                    child_thread_id = %new_thread.thread_id,
                    %cleanup_error,
                    "failed to reconcile cancellation-owned unpublished child"
                );
            }
            return Err(CodexErr::TurnAborted);
        }
        // Claim initial delivery immediately before the first child submission. Cancellation may
        // still win while the child is only provisioned, but once this claim succeeds it must
        // await our real delivery result; an initial prompt can begin a child turn before the
        // child is parent-visible.
        let delivery_decision = publication_key
            .as_ref()
            .map_or(SpawnPublicationDecision::Untracked, |key| {
                self.state.claim_spawn_publication_delivery(key)
            });
        if delivery_decision == SpawnPublicationDecision::CancellationOwned {
            if let Err(cleanup_error) = self
                .reconcile_unpublished_spawn(
                    &state,
                    new_thread.thread_id,
                    &new_thread.thread,
                    &mut reservation,
                    &mut residency_slot,
                )
                .await
            {
                tracing::error!(
                    child_thread_id = %new_thread.thread_id,
                    %cleanup_error,
                    "failed to reconcile cancellation-owned unpublished child"
                );
            }
            return Err(CodexErr::TurnAborted);
        }
        if !matches!(
            delivery_decision,
            SpawnPublicationDecision::Untracked | SpawnPublicationDecision::DeliveryOwned
        ) {
            let error = CodexErr::Fatal(format!(
                "spawn publication entered invalid initial-delivery state: {delivery_decision:?}"
            ));
            if let Err(cleanup_error) = self
                .reconcile_unpublished_spawn(
                    &state,
                    new_thread.thread_id,
                    &new_thread.thread,
                    &mut reservation,
                    &mut residency_slot,
                )
                .await
            {
                tracing::error!(
                    child_thread_id = %new_thread.thread_id,
                    %cleanup_error,
                    "failed to reconcile unpublished child after invalid delivery state"
                );
            }
            return Err(error);
        }

        let start_options = TurnStartOptions {
            parent_turn_id: options.parent_turn_id.clone(),
            turn_trigger: options.turn_trigger.clone(),
            root_turn_id: options.root_turn_id.clone(),
            cyber_access_program: options.cyber_access_program.clone(),
            ..Default::default()
        };
        let initial_input_result = match initial_input {
            SpawnInitialInput::UserInput(input) => self
                .send_input_after_capacity_check(new_thread.thread_id, &state, input)
                .await
                .map(drop),
            SpawnInitialInput::InterAgentCommunication(communication, context) => {
                if multi_agent_version == MultiAgentVersion::V2 {
                    let mut provisional_metadata = agent_metadata.clone();
                    provisional_metadata.agent_id = Some(new_thread.thread_id);
                    match self
                        .prepare_provisional_v2_agent_delivery(
                            new_thread.thread_id,
                            provisional_metadata,
                        )
                        .await
                    {
                        Ok(delivery) => delivery
                            .send_after_capacity_check(
                                communication,
                                context,
                                /*interrupt*/ false,
                            )
                            .await
                            .map(drop),
                        Err(error) => Err(error),
                    }
                } else {
                    self.send_inter_agent_communication_after_capacity_check(
                        new_thread.thread_id,
                        &state,
                        communication,
                        context,
                        start_options.clone(),
                    )
                    .await
                    .map(drop)
                }
            }
        };
        if let Err(error) = initial_input_result {
            if let Err(cleanup_error) = self
                .reconcile_unpublished_spawn(
                    &state,
                    new_thread.thread_id,
                    &new_thread.thread,
                    &mut reservation,
                    &mut residency_slot,
                )
                .await
            {
                tracing::error!(
                    child_thread_id = %new_thread.thread_id,
                    spawn_error = %error,
                    %cleanup_error,
                    "failed to reconcile unpublished child after initial delivery error"
                );
            }
            return Err(error);
        }

        #[cfg(test)]
        self.await_after_initial_delivery_test_hook(new_thread.thread_id)
            .await;

        // This compare-and-swap is the parent-visible publication boundary. Initial delivery
        // already owns cancellation at this point, so the only tracked terminal transition here
        // is `DeliveryOwned -> Published`; cancellation can still only win before delivery.
        let publication_decision = publication_key
            .as_ref()
            .map_or(SpawnPublicationDecision::Untracked, |key| {
                self.state.publish_spawn_publication(key)
            });
        if publication_decision == SpawnPublicationDecision::CancellationOwned {
            if let Err(cleanup_error) = self
                .reconcile_unpublished_spawn(
                    &state,
                    new_thread.thread_id,
                    &new_thread.thread,
                    &mut reservation,
                    &mut residency_slot,
                )
                .await
            {
                tracing::error!(
                    child_thread_id = %new_thread.thread_id,
                    %cleanup_error,
                    "failed to reconcile cancellation-owned unpublished child"
                );
            }
            return Err(CodexErr::TurnAborted);
        }
        if !matches!(
            publication_decision,
            SpawnPublicationDecision::Untracked | SpawnPublicationDecision::Published
        ) {
            let error = CodexErr::Fatal(format!(
                "spawn publication entered invalid parent-visible state: {publication_decision:?}"
            ));
            if let Err(cleanup_error) = self
                .reconcile_unpublished_spawn(
                    &state,
                    new_thread.thread_id,
                    &new_thread.thread,
                    &mut reservation,
                    &mut residency_slot,
                )
                .await
            {
                tracing::error!(
                    child_thread_id = %new_thread.thread_id,
                    %cleanup_error,
                    "failed to reconcile unpublished child after invalid publication state"
                );
            }
            return Err(error);
        }

        // Capacity and residency reservations remain private until the publication CAS above
        // wins, so `/agents` cannot observe a child while cancellation still owns the outcome.
        agent_metadata.agent_id = Some(new_thread.thread_id);
        let Some(reservation) = reservation.take() else {
            return Err(CodexErr::Fatal(
                "spawn reservation missing at publication".to_string(),
            ));
        };
        reservation.commit(agent_metadata.clone());
        if let Some(residency_slot) = residency_slot.take() {
            residency_slot.commit(new_thread.thread_id);
        }
        if spawn_uses_v2_residency {
            self.start_terminal_idle_unload_watcher(
                Arc::clone(&new_thread.thread),
                agent_metadata.clone(),
                terminal_idle_unload_timeout_ms,
            )
            .await;
        }

        if let Some(SessionSource::SubAgent(
            subagent_source @ SubAgentSource::ThreadSpawn {
                parent_thread_id, ..
            },
        )) = notification_source.as_ref()
        {
            let client_metadata = match state.get_thread(*parent_thread_id).await {
                Ok(parent_thread) => parent_thread.session.app_server_client_metadata().await,
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        parent_thread_id = %parent_thread_id,
                        "skipping subagent thread analytics: failed to load parent thread metadata"
                    );
                    crate::session::session::AppServerClientMetadata {
                        client_name: None,
                        client_version: None,
                    }
                }
            };
            let thread_config = new_thread.thread.config_snapshot().await;
            let parent_thread_id = thread_config.parent_thread_id;
            emit_subagent_session_started(
                &new_thread.thread.session.services.analytics_events_client,
                client_metadata,
                new_thread.thread.session.session_id(),
                new_thread.thread_id,
                parent_thread_id,
                thread_config,
                subagent_source.clone(),
            );
        }

        // The child event receiver is an unbounded, single-consumer buffer. Initial delivery
        // happens before this notification, but every event remains queued until the app-server
        // listener attaches and drains it. Publishing only after the CAS therefore preserves
        // cancellation-owned invisibility without dropping a fast child's early stream events.
        state.notify_thread_created(new_thread.thread_id);

        self.persist_thread_spawn_edge_for_source(
            new_thread.thread.as_ref(),
            new_thread.thread_id,
            notification_source.as_ref(),
        )
        .await;

        if multi_agent_version != MultiAgentVersion::V2 {
            let child_reference = agent_metadata
                .agent_path
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| new_thread.thread_id.to_string());
            self.maybe_start_completion_watcher(
                new_thread.thread_id,
                notification_source,
                child_reference,
                agent_metadata.agent_path.clone(),
            );
        }

        Ok(LiveAgent {
            thread_id: new_thread.thread_id,
            metadata: agent_metadata,
            status: self.get_status(new_thread.thread_id).await,
        })
    }

    /// Reconcile a child that was created but never crossed the parent-visible publication
    /// boundary.
    ///
    /// The reservation must outlive a failed cleanup while the child is still live: dropping it
    /// would make capacity and a requested agent path reusable even though the manager still owns
    /// the runtime. The caller retains its original cancellation or initial-delivery error; a
    /// cleanup failure is logged and handed to a bounded, explicit cleanup owner instead.
    pub(crate) async fn reconcile_unpublished_spawn(
        &self,
        state: &Arc<ThreadManagerState>,
        child_thread_id: ThreadId,
        child_thread: &Arc<CodexThread>,
        reservation: &mut Option<crate::agent::registry::SpawnReservation>,
        residency_slot: &mut Option<V2ResidencySlot>,
    ) -> CodexResult<()> {
        match self
            .attempt_unpublished_spawn_cleanup(state, child_thread_id, child_thread)
            .await
        {
            UnpublishedSpawnCleanupAttempt::Terminal { cleanup_error } => {
                cleanup_error.map_or(Ok(()), Err)
            }
            UnpublishedSpawnCleanupAttempt::StillLive { cleanup_error } => {
                self.retain_unpublished_spawn_cleanup(
                    Arc::clone(state),
                    child_thread_id,
                    Arc::clone(child_thread),
                    reservation.take(),
                    residency_slot.take(),
                    /*shutdown_exact_child*/ true,
                );
                Err(cleanup_error)
            }
            UnpublishedSpawnCleanupAttempt::Replaced { cleanup_error } => {
                let cleanup_error = cleanup_error.unwrap_or_else(|| {
                    CodexErr::Fatal(format!(
                        "unpublished child {child_thread_id} was replaced before cleanup could remove its exact manager instance"
                    ))
                });
                self.retain_unpublished_spawn_cleanup(
                    Arc::clone(state),
                    child_thread_id,
                    Arc::clone(child_thread),
                    reservation.take(),
                    residency_slot.take(),
                    /*shutdown_exact_child*/ false,
                );
                Err(cleanup_error)
            }
        }
    }

    /// Performs one bounded cleanup attempt for the exact child instance. The function never
    /// removes a live child and uses `remove_thread_if_same` only after terminalization, so a
    /// same-ID replacement remains managed by its own manager entry.
    async fn attempt_unpublished_spawn_cleanup(
        &self,
        state: &Arc<ThreadManagerState>,
        child_thread_id: ThreadId,
        child_thread: &Arc<CodexThread>,
    ) -> UnpublishedSpawnCleanupAttempt {
        let initial_status = child_thread.agent_status().await;
        let reconciliation = unpublished_spawn_reconciliation(&initial_status);
        let mut cleanup_error = None;

        if !matches!(reconciliation, UnpublishedSpawnReconciliation::ChildDied) {
            #[cfg(test)]
            let shutdown_result = if self.take_unpublished_shutdown_failure_for_test() {
                Err(CodexErr::Fatal(
                    "injected unpublished spawn shutdown failure".to_string(),
                ))
            } else {
                child_thread.shutdown_and_wait().await
            };
            #[cfg(not(test))]
            let shutdown_result = child_thread.shutdown_and_wait().await;

            if let Err(error) = shutdown_result
                && !is_benign_unpublished_spawn_cleanup_error(&error)
            {
                cleanup_error = Some(error);
            }
        }

        // A PendingInit/Running sample can become a natural terminal event just before the
        // shutdown reaches the child. Read again after teardown and materialize every terminal
        // outcome: Shutdown can overwrite the status watch after a preceding TurnComplete, so
        // flushing the queued rollout rather than trusting that final label preserves either
        // terminal history.
        let final_status = child_thread.agent_status().await;
        if !is_final(&final_status) {
            return UnpublishedSpawnCleanupAttempt::StillLive {
                cleanup_error: cleanup_error.unwrap_or_else(|| {
                    CodexErr::Fatal(format!(
                        "unpublished child {child_thread_id} remained live after cleanup: {final_status:?}"
                    ))
                }),
            };
        }

        self.finalize_unpublished_spawn_cleanup(state, child_thread_id, child_thread, cleanup_error)
            .await
    }

    /// Materialize and remove a child only after its session loop has terminated. A `Replaced`
    /// result deliberately retains the caller's reservation; removing or releasing at that point
    /// would make an unrelated same-ID manager instance invisible or permit a path collision.
    async fn finalize_unpublished_spawn_cleanup(
        &self,
        state: &Arc<ThreadManagerState>,
        child_thread_id: ThreadId,
        child_thread: &Arc<CodexThread>,
        mut cleanup_error: Option<CodexErr>,
    ) -> UnpublishedSpawnCleanupAttempt {
        if let Err(error) = child_thread.session.try_ensure_rollout_materialized().await
            && cleanup_error.is_none()
            && !is_benign_unpublished_spawn_persistence_error(&error)
        {
            cleanup_error = Some(CodexErr::Io(error));
        }
        if let Err(error) = child_thread.flush_rollout().await
            && cleanup_error.is_none()
            && !is_benign_unpublished_spawn_persistence_error(&error)
        {
            cleanup_error = Some(CodexErr::Io(error));
        }

        let removal = state
            .remove_thread_if_same(&child_thread_id, child_thread, || {})
            .await;

        if matches!(removal, RemoveThreadIfSameResult::Replaced) {
            tracing::warn!(
                %child_thread_id,
                "unpublished terminal child was replaced before cleanup; retaining capacity for the replacement"
            );
            return UnpublishedSpawnCleanupAttempt::Replaced { cleanup_error };
        }

        UnpublishedSpawnCleanupAttempt::Terminal { cleanup_error }
    }

    /// Transfers a still-manager-owned child into a durable cleanup owner before the spawning
    /// caller returns. The owner holds capacity/path and V2 pending residency and is therefore
    /// visible through the manager while preserving the no-publication contract to the parent.
    fn retain_unpublished_spawn_cleanup(
        &self,
        state: Arc<ThreadManagerState>,
        child_thread_id: ThreadId,
        child_thread: Arc<CodexThread>,
        reservation: Option<crate::agent::registry::SpawnReservation>,
        residency_slot: Option<V2ResidencySlot>,
        shutdown_exact_child: bool,
    ) {
        let Some(reservation) = reservation else {
            tracing::error!(
                %child_thread_id,
                "unpublished cleanup lost its spawn reservation"
            );
            return;
        };
        let control = self.clone();
        std::mem::drop(tokio::spawn(async move {
            let _retained = RetainedUnpublishedSpawnCleanup {
                _reservation: reservation,
                _residency_slot: residency_slot,
            };
            #[cfg(test)]
            control
                .await_retained_unpublished_spawn_cleanup_test_hook(child_thread_id)
                .await;

            if shutdown_exact_child {
                control
                    .retry_unpublished_spawn_shutdown_until_terminated(
                        &state,
                        child_thread_id,
                        &child_thread,
                    )
                    .await;
            } else {
                control
                    .observe_unpublished_spawn_replacements_until_terminal(&state, child_thread_id)
                    .await;
            }
        }));
    }

    /// Retry shutdown only a fixed number of times. On exhaustion, do not continue an invisible
    /// retry loop: keep the durable cleanup owner and wait for the runtime's existing termination
    /// signal before the final same-instance removal.
    async fn retry_unpublished_spawn_shutdown_until_terminated(
        &self,
        state: &Arc<ThreadManagerState>,
        child_thread_id: ThreadId,
        child_thread: &Arc<CodexThread>,
    ) {
        for attempt in 1..=UNPUBLISHED_SPAWN_CLEANUP_MAX_SHUTDOWN_ATTEMPTS {
            match self
                .attempt_unpublished_spawn_cleanup(state, child_thread_id, child_thread)
                .await
            {
                UnpublishedSpawnCleanupAttempt::Terminal { cleanup_error } => {
                    self.log_unpublished_spawn_cleanup_result(child_thread_id, cleanup_error);
                    return;
                }
                UnpublishedSpawnCleanupAttempt::Replaced { cleanup_error } => {
                    self.log_unpublished_spawn_cleanup_result(child_thread_id, cleanup_error);
                    self.observe_unpublished_spawn_replacements_until_terminal(
                        state,
                        child_thread_id,
                    )
                    .await;
                    return;
                }
                UnpublishedSpawnCleanupAttempt::StillLive { cleanup_error } => {
                    tracing::warn!(
                        %child_thread_id,
                        %cleanup_error,
                        attempt,
                        max_attempts = UNPUBLISHED_SPAWN_CLEANUP_MAX_SHUTDOWN_ATTEMPTS,
                        "unpublished child is still live; retaining its reservation for bounded cleanup"
                    );
                    if attempt < UNPUBLISHED_SPAWN_CLEANUP_MAX_SHUTDOWN_ATTEMPTS {
                        tokio::time::sleep(UNPUBLISHED_SPAWN_CLEANUP_RETRY_DELAY).await;
                    }
                }
            }
        }

        tracing::error!(
            %child_thread_id,
            max_attempts = UNPUBLISHED_SPAWN_CLEANUP_MAX_SHUTDOWN_ATTEMPTS,
            "unpublished cleanup shutdown retries exhausted; retaining manager ownership until runtime termination"
        );
        child_thread.wait_until_terminated().await;
        match self
            .finalize_unpublished_spawn_cleanup(
                state,
                child_thread_id,
                child_thread,
                /*cleanup_error*/ None,
            )
            .await
        {
            UnpublishedSpawnCleanupAttempt::Terminal { cleanup_error } => {
                self.log_unpublished_spawn_cleanup_result(child_thread_id, cleanup_error);
            }
            UnpublishedSpawnCleanupAttempt::Replaced { cleanup_error } => {
                self.log_unpublished_spawn_cleanup_result(child_thread_id, cleanup_error);
                self.observe_unpublished_spawn_replacements_until_terminal(state, child_thread_id)
                    .await;
            }
            UnpublishedSpawnCleanupAttempt::StillLive { .. } => {
                unreachable!(
                    "finalization after session-loop termination cannot report a live child"
                )
            }
        }
    }

    /// A replacement must never be shut down by the cancelled spawn's owner. Hold the original
    /// reservation while observing each replacement manager instance to its natural runtime
    /// termination, then remove only the same instance. This is observation, not a retry loop.
    async fn observe_unpublished_spawn_replacements_until_terminal(
        &self,
        state: &Arc<ThreadManagerState>,
        child_thread_id: ThreadId,
    ) {
        loop {
            let Ok(replacement) = state.get_thread(child_thread_id).await else {
                return;
            };
            replacement.wait_until_terminated().await;
            match self
                .finalize_unpublished_spawn_cleanup(
                    state,
                    child_thread_id,
                    &replacement,
                    /*cleanup_error*/ None,
                )
                .await
            {
                UnpublishedSpawnCleanupAttempt::Terminal { cleanup_error } => {
                    self.log_unpublished_spawn_cleanup_result(child_thread_id, cleanup_error);
                    return;
                }
                UnpublishedSpawnCleanupAttempt::Replaced { cleanup_error } => {
                    self.log_unpublished_spawn_cleanup_result(child_thread_id, cleanup_error);
                    tracing::warn!(
                        %child_thread_id,
                        "same-ID replacement changed again during cleanup observation; retaining reservation for the current manager instance"
                    );
                }
                UnpublishedSpawnCleanupAttempt::StillLive { .. } => {
                    unreachable!(
                        "finalization after replacement termination cannot report a live child"
                    )
                }
            }
        }
    }

    fn log_unpublished_spawn_cleanup_result(
        &self,
        child_thread_id: ThreadId,
        cleanup_error: Option<CodexErr>,
    ) {
        if let Some(cleanup_error) = cleanup_error {
            tracing::error!(
                %child_thread_id,
                %cleanup_error,
                "unpublished spawn cleanup reached terminal manager state with a persistence fault"
            );
        }
    }

    async fn spawn_forked_thread(
        &self,
        state: &Arc<ThreadManagerState>,
        config: Config,
        session_source: SessionSource,
        options: &SpawnAgentOptions,
        inheritance: SpawnAgentThreadInheritance,
        multi_agent_version: MultiAgentVersion,
    ) -> CodexResult<crate::thread_manager::NewThread> {
        let SpawnAgentThreadInheritance {
            environments: inherited_environments,
            exec_policy: inherited_exec_policy,
        } = inheritance;
        if options.fork_parent_spawn_call_id.is_none() {
            return Err(CodexErr::Fatal(
                "spawn_agent fork requires a parent spawn call id".to_string(),
            ));
        }
        let Some(fork_mode) = options.fork_mode.as_ref() else {
            return Err(CodexErr::Fatal(
                "spawn_agent fork requires a fork mode".to_string(),
            ));
        };
        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        }) = &session_source
        else {
            return Err(CodexErr::Fatal(
                "spawn_agent fork requires a thread-spawn session source".to_string(),
            ));
        };

        let parent_thread_id = *parent_thread_id;
        let parent_thread = state.get_thread(parent_thread_id).await?;
        let (subagent_developer_instructions, parent_developer_instructions) = match (
            multi_agent_version,
            config
                .multi_agent_v2
                .subagent_developer_instructions
                .as_ref(),
        ) {
            (MultiAgentVersion::V2, override_instructions)
                if override_instructions.is_some() || session_source.get_agent_role().is_some() =>
            {
                let parent_developer_instructions = match parent_thread
                    .session
                    .new_default_turn()
                    .await
                    .developer_instructions
                    .clone()
                {
                    Some(instructions) if !instructions.is_empty() => Some(instructions),
                    Some(_) | None => None,
                };
                (
                    Some(config.developer_instructions.clone().unwrap_or_default()),
                    parent_developer_instructions,
                )
            }
            (MultiAgentVersion::Disabled | MultiAgentVersion::V1, _)
            | (MultiAgentVersion::V2, _) => (None, None),
        };
        let parent_history_mode = parent_thread.config_snapshot().await.history_mode;
        // `record_conversation_items` only queues persistence writes asynchronously.
        // Flush before snapshotting store history for a fork.
        parent_thread.ensure_rollout_materialized().await;
        parent_thread.flush_rollout().await?;

        let destination_history_mode = matches!(parent_history_mode, ThreadHistoryMode::Paginated)
            .then_some(ThreadHistoryMode::Paginated);
        let mut forked_rollout_items =
            load_agent_model_context(state, parent_thread_id, parent_history_mode)
                .await?
                .ok_or_else(|| {
                    CodexErr::Fatal(format!(
                        "parent thread history unavailable for fork: {parent_thread_id}"
                    ))
                })?;

        let selected_capability_roots = forked_rollout_items
            .iter()
            .find_map(|item| {
                let RolloutItem::SessionMeta(meta_line) = item else {
                    return None;
                };
                Some(meta_line.meta.selected_capability_roots.clone())
            })
            .unwrap_or_default();
        if let SpawnAgentForkMode::LastNTurns(last_n_turns) = fork_mode {
            forked_rollout_items =
                truncate_rollout_to_last_n_fork_turns(forked_rollout_items, *last_n_turns);
        }
        let multi_agent_v2_usage_hint_texts_to_filter: Vec<String> =
            if multi_agent_version == MultiAgentVersion::V2 {
                let parent_config = parent_thread.session.get_config().await;
                let parent_usage_hints = resolve_usage_hints(
                    &parent_config.multi_agent_v2,
                    /*catalog*/ None,
                    !parent_config.update_plan_enabled,
                );
                [parent_usage_hints.root, parent_usage_hints.subagent]
                    .into_iter()
                    .flatten()
                    .map(|instructions| instructions.render())
                    .collect()
            } else {
                Vec::new()
            };
        let mut preserve_reference_context_item =
            matches!(fork_mode, SpawnAgentForkMode::FullHistory);
        if preserve_reference_context_item {
            for item in forked_rollout_items.iter().rev() {
                let RolloutItem::Compacted(compacted) = item else {
                    continue;
                };
                // Legacy checkpoints force the child to rebuild context regardless of the
                // live parent's reference baseline; an older superseded checkpoint does not.
                if compacted.replacement_history.is_none() {
                    preserve_reference_context_item = false;
                }
                break;
            }
        }
        let context_mode = GuardianContextMode::from_features(&config.features);
        let mut replaced_parent_developer_instructions = false;
        // Scrub inherited hints and replace only the parent's developer-instruction fragment.
        // Compaction stores response items separately, so sanitize both top-level messages and
        // compacted replacement histories with the same policy.
        let retain_forked_item = |envelope: &mut ResponseItemEnvelope, replaced: &mut bool| {
            if context_mode == GuardianContextMode::ThreadOwned
                && multi_agent_version == MultiAgentVersion::V2
                && matches!(&envelope.item, ResponseItem::Message { role, .. } if role == "user")
            {
                // Persist the scope of every inherited user message, including the suffix
                // after a checkpoint. Resume must not recapture it as local authorization.
                envelope
                    .metadata
                    .get_or_insert_default()
                    .inherited_user_message = true;
            }
            let response_item = &mut envelope.item;
            if matches!(response_item, ResponseItem::AgentMessage { .. }) {
                return false;
            }
            if !retain_forked_developer_message(
                response_item,
                &multi_agent_v2_usage_hint_texts_to_filter,
                context_mode,
            ) {
                return false;
            }

            if matches!(response_item, ResponseItem::Message { role, .. } if role == "developer") {
                let Some(mut content) = to_annotated_content(response_item) else {
                    return false;
                };
                content.retain_mut(|content_item| {
                    let ContentItem::InputText { text } = content_item.content_mut() else {
                        return true;
                    };
                    if ManagedDeveloperInstructions::matches_text(text)
                        || PersistentModeState::matches_text(text)
                    {
                        // If the child will rebuild its initial context, drop the inherited
                        // instructions; startup will add the current requirements and effort
                        // instructions once.
                        return preserve_reference_context_item;
                    }
                    let (
                        Some(parent_developer_instructions),
                        Some(subagent_developer_instructions),
                    ) = (
                        parent_developer_instructions.as_ref(),
                        subagent_developer_instructions.as_ref(),
                    )
                    else {
                        return true;
                    };
                    // TODO(anp) track better message fragment provenance in rollouts.
                    if !text.contains(parent_developer_instructions) {
                        return true;
                    }

                    *replaced = true;
                    let replacement = if preserve_reference_context_item {
                        subagent_developer_instructions.as_str()
                    } else {
                        ""
                    };
                    *text = text.replace(parent_developer_instructions, replacement);
                    !text.is_empty()
                });
                return !content.is_empty()
                    && set_annotated_content(response_item, content).is_some();
            }

            true
        };
        forked_rollout_items.retain_mut(|item| {
            if !keep_forked_rollout_item(item, preserve_reference_context_item)
                || destination_history_mode == Some(ThreadHistoryMode::Paginated)
                    && matches!(
                        &*item,
                        RolloutItem::EventMsg(
                            EventMsg::ItemCompleted(_)
                                | EventMsg::TokenCount(_)
                                | EventMsg::ThreadGoalUpdated(_)
                                | EventMsg::ThreadSettingsApplied(_),
                        )
                    )
            {
                return false;
            }

            match item {
                RolloutItem::ResponseItem(response_item) => {
                    retain_forked_item(response_item, &mut replaced_parent_developer_instructions)
                }
                RolloutItem::Compacted(compacted) => {
                    // This checkpoint belongs to the inherited parent prefix.
                    compacted.latest_token_usage_record = None;
                    // Parent-local review evidence must not become the child's authorization.
                    // Root user authorization is collected separately by the host.
                    compacted.guardian_history = None;
                    // Only V2 fetches root authorization live. Its local scope starts known-empty;
                    // V1 must remain incomplete when inherited authorization has been stripped.
                    compacted.retained_context = (context_mode == GuardianContextMode::ThreadOwned
                        && multi_agent_version == MultiAgentVersion::V2)
                        .then(codex_history::RetainedContext::default);
                    if let Some(replacement_history) = compacted.replacement_history.as_mut() {
                        // Matches before this checkpoint cannot survive its replacement history.
                        replaced_parent_developer_instructions = false;
                        replacement_history.retain_mut(|response_item| {
                            retain_forked_item(
                                response_item,
                                &mut replaced_parent_developer_instructions,
                            )
                        });
                    }
                    true
                }
                RolloutItem::WorldState(world_state) => {
                    if multi_agent_version == MultiAgentVersion::V2 {
                        world_state.state.remove("multi_agent_usage_hint");
                    }
                    true
                }
                RolloutItem::RealtimeItem(_) => false,
                RolloutItem::EventMsg(_)
                | RolloutItem::SessionMeta(_)
                | RolloutItem::TurnContext(_)
                | RolloutItem::InterAgentCommunication(_)
                | RolloutItem::InterAgentCommunicationMetadata { .. } => true,
                RolloutItem::RetainedContext(_)
                | RolloutItem::TokenUsageRecord(_)
                | RolloutItem::SecurityRiskScore(_) => false,
            }
        });
        // Full forks reuse the parent's reference context instead of rebuilding it. If that
        // context omitted the parent's developer fragment, append the child's override so its
        // instructions still reach the model exactly once.
        if let Some(subagent_developer_instructions) = subagent_developer_instructions.as_ref()
            && preserve_reference_context_item
            && !replaced_parent_developer_instructions
            && !subagent_developer_instructions.is_empty()
            && parent_thread
                .session
                .reference_context_item()
                .await
                .is_some()
        {
            let developer_message = ContextualUserFragment::into(DeveloperInstructions::new(
                subagent_developer_instructions,
            ));
            forked_rollout_items.push(RolloutItem::ResponseItem(developer_message.into()));
        }
        if preserve_reference_context_item
            && multi_agent_version == MultiAgentVersion::V2
            && let Some(subagent_usage_hint) = options
                .multi_agent_v2_usage_hints
                .as_ref()
                .map(|hints| hints.subagent.clone())
                .unwrap_or_else(|| {
                    resolve_usage_hints(
                        &config.multi_agent_v2,
                        /*catalog*/ None,
                        !config.update_plan_enabled,
                    )
                    .subagent
                })
        {
            let subagent_usage_hint_message = ContextualUserFragment::into(subagent_usage_hint);
            forked_rollout_items.push(RolloutItem::ResponseItem(
                subagent_usage_hint_message.into(),
            ));
        }
        let mut thread_extension_init = ExtensionDataInit::new();
        thread_extension_init.insert(selected_capability_roots);

        state
            .fork_thread_with_source(
                config.clone(),
                InitialHistory::Forked(forked_rollout_items),
                destination_history_mode,
                self.clone(),
                session_source,
                /*thread_source*/ Some(ThreadSource::Subagent),
                /*parent_thread_id*/ Some(parent_thread_id),
                /*forked_from_thread_id*/ Some(parent_thread_id),
                configured_browser_dynamic_tools(&config),
                inherited_environments,
                inherited_exec_policy,
                options.environments.clone(),
                thread_extension_init,
            )
            .await
    }

    /// Resume an existing agent thread from a recorded rollout file.
    pub(crate) async fn resume_agent_from_rollout(
        &self,
        config: Config,
        thread_id: ThreadId,
        session_source: SessionSource,
    ) -> CodexResult<ThreadId> {
        let root_depth = thread_spawn_depth(&session_source).unwrap_or(0);
        let (resumed_thread_id, resumed_multi_agent_version) = Box::pin(
            self.resume_single_agent_from_rollout(config.clone(), thread_id, session_source),
        )
        .await?;
        let state = self.upgrade()?;
        if config.multi_agent_version_from_features() == MultiAgentVersion::V2
            || resumed_multi_agent_version == MultiAgentVersion::V2
        {
            return Ok(resumed_thread_id);
        }
        let Some(agent_graph_store) = state.agent_graph_store() else {
            return Ok(resumed_thread_id);
        };

        let mut resume_queue = VecDeque::from([(thread_id, root_depth)]);
        while let Some((parent_thread_id, parent_depth)) = resume_queue.pop_front() {
            let child_ids = match agent_graph_store
                .list_thread_spawn_children(
                    parent_thread_id,
                    Some(codex_agent_graph_store::ThreadSpawnEdgeStatus::Open),
                )
                .await
            {
                Ok(child_ids) => child_ids,
                Err(err) => {
                    warn!(
                        "failed to load persisted thread-spawn children for {parent_thread_id}: {err}"
                    );
                    continue;
                }
            };

            for child_thread_id in child_ids {
                let child_depth = parent_depth + 1;
                let child_resumed = if state.get_thread(child_thread_id).await.is_ok() {
                    true
                } else {
                    let child_session_source =
                        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                            parent_thread_id,
                            depth: child_depth,
                            agent_path: None,
                            agent_nickname: None,
                            agent_role: None,
                        });
                    match Box::pin(self.resume_single_agent_from_rollout(
                        config.clone(),
                        child_thread_id,
                        child_session_source,
                    ))
                    .await
                    {
                        Ok((_, _)) => true,
                        Err(err) => {
                            warn!("failed to resume descendant thread {child_thread_id}: {err}");
                            false
                        }
                    }
                };
                if child_resumed {
                    resume_queue.push_back((child_thread_id, child_depth));
                }
            }
        }

        Ok(resumed_thread_id)
    }

    async fn resume_single_agent_from_rollout(
        &self,
        config: Config,
        thread_id: ThreadId,
        session_source: SessionSource,
    ) -> CodexResult<(ThreadId, MultiAgentVersion)> {
        let state = self.upgrade()?;
        let stored_thread = state
            .read_stored_thread(ReadThreadParams {
                thread_id,
                include_archived: true,
                include_history: false,
            })
            .await?;
        let resumed_agent_path = stored_thread
            .agent_path
            .as_deref()
            .map(AgentPath::try_from)
            .transpose()
            .map_err(|err| CodexErr::InvalidRequest(format!("invalid stored agent path: {err}")))?;
        let resumed_agent_nickname = stored_thread.agent_nickname.clone();
        let resumed_agent_role = stored_thread.agent_role.clone();
        let terminal_idle_unload_role = resumed_agent_role
            .clone()
            .or_else(|| stored_thread.source.get_agent_role())
            .or_else(|| session_source.get_agent_role());
        let history = load_agent_model_context(&state, thread_id, stored_thread.history_mode)
            .await?
            .ok_or(CodexErr::ThreadNotFound(thread_id))?;
        let initial_history = InitialHistory::Resumed(ResumedHistory {
            conversation_id: thread_id,
            history: Arc::new(history),
            rollout_path: stored_thread.rollout_path,
        });
        let parent_thread_id = stored_thread.parent_thread_id;
        let multi_agent_version = state
            .effective_multi_agent_version_for_spawn(
                &initial_history,
                Some(&session_source),
                parent_thread_id,
                /*forked_from_thread_id*/ None,
                &config,
            )
            .await;
        let agent_max_threads = config.effective_agent_max_threads(multi_agent_version);
        let mut reservation = self.state.reserve_spawn_slot(agent_max_threads)?;
        let (session_source, agent_metadata) = match session_source {
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth,
                agent_path,
                agent_role: _,
                agent_nickname: _,
            }) => self.prepare_thread_spawn(
                &mut reservation,
                &config,
                parent_thread_id,
                depth,
                agent_path.or(resumed_agent_path),
                resumed_agent_role,
                resumed_agent_nickname,
            )?,
            other => (other, AgentMetadata::default()),
        };
        let notification_source = session_source.clone();
        let uses_v2_residency = multi_agent_version == MultiAgentVersion::V2
            && is_v2_resident_session_source(&session_source);
        let terminal_idle_unload_timeout_ms = if uses_v2_residency {
            terminal_idle_unload_timeout_for_resumed_role(
                &config,
                terminal_idle_unload_role.as_deref(),
                thread_id,
            )
            .await
        } else {
            config.multi_agent_v2.terminal_idle_unload_timeout_ms
        };
        let residency_slot = if uses_v2_residency {
            Some(
                self.reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
                    .await?,
            )
        } else {
            None
        };
        let inherited_environments = self
            .inherited_environments_for_source(&state, Some(&session_source))
            .await;
        let inherited_exec_policy = self
            .inherited_exec_policy_for_source(&state, Some(&session_source), &config)
            .await;

        let resumed_thread = state
            .resume_thread_with_history_with_source(ResumeThreadWithHistoryOptions {
                config: config.clone(),
                initial_history,
                agent_control: self.clone(),
                session_source,
                parent_thread_id,
                dynamic_tools: configured_browser_dynamic_tools(&config),
                environment_selections: None,
                inherited_environments,
                inherited_instructions: None,
                inherited_exec_policy,
                client_mcp_extensions: None,
            })
            .await?;
        let mut agent_metadata = agent_metadata;
        agent_metadata.agent_id = Some(resumed_thread.thread_id);
        reservation.commit(agent_metadata.clone());
        if let Some(residency_slot) = residency_slot {
            residency_slot.commit(resumed_thread.thread_id);
            self.start_terminal_idle_unload_watcher(
                Arc::clone(&resumed_thread.thread),
                agent_metadata.clone(),
                terminal_idle_unload_timeout_ms,
            )
            .await;
        }
        // Resumed threads are re-registered in-memory and need the same listener
        // attachment path as freshly spawned threads.
        state.notify_thread_created(resumed_thread.thread_id);
        if multi_agent_version != MultiAgentVersion::V2 {
            let child_reference = agent_metadata
                .agent_path
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| resumed_thread.thread_id.to_string());
            self.maybe_start_completion_watcher(
                resumed_thread.thread_id,
                Some(notification_source.clone()),
                child_reference,
                agent_metadata.agent_path.clone(),
            );
        }
        self.persist_thread_spawn_edge_for_source(
            resumed_thread.thread.as_ref(),
            resumed_thread.thread_id,
            Some(&notification_source),
        )
        .await;

        Ok((resumed_thread.thread_id, multi_agent_version))
    }
}
