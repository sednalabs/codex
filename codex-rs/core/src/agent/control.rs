use crate::agent::AgentStatus;
use crate::agent::registry::AgentMetadata;
use crate::agent::registry::AgentRegistry;
use crate::agent::role::DEFAULT_ROLE_NAME;
use crate::agent::role::resolve_role_config;
use crate::agent::status::is_final;
use crate::codex_thread::ThreadConfigSnapshot;
use crate::session::emit_subagent_session_started;
use crate::session_prefix::format_subagent_context_line;
use crate::session_prefix::format_subagent_notification_message;
use crate::shell_snapshot::ShellSnapshot;
use crate::state_db;
use crate::thread_manager::ResumeThreadWithHistoryOptions;
use crate::thread_manager::ThreadManagerState;
use crate::thread_rollout_truncation::truncate_rollout_to_last_n_fork_turns;
use codex_features::Feature;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ResumedHistory;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::user_input::UserInput;
use codex_thread_store::ReadThreadParams;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Weak;
use tokio::sync::watch;
use tracing::warn;

const AGENT_NAMES: &str = include_str!("agent_names.txt");
const FORKED_SPAWN_AGENT_OUTPUT_MESSAGE: &str = "You are the newly spawned agent. The prior conversation history was forked from your parent agent. Treat the next user message as your new task, and use the forked history only as background context.";
pub(crate) const SUBAGENT_IDENTITY_SOURCE_THREAD_CONFIG_SNAPSHOT: &str = "thread_config_snapshot";
const ROOT_LAST_TASK_MESSAGE: &str = "Main thread";
const INSPECT_AGENT_TREE_STATE_DB_UNAVAILABLE_MESSAGE: &str = concat!(
    "inspect_agent_tree cannot include stale descendants because this session has no configured ",
    "state_db. Retry with scope=\"live\" for live-only inspection. For a completed sidecar, use ",
    "$subagent-session-tail with the child thread id (`inspect_subagent_tail.py --child-thread-id ",
    "<child-thread-id>`), or with parent thread id plus the exact agent_path if the child id is ",
    "unavailable."
);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SpawnAgentForkMode {
    FullHistory,
    LastNTurns(usize),
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SpawnAgentOptions {
    pub(crate) fork_parent_spawn_call_id: Option<String>,
    pub(crate) fork_mode: Option<SpawnAgentForkMode>,
    pub(crate) environments: Option<Vec<TurnEnvironmentSelection>>,
}

#[derive(Clone, Debug)]
pub(crate) struct LiveAgent {
    pub(crate) thread_id: ThreadId,
    pub(crate) metadata: AgentMetadata,
    pub(crate) status: AgentStatus,
}

/// Internal inventory snapshot for a spawned sub-agent.
///
/// `status` is the live agent state, while `effective_*` and `identity_source`
/// are resolved from the config snapshot used to reconstruct the agent record.
#[derive(Debug, Clone)]
pub(crate) struct SubAgentInventoryInfo {
    pub(crate) nickname: Option<String>,
    pub(crate) role: Option<String>,
    pub(crate) status: AgentStatus,
    pub(crate) effective_model: Option<String>,
    pub(crate) effective_reasoning_effort: Option<ReasoningEffort>,
    pub(crate) effective_model_provider_id: String,
    pub(crate) identity_source: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct ListedAgent {
    pub(crate) agent_name: String,
    pub(crate) agent_status: AgentStatus,
    pub(crate) last_task_message: Option<String>,
    pub(crate) has_active_subagents: bool,
    pub(crate) active_subagent_count: usize,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentTreeScope {
    Live,
    Stale,
    All,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentSessionState {
    Live,
    Stale,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct AgentTreeSummary {
    pub(crate) total_agents: usize,
    pub(crate) live_agents: usize,
    pub(crate) stale_agents: usize,
    pub(crate) pending_init_agents: usize,
    pub(crate) running_agents: usize,
    pub(crate) interrupted_agents: usize,
    pub(crate) completed_agents: usize,
    pub(crate) errored_agents: usize,
    pub(crate) shutdown_agents: usize,
    pub(crate) not_found_agents: usize,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct AgentTreeNode {
    pub(crate) agent_name: String,
    pub(crate) depth: usize,
    pub(crate) session_state: AgentSessionState,
    pub(crate) agent_status: Option<AgentStatus>,
    pub(crate) nickname: Option<String>,
    pub(crate) role: Option<String>,
    pub(crate) direct_child_count: usize,
    pub(crate) descendant_count: usize,
    pub(crate) last_task_message_preview: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct AgentTreeInspection {
    pub(crate) root_agent_name: String,
    pub(crate) scope_applied: AgentTreeScope,
    pub(crate) agent_roots_applied: Vec<String>,
    pub(crate) max_depth_applied: usize,
    pub(crate) max_agents_applied: usize,
    pub(crate) truncated: bool,
    pub(crate) summary: AgentTreeSummary,
    pub(crate) agents: Vec<AgentTreeNode>,
}

#[derive(Clone, Debug)]
struct AgentTreeRecord {
    agent_name: String,
    session_state: AgentSessionState,
    agent_status: Option<AgentStatus>,
    nickname: Option<String>,
    role: Option<String>,
    last_task_message_preview: Option<String>,
}

fn default_agent_nickname_list() -> Vec<&'static str> {
    AGENT_NAMES
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect()
}

fn agent_nickname_candidates(
    config: &crate::config::Config,
    role_name: Option<&str>,
) -> Vec<String> {
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

fn keep_forked_rollout_item(item: &RolloutItem) -> bool {
    match item {
        RolloutItem::ResponseItem(ResponseItem::Message { role, phase, .. }) => match role.as_str()
        {
            "system" | "developer" | "user" => true,
            "assistant" => *phase == Some(MessagePhase::FinalAnswer),
            _ => false,
        },
        RolloutItem::ResponseItem(
            ResponseItem::Reasoning { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::FunctionCallOutput { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::CustomToolCallOutput { .. }
            | ResponseItem::ToolSearchOutput { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::ContextCompaction { .. }
            | ResponseItem::Other,
        ) => false,
        // A forked child gets its own runtime config, including spawned-agent
        // instructions, so it must establish a fresh context diff baseline.
        RolloutItem::TurnContext(_) => false,
        RolloutItem::Compacted(_) | RolloutItem::EventMsg(_) | RolloutItem::SessionMeta(_) => true,
    }
}
/// Control-plane handle for multi-agent operations.
/// `AgentControl` is held by each session (via `SessionServices`). It provides capability to
/// spawn new agents and the inter-agent communication layer.
/// An `AgentControl` instance is intended to be created at most once per root thread/session
/// tree. That same `AgentControl` is then shared with every sub-agent spawned from that root,
/// which keeps the registry scoped to that root thread rather than the entire `ThreadManager`.
#[derive(Clone, Default)]
pub(crate) struct AgentControl {
    /// Weak handle back to the global thread registry/state.
    /// This is `Weak` to avoid reference cycles and shadow persistence of the form
    /// `ThreadManagerState -> CodexThread -> Session -> SessionServices -> ThreadManagerState`.
    manager: Weak<ThreadManagerState>,
    state: Arc<AgentRegistry>,
}

impl AgentControl {
    /// Construct a new `AgentControl` that can spawn/message agents via the given manager state.
    pub(crate) fn new(manager: Weak<ThreadManagerState>) -> Self {
        Self {
            manager,
            ..Default::default()
        }
    }

    /// Spawn a new agent thread and submit the initial prompt.
    #[cfg(test)]
    pub(crate) async fn spawn_agent(
        &self,
        config: crate::config::Config,
        initial_operation: Op,
        session_source: Option<SessionSource>,
    ) -> CodexResult<ThreadId> {
        let spawned_agent = Box::pin(self.spawn_agent_internal(
            config,
            initial_operation,
            session_source,
            SpawnAgentOptions::default(),
        ))
        .await?;
        Ok(spawned_agent.thread_id)
    }

    /// Spawn an agent thread with some metadata.
    pub(crate) async fn spawn_agent_with_metadata(
        &self,
        config: crate::config::Config,
        initial_operation: Op,
        session_source: Option<SessionSource>,
        options: SpawnAgentOptions, // TODO(jif) drop with new fork.
    ) -> CodexResult<LiveAgent> {
        Box::pin(self.spawn_agent_internal(config, initial_operation, session_source, options))
            .await
    }

    async fn spawn_agent_internal(
        &self,
        config: crate::config::Config,
        initial_operation: Op,
        session_source: Option<SessionSource>,
        options: SpawnAgentOptions,
    ) -> CodexResult<LiveAgent> {
        let state = self.upgrade()?;
        let mut reservation = self.state.reserve_spawn_slot(config.agent_max_threads)?;
        let inherited_shell_snapshot = self
            .inherited_shell_snapshot_for_source(&state, session_source.as_ref())
            .await;
        let inherited_exec_policy = self
            .inherited_exec_policy_for_source(&state, session_source.as_ref(), &config)
            .await;
        let (session_source, mut agent_metadata) = match session_source {
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth,
                agent_path,
                agent_role,
                ..
            })) => {
                let (session_source, agent_metadata) = self.prepare_thread_spawn(
                    &mut reservation,
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
        let should_fork_from_parent =
            options.fork_parent_spawn_call_id.is_some() || options.fork_mode.is_some();
        let new_thread = match (session_source, should_fork_from_parent) {
            (Some(session_source), true) => {
                self.spawn_forked_thread(
                    &state,
                    config,
                    session_source,
                    &options,
                    inherited_shell_snapshot,
                    inherited_exec_policy,
                )
                .await?
            }
            (Some(session_source), false) => {
                state
                    .spawn_new_thread_with_source(
                        config.clone(),
                        self.clone(),
                        session_source,
                        /*persist_extended_history*/ false,
                        /*metrics_service_name*/ None,
                        inherited_shell_snapshot,
                        inherited_exec_policy,
                        options.environments.clone(),
                    )
                    .await?
            }
            (None, _) => state.spawn_new_thread(config.clone(), self.clone()).await?,
        };
        agent_metadata.agent_id = Some(new_thread.thread_id);
        reservation.commit(agent_metadata.clone());

        if let Some(SessionSource::SubAgent(
            subagent_source @ SubAgentSource::ThreadSpawn {
                parent_thread_id, ..
            },
        )) = notification_source.as_ref()
        {
            let client_metadata = match state.get_thread(*parent_thread_id).await {
                Ok(parent_thread) => {
                    parent_thread
                        .codex
                        .session
                        .app_server_client_metadata()
                        .await
                }
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
            let thread_config = new_thread.thread.codex.thread_config_snapshot().await;
            emit_subagent_session_started(
                &new_thread
                    .thread
                    .codex
                    .session
                    .services
                    .analytics_events_client,
                client_metadata,
                new_thread.thread_id,
                /*parent_thread_id*/ None,
                thread_config,
                subagent_source.clone(),
            );
        }

        // Notify a new thread has been created. This notification will be processed by clients
        // to subscribe or drain this newly created thread.
        // TODO(jif) add helper for drain
        state.notify_thread_created(new_thread.thread_id);

        self.persist_thread_spawn_edge_for_source(
            new_thread.thread_id,
            notification_source.as_ref(),
        )
        .await;

        self.send_input(new_thread.thread_id, initial_operation)
            .await?;
        if !new_thread.thread.enabled(Feature::MultiAgentV2) {
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

    async fn spawn_forked_thread(
        &self,
        state: &Arc<ThreadManagerState>,
        config: crate::config::Config,
        session_source: SessionSource,
        options: &SpawnAgentOptions,
        inherited_shell_snapshot: Option<Arc<ShellSnapshot>>,
        inherited_exec_policy: Option<Arc<crate::exec_policy::ExecPolicyManager>>,
    ) -> CodexResult<crate::thread_manager::NewThread> {
        let Some(call_id) = options.fork_parent_spawn_call_id.as_deref() else {
            return Err(CodexErr::Fatal(
                "spawn_agent fork requires a parent spawn call id".to_string(),
            ));
        };
        let default_fork_mode = SpawnAgentForkMode::FullHistory;
        let fork_mode = options.fork_mode.as_ref().unwrap_or(&default_fork_mode);
        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        }) = &session_source
        else {
            return Err(CodexErr::Fatal(
                "spawn_agent fork requires a thread-spawn session source".to_string(),
            ));
        };

        let parent_thread_id = *parent_thread_id;
        let parent_thread = state.get_thread(parent_thread_id).await.ok();
        if let Some(parent_thread) = parent_thread.as_ref() {
            // `record_conversation_items` only queues persistence writes asynchronously.
            // Flush before snapshotting store history for a fork.
            parent_thread.ensure_rollout_materialized().await;
            parent_thread.flush_rollout().await?;
        }

        let parent_history = state
            .read_stored_thread(ReadThreadParams {
                thread_id: parent_thread_id,
                include_archived: true,
                include_history: true,
            })
            .await?
            .history
            .ok_or_else(|| {
                CodexErr::Fatal(format!(
                    "parent thread history unavailable for fork: {parent_thread_id}"
                ))
            })?;

        let mut forked_rollout_items = parent_history.items;
        if let SpawnAgentForkMode::LastNTurns(last_n_turns) = fork_mode {
            forked_rollout_items =
                truncate_rollout_to_last_n_fork_turns(&forked_rollout_items, *last_n_turns);
        }
        // MultiAgentV2 root/subagent usage hints are injected as standalone developer
        // messages at thread start. When forking history, drop hints from the parent
        // so the child gets a fresh hint that matches its own session source/config.
        let multi_agent_v2_usage_hint_texts_to_filter: Vec<String> =
            if let Some(parent_thread) = parent_thread.as_ref() {
                parent_thread
                    .codex
                    .session
                    .configured_multi_agent_v2_usage_hint_texts()
                    .await
            } else if config.features.enabled(Feature::MultiAgentV2) {
                [
                    config.multi_agent_v2.root_agent_usage_hint_text.clone(),
                    config.multi_agent_v2.subagent_usage_hint_text.clone(),
                ]
                .into_iter()
                .flatten()
                .collect()
            } else {
                Vec::new()
            };
        forked_rollout_items.retain(|item| {
            if let RolloutItem::ResponseItem(ResponseItem::Message { role, content, .. }) = item
                && role == "developer"
                && let [ContentItem::InputText { text }] = content.as_slice()
                && multi_agent_v2_usage_hint_texts_to_filter
                    .iter()
                    .any(|usage_hint_text| usage_hint_text == text)
            {
                return false;
            }

            keep_forked_rollout_item(item)
        });

        state
            .fork_thread_with_source(
                config.clone(),
                InitialHistory::Forked(forked_rollout_items),
                self.clone(),
                session_source,
                /*persist_extended_history*/ false,
                inherited_shell_snapshot,
                inherited_exec_policy,
                options.environments.clone(),
            )
            .await
    }

    /// Resume an existing agent thread from a recorded rollout file.
    pub(crate) async fn resume_agent_from_rollout(
        &self,
        config: crate::config::Config,
        thread_id: ThreadId,
        session_source: SessionSource,
    ) -> CodexResult<ThreadId> {
        let root_depth = thread_spawn_depth(&session_source).unwrap_or(0);
        let resumed_thread_id = Box::pin(self.resume_single_agent_from_rollout(
            config.clone(),
            thread_id,
            session_source,
        ))
        .await?;
        let state = self.upgrade()?;
        let agent_graph_store = state.agent_graph_store();

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
                    match self
                        .resume_single_agent_from_rollout(
                            config.clone(),
                            child_thread_id,
                            child_session_source,
                        )
                        .await
                    {
                        Ok(_) => true,
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
        mut config: crate::config::Config,
        thread_id: ThreadId,
        session_source: SessionSource,
    ) -> CodexResult<ThreadId> {
        if let SessionSource::SubAgent(SubAgentSource::ThreadSpawn { depth, .. }) = &session_source
            && *depth >= config.agent_max_depth
            && !config.features.enabled(Feature::MultiAgentV2)
        {
            let _ = config.features.disable(Feature::SpawnCsv);
            let _ = config.features.disable(Feature::Collab);
        }
        let state = self.upgrade()?;
        let state_db_ctx = state.state_db();
        let mut reservation = self.state.reserve_spawn_slot(config.agent_max_threads)?;
        let (session_source, agent_metadata) = match session_source {
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth,
                agent_path,
                agent_role: _,
                agent_nickname: _,
            }) => {
                let state_db_ctx = state.state_db();
                let (resumed_agent_nickname, resumed_agent_role) =
                    match state_db_ctx.get_thread(thread_id).await {
                        Ok(Some(metadata)) => (metadata.agent_nickname, metadata.agent_role),
                        Ok(None) | Err(_) => (None, None),
                    };
                self.prepare_thread_spawn(
                    &mut reservation,
                    &config,
                    parent_thread_id,
                    depth,
                    agent_path,
                    resumed_agent_role,
                    resumed_agent_nickname,
                )?
            }
            other => (other, AgentMetadata::default()),
        };
        let notification_source = session_source.clone();
        let inherited_shell_snapshot = self
            .inherited_shell_snapshot_for_source(&state, Some(&session_source))
            .await;
        let inherited_exec_policy = self
            .inherited_exec_policy_for_source(&state, Some(&session_source), &config)
            .await;
        let stored_thread = state
            .read_stored_thread(ReadThreadParams {
                thread_id,
                include_archived: true,
                include_history: true,
            })
            .await?;
        let history = stored_thread
            .history
            .ok_or_else(|| CodexErr::ThreadNotFound(thread_id))?
            .items;

        let resumed_thread = state
            .resume_thread_with_history_with_source(ResumeThreadWithHistoryOptions {
                config: config.clone(),
                initial_history: InitialHistory::Resumed(ResumedHistory {
                    conversation_id: thread_id,
                    history,
                    rollout_path: stored_thread.rollout_path,
                }),
                agent_control: self.clone(),
                session_source,
                inherited_shell_snapshot,
                inherited_exec_policy,
            })
            .await?;
        let mut agent_metadata = agent_metadata;
        agent_metadata.agent_id = Some(resumed_thread.thread_id);
        reservation.commit(agent_metadata.clone());
        // Resumed threads are re-registered in-memory and need the same listener
        // attachment path as freshly spawned threads.
        state.notify_thread_created(resumed_thread.thread_id);
        if !resumed_thread.thread.enabled(Feature::MultiAgentV2) {
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
            resumed_thread.thread_id,
            Some(&notification_source),
        )
        .await;

        Ok(resumed_thread.thread_id)
    }

    /// Send rich user input items to an existing agent thread.
    pub(crate) async fn send_input(
        &self,
        agent_id: ThreadId,
        initial_operation: Op,
    ) -> CodexResult<String> {
        let last_task_message = render_input_preview(&initial_operation);
        let state = self.upgrade()?;
        let result = self
            .handle_thread_request_result(
                agent_id,
                &state,
                state.send_op(agent_id, initial_operation).await,
            )
            .await;
        if result.is_ok() {
            self.state
                .update_last_task_message(agent_id, last_task_message);
        }
        result
    }

    /// Append a prebuilt message to an existing agent thread outside the normal user-input path.
    #[allow(dead_code)]
    #[cfg(test)]
    pub(crate) async fn append_message(
        &self,
        agent_id: ThreadId,
        message: ResponseItem,
    ) -> CodexResult<String> {
        let state = self.upgrade()?;
        self.handle_thread_request_result(
            agent_id,
            &state,
            state.append_message(agent_id, message).await,
        )
        .await
    }

    pub(crate) async fn send_inter_agent_communication(
        &self,
        agent_id: ThreadId,
        communication: InterAgentCommunication,
    ) -> CodexResult<String> {
        let last_task_message = communication.content.clone();
        let state = self.upgrade()?;
        let result = self
            .handle_thread_request_result(
                agent_id,
                &state,
                state
                    .send_op(agent_id, Op::InterAgentCommunication { communication })
                    .await,
            )
            .await;
        if result.is_ok() {
            self.state
                .update_last_task_message(agent_id, last_task_message);
        }
        result
    }

    /// Interrupt the current task for an existing agent thread.
    pub(crate) async fn interrupt_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.upgrade()?;
        state.send_op(agent_id, Op::Interrupt).await
    }

    async fn handle_thread_request_result(
        &self,
        agent_id: ThreadId,
        state: &Arc<ThreadManagerState>,
        result: CodexResult<String>,
    ) -> CodexResult<String> {
        if matches!(result, Err(CodexErr::InternalAgentDied)) {
            let _ = state.remove_thread(&agent_id).await;
            self.state.release_spawned_thread(agent_id);
        }
        result
    }

    /// Submit a shutdown request for a live agent without marking it explicitly closed in
    /// persisted spawn-edge state.
    pub(crate) async fn shutdown_live_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.upgrade()?;
        let result = if let Ok(thread) = state.get_thread(agent_id).await {
            thread.codex.session.ensure_rollout_materialized().await;
            thread.codex.session.flush_rollout().await;
            if matches!(thread.agent_status().await, AgentStatus::Shutdown) {
                Ok(String::new())
            } else {
                state.send_op(agent_id, Op::Shutdown {}).await
            }
        } else {
            state.send_op(agent_id, Op::Shutdown {}).await
        };
        let _ = state.remove_thread(&agent_id).await;
        self.state.release_spawned_thread(agent_id);
        result
    }

    /// Mark `agent_id` as explicitly closed in persisted spawn-edge state, then shut down the
    /// agent and any live descendants reached from the in-memory tree.
    pub(crate) async fn close_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.upgrade()?;
        if let Err(err) = state
            .agent_graph_store()
            .set_thread_spawn_edge_status(
                agent_id,
                codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed,
            )
            .await
        {
            warn!("failed to persist thread-spawn edge status for {agent_id}: {err}");
        }
        Box::pin(self.shutdown_agent_tree(agent_id)).await
    }

    /// Shut down `agent_id` and any live descendants reachable from the in-memory spawn tree.
    async fn shutdown_agent_tree(&self, agent_id: ThreadId) -> CodexResult<String> {
        let descendant_ids = self.live_thread_spawn_descendants(agent_id).await?;
        let result = self.shutdown_live_agent(agent_id).await;
        for descendant_id in descendant_ids {
            match self.shutdown_live_agent(descendant_id).await {
                Ok(_) | Err(CodexErr::ThreadNotFound(_)) | Err(CodexErr::InternalAgentDied) => {}
                Err(err) => return Err(err),
            }
        }
        result
    }

    /// Fetch the last known status for `agent_id`, returning `NotFound` when unavailable.
    pub(crate) async fn get_status(&self, agent_id: ThreadId) -> AgentStatus {
        let Ok(state) = self.upgrade() else {
            // No agent available if upgrade fails.
            return AgentStatus::NotFound;
        };
        let Ok(thread) = state.get_thread(agent_id).await else {
            return AgentStatus::NotFound;
        };
        thread.agent_status().await
    }

    pub(crate) fn register_session_root(
        &self,
        current_thread_id: ThreadId,
        current_session_source: &SessionSource,
    ) {
        if thread_spawn_parent_thread_id(current_session_source).is_none() {
            self.state.register_root_thread(current_thread_id);
        }
    }

    pub(crate) fn get_agent_metadata(&self, agent_id: ThreadId) -> Option<AgentMetadata> {
        self.state.agent_metadata_for_thread(agent_id)
    }

    pub(crate) async fn get_subagent_inventory_info(
        &self,
        thread_id: ThreadId,
    ) -> Option<SubAgentInventoryInfo> {
        let state = self.upgrade().ok()?;
        let thread = state.get_thread(thread_id).await.ok()?;
        let snapshot = thread.config_snapshot().await;
        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            agent_nickname,
            agent_role,
            ..
        }) = snapshot.session_source
        else {
            return None;
        };

        Some(SubAgentInventoryInfo {
            nickname: agent_nickname,
            role: agent_role,
            status: thread.agent_status().await,
            effective_model: Some(snapshot.model),
            effective_reasoning_effort: snapshot.reasoning_effort,
            effective_model_provider_id: snapshot.model_provider_id,
            identity_source: SUBAGENT_IDENTITY_SOURCE_THREAD_CONFIG_SNAPSHOT.to_string(),
        })
    }

    pub(crate) async fn get_agent_config_snapshot(
        &self,
        agent_id: ThreadId,
    ) -> Option<ThreadConfigSnapshot> {
        let Ok(state) = self.upgrade() else {
            return None;
        };
        let Ok(thread) = state.get_thread(agent_id).await else {
            return None;
        };
        Some(thread.config_snapshot().await)
    }

    pub(crate) async fn resolve_agent_reference(
        &self,
        _current_thread_id: ThreadId,
        current_session_source: &SessionSource,
        agent_reference: &str,
    ) -> CodexResult<ThreadId> {
        let current_agent_path = current_session_source
            .get_agent_path()
            .unwrap_or_else(AgentPath::root);
        let agent_path = current_agent_path
            .resolve(agent_reference)
            .map_err(CodexErr::UnsupportedOperation)?;
        if let Some(thread_id) = self.state.agent_id_for_path(&agent_path) {
            return Ok(thread_id);
        }
        Err(CodexErr::UnsupportedOperation(format!(
            "live agent path `{}` not found",
            agent_path.as_str()
        )))
    }

    /// Subscribe to status updates for `agent_id`, yielding the latest value and changes.
    pub(crate) async fn subscribe_status(
        &self,
        agent_id: ThreadId,
    ) -> CodexResult<watch::Receiver<AgentStatus>> {
        let state = self.upgrade()?;
        let thread = state.get_thread(agent_id).await?;
        Ok(thread.subscribe_status())
    }

    pub(crate) async fn format_environment_context_subagents(
        &self,
        parent_thread_id: ThreadId,
    ) -> String {
        let Ok(agents) = self.open_thread_spawn_children(parent_thread_id).await else {
            return String::new();
        };

        agents
            .into_iter()
            .map(|(thread_id, metadata)| {
                let reference = metadata
                    .agent_path
                    .as_ref()
                    .map(|agent_path| agent_path.name().to_string())
                    .unwrap_or_else(|| thread_id.to_string());
                format_subagent_context_line(reference.as_str(), metadata.agent_nickname.as_deref())
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(crate) async fn list_agents(
        &self,
        current_session_source: &SessionSource,
        path_prefix: Option<&str>,
    ) -> CodexResult<Vec<ListedAgent>> {
        let state = self.upgrade()?;
        let live_children_by_parent = self.live_thread_spawn_children().await?;
        let resolved_prefix = path_prefix
            .map(|prefix| {
                current_session_source
                    .get_agent_path()
                    .unwrap_or_else(AgentPath::root)
                    .resolve(prefix)
                    .map_err(CodexErr::UnsupportedOperation)
            })
            .transpose()?;

        let mut live_agents = self.state.live_agents();
        live_agents.sort_by(|left, right| {
            left.agent_path
                .as_deref()
                .unwrap_or_default()
                .cmp(right.agent_path.as_deref().unwrap_or_default())
                .then_with(|| {
                    left.agent_id
                        .map(|id| id.to_string())
                        .unwrap_or_default()
                        .cmp(&right.agent_id.map(|id| id.to_string()).unwrap_or_default())
                })
        });

        let root_path = AgentPath::root();
        let mut listed_rows = Vec::with_capacity(live_agents.len().saturating_add(1));
        let mut status_by_thread_id = HashMap::<ThreadId, AgentStatus>::new();
        if let Some(root_thread_id) = self.state.agent_id_for_path(&root_path)
            && let Ok(root_thread) = state.get_thread(root_thread_id).await
        {
            let root_status = root_thread.agent_status().await;
            status_by_thread_id.insert(root_thread_id, root_status.clone());
            if resolved_prefix
                .as_ref()
                .is_none_or(|prefix| agent_matches_prefix(Some(&root_path), prefix))
            {
                listed_rows.push((
                    root_thread_id,
                    root_path.to_string(),
                    root_status,
                    Some(ROOT_LAST_TASK_MESSAGE.to_string()),
                ));
            }
        }

        for metadata in live_agents {
            let Some(thread_id) = metadata.agent_id else {
                continue;
            };
            let Ok(thread) = state.get_thread(thread_id).await else {
                continue;
            };
            let agent_status = thread.agent_status().await;
            status_by_thread_id.insert(thread_id, agent_status.clone());
            if resolved_prefix
                .as_ref()
                .is_some_and(|prefix| !agent_matches_prefix(metadata.agent_path.as_ref(), prefix))
            {
                continue;
            }
            let agent_name = metadata
                .agent_path
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| thread_id.to_string());
            let last_task_message = metadata.last_task_message.clone();
            listed_rows.push((thread_id, agent_name, agent_status, last_task_message));
        }

        let mut active_descendant_counts = HashMap::<ThreadId, usize>::new();
        let agents = listed_rows
            .into_iter()
            .map(|(thread_id, agent_name, agent_status, last_task_message)| {
                let active_subagent_count = compute_active_live_descendant_count(
                    thread_id,
                    &live_children_by_parent,
                    &status_by_thread_id,
                    &mut active_descendant_counts,
                );
                ListedAgent {
                    agent_name,
                    agent_status,
                    last_task_message,
                    has_active_subagents: active_subagent_count > 0,
                    active_subagent_count,
                }
            })
            .collect::<Vec<_>>();

        Ok(agents)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn inspect_agent_tree(
        &self,
        current_thread_id: ThreadId,
        current_session_source: &SessionSource,
        target: Option<&str>,
        agent_roots: Option<&[String]>,
        scope: AgentTreeScope,
        max_depth: usize,
        max_agents: usize,
    ) -> CodexResult<AgentTreeInspection> {
        let state = self.upgrade()?;
        let current_thread = state.get_thread(current_thread_id).await?;
        let state_db_ctx = current_thread.state_db();
        let root_live_thread_id = self
            .state
            .agent_id_for_path(&AgentPath::root())
            .unwrap_or(current_thread_id);
        let target_path = target
            .map(|reference| {
                current_session_source
                    .get_agent_path()
                    .unwrap_or_else(AgentPath::root)
                    .resolve(reference)
                    .map_err(CodexErr::UnsupportedOperation)
            })
            .transpose()?;
        let filter_base_path = target_path.clone().unwrap_or_else(|| {
            current_session_source
                .get_agent_path()
                .unwrap_or_else(AgentPath::root)
        });

        if !matches!(scope, AgentTreeScope::Live) && state_db_ctx.is_none() {
            return Err(inspect_agent_tree_state_db_unavailable());
        }

        let (tree_root_thread_id, tree_root_session_state) = match target_path.as_ref() {
            Some(target_path) => {
                if let Some(thread_id) = self.state.agent_id_for_path(target_path) {
                    (thread_id, AgentSessionState::Live)
                } else {
                    let Some(state_db_ctx) = state_db_ctx.as_ref() else {
                        return Err(CodexErr::UnsupportedOperation(format!(
                            "agent path `{}` not found in the live tree",
                            target_path.as_str()
                        )));
                    };
                    let thread_id = if target_path.is_root() {
                        Some(root_live_thread_id)
                    } else {
                        state_db_ctx
                            .find_thread_spawn_descendant_by_path(
                                root_live_thread_id,
                                target_path.as_str(),
                            )
                            .await
                            .map_err(|err| {
                                CodexErr::Fatal(format!(
                                    "failed to inspect persisted agent path `{}`: {err}",
                                    target_path.as_str()
                                ))
                            })?
                    }
                    .ok_or_else(|| {
                        CodexErr::UnsupportedOperation(format!(
                            "agent path `{}` not found",
                            target_path.as_str()
                        ))
                    })?;
                    (thread_id, AgentSessionState::Stale)
                }
            }
            None => (current_thread_id, AgentSessionState::Live),
        };
        let tree_root_name = match tree_root_session_state {
            AgentSessionState::Live => self
                .state
                .agent_metadata_for_thread(tree_root_thread_id)
                .and_then(|metadata| metadata.agent_path.map(|agent_path| agent_path.to_string()))
                .unwrap_or_else(|| tree_root_thread_id.to_string()),
            AgentSessionState::Stale => {
                let Some(state_db_ctx) = state_db_ctx.as_ref() else {
                    return Err(inspect_agent_tree_state_db_unavailable());
                };
                state_db_ctx
                    .get_thread(tree_root_thread_id)
                    .await
                    .map_err(|err| {
                        CodexErr::Fatal(format!(
                            "failed to inspect stale agent metadata for {tree_root_thread_id}: {err}"
                        ))
                    })?
                    .and_then(|metadata| metadata.agent_path)
                    .unwrap_or_else(|| tree_root_thread_id.to_string())
            }
        };
        let agent_roots_applied = agent_roots
            .map(|references| {
                references
                    .iter()
                    .map(|reference| {
                        filter_base_path
                            .resolve(reference)
                            .map_err(CodexErr::UnsupportedOperation)
                    })
                    .collect::<CodexResult<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default();
        for agent_root in &agent_roots_applied {
            if !agent_name_is_same_or_descendant_of(agent_root.as_str(), tree_root_name.as_str()) {
                return Err(CodexErr::UnsupportedOperation(format!(
                    "agent_roots entry `{}` is outside inspected subtree `{}`",
                    agent_root.as_str(),
                    tree_root_name
                )));
            }
        }

        let live_children_by_parent = if matches!(scope, AgentTreeScope::Stale) {
            None
        } else {
            Some(self.live_thread_spawn_children().await?)
        };
        let mut queue = VecDeque::from([(tree_root_thread_id, tree_root_session_state, 0usize)]);
        let mut depth_by_thread_id = HashMap::<ThreadId, usize>::new();
        let mut tree_children = HashMap::<ThreadId, Vec<ThreadId>>::new();
        let mut tree_records = HashMap::<ThreadId, AgentTreeRecord>::new();

        while let Some((thread_id, session_state, depth)) = queue.pop_front() {
            if tree_records.contains_key(&thread_id) {
                continue;
            }

            let record = self
                .load_agent_tree_record(&state, state_db_ctx.as_ref(), thread_id, session_state)
                .await?;
            depth_by_thread_id.insert(thread_id, depth);

            let child_states = self
                .tree_child_session_states(
                    live_children_by_parent.as_ref(),
                    state_db_ctx.as_ref(),
                    thread_id,
                    scope,
                )
                .await?;
            let mut child_ids = child_states.keys().copied().collect::<Vec<_>>();
            child_ids.sort_by_key(std::string::ToString::to_string);
            tree_children.insert(thread_id, child_ids.clone());
            tree_records.insert(thread_id, record);

            for child_id in child_ids {
                if let Some(child_state) = child_states.get(&child_id).copied() {
                    queue.push_back((child_id, child_state, depth.saturating_add(1)));
                }
            }
        }

        for child_ids in tree_children.values_mut() {
            child_ids.sort_by(|left, right| {
                let left_name = tree_records
                    .get(left)
                    .map(|record| record.agent_name.as_str())
                    .unwrap_or_default();
                let right_name = tree_records
                    .get(right)
                    .map(|record| record.agent_name.as_str())
                    .unwrap_or_default();
                left_name
                    .cmp(right_name)
                    .then_with(|| left.to_string().cmp(&right.to_string()))
            });
        }

        let mut descendant_counts = HashMap::<ThreadId, usize>::new();
        compute_descendant_counts(tree_root_thread_id, &tree_children, &mut descendant_counts);

        let mut ordered_thread_ids = Vec::with_capacity(tree_records.len());
        let mut stack = vec![tree_root_thread_id];
        while let Some(thread_id) = stack.pop() {
            ordered_thread_ids.push(thread_id);
            if let Some(children) = tree_children.get(&thread_id) {
                for child_id in children.iter().rev().copied() {
                    stack.push(child_id);
                }
            }
        }

        let filtered_thread_ids = ordered_thread_ids
            .into_iter()
            .filter(|thread_id| {
                agent_roots_applied.is_empty()
                    || tree_records.get(thread_id).is_some_and(|record| {
                        agent_roots_applied.iter().any(|agent_root| {
                            agent_name_is_same_or_descendant_of(
                                record.agent_name.as_str(),
                                agent_root.as_str(),
                            )
                        })
                    })
            })
            .collect::<Vec<_>>();

        let mut summary = AgentTreeSummary {
            total_agents: filtered_thread_ids.len(),
            live_agents: 0,
            stale_agents: 0,
            pending_init_agents: 0,
            running_agents: 0,
            interrupted_agents: 0,
            completed_agents: 0,
            errored_agents: 0,
            shutdown_agents: 0,
            not_found_agents: 0,
        };

        for thread_id in &filtered_thread_ids {
            let Some(record) = tree_records.get(thread_id) else {
                continue;
            };
            match record.session_state {
                AgentSessionState::Live => summary.live_agents += 1,
                AgentSessionState::Stale => summary.stale_agents += 1,
            }
            match record.agent_status.as_ref() {
                Some(AgentStatus::PendingInit) => summary.pending_init_agents += 1,
                Some(AgentStatus::Running) => summary.running_agents += 1,
                Some(AgentStatus::Interrupted) => summary.interrupted_agents += 1,
                Some(AgentStatus::Completed { .. }) => summary.completed_agents += 1,
                Some(AgentStatus::Errored { .. }) => summary.errored_agents += 1,
                Some(AgentStatus::Shutdown) => summary.shutdown_agents += 1,
                Some(AgentStatus::NotFound) => summary.not_found_agents += 1,
                None => {}
            }
        }

        let filtered_count = filtered_thread_ids.len();
        let within_depth = filtered_thread_ids
            .into_iter()
            .filter(|thread_id| {
                depth_by_thread_id
                    .get(thread_id)
                    .copied()
                    .unwrap_or_default()
                    <= max_depth
            })
            .collect::<Vec<_>>();
        let within_depth_count = within_depth.len();
        let truncated = filtered_count > within_depth_count || within_depth_count > max_agents;
        let agents = within_depth
            .into_iter()
            .take(max_agents)
            .filter_map(|thread_id| {
                let record = tree_records.get(&thread_id)?;
                Some(AgentTreeNode {
                    agent_name: record.agent_name.clone(),
                    depth: depth_by_thread_id
                        .get(&thread_id)
                        .copied()
                        .unwrap_or_default(),
                    session_state: record.session_state,
                    agent_status: record.agent_status.clone(),
                    nickname: record.nickname.clone(),
                    role: record.role.clone(),
                    direct_child_count: tree_children.get(&thread_id).map_or(0, Vec::len),
                    descendant_count: descendant_counts.get(&thread_id).copied().unwrap_or(0),
                    last_task_message_preview: record.last_task_message_preview.clone(),
                })
            })
            .collect::<Vec<_>>();
        let root_agent_name = tree_records
            .get(&tree_root_thread_id)
            .map(|record| record.agent_name.clone())
            .unwrap_or_else(|| tree_root_thread_id.to_string());

        Ok(AgentTreeInspection {
            root_agent_name,
            scope_applied: scope,
            agent_roots_applied: agent_roots_applied
                .into_iter()
                .map(|agent_root| agent_root.to_string())
                .collect(),
            max_depth_applied: max_depth,
            max_agents_applied: max_agents,
            truncated,
            summary,
            agents,
        })
    }

    /// Starts a detached watcher for sub-agents spawned from another thread.
    ///
    /// This is only enabled for `SubAgentSource::ThreadSpawn`, where a parent thread exists and
    /// can receive completion notifications.
    fn maybe_start_completion_watcher(
        &self,
        child_thread_id: ThreadId,
        session_source: Option<SessionSource>,
        child_reference: String,
        child_agent_path: Option<AgentPath>,
    ) {
        let Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        })) = session_source
        else {
            return;
        };
        let control = self.clone();
        tokio::spawn(async move {
            let status = match control.subscribe_status(child_thread_id).await {
                Ok(mut status_rx) => {
                    let mut status = status_rx.borrow().clone();
                    while !is_final(&status) {
                        if status_rx.changed().await.is_err() {
                            status = control.get_status(child_thread_id).await;
                            break;
                        }
                        status = status_rx.borrow().clone();
                    }
                    status
                }
                Err(_) => control.get_status(child_thread_id).await,
            };
            if !is_final(&status) {
                return;
            }

            let Ok(state) = control.upgrade() else {
                return;
            };
            let child_thread = state.get_thread(child_thread_id).await.ok();
            let message = format_subagent_notification_message(child_reference.as_str(), &status);
            if child_agent_path.is_some()
                && child_thread
                    .as_ref()
                    .map(|thread| thread.enabled(Feature::MultiAgentV2))
                    .unwrap_or(true)
            {
                let Some(child_agent_path) = child_agent_path.clone() else {
                    return;
                };
                let Some(parent_agent_path) = child_agent_path
                    .as_str()
                    .rsplit_once('/')
                    .and_then(|(parent, _)| AgentPath::try_from(parent).ok())
                else {
                    return;
                };
                let communication = InterAgentCommunication::new(
                    child_agent_path,
                    parent_agent_path,
                    Vec::new(),
                    message,
                    /*trigger_turn*/ false,
                );
                let _ = control
                    .send_inter_agent_communication(parent_thread_id, communication)
                    .await;
                return;
            }
            let Ok(parent_thread) = state.get_thread(parent_thread_id).await else {
                return;
            };
            parent_thread
                .inject_user_message_without_turn(message)
                .await;
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_thread_spawn(
        &self,
        reservation: &mut crate::agent::registry::SpawnReservation,
        config: &crate::config::Config,
        parent_thread_id: ThreadId,
        depth: i32,
        agent_path: Option<AgentPath>,
        agent_role: Option<String>,
        preferred_agent_nickname: Option<String>,
    ) -> CodexResult<(SessionSource, AgentMetadata)> {
        if depth == 1 {
            self.state.register_root_thread(parent_thread_id);
        }
        if let Some(agent_path) = agent_path.as_ref() {
            reservation.reserve_agent_path(agent_path)?;
        }
        let candidate_names = agent_nickname_candidates(config, agent_role.as_deref());
        let candidate_name_refs: Vec<&str> = candidate_names.iter().map(String::as_str).collect();
        let agent_nickname = Some(reservation.reserve_agent_nickname_with_preference(
            &candidate_name_refs,
            preferred_agent_nickname.as_deref(),
        )?);
        let session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth,
            agent_path: agent_path.clone(),
            agent_nickname: agent_nickname.clone(),
            agent_role: agent_role.clone(),
        });
        let agent_metadata = AgentMetadata {
            agent_id: None,
            agent_path,
            agent_nickname,
            agent_role,
            last_task_message: None,
        };
        Ok((session_source, agent_metadata))
    }

    fn upgrade(&self) -> CodexResult<Arc<ThreadManagerState>> {
        self.manager
            .upgrade()
            .ok_or_else(|| CodexErr::UnsupportedOperation("thread manager dropped".to_string()))
    }

    async fn inherited_shell_snapshot_for_source(
        &self,
        state: &Arc<ThreadManagerState>,
        session_source: Option<&SessionSource>,
    ) -> Option<Arc<ShellSnapshot>> {
        let Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        })) = session_source
        else {
            return None;
        };

        let parent_thread = state.get_thread(*parent_thread_id).await.ok()?;
        parent_thread.codex.session.user_shell().shell_snapshot()
    }

    async fn inherited_exec_policy_for_source(
        &self,
        state: &Arc<ThreadManagerState>,
        session_source: Option<&SessionSource>,
        child_config: &crate::config::Config,
    ) -> Option<Arc<crate::exec_policy::ExecPolicyManager>> {
        let Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        })) = session_source
        else {
            return None;
        };

        let parent_thread = state.get_thread(*parent_thread_id).await.ok()?;
        let parent_config = parent_thread.codex.session.get_config().await;
        if !crate::exec_policy::child_uses_parent_exec_policy(&parent_config, child_config) {
            return None;
        }

        Some(Arc::clone(
            &parent_thread.codex.session.services.exec_policy,
        ))
    }

    async fn open_thread_spawn_children(
        &self,
        parent_thread_id: ThreadId,
    ) -> CodexResult<Vec<(ThreadId, AgentMetadata)>> {
        let mut children_by_parent = self.live_thread_spawn_children().await?;
        Ok(children_by_parent
            .remove(&parent_thread_id)
            .unwrap_or_default())
    }

    async fn live_thread_spawn_children(
        &self,
    ) -> CodexResult<HashMap<ThreadId, Vec<(ThreadId, AgentMetadata)>>> {
        let state = self.upgrade()?;
        let mut children_by_parent = HashMap::<ThreadId, Vec<(ThreadId, AgentMetadata)>>::new();

        for thread_id in state.list_thread_ids().await {
            let Ok(thread) = state.get_thread(thread_id).await else {
                continue;
            };
            let snapshot = thread.config_snapshot().await;
            let Some(parent_thread_id) = thread_spawn_parent_thread_id(&snapshot.session_source)
            else {
                continue;
            };
            children_by_parent
                .entry(parent_thread_id)
                .or_default()
                .push((
                    thread_id,
                    self.state
                        .agent_metadata_for_thread(thread_id)
                        .unwrap_or(AgentMetadata {
                            agent_id: Some(thread_id),
                            ..Default::default()
                        }),
                ));
        }

        for children in children_by_parent.values_mut() {
            children.sort_by(|left, right| {
                left.1
                    .agent_path
                    .as_deref()
                    .unwrap_or_default()
                    .cmp(right.1.agent_path.as_deref().unwrap_or_default())
                    .then_with(|| left.0.to_string().cmp(&right.0.to_string()))
            });
        }

        Ok(children_by_parent)
    }

    async fn persist_thread_spawn_edge_for_source(
        &self,
        child_thread_id: ThreadId,
        session_source: Option<&SessionSource>,
    ) {
        let Some(parent_thread_id) = session_source.and_then(thread_spawn_parent_thread_id) else {
            return;
        };
        let Ok(state) = self.upgrade() else {
            return;
        };
        if let Err(err) = state
            .agent_graph_store()
            .upsert_thread_spawn_edge(
                parent_thread_id,
                child_thread_id,
                codex_agent_graph_store::ThreadSpawnEdgeStatus::Open,
            )
            .await
        {
            warn!("failed to persist thread-spawn edge: {err}");
        }
    }

    #[allow(dead_code)]
    #[cfg(test)]
    /// Enumerate persisted descendants and filter them by the desired spawn-edge status.
    pub(crate) async fn list_persisted_subagent_descendants(
        &self,
        root_thread_id: ThreadId,
        status: DirectionalThreadSpawnEdgeStatus,
    ) -> CodexResult<Vec<ThreadId>> {
        let state = self.upgrade()?;
        let thread = state.get_thread(root_thread_id).await?;
        let Some(state_db_ctx) = thread.state_db() else {
            return Ok(Vec::new());
        };
        state_db_ctx
            .list_thread_spawn_descendants_with_status(root_thread_id, status)
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to list persisted thread-spawn descendants for {root_thread_id}: {err}"
                ))
            })
    }

    async fn live_thread_spawn_descendants(
        &self,
        root_thread_id: ThreadId,
    ) -> CodexResult<Vec<ThreadId>> {
        let mut children_by_parent = self.live_thread_spawn_children().await?;
        let mut descendants = Vec::new();
        let mut stack = children_by_parent
            .remove(&root_thread_id)
            .unwrap_or_default()
            .into_iter()
            .map(|(child_thread_id, _)| child_thread_id)
            .rev()
            .collect::<Vec<_>>();

        while let Some(thread_id) = stack.pop() {
            descendants.push(thread_id);
            if let Some(children) = children_by_parent.remove(&thread_id) {
                for (child_thread_id, _) in children.into_iter().rev() {
                    stack.push(child_thread_id);
                }
            }
        }

        Ok(descendants)
    }

    pub(crate) async fn list_live_agent_subtree_thread_ids(
        &self,
        root_thread_id: ThreadId,
    ) -> CodexResult<Vec<ThreadId>> {
        self.live_thread_spawn_descendants(root_thread_id).await
    }

    async fn load_agent_tree_record(
        &self,
        state: &Arc<ThreadManagerState>,
        state_db_ctx: Option<&state_db::StateDbHandle>,
        thread_id: ThreadId,
        session_state: AgentSessionState,
    ) -> CodexResult<AgentTreeRecord> {
        match session_state {
            AgentSessionState::Live => {
                let thread = state.get_thread(thread_id).await?;
                let metadata =
                    self.state
                        .agent_metadata_for_thread(thread_id)
                        .unwrap_or(AgentMetadata {
                            agent_id: Some(thread_id),
                            ..Default::default()
                        });
                let last_task_message_preview =
                    if metadata.agent_path.as_ref().is_some_and(AgentPath::is_root) {
                        Some(ROOT_LAST_TASK_MESSAGE.to_string())
                    } else {
                        metadata
                            .last_task_message
                            .as_deref()
                            .map(preview_agent_message)
                    };

                Ok(AgentTreeRecord {
                    agent_name: metadata
                        .agent_path
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_else(|| thread_id.to_string()),
                    session_state,
                    agent_status: Some(thread.agent_status().await),
                    nickname: metadata.agent_nickname,
                    role: metadata.agent_role,
                    last_task_message_preview,
                })
            }
            AgentSessionState::Stale => {
                let Some(state_db_ctx) = state_db_ctx else {
                    return Err(inspect_agent_tree_state_db_unavailable());
                };
                let metadata = state_db_ctx
                    .get_thread(thread_id)
                    .await
                    .map_err(|err| {
                        CodexErr::Fatal(format!(
                            "failed to inspect stale agent metadata for {thread_id}: {err}"
                        ))
                    })?
                    .ok_or_else(|| {
                        CodexErr::UnsupportedOperation(format!(
                            "stale agent metadata for {thread_id} is unavailable"
                        ))
                    })?;

                Ok(AgentTreeRecord {
                    agent_name: metadata.agent_path.unwrap_or_else(|| thread_id.to_string()),
                    session_state,
                    agent_status: None,
                    nickname: metadata.agent_nickname,
                    role: metadata.agent_role,
                    last_task_message_preview: None,
                })
            }
        }
    }

    async fn tree_child_session_states(
        &self,
        live_children_by_parent: Option<&HashMap<ThreadId, Vec<(ThreadId, AgentMetadata)>>>,
        state_db_ctx: Option<&state_db::StateDbHandle>,
        parent_thread_id: ThreadId,
        scope: AgentTreeScope,
    ) -> CodexResult<HashMap<ThreadId, AgentSessionState>> {
        let mut child_states = HashMap::<ThreadId, AgentSessionState>::new();

        if !matches!(scope, AgentTreeScope::Stale)
            && let Some(live_children_by_parent) = live_children_by_parent
            && let Some(children) = live_children_by_parent.get(&parent_thread_id)
        {
            for (child_thread_id, _) in children {
                child_states.insert(*child_thread_id, AgentSessionState::Live);
            }
        }

        if !matches!(scope, AgentTreeScope::Live) {
            let Some(state_db_ctx) = state_db_ctx else {
                return Err(inspect_agent_tree_state_db_unavailable());
            };
            let closed_children = state_db_ctx
                .list_thread_spawn_children_with_status(
                    parent_thread_id,
                    DirectionalThreadSpawnEdgeStatus::Closed,
                )
                .await
                .map_err(|err| {
                    CodexErr::Fatal(format!(
                        "failed to inspect stale child agents for {parent_thread_id}: {err}"
                    ))
                })?;
            for child_thread_id in closed_children {
                child_states
                    .entry(child_thread_id)
                    .or_insert(AgentSessionState::Stale);
            }
        }

        Ok(child_states)
    }
}

fn thread_spawn_parent_thread_id(session_source: &SessionSource) -> Option<ThreadId> {
    match session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        }) => Some(*parent_thread_id),
        _ => None,
    }
}

fn inspect_agent_tree_state_db_unavailable() -> CodexErr {
    CodexErr::UnsupportedOperation(INSPECT_AGENT_TREE_STATE_DB_UNAVAILABLE_MESSAGE.to_string())
}

fn agent_matches_prefix(agent_path: Option<&AgentPath>, prefix: &AgentPath) -> bool {
    if prefix.is_root() {
        return true;
    }

    agent_path.is_some_and(|agent_path| {
        agent_path == prefix
            || agent_path
                .as_str()
                .strip_prefix(prefix.as_str())
                .is_some_and(|suffix| suffix.starts_with('/'))
    })
}

fn preview_agent_message(message: &str) -> String {
    let mut words = message.split_whitespace();
    let Some(first) = words.next() else {
        return String::new();
    };
    let mut normalized = first.to_string();
    for word in words {
        normalized.push(' ');
        normalized.push_str(word);
    }
    let mut preview = normalized.chars().take(120).collect::<String>();
    if normalized.chars().count() > 120 {
        preview.push('…');
    }
    preview
}

fn compute_descendant_counts(
    thread_id: ThreadId,
    tree_children: &HashMap<ThreadId, Vec<ThreadId>>,
    descendant_counts: &mut HashMap<ThreadId, usize>,
) -> usize {
    if let Some(count) = descendant_counts.get(&thread_id).copied() {
        return count;
    }

    let count = tree_children.get(&thread_id).map_or(0, |children| {
        children.len()
            + children
                .iter()
                .map(|child_thread_id| {
                    compute_descendant_counts(*child_thread_id, tree_children, descendant_counts)
                })
                .sum::<usize>()
    });
    descendant_counts.insert(thread_id, count);
    count
}

fn compute_active_live_descendant_count(
    thread_id: ThreadId,
    live_children_by_parent: &HashMap<ThreadId, Vec<(ThreadId, AgentMetadata)>>,
    status_by_thread_id: &HashMap<ThreadId, AgentStatus>,
    active_descendant_counts: &mut HashMap<ThreadId, usize>,
) -> usize {
    if let Some(count) = active_descendant_counts.get(&thread_id).copied() {
        return count;
    }

    let count = live_children_by_parent
        .get(&thread_id)
        .map_or(0, |children| {
            children
                .iter()
                .map(|(child_thread_id, _)| {
                    let child_is_active = status_by_thread_id
                        .get(child_thread_id)
                        .is_some_and(|status| !is_final(status));
                    usize::from(child_is_active)
                        + compute_active_live_descendant_count(
                            *child_thread_id,
                            live_children_by_parent,
                            status_by_thread_id,
                            active_descendant_counts,
                        )
                })
                .sum()
        });
    active_descendant_counts.insert(thread_id, count);
    count
}

fn agent_name_is_same_or_descendant_of(agent_name: &str, parent_name: &str) -> bool {
    agent_name == parent_name
        || agent_name
            .strip_prefix(parent_name)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

pub(crate) fn render_input_preview(initial_operation: &Op) -> String {
    match initial_operation {
        Op::UserInput { items, .. } => items
            .iter()
            .map(|item| match item {
                UserInput::Text { text, .. } => text.clone(),
                UserInput::Image { .. } => "[image]".to_string(),
                UserInput::LocalImage { path } => format!("[local_image:{}]", path.display()),
                UserInput::Skill { name, path } => format!("[skill:${name}]({})", path.display()),
                UserInput::Mention { name, path } => format!("[mention:${name}]({path})"),
                _ => "[input]".to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Op::InterAgentCommunication { communication } => communication.content.clone(),
        _ => String::new(),
    }
}

fn thread_spawn_depth(session_source: &SessionSource) -> Option<i32> {
    match session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { depth, .. }) => Some(*depth),
        _ => None,
    }
}
#[cfg(test)]
#[path = "control_tests.rs"]
mod tests;
