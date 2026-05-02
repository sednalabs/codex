use crate::agent::AgentStatus;
use crate::agent::status::is_final;
use crate::config::Config;
use crate::config::DEFAULT_MULTI_AGENT_V2_MIN_WAIT_TIMEOUT_MS;
use crate::config::MAX_MULTI_AGENT_V2_WAIT_TIMEOUT_MS;
use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use codex_features::Feature;
use codex_models_manager::manager::RefreshStrategy;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_protocol::protocol::CollabAgentRef;
use codex_protocol::protocol::CollabAgentStatusEntry;
use codex_protocol::protocol::CollabWaitingCompletionReason;
use codex_protocol::protocol::CollabWaitingEndEvent;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::request_user_input::RequestUserInputArgs;
use codex_protocol::request_user_input::RequestUserInputQuestion;
use codex_protocol::request_user_input::RequestUserInputQuestionOption;
use codex_protocol::user_input::UserInput;
use codex_tools::request_user_input_unavailable_message;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::collections::HashMap;

/// Minimum wait timeout to prevent tight polling loops from burning CPU.
pub(crate) const MIN_WAIT_TIMEOUT_MS: i64 = DEFAULT_MULTI_AGENT_V2_MIN_WAIT_TIMEOUT_MS;
pub(crate) const DEFAULT_WAIT_TIMEOUT_MS: i64 = 30_000;
pub(crate) const MAX_WAIT_TIMEOUT_MS: i64 = MAX_MULTI_AGENT_V2_WAIT_TIMEOUT_MS;
const SPAWN_AGENT_APPROVAL_QUESTION_ID: &str = "spawn_agent_approval";
const SPAWN_AGENT_APPROVAL_ACCEPT_OPTION: &str = "Approve";
const SPAWN_AGENT_APPROVAL_DECLINE_OPTION: &str = "Decline";

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SpawnAgentApproval {
    #[default]
    Auto,
    AskUser,
}

pub(crate) fn function_arguments(payload: ToolPayload) -> Result<String, FunctionCallError> {
    match payload {
        ToolPayload::Function { arguments } => Ok(arguments),
        _ => Err(FunctionCallError::RespondToModel(
            "collab handler received unsupported payload".to_string(),
        )),
    }
}

pub(crate) fn tool_output_json_text<T>(value: &T, tool_name: &str) -> String
where
    T: Serialize,
{
    serde_json::to_string(value).unwrap_or_else(|err| {
        JsonValue::String(format!("failed to serialize {tool_name} result: {err}")).to_string()
    })
}

pub(crate) fn tool_output_response_item<T>(
    call_id: &str,
    payload: &ToolPayload,
    value: &T,
    success: Option<bool>,
    tool_name: &str,
) -> ResponseInputItem
where
    T: Serialize,
{
    FunctionToolOutput::from_text(tool_output_json_text(value, tool_name), success)
        .to_response_item(call_id, payload)
}

pub(crate) fn tool_output_code_mode_result<T>(value: &T, tool_name: &str) -> JsonValue
where
    T: Serialize,
{
    serde_json::to_value(value).unwrap_or_else(|err| {
        JsonValue::String(format!("failed to serialize {tool_name} result: {err}"))
    })
}

pub(crate) fn build_wait_agent_statuses(
    statuses: &HashMap<ThreadId, AgentStatus>,
    receiver_agents: &[CollabAgentRef],
) -> Vec<CollabAgentStatusEntry> {
    if statuses.is_empty() {
        return Vec::new();
    }

    let mut entries = Vec::with_capacity(statuses.len());
    let mut seen = HashMap::with_capacity(receiver_agents.len());
    for receiver_agent in receiver_agents {
        seen.insert(receiver_agent.thread_id, ());
        if let Some(status) = statuses.get(&receiver_agent.thread_id) {
            entries.push(CollabAgentStatusEntry {
                thread_id: receiver_agent.thread_id,
                agent_nickname: receiver_agent.agent_nickname.clone(),
                agent_role: receiver_agent.agent_role.clone(),
                status: status.clone(),
            });
        }
    }

    let mut extras = statuses
        .iter()
        .filter(|(thread_id, _)| !seen.contains_key(thread_id))
        .map(|(thread_id, status)| CollabAgentStatusEntry {
            thread_id: *thread_id,
            agent_nickname: None,
            agent_role: None,
            status: status.clone(),
        })
        .collect::<Vec<_>>();
    extras.sort_by(|left, right| left.thread_id.to_string().cmp(&right.thread_id.to_string()));
    entries.extend(extras);
    entries
}

pub(crate) async fn collect_wait_statuses(
    session: &Session,
    receiver_thread_ids: &[ThreadId],
) -> HashMap<ThreadId, AgentStatus> {
    let mut statuses = HashMap::with_capacity(receiver_thread_ids.len());
    for receiver_thread_id in receiver_thread_ids {
        statuses.insert(
            *receiver_thread_id,
            session
                .services
                .agent_control
                .get_status(*receiver_thread_id)
                .await,
        );
    }
    statuses
}

pub(crate) fn pending_wait_thread_ids(
    receiver_thread_ids: &[ThreadId],
    statuses: &HashMap<ThreadId, AgentStatus>,
) -> Vec<ThreadId> {
    receiver_thread_ids
        .iter()
        .filter(|thread_id| !is_final(statuses.get(thread_id).unwrap_or(&AgentStatus::NotFound)))
        .copied()
        .collect()
}

pub(crate) async fn send_wait_end_event(
    session: &Session,
    turn: &TurnContext,
    call_id: String,
    receiver_thread_ids: Vec<ThreadId>,
    receiver_agents: &[CollabAgentRef],
    pending_thread_ids: Vec<ThreadId>,
    completion_reason: CollabWaitingCompletionReason,
    timed_out: bool,
    statuses: HashMap<ThreadId, AgentStatus>,
) {
    let agent_statuses = build_wait_agent_statuses(&statuses, receiver_agents);
    session
        .send_event(
            turn,
            CollabWaitingEndEvent {
                sender_thread_id: session.conversation_id,
                call_id,
                receiver_thread_ids,
                pending_thread_ids,
                completion_reason,
                timed_out,
                agent_statuses,
                statuses,
            }
            .into(),
        )
        .await;
}

pub(crate) fn collab_spawn_error(err: CodexErr) -> FunctionCallError {
    match err {
        CodexErr::UnsupportedOperation(message) if message == "thread manager dropped" => {
            FunctionCallError::RespondToModel("collab manager unavailable".to_string())
        }
        CodexErr::UnsupportedOperation(message) => FunctionCallError::RespondToModel(message),
        err => FunctionCallError::RespondToModel(format!("collab spawn failed: {err}")),
    }
}

pub(crate) fn collab_agent_error(agent_id: ThreadId, err: CodexErr) -> FunctionCallError {
    match err {
        CodexErr::ThreadNotFound(id) => {
            FunctionCallError::RespondToModel(format!("agent with id {id} not found"))
        }
        CodexErr::InternalAgentDied => {
            FunctionCallError::RespondToModel(format!("agent with id {agent_id} is closed"))
        }
        CodexErr::UnsupportedOperation(_) => {
            FunctionCallError::RespondToModel("collab manager unavailable".to_string())
        }
        err => FunctionCallError::RespondToModel(format!("collab tool failed: {err}")),
    }
}

pub(crate) fn thread_spawn_source(
    parent_thread_id: ThreadId,
    parent_session_source: &SessionSource,
    depth: i32,
    agent_role: Option<&str>,
    task_name: Option<String>,
) -> Result<SessionSource, FunctionCallError> {
    let agent_path = task_name
        .as_deref()
        .map(|task_name| {
            parent_session_source
                .get_agent_path()
                .unwrap_or_else(AgentPath::root)
                .join(task_name)
                .map_err(FunctionCallError::RespondToModel)
        })
        .transpose()?;
    Ok(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth,
        agent_path,
        agent_nickname: None,
        agent_role: agent_role.map(str::to_string),
    }))
}

pub(crate) fn parse_collab_input(
    message: Option<String>,
    items: Option<Vec<UserInput>>,
) -> Result<Op, FunctionCallError> {
    match (message, items) {
        (Some(_), Some(_)) => Err(FunctionCallError::RespondToModel(
            "Provide either message or items, but not both".to_string(),
        )),
        (None, None) => Err(FunctionCallError::RespondToModel(
            "Provide one of: message or items".to_string(),
        )),
        (Some(message), None) => {
            if message.trim().is_empty() {
                return Err(FunctionCallError::RespondToModel(
                    "Empty message can't be sent to an agent".to_string(),
                ));
            }
            Ok(vec![UserInput::Text {
                text: message,
                text_elements: Vec::new(),
            }]
            .into())
        }
        (None, Some(items)) => {
            if items.is_empty() {
                return Err(FunctionCallError::RespondToModel(
                    "Items can't be empty".to_string(),
                ));
            }
            Ok(items.into())
        }
    }
}

pub(crate) async fn require_spawn_agent_approval_if_requested(
    session: &Session,
    turn: &TurnContext,
    spawn_approval: SpawnAgentApproval,
    call_id: &str,
    role_name: Option<&str>,
    requested_model: Option<&str>,
    prompt_preview: &str,
) -> Result<(), FunctionCallError> {
    if !matches!(spawn_approval, SpawnAgentApproval::AskUser) {
        return Ok(());
    }

    let mode = session.collaboration_mode().await.mode;
    if let Some(message) = request_user_input_unavailable_message(
        mode,
        turn.tools_config.default_mode_request_user_input,
    ) {
        return Err(FunctionCallError::RespondToModel(message));
    }

    let question = RequestUserInputQuestion {
        id: SPAWN_AGENT_APPROVAL_QUESTION_ID.to_string(),
        header: "Confirm subagent spawn".to_string(),
        question: build_spawn_agent_approval_question_text(
            role_name,
            requested_model,
            prompt_preview,
        ),
        is_other: false,
        is_secret: false,
        options: Some(vec![
            RequestUserInputQuestionOption {
                label: SPAWN_AGENT_APPROVAL_ACCEPT_OPTION.to_string(),
                description: "Spawn the subagent and continue.".to_string(),
            },
            RequestUserInputQuestionOption {
                label: SPAWN_AGENT_APPROVAL_DECLINE_OPTION.to_string(),
                description: "Block this spawn and keep work in the current thread.".to_string(),
            },
        ]),
    };
    let args = RequestUserInputArgs {
        questions: vec![question],
    };
    let approval_call_id = format!("spawn-agent-approval-{call_id}");
    let response = session
        .request_user_input(turn, approval_call_id, args)
        .await
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "spawn_agent cancelled because no user approval response was received".to_string(),
            )
        })?;

    let approved = response
        .answers
        .get(SPAWN_AGENT_APPROVAL_QUESTION_ID)
        .is_some_and(|answer| {
            answer
                .answers
                .iter()
                .any(|selection| selection == SPAWN_AGENT_APPROVAL_ACCEPT_OPTION)
        });
    if approved {
        Ok(())
    } else {
        Err(FunctionCallError::RespondToModel(
            "spawn_agent blocked because the user declined this spawn".to_string(),
        ))
    }
}

pub(crate) fn build_spawn_agent_approval_question_text(
    role_name: Option<&str>,
    requested_model: Option<&str>,
    prompt_preview: &str,
) -> String {
    let prompt_preview = prompt_preview.trim();
    let prompt_suffix = if prompt_preview.is_empty() {
        String::new()
    } else {
        format!(" Task preview: `{prompt_preview}`.")
    };
    let role_suffix = role_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| format!(" Requested role: `{value}`."))
        .unwrap_or_default();
    let model_suffix = requested_model
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| format!(" Requested model: `{value}`."))
        .unwrap_or_default();

    format!(
        "The agent requested spawning a subagent. Approve this spawn now?{prompt_suffix}{role_suffix}{model_suffix}"
    )
}

/// Builds the base config snapshot for a newly spawned sub-agent.
///
/// The returned config starts from the parent's effective config and then refreshes the
/// runtime-owned fields carried on `turn`, including model selection, reasoning settings,
/// approval policy, sandbox, and cwd. Role-specific overrides are layered after this step;
/// skipping this helper and cloning stale config state directly can send the child agent out with
/// the wrong provider or runtime policy.
pub(crate) fn build_agent_spawn_config(
    base_instructions: &BaseInstructions,
    turn: &TurnContext,
) -> Result<Config, FunctionCallError> {
    let mut config = build_agent_shared_config(turn)?;
    config.base_instructions = Some(base_instructions.text.clone());
    Ok(config)
}

pub(crate) fn build_agent_resume_config(
    turn: &TurnContext,
    child_depth: i32,
) -> Result<Config, FunctionCallError> {
    let mut config = build_agent_shared_config(turn)?;
    apply_spawn_agent_overrides(&mut config, child_depth);
    // For resume, keep base instructions sourced from rollout/session metadata.
    config.base_instructions = None;
    Ok(config)
}

fn build_agent_shared_config(turn: &TurnContext) -> Result<Config, FunctionCallError> {
    let base_config = turn.config.clone();
    let mut config = (*base_config).clone();
    config.model = Some(turn.model_info.slug.clone());
    config.model_provider = turn.provider.info().clone();
    config.model_reasoning_effort = turn
        .reasoning_effort
        .or(turn.model_info.default_reasoning_level);
    config.model_reasoning_summary = Some(turn.reasoning_summary);
    config.developer_instructions = turn.developer_instructions.clone();
    config.compact_prompt = turn.compact_prompt.clone();
    apply_spawn_agent_runtime_overrides(&mut config, turn)?;

    Ok(config)
}

pub(crate) fn reject_full_fork_spawn_overrides(
    agent_type: Option<&str>,
    model: Option<&str>,
    reasoning_effort: Option<ReasoningEffort>,
) -> Result<(), FunctionCallError> {
    if agent_type.is_some() || model.is_some() || reasoning_effort.is_some() {
        return Err(FunctionCallError::RespondToModel(
            "Full-history forked agents inherit the parent agent type, model, and reasoning effort; omit agent_type, model, and reasoning_effort, or spawn without a full-history fork.".to_string(),
        ));
    }
    Ok(())
}

/// Copies runtime-only turn state onto a child config before it is handed to `AgentControl`.
///
/// These values are chosen by the live turn rather than persisted config, so leaving them stale
/// can make a child agent disagree with its parent about approval policy, cwd, or sandboxing.
pub(crate) fn apply_spawn_agent_runtime_overrides(
    config: &mut Config,
    turn: &TurnContext,
) -> Result<(), FunctionCallError> {
    config
        .permissions
        .approval_policy
        .set(turn.approval_policy.value())
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("approval_policy is invalid: {err}"))
        })?;
    config.permissions.shell_environment_policy = turn.shell_environment_policy.clone();
    config.codex_linux_sandbox_exe = turn.codex_linux_sandbox_exe.clone();
    config.cwd = turn.cwd.clone();
    config
        .permissions
        .set_permission_profile(turn.permission_profile())
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("permission_profile is invalid: {err}"))
        })?;
    Ok(())
}

pub(crate) fn apply_spawn_agent_overrides(config: &mut Config, child_depth: i32) {
    if child_depth >= config.agent_max_depth && !config.features.enabled(Feature::MultiAgentV2) {
        let _ = config.features.disable(Feature::SpawnCsv);
        let _ = config.features.disable(Feature::Collab);
    }
}

pub(crate) async fn apply_requested_spawn_agent_model_overrides(
    session: &Session,
    turn: &TurnContext,
    config: &mut Config,
    requested_model: Option<&str>,
    requested_reasoning_effort: Option<ReasoningEffort>,
) -> Result<(), FunctionCallError> {
    if requested_model.is_none() && requested_reasoning_effort.is_none() {
        return Ok(());
    }

    if let Some(requested_model) = requested_model {
        let available_models = session
            .services
            .models_manager
            .list_models(RefreshStrategy::Offline)
            .await;
        let selected_model_name = find_spawn_agent_model_name(&available_models, requested_model)?;
        let selected_model_info = session
            .services
            .models_manager
            .get_model_info(&selected_model_name, &config.to_models_manager_config())
            .await;

        config.model = Some(selected_model_name.clone());
        if let Some(reasoning_effort) = requested_reasoning_effort {
            validate_spawn_agent_reasoning_effort(
                &selected_model_name,
                &selected_model_info.supported_reasoning_levels,
                reasoning_effort,
            )?;
            config.model_reasoning_effort = Some(reasoning_effort);
        } else {
            config.model_reasoning_effort = selected_model_info.default_reasoning_level;
        }

        return Ok(());
    }

    if let Some(reasoning_effort) = requested_reasoning_effort {
        validate_spawn_agent_reasoning_effort(
            &turn.model_info.slug,
            &turn.model_info.supported_reasoning_levels,
            reasoning_effort,
        )?;
        config.model_reasoning_effort = Some(reasoning_effort);
    }

    Ok(())
}

fn find_spawn_agent_model_name(
    available_models: &[codex_protocol::openai_models::ModelPreset],
    requested_model: &str,
) -> Result<String, FunctionCallError> {
    available_models
        .iter()
        .find(|model| model.model == requested_model)
        .map(|model| model.model.clone())
        .ok_or_else(|| {
            let available = available_models
                .iter()
                .map(|model| model.model.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            FunctionCallError::RespondToModel(format!(
                "Unknown model `{requested_model}` for spawn_agent. Available models: {available}"
            ))
        })
}

pub(crate) fn validate_spawn_agent_reasoning_effort(
    model: &str,
    supported_reasoning_levels: &[ReasoningEffortPreset],
    requested_reasoning_effort: ReasoningEffort,
) -> Result<(), FunctionCallError> {
    if supported_reasoning_levels
        .iter()
        .any(|preset| preset.effort == requested_reasoning_effort)
    {
        return Ok(());
    }

    let supported = supported_reasoning_levels
        .iter()
        .map(|preset| preset.effort.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(FunctionCallError::RespondToModel(format!(
        "Reasoning effort `{requested_reasoning_effort}` is not supported for model `{model}`. Supported reasoning efforts: {supported}"
    )))
}

/// Returns whether the requested model was honored by the spawned agent.
///
/// `Some(true)` means both values are present and equal, `Some(false)` means
/// both are present but differ, and `None` means one or both values are
/// unavailable.
pub(crate) fn requested_model_honored(
    requested_model: Option<&str>,
    effective_model: Option<&str>,
) -> Option<bool> {
    match (requested_model, effective_model) {
        (Some(requested), Some(effective)) => Some(requested == effective),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::requested_model_honored;

    #[test]
    fn requested_model_honored_reports_match() {
        assert_eq!(
            requested_model_honored(Some("gpt-5.1-codex-mini"), Some("gpt-5.1-codex-mini")),
            Some(true)
        );
    }

    #[test]
    fn requested_model_honored_reports_mismatch() {
        assert_eq!(
            requested_model_honored(Some("gpt-5.1-codex-mini"), Some("gpt-5.3-codex")),
            Some(false)
        );
    }

    #[test]
    fn requested_model_honored_reports_unknown_when_missing_values() {
        assert_eq!(
            requested_model_honored(/*requested_model*/ None, Some("gpt-5.3-codex"),),
            None
        );
        assert_eq!(
            requested_model_honored(Some("gpt-5.1-codex-mini"), /*effective_model*/ None,),
            None
        );
    }
}
