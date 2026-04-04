use super::*;
use crate::agent::control::SUBAGENT_IDENTITY_SOURCE_THREAD_CONFIG_SNAPSHOT;
use crate::agent::control::SpawnAgentForkMode;
use crate::agent::control::SpawnAgentOptions;
use crate::agent::control::SubAgentInventoryInfo;
use crate::agent::control::render_input_preview;
use crate::agent::role::DEFAULT_ROLE_NAME;
use crate::agent::role::apply_role_to_spawn_config;

use crate::agent::exceeds_thread_spawn_depth_limit;
use crate::agent::next_thread_spawn_depth;

pub(crate) struct Handler;

#[async_trait]
impl ToolHandler for Handler {
    type Output = SpawnAgentResult;

    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<Self::Output, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            payload,
            call_id,
            ..
        } = invocation;
        let arguments = function_arguments(payload)?;
        let args: SpawnAgentArgs = parse_arguments(&arguments)?;
        let requested_model = args.model.clone();
        let requested_reasoning_effort = args.reasoning_effort;
        let role_name = args
            .agent_type
            .as_deref()
            .map(str::trim)
            .filter(|role| !role.is_empty());
        let input_items = parse_collab_input(args.message, args.items)?;
        let prompt = render_input_preview(&input_items);
        let session_source = turn.session_source.clone();
        let child_depth = next_thread_spawn_depth(&session_source);
        let requested_task_name = args.task_name.clone();
        let max_depth = turn.config.agent_max_depth;
        if exceeds_thread_spawn_depth_limit(child_depth, max_depth) {
            return Err(FunctionCallError::RespondToModel(
                "Agent depth limit reached. Solve the task yourself.".to_string(),
            ));
        }
        session
            .send_event(
                &turn,
                CollabAgentSpawnBeginEvent {
                    call_id: call_id.clone(),
                    sender_thread_id: session.conversation_id,
                    prompt: prompt.clone(),
                    model: args.model.clone().unwrap_or_default(),
                    reasoning_effort: args.reasoning_effort.unwrap_or_default(),
                }
                .into(),
            )
            .await;
        let mut config =
            build_agent_spawn_config(&session.get_base_instructions().await, turn.as_ref())?;
        let pre_role_reasoning_effort = config.model_reasoning_effort;
        let spawn_model_selection_carry = apply_role_to_spawn_config(&mut config, role_name)
            .await
            .map_err(FunctionCallError::RespondToModel)?;
        spawn_model_selection_carry.apply_to_config(&mut config);
        apply_requested_spawn_agent_model_overrides(
            &session,
            turn.as_ref(),
            &mut config,
            requested_model.as_deref(),
            requested_reasoning_effort,
        )
        .await?;
        if let Some(model) = config.model.clone() {
            let model_info = session
                .services
                .models_manager
                .get_model_info(&model, &config)
                .await;

            match config.model_reasoning_effort {
                Some(reasoning_effort) => {
                    if !model_info
                        .supported_reasoning_levels
                        .iter()
                        .any(|preset| preset.effort == reasoning_effort)
                    {
                        let role_changed_reasoning_effort =
                            config.model_reasoning_effort != pre_role_reasoning_effort;
                        if args.reasoning_effort.is_some() || role_changed_reasoning_effort {
                            validate_spawn_agent_reasoning_effort(
                                &model,
                                &model_info.supported_reasoning_levels,
                                reasoning_effort,
                            )?;
                        }

                        config.model_reasoning_effort = model_info.default_reasoning_level;
                    }
                }
                None => {
                    config.model_reasoning_effort = model_info.default_reasoning_level;
                }
            }
        }
        apply_spawn_agent_runtime_overrides(&mut config, turn.as_ref())?;
        apply_spawn_agent_overrides(&mut config, child_depth);

        let result = session
            .services
            .agent_control
            .spawn_agent_with_metadata(
                config,
                input_items,
                Some(thread_spawn_source(
                    session.conversation_id,
                    &turn.session_source,
                    child_depth,
                    role_name,
                    requested_task_name.clone(),
                )?),
                SpawnAgentOptions {
                    fork_parent_spawn_call_id: args.fork_context.then(|| call_id.clone()),
                    fork_mode: args.fork_context.then_some(SpawnAgentForkMode::FullHistory),
                },
            )
            .await
            .map_err(collab_spawn_error);
        let spawned_thread_id = result.as_ref().ok().map(|agent| agent.thread_id);
        let new_agent = match spawned_thread_id {
            Some(thread_id) => {
                if let Some(agent) = session
                    .services
                    .agent_control
                    .get_subagent_inventory_info(thread_id)
                    .await
                {
                    agent
                } else if let Some(snapshot) = session
                    .services
                    .agent_control
                    .get_agent_config_snapshot(thread_id)
                    .await
                {
                    SubAgentInventoryInfo {
                        nickname: snapshot.session_source.get_nickname(),
                        role: snapshot.session_source.get_agent_role(),
                        status: session.services.agent_control.get_status(thread_id).await,
                        effective_model: Some(snapshot.model),
                        effective_reasoning_effort: snapshot.reasoning_effort,
                        effective_model_provider_id: snapshot.model_provider_id,
                        identity_source: SUBAGENT_IDENTITY_SOURCE_THREAD_CONFIG_SNAPSHOT
                            .to_string(),
                    }
                } else {
                    SubAgentInventoryInfo {
                        nickname: None,
                        role: None,
                        status: session.services.agent_control.get_status(thread_id).await,
                        effective_model: None,
                        effective_reasoning_effort: None,
                        effective_model_provider_id: String::new(),
                        identity_source: SUBAGENT_IDENTITY_SOURCE_THREAD_CONFIG_SNAPSHOT
                            .to_string(),
                    }
                }
            }
            None => SubAgentInventoryInfo {
                nickname: None,
                role: None,
                status: AgentStatus::NotFound,
                effective_model: None,
                effective_reasoning_effort: None,
                effective_model_provider_id: String::new(),
                identity_source: SUBAGENT_IDENTITY_SOURCE_THREAD_CONFIG_SNAPSHOT.to_string(),
            },
        };
        let nickname = new_agent.nickname.clone();
        let role = new_agent.role.clone();
        let status = new_agent.status.clone();
        let effective_model = new_agent.effective_model.clone().unwrap_or_default();
        let effective_reasoning_effort = new_agent.effective_reasoning_effort.unwrap_or_default();
        session
            .send_event(
                &turn,
                CollabAgentSpawnEndEvent {
                    call_id,
                    sender_thread_id: session.conversation_id,
                    new_thread_id: spawned_thread_id,
                    new_agent_nickname: nickname.clone(),
                    new_agent_role: role.clone(),
                    prompt,
                    model: effective_model,
                    reasoning_effort: effective_reasoning_effort,
                    status: status.clone(),
                }
                .into(),
            )
            .await;
        let new_thread_id = result?.thread_id;
        let role_tag = role_name.unwrap_or(DEFAULT_ROLE_NAME);
        turn.session_telemetry.counter(
            "codex.multi_agent.spawn",
            /*inc*/ 1,
            &[("role", role_tag)],
        );

        Ok(SpawnAgentResult {
            agent_id: new_thread_id.to_string(),
            nickname,
            role,
            status,
            requested_model,
            requested_reasoning_effort,
            effective_model: new_agent.effective_model,
            effective_reasoning_effort: new_agent.effective_reasoning_effort,
            effective_model_provider_id: new_agent.effective_model_provider_id,
            identity_source: new_agent.identity_source,
        })
    }
}

#[derive(Debug, Deserialize)]
struct SpawnAgentArgs {
    message: Option<String>,
    items: Option<Vec<UserInput>>,
    task_name: Option<String>,
    agent_type: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    fork_context: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct SpawnAgentResult {
    agent_id: String,
    nickname: Option<String>,
    role: Option<String>,
    status: AgentStatus,
    requested_model: Option<String>,
    requested_reasoning_effort: Option<ReasoningEffort>,
    effective_model: Option<String>,
    effective_reasoning_effort: Option<ReasoningEffort>,
    effective_model_provider_id: String,
    identity_source: String,
}

impl ToolOutput for SpawnAgentResult {
    fn log_preview(&self) -> String {
        tool_output_json_text(self, "spawn_agent")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), "spawn_agent")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "spawn_agent")
    }
}
