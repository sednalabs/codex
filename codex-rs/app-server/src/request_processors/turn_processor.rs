use super::*;
use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::automatic_turn_connection_principal;
use crate::outgoing_message::is_current_automatic_turn_principal;
use crate::outgoing_message::parse_automatic_turn_connection_principal;
use codex_agent_extension::AgentInvocation;
use codex_agent_extension::AgentRun;
use codex_agent_extension::AgentRunner;
use codex_core::automatic_turn_context_fingerprint;
use codex_protocol::automatic_turn::AutomaticTurnProvenance;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AdditionalContextEntry as CoreAdditionalContextEntry;
use codex_protocol::protocol::AdditionalContextKind as CoreAdditionalContextKind;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_skills::system_cache_root_dir;

use crate::image_url::REMOTE_IMAGE_URL_ERROR;
use crate::image_url::is_remote_image_url;

const DIRECT_INPUT_TO_MULTI_AGENT_V2_SUBAGENT_ERROR: &str =
    "direct app-server input is not allowed for multi-agent v2 sub-agents";

/// Mirrors the direct-input policy in both request validation and thread capability responses.
pub(super) fn can_accept_direct_input(
    multi_agent_version: Option<MultiAgentVersion>,
    session_source: &SessionSource,
) -> bool {
    multi_agent_version != Some(MultiAgentVersion::V2)
        || !matches!(
            session_source,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
        )
}

fn validate_user_input_image_urls(input: &[V2UserInput]) -> Result<(), JSONRPCErrorError> {
    if input.iter().any(|item| {
        matches!(
            item,
            V2UserInput::Image { url, .. } if is_remote_image_url(url)
        )
    }) {
        return Err(invalid_request(REMOTE_IMAGE_URL_ERROR));
    }
    Ok(())
}

fn validate_response_item_image_urls(items: &[ResponseItem]) -> Result<(), JSONRPCErrorError> {
    if items.iter().any(|item| match item {
        ResponseItem::Message { content, .. } => content.iter().any(|item| {
            matches!(
                item,
                ContentItem::InputImage { image_url, .. } if is_remote_image_url(image_url)
            )
        }),
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => {
            output.content_items().is_some_and(|content| {
                content.iter().any(|item| {
                    matches!(
                        item,
                        FunctionCallOutputContentItem::InputImage { image_url, .. }
                            if is_remote_image_url(image_url)
                    )
                })
            })
        }
        ResponseItem::Reasoning { .. }
        | ResponseItem::AgentMessage { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::CompactionTrigger { .. }
        | ResponseItem::ContextCompaction { .. }
        | ResponseItem::AdditionalTools { .. }
        | ResponseItem::Other => false,
    }) {
        return Err(invalid_request(REMOTE_IMAGE_URL_ERROR));
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct TurnRequestProcessor {
    agent_runner: AgentRunner,
    auth_manager: Arc<AuthManager>,
    auth_admission: Arc<Mutex<()>>,
    thread_manager: Arc<ThreadManager>,
    outgoing: Arc<OutgoingMessageSender>,
    analytics_events_client: AnalyticsEventsClient,
    arg0_paths: Arg0DispatchPaths,
    config: Arc<Config>,
    config_manager: ConfigManager,
    pending_thread_unloads: Arc<Mutex<HashSet<ThreadId>>>,
    thread_state_manager: ThreadStateManager,
    thread_watch_manager: ThreadWatchManager,
    thread_list_state_permit: Arc<Semaphore>,
    skills_watcher: Arc<SkillsWatcher>,
}

fn map_additional_context(
    additional_context: Option<HashMap<String, AdditionalContextEntry>>,
) -> BTreeMap<String, CoreAdditionalContextEntry> {
    additional_context
        .unwrap_or_default()
        .into_iter()
        .map(|(key, entry)| {
            (
                key,
                CoreAdditionalContextEntry {
                    value: entry.value,
                    kind: match entry.kind {
                        AdditionalContextKind::Untrusted => CoreAdditionalContextKind::Untrusted,
                        AdditionalContextKind::Application => {
                            CoreAdditionalContextKind::Application
                        }
                    },
                },
            )
        })
        .collect()
}

struct ThreadSettingsBuildParams {
    method: &'static str,
    environments: Option<TurnEnvironmentSelections>,
    approval_policy: Option<codex_app_server_protocol::AskForApproval>,
    approvals_reviewer: Option<codex_app_server_protocol::ApprovalsReviewer>,
    sandbox_policy: Option<codex_app_server_protocol::SandboxPolicy>,
    permissions: Option<String>,
    model: Option<String>,
    service_tier: Option<Option<String>>,
    effort: Option<ReasoningEffort>,
    summary: Option<ReasoningSummary>,
    collaboration_mode: Option<CollaborationMode>,
    personality: Option<Personality>,
}

impl TurnRequestProcessor {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        auth_manager: Arc<AuthManager>,
        auth_admission: Arc<Mutex<()>>,
        thread_manager: Arc<ThreadManager>,
        outgoing: Arc<OutgoingMessageSender>,
        analytics_events_client: AnalyticsEventsClient,
        arg0_paths: Arg0DispatchPaths,
        config: Arc<Config>,
        config_manager: ConfigManager,
        pending_thread_unloads: Arc<Mutex<HashSet<ThreadId>>>,
        thread_state_manager: ThreadStateManager,
        thread_watch_manager: ThreadWatchManager,
        thread_list_state_permit: Arc<Semaphore>,
        skills_watcher: Arc<SkillsWatcher>,
    ) -> Self {
        let agent_runner = AgentRunner::new(Arc::downgrade(&thread_manager));
        Self {
            agent_runner,
            auth_manager,
            auth_admission,
            thread_manager,
            outgoing,
            analytics_events_client,
            arg0_paths,
            config,
            config_manager,
            pending_thread_unloads,
            thread_state_manager,
            thread_watch_manager,
            thread_list_state_permit,
            skills_watcher,
        }
    }

    #[cfg(test)]
    pub(crate) async fn config_snapshot_for_test(
        &self,
        thread_id: ThreadId,
    ) -> Option<ThreadConfigSnapshot> {
        let thread = self.thread_manager.get_thread(thread_id).await.ok()?;
        Some(thread.config_snapshot().await)
    }

    pub(crate) async fn turn_start(
        &self,
        request_id: ConnectionRequestId,
        params: TurnStartParams,
        app_server_client_name: Option<String>,
        app_server_client_version: Option<String>,
        supports_openai_form_elicitation: bool,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        validate_user_input_image_urls(&params.input)?;
        self.turn_start_inner(
            request_id,
            params,
            app_server_client_name,
            app_server_client_version,
            /*supports_openai_form_elicitation*/ supports_openai_form_elicitation,
        )
        .await
        .map(|response| Some(response.into()))
    }

    pub(crate) async fn thread_inject_items(
        &self,
        params: ThreadInjectItemsParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_inject_items_response_inner(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn thread_settings_update(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadSettingsUpdateParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_settings_update_inner(request_id, params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn turn_steer(
        &self,
        request_id: &ConnectionRequestId,
        params: TurnSteerParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        if is_automatic_turn_capability(params.client_user_message_id.as_deref()) {
            return Err(invalid_request(
                "automatic turn capabilities must be redeemed with turn/start",
            ));
        }
        validate_user_input_image_urls(&params.input)?;
        self.turn_steer_inner(request_id, params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn turn_interrupt(
        &self,
        request_id: &ConnectionRequestId,
        params: TurnInterruptParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.turn_interrupt_inner(request_id, params)
            .await
            .map(|response| response.map(Into::into))
    }

    pub(crate) async fn thread_realtime_start(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeStartParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_realtime_start_inner(request_id, params)
            .await
            .map(|response| response.map(Into::into))
    }

    pub(crate) async fn thread_realtime_append_audio(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeAppendAudioParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_realtime_append_audio_inner(request_id, params)
            .await
            .map(|response| response.map(Into::into))
    }

    pub(crate) async fn thread_realtime_append_text(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeAppendTextParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_realtime_append_text_inner(request_id, params)
            .await
            .map(|response| response.map(Into::into))
    }

    pub(crate) async fn thread_realtime_append_speech(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeAppendSpeechParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_realtime_append_speech_inner(request_id, params)
            .await
            .map(|response| response.map(Into::into))
    }

    pub(crate) async fn thread_realtime_stop(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeStopParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_realtime_stop_inner(request_id, params)
            .await
            .map(|response| response.map(Into::into))
    }

    pub(crate) async fn thread_realtime_list_voices(
        &self,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        Ok(Some(
            ThreadRealtimeListVoicesResponse {
                voices: RealtimeVoicesList::builtin(),
            }
            .into(),
        ))
    }

    pub(crate) async fn review_start(
        &self,
        request_id: &ConnectionRequestId,
        params: ReviewStartParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.review_start_inner(request_id, params)
            .await
            .map(|()| None)
    }

    fn track_error_response(
        &self,
        request_id: &ConnectionRequestId,
        error: &JSONRPCErrorError,
        error_type: Option<AnalyticsJsonRpcError>,
    ) {
        self.analytics_events_client.track_error_response(
            request_id.connection_id.0,
            request_id.request_id.clone(),
            error.clone(),
            error_type,
        );
    }

    async fn load_thread(
        &self,
        thread_id: &str,
    ) -> Result<(ThreadId, Arc<CodexThread>), JSONRPCErrorError> {
        // Resolve the core conversation handle from a v2 thread id string.
        let thread_id = ThreadId::from_string(thread_id)
            .map_err(|err| invalid_request(format!("invalid thread id: {err}")))?;

        let thread = self
            .thread_manager
            .get_thread(thread_id)
            .await
            .map_err(|_| invalid_request(format!("thread not found: {thread_id}")))?;

        Ok((thread_id, thread))
    }

    async fn ensure_direct_input_allowed(
        &self,
        request_id: &ConnectionRequestId,
        thread: &CodexThread,
    ) -> Result<(), JSONRPCErrorError> {
        let config_snapshot = thread.config_snapshot().await;
        if !can_accept_direct_input(
            thread.multi_agent_version(),
            &config_snapshot.session_source,
        ) {
            let error = invalid_request(DIRECT_INPUT_TO_MULTI_AGENT_V2_SUBAGENT_ERROR);
            self.track_error_response(request_id, &error, /*error_type*/ None);
            return Err(error);
        }

        Ok(())
    }

    async fn validate_automatic_turn_capability(
        &self,
        request_id: &ConnectionRequestId,
        thread_id: ThreadId,
        thread: &CodexThread,
        client_user_message_id: Option<&str>,
        operation_kind: &str,
        expected_turn_id: Option<&str>,
        request_settings_match: bool,
    ) -> Result<(), JSONRPCErrorError> {
        let Some(client_user_message_id) = client_user_message_id else {
            return Ok(());
        };
        let Some(provenance) =
            AutomaticTurnProvenance::decode_client_user_message_id(client_user_message_id)
        else {
            // Preserve ordinary client-message compatibility. The state projection will ignore
            // malformed or unknown envelopes and never treat them as trusted provenance.
            return Ok(());
        };
        if provenance.thread_id != thread_id.to_string() {
            return Err(invalid_request(
                "automatic turn capability belongs to another thread",
            ));
        }
        let Some(state_db) = thread.state_db() else {
            return Err(invalid_request("automatic turn capability is unavailable"));
        };

        let principal = automatic_turn_connection_principal(request_id.connection_id);
        let Some((expected_principal, allowed_operation_kind, allowed_expected_turn_id, context)) =
            state_db
                .automatic_turn_capability_contract(
                    thread_id,
                    &provenance.trigger_turn_id,
                    &provenance.capability,
                )
                .await
        else {
            return Err(invalid_request("automatic turn capability is not pending"));
        };
        let Some(expected_principal) = expected_principal else {
            return Err(invalid_request(
                "automatic turn capability has no server-selected owner",
            ));
        };
        if allowed_operation_kind.as_deref() != Some(operation_kind)
            || allowed_expected_turn_id.as_deref() != expected_turn_id
        {
            return Err(invalid_request(
                "automatic turn capability does not authorize this operation",
            ));
        }
        if !is_current_automatic_turn_principal(&expected_principal) {
            return Err(invalid_request(
                "automatic turn capability belongs to another app-server epoch",
            ));
        }
        let Some((_, previous_connection)) =
            parse_automatic_turn_connection_principal(&expected_principal)
        else {
            return Err(invalid_request(
                "automatic turn capability has an invalid owner",
            ));
        };
        if previous_connection != request_id.connection_id || principal != expected_principal {
            return Err(invalid_request(
                "automatic turn capability is bound to another connection",
            ));
        }
        let Some(ticket_epochs) = state_db
            .automatic_turn_capability_epochs(
                thread_id,
                &provenance.trigger_turn_id,
                &provenance.capability,
            )
            .await
        else {
            return Err(invalid_request("automatic turn capability is not pending"));
        };
        let Some(current_epochs) = state_db.automatic_turn_current_epochs(thread_id).await else {
            return Err(invalid_request(
                "automatic turn capability epochs are unavailable",
            ));
        };
        let current_context = automatic_turn_context_fingerprint(&thread.config_snapshot().await);
        if ticket_epochs != current_epochs
            || context.as_deref() != Some(current_context.as_str())
            || !request_settings_match
        {
            let _ = state_db
                .invalidate_automatic_turn_capability(
                    thread_id,
                    &provenance.trigger_turn_id,
                    &provenance.capability,
                )
                .await;
            return Err(invalid_request(
                "automatic turn capability no longer matches the server-canonical context",
            ));
        }
        Ok(())
    }

    async fn reserve_automatic_turn_capability(
        &self,
        thread_id: ThreadId,
        thread: &CodexThread,
        client_user_message_id: Option<&str>,
        operation_kind: &str,
        expected_turn_id: Option<&str>,
        connection_id: ConnectionId,
    ) -> Result<bool, JSONRPCErrorError> {
        let Some(client_user_message_id) = client_user_message_id else {
            return Ok(false);
        };
        let Some(provenance) =
            AutomaticTurnProvenance::decode_client_user_message_id(client_user_message_id)
        else {
            return Ok(false);
        };
        let Some(state_db) = thread.state_db() else {
            return Err(internal_error(
                "automatic turn capability state is unavailable",
            ));
        };
        let reserved = state_db
            .reserve_automatic_turn_capability(
                thread_id,
                &provenance.trigger_turn_id,
                &provenance.capability,
                &automatic_turn_connection_principal(connection_id),
                client_user_message_id,
                operation_kind,
                expected_turn_id,
            )
            .await
            .map_err(|_| internal_error("failed to reserve automatic turn capability"))?;
        if !reserved {
            return Err(invalid_request(
                "automatic turn capability was already admitted or is no longer pending",
            ));
        }
        Ok(true)
    }

    async fn release_automatic_turn_capability(
        &self,
        thread_id: ThreadId,
        thread: &CodexThread,
        client_user_message_id: Option<&str>,
    ) {
        let Some(client_user_message_id) = client_user_message_id else {
            return;
        };
        let Some(provenance) =
            AutomaticTurnProvenance::decode_client_user_message_id(client_user_message_id)
        else {
            return;
        };
        let Some(state_db) = thread.state_db() else {
            return;
        };
        if let Err(error) = state_db
            .release_automatic_turn_capability(
                thread_id,
                &provenance.capability,
                client_user_message_id,
            )
            .await
        {
            tracing::warn!(%error, "failed to release automatic turn capability admission");
        }
    }

    fn normalize_collaboration_mode(
        &self,
        mut collaboration_mode: CollaborationMode,
    ) -> CollaborationMode {
        if collaboration_mode.settings.developer_instructions.is_none()
            && let Some(instructions) = builtin_collaboration_mode_presets()
                .into_iter()
                .find(|preset| preset.mode == Some(collaboration_mode.mode))
                .and_then(|preset| preset.developer_instructions.flatten())
                .filter(|instructions| !instructions.is_empty())
        {
            collaboration_mode.settings.developer_instructions = Some(instructions);
        }

        collaboration_mode
    }

    fn review_request_from_target(
        target: ApiReviewTarget,
    ) -> Result<(ReviewRequest, String, String), JSONRPCErrorError> {
        let cleaned_target = match target {
            ApiReviewTarget::UncommittedChanges => ApiReviewTarget::UncommittedChanges,
            ApiReviewTarget::BaseBranch { branch } => {
                let branch = branch.trim().to_string();
                if branch.is_empty() {
                    return Err(invalid_request("branch must not be empty".to_string()));
                }
                ApiReviewTarget::BaseBranch { branch }
            }
            ApiReviewTarget::Commit { sha, title } => {
                let sha = sha.trim().to_string();
                if sha.is_empty() {
                    return Err(invalid_request("sha must not be empty".to_string()));
                }
                let title = title
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty());
                ApiReviewTarget::Commit { sha, title }
            }
            ApiReviewTarget::Custom { instructions } => {
                let trimmed = instructions.trim().to_string();
                if trimmed.is_empty() {
                    return Err(invalid_request(
                        "instructions must not be empty".to_string(),
                    ));
                }
                ApiReviewTarget::Custom {
                    instructions: trimmed,
                }
            }
        };

        let core_target = match cleaned_target {
            ApiReviewTarget::UncommittedChanges => CoreReviewTarget::UncommittedChanges,
            ApiReviewTarget::BaseBranch { branch } => CoreReviewTarget::BaseBranch { branch },
            ApiReviewTarget::Commit { sha, title } => CoreReviewTarget::Commit { sha, title },
            ApiReviewTarget::Custom { instructions } => CoreReviewTarget::Custom { instructions },
        };
        let target_prompt = match &core_target {
            CoreReviewTarget::UncommittedChanges => {
                "Review the current code changes (staged, unstaged, and untracked files)."
                    .to_string()
            }
            CoreReviewTarget::BaseBranch { branch } => {
                format!("Review the code changes against the base branch {branch:?}.")
            }
            CoreReviewTarget::Commit { sha, .. } => {
                format!("Review the changes introduced by commit {sha:?}.")
            }
            CoreReviewTarget::Custom { instructions } => instructions.clone(),
        };

        let hint = codex_core::review_prompts::user_facing_hint(&core_target);
        let review_request = ReviewRequest {
            target: core_target,
            user_facing_hint: Some(hint.clone()),
        };

        Ok((review_request, hint, target_prompt))
    }

    async fn request_trace_context(
        &self,
        request_id: &ConnectionRequestId,
    ) -> Option<codex_protocol::protocol::W3cTraceContext> {
        self.outgoing.request_trace_context(request_id).await
    }

    async fn submit_core_op(
        &self,
        request_id: &ConnectionRequestId,
        thread: &CodexThread,
        op: Op,
    ) -> CodexResult<String> {
        self.thread_manager
            .send_op_to_current_thread_with_trace(
                thread,
                op,
                self.request_trace_context(request_id).await,
            )
            .await
    }

    fn input_too_large_error(actual_chars: usize) -> JSONRPCErrorError {
        let mut error = invalid_params(format!(
            "Input exceeds the maximum length of {MAX_USER_INPUT_TEXT_CHARS} characters."
        ));
        error.data = Some(serde_json::json!({
            "input_error_code": INPUT_TOO_LARGE_ERROR_CODE,
            "max_chars": MAX_USER_INPUT_TEXT_CHARS,
            "actual_chars": actual_chars,
        }));
        error
    }

    fn validate_v2_input_limit(items: &[V2UserInput]) -> Result<(), JSONRPCErrorError> {
        let actual_chars: usize = items.iter().map(V2UserInput::text_char_count).sum();
        if actual_chars > MAX_USER_INPUT_TEXT_CHARS {
            return Err(Self::input_too_large_error(actual_chars));
        }
        Ok(())
    }

    async fn turn_start_inner(
        &self,
        request_id: ConnectionRequestId,
        params: TurnStartParams,
        app_server_client_name: Option<String>,
        app_server_client_version: Option<String>,
        supports_openai_form_elicitation: bool,
    ) -> Result<TurnStartResponse, JSONRPCErrorError> {
        // Settings invalidation and automatic-turn admission must observe one ordered
        // settings/auth boundary. Keep the guard through validation, reservation, and core
        // submission so a settings transition cannot interleave between those steps.
        let _auth_admission = self.auth_admission.lock().await;
        let (thread_id, thread) =
            self.load_thread(&params.thread_id)
                .await
                .inspect_err(|error| {
                    self.track_error_response(&request_id, error, /*error_type*/ None);
                })?;
        let automatic_turn = validate_automatic_turn_start_shape(&params)?;
        let request_settings_match = if automatic_turn {
            let snapshot = thread.config_snapshot().await;
            automatic_turn_start_settings_match_current(&params, &snapshot, self)
        } else {
            true
        };
        self.validate_automatic_turn_capability(
            &request_id,
            thread_id,
            thread.as_ref(),
            params.client_user_message_id.as_deref(),
            "start",
            /*expected_turn_id*/ None,
            request_settings_match,
        )
        .await?;
        self.ensure_direct_input_allowed(&request_id, thread.as_ref())
            .await?;
        if let Err(error) = Self::validate_v2_input_limit(&params.input) {
            self.track_error_response(
                &request_id,
                &error,
                Some(AnalyticsJsonRpcError::Input(InputError::TooLarge)),
            );
            return Err(error);
        }
        Self::set_app_server_client_info(
            thread.as_ref(),
            app_server_client_name,
            app_server_client_version,
        )
        .await
        .inspect_err(|error| {
            self.track_error_response(&request_id, error, /*error_type*/ None);
        })?;
        thread
            .set_openai_form_elicitation_support(supports_openai_form_elicitation)
            .await
            .map_err(|err| {
                internal_error(format!(
                    "failed to update OpenAI form elicitation support: {err}"
                ))
            })?;

        let runtime_workspace_roots = params
            .runtime_workspace_roots
            .map(resolve_runtime_workspace_roots);
        let environment_selections =
            resolve_turn_environment_selections(self.thread_manager.as_ref(), params.environments)?;

        // Map v2 input items to core input items.
        let mapped_items: Vec<CoreInputItem> = params
            .input
            .into_iter()
            .map(V2UserInput::into_core)
            .collect();
        let client_user_message_id = params.client_user_message_id;
        let additional_context = map_additional_context(params.additional_context);
        let turn_has_input = !mapped_items.is_empty();
        let cwd = resolve_request_cwd(params.cwd)?;
        let environments = self
            .build_environment_override(
                thread.as_ref(),
                cwd,
                runtime_workspace_roots,
                environment_selections,
                automatic_turn,
            )
            .await;
        let (thread_settings, settings_context_changed) = self
            .build_thread_settings_overrides(
                thread.as_ref(),
                ThreadSettingsBuildParams {
                    method: "turn/start",
                    environments,
                    approval_policy: params.approval_policy,
                    approvals_reviewer: params.approvals_reviewer,
                    sandbox_policy: params.sandbox_policy,
                    permissions: params.permissions,
                    model: params.model,
                    service_tier: params.service_tier,
                    effort: params.effort,
                    summary: params.summary,
                    collaboration_mode: params.collaboration_mode,
                    personality: params.personality,
                },
            )
            .await?;
        if automatic_turn && settings_context_changed {
            return Err(invalid_request(
                "automatic turn capability no longer matches the server-canonical context",
            ));
        }
        if !automatic_turn && settings_context_changed {
            if let Some(state_db) = thread.state_db() {
                state_db
                    .invalidate_automatic_turn_capabilities_for_thread(thread_id)
                    .await
                    .map_err(|err| {
                        internal_error(format!(
                            "failed to invalidate automatic turn capabilities before turn settings enqueue: {err}"
                        ))
                    })?;
            }
        }
        let parent_permission_profile_override =
            thread_settings.permission_profile.clone().or_else(|| {
                thread_settings
                    .sandbox_policy
                    .as_ref()
                    .map(PermissionProfile::from_legacy_sandbox_policy)
            });

        // Start the turn by submitting the user input. Return its submission id as turn_id.
        let turn_op = Op::UserInput {
            items: mapped_items,
            final_output_json_schema: params.output_schema,
            responsesapi_client_metadata: params.responsesapi_client_metadata,
            additional_context,
            thread_settings,
        };
        let automatic_turn_admitted = if automatic_turn {
            self.reserve_automatic_turn_capability(
                thread_id,
                thread.as_ref(),
                client_user_message_id.as_deref(),
                "start",
                /*expected_turn_id*/ None,
                request_id.connection_id,
            )
            .await?
        } else {
            false
        };
        let client_user_message_id_for_release = client_user_message_id.clone();
        let turn_id = match thread
            .submit_user_input_with_client_user_message_id_and_principal(
                turn_op,
                self.request_trace_context(&request_id).await,
                client_user_message_id,
                automatic_turn
                    .then(|| automatic_turn_connection_principal(request_id.connection_id)),
            )
            .await
        {
            Ok(turn_id) => turn_id,
            Err(err) => {
                if automatic_turn_admitted {
                    self.release_automatic_turn_capability(
                        thread_id,
                        thread.as_ref(),
                        client_user_message_id_for_release.as_deref(),
                    )
                    .await;
                }
                let error = internal_error(format!("failed to start turn: {err}"));
                self.track_error_response(&request_id, &error, /*error_type*/ None);
                return Err(error);
            }
        };

        if turn_has_input {
            let config_snapshot = thread.config_snapshot().await;
            let parent_permission_profile =
                parent_permission_profile_override.unwrap_or(config_snapshot.permission_profile);
            codex_memories_write::start_memories_startup_task(
                Arc::clone(&self.thread_manager),
                Arc::clone(&self.auth_manager),
                thread_id,
                Arc::clone(&thread),
                thread.config().await,
                parent_permission_profile,
                &config_snapshot.session_source,
            );
        }

        self.outgoing
            .record_request_turn_id(&request_id, &turn_id)
            .await;
        let turn = Turn {
            id: turn_id,
            items: vec![],
            items_view: TurnItemsView::NotLoaded,
            error: None,
            status: TurnStatus::InProgress,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        };

        Ok(TurnStartResponse { turn })
    }

    async fn build_environment_override(
        &self,
        thread: &CodexThread,
        cwd: Option<AbsolutePathBuf>,
        workspace_roots: Option<Vec<AbsolutePathBuf>>,
        environment_selections: Option<Vec<TurnEnvironmentSelection>>,
        preserve_existing_selections: bool,
    ) -> Option<TurnEnvironmentSelections> {
        if cwd.is_none() && workspace_roots.is_none() && environment_selections.is_none() {
            return None;
        }

        // Explicit environment selections own their roots and pass through unchanged. Top-level
        // `runtimeWorkspaceRoots` is only a compatibility input for default environments.
        if let Some(environment_selections) = environment_selections {
            let legacy_fallback_cwd = match cwd {
                Some(cwd) => cwd,
                None => match environment_selections
                    .iter()
                    .find(|selection| selection.environment_id == LOCAL_ENVIRONMENT_ID)
                    .and_then(|selection| selection.cwd.to_abs_path().ok())
                {
                    Some(cwd) => cwd,
                    None => thread.config_snapshot().await.cwd().clone(),
                },
            };
            return Some(TurnEnvironmentSelections::new(
                legacy_fallback_cwd,
                environment_selections,
            ));
        }

        let snapshot = thread.config_snapshot().await;
        if preserve_existing_selections {
            return Some(snapshot.environments);
        }
        let current_cwd = snapshot.cwd().clone();
        let legacy_fallback_cwd = cwd.unwrap_or_else(|| current_cwd.clone());
        let workspace_roots = match workspace_roots {
            Some(workspace_roots) => workspace_roots,
            None => {
                // Match the pre-environment partial-update behavior: a cwd-only update retargets
                // the old cwd root while preserving any additional roots. Deduplicate because the
                // new cwd may already be present as an additional root.
                let mut retargeted_workspace_roots = Vec::new();
                for root in snapshot.workspace_roots {
                    let root = if root == current_cwd {
                        legacy_fallback_cwd.clone()
                    } else {
                        root
                    };
                    if !retargeted_workspace_roots.contains(&root) {
                        retargeted_workspace_roots.push(root);
                    }
                }
                retargeted_workspace_roots
            }
        };
        let environment_selections = self
            .thread_manager
            .default_environment_selections(&legacy_fallback_cwd, &workspace_roots);
        Some(TurnEnvironmentSelections::new(
            legacy_fallback_cwd,
            environment_selections,
        ))
    }

    async fn build_thread_settings_overrides(
        &self,
        thread: &CodexThread,
        params: ThreadSettingsBuildParams,
    ) -> Result<(codex_protocol::protocol::ThreadSettingsOverrides, bool), JSONRPCErrorError> {
        let ThreadSettingsBuildParams {
            method,
            environments,
            approval_policy,
            approvals_reviewer,
            sandbox_policy,
            permissions,
            model,
            service_tier,
            effort,
            summary,
            collaboration_mode,
            personality,
        } = params;

        if sandbox_policy.is_some() && permissions.is_some() {
            return Err(invalid_request(
                "`permissions` cannot be combined with `sandboxPolicy`",
            ));
        }

        let collaboration_mode =
            collaboration_mode.map(|mode| self.normalize_collaboration_mode(mode));
        let has_environment_override = environments.is_some();
        // `thread/settings/update` only acknowledges that the update was queued.
        // Clients that send dependent partial updates should wait for
        // `thread/settings/updated` or combine the fields in one request.
        let snapshot = if permissions.is_some() {
            Some(thread.config_snapshot().await)
        } else {
            None
        };

        let has_any_overrides = has_environment_override
            || approval_policy.is_some()
            || approvals_reviewer.is_some()
            || sandbox_policy.is_some()
            || permissions.is_some()
            || model.is_some()
            || service_tier.is_some()
            || effort.is_some()
            || summary.is_some()
            || collaboration_mode.is_some()
            || personality.is_some();

        let approval_policy =
            approval_policy.map(codex_app_server_protocol::AskForApproval::to_core);
        let approvals_reviewer =
            approvals_reviewer.map(codex_app_server_protocol::ApprovalsReviewer::to_core);
        let sandbox_policy = sandbox_policy.map(|policy| policy.to_core());
        let (permission_profile, active_permission_profile, profile_workspace_roots) =
            if let Some(permissions) = permissions {
                let Some(snapshot) = snapshot.as_ref() else {
                    return Err(internal_error(format!(
                        "{method} permission selection missing thread snapshot"
                    )));
                };
                let overrides = ConfigOverrides {
                    cwd: environments
                        .as_ref()
                        .map(|environments| environments.legacy_fallback_cwd.to_path_buf()),
                    default_permissions: Some(permissions),
                    codex_linux_sandbox_exe: self.arg0_paths.codex_linux_sandbox_exe.clone(),
                    main_execve_wrapper_exe: self.arg0_paths.main_execve_wrapper_exe.clone(),
                    ..Default::default()
                };
                let config = self
                    .config_manager
                    .load_for_cwd(
                        /*request_overrides*/ None,
                        overrides,
                        Some(snapshot.cwd().to_path_buf()),
                    )
                    .await
                    .map_err(|err| config_load_error(&err))?;
                // Startup config is allowed to fall back when requirements
                // disallow a configured profile. An explicit settings update
                // is different: reject it before accepting the request.
                if let Some(warning) = config.startup_warnings.iter().find(|warning| {
                    warning.contains("Configured value for `permission_profile` is disallowed")
                }) {
                    return Err(invalid_request(format!(
                        "invalid thread settings override: {warning}"
                    )));
                }
                (
                    Some(config.permissions.permission_profile().clone()),
                    config.permissions.active_permission_profile(),
                    Some(config.permissions.profile_workspace_roots().to_vec()),
                )
            } else {
                (None, None, None)
            };
        let effort = effort.map(Some);

        let mut settings_context_changed = false;
        if has_any_overrides {
            let before = thread.config_snapshot().await;
            let after = thread
                .preview_thread_settings_overrides(CodexThreadSettingsOverrides {
                    environments: environments.clone(),
                    approval_policy,
                    approvals_reviewer,
                    sandbox_policy: sandbox_policy.clone(),
                    permission_profile: permission_profile.clone(),
                    active_permission_profile: active_permission_profile.clone(),
                    profile_workspace_roots: profile_workspace_roots.clone(),
                    windows_sandbox_level: None,
                    model: model.clone(),
                    effort: effort.clone(),
                    summary,
                    service_tier: service_tier.clone(),
                    collaboration_mode: collaboration_mode.clone(),
                    personality,
                })
                .await
                .map_err(|err| {
                    invalid_request(format!("invalid thread settings override: {err}"))
                })?;
            settings_context_changed = automatic_turn_context_fingerprint(&before)
                != automatic_turn_context_fingerprint(&after);
        }

        Ok((
            codex_protocol::protocol::ThreadSettingsOverrides {
                environments,
                profile_workspace_roots,
                approval_policy,
                approvals_reviewer,
                sandbox_policy,
                permission_profile,
                active_permission_profile,
                windows_sandbox_level: None,
                model,
                effort,
                summary,
                service_tier,
                collaboration_mode,
                personality,
            },
            settings_context_changed,
        ))
    }

    async fn thread_settings_update_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadSettingsUpdateParams,
    ) -> Result<ThreadSettingsUpdateResponse, JSONRPCErrorError> {
        // Serialize settings invalidation with automatic-turn validation/reservation. The guard
        // spans the snapshot, invalidation, and core enqueue so no admitted turn can cross the
        // settings transition boundary.
        let _auth_admission = self.auth_admission.lock().await;
        let (thread_id, thread) = self.load_thread(&params.thread_id).await?;
        let cwd = resolve_request_cwd(params.cwd)?;
        let environments = self
            .build_environment_override(
                thread.as_ref(),
                cwd,
                /*workspace_roots*/ None,
                /*environment_selections*/ None,
                /*preserve_existing_selections*/ false,
            )
            .await;
        let (thread_settings, _settings_context_changed) = self
            .build_thread_settings_overrides(
                thread.as_ref(),
                ThreadSettingsBuildParams {
                    method: "thread/settings/update",
                    environments,
                    approval_policy: params.approval_policy,
                    approvals_reviewer: params.approvals_reviewer,
                    sandbox_policy: params.sandbox_policy,
                    permissions: params.permissions,
                    model: params.model,
                    service_tier: params.service_tier,
                    effort: params.effort,
                    summary: params.summary,
                    collaboration_mode: params.collaboration_mode,
                    personality: params.personality,
                },
            )
            .await?;

        if thread_settings != codex_protocol::protocol::ThreadSettingsOverrides::default() {
            if let Some(state_db) = thread.state_db() {
                state_db
                    .invalidate_automatic_turn_capabilities_for_thread(thread_id)
                    .await
                    .map_err(|err| {
                        internal_error(format!(
                            "failed to invalidate automatic turn capabilities: {err}"
                        ))
                    })?;
            }
            self.submit_core_op(
                request_id,
                thread.as_ref(),
                Op::ThreadSettings { thread_settings },
            )
            .await
            .map_err(|err| internal_error(format!("failed to update thread settings: {err}")))?;
        }

        Ok(ThreadSettingsUpdateResponse {})
    }

    async fn thread_inject_items_response_inner(
        &self,
        params: ThreadInjectItemsParams,
    ) -> Result<ThreadInjectItemsResponse, JSONRPCErrorError> {
        let thread_id = ThreadId::from_string(&params.thread_id)
            .map_err(|err| invalid_request(format!("invalid thread id: {err}")))?;

        self.thread_manager
            .ensure_response_item_injection_target(thread_id)
            .await
            .map_err(|err| match err.details() {
                CodexErrorDetails::ThreadNotFound(thread_id) => {
                    invalid_request(format!("thread not found: {thread_id}"))
                }
                _ => internal_error(format!("failed to resolve injection target: {err}")),
            })?;

        let items = params
            .items
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                serde_json::from_value::<ResponseItem>(value)
                    .map_err(|err| format!("items[{index}] is not a valid response item: {err}"))
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(invalid_request)?;
        validate_response_item_image_urls(&items)?;

        // Keep the cold-reload future out of the monolithic request dispatch stack.
        Box::pin(self.thread_manager.inject_response_items(
            (*self.config).clone(),
            thread_id,
            items,
        ))
        .await
        .map_err(|err| match err.details() {
            CodexErrorDetails::InvalidRequest(message) => invalid_request(message.clone()),
            CodexErrorDetails::ThreadNotFound(thread_id) => {
                invalid_request(format!("thread not found: {thread_id}"))
            }
            _ => internal_error(format!("failed to inject response items: {err}")),
        })?;
        Ok(ThreadInjectItemsResponse {})
    }

    async fn set_app_server_client_info(
        thread: &CodexThread,
        app_server_client_name: Option<String>,
        app_server_client_version: Option<String>,
    ) -> Result<(), JSONRPCErrorError> {
        let mcp_elicitations_auto_deny = xcode_26_4_mcp_elicitations_auto_deny(
            app_server_client_name.as_deref(),
            app_server_client_version.as_deref(),
        );
        thread
            .set_app_server_client_info(
                app_server_client_name,
                app_server_client_version,
                mcp_elicitations_auto_deny,
            )
            .await
            .map_err(|err| internal_error(format!("failed to set app server client info: {err}")))
    }

    async fn turn_steer_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: TurnSteerParams,
    ) -> Result<TurnSteerResponse, JSONRPCErrorError> {
        let (_, thread) = self
            .load_thread(&params.thread_id)
            .await
            .inspect_err(|error| {
                self.track_error_response(request_id, error, /*error_type*/ None);
            })?;
        self.ensure_direct_input_allowed(request_id, thread.as_ref())
            .await?;

        if params.expected_turn_id.is_empty() {
            return Err(invalid_request("expectedTurnId must not be empty"));
        }
        self.outgoing
            .record_request_turn_id(request_id, &params.expected_turn_id)
            .await;
        if let Err(error) = Self::validate_v2_input_limit(&params.input) {
            self.track_error_response(
                request_id,
                &error,
                Some(AnalyticsJsonRpcError::Input(InputError::TooLarge)),
            );
            return Err(error);
        }

        let mapped_items: Vec<CoreInputItem> = params
            .input
            .into_iter()
            .map(V2UserInput::into_core)
            .collect();
        let additional_context = map_additional_context(params.additional_context);

        let turn_id = match thread
            .steer_input(
                mapped_items,
                additional_context,
                Some(&params.expected_turn_id),
                params.client_user_message_id,
                params.responsesapi_client_metadata,
            )
            .await
        {
            Ok(turn_id) => turn_id,
            Err(err) => {
                let (message, data, error_type) = match err {
                    SteerInputError::NoActiveTurn(_) => (
                        "no active turn to steer".to_string(),
                        None,
                        Some(AnalyticsJsonRpcError::TurnSteer(
                            TurnSteerRequestError::NoActiveTurn,
                        )),
                    ),
                    SteerInputError::ExpectedTurnMismatch { expected, actual } => (
                        format!("expected active turn id `{expected}` but found `{actual}`"),
                        None,
                        Some(AnalyticsJsonRpcError::TurnSteer(
                            TurnSteerRequestError::ExpectedTurnMismatch,
                        )),
                    ),
                    SteerInputError::ActiveTurnNotSteerable { turn_kind } => {
                        let (message, turn_steer_error) = match turn_kind {
                            codex_protocol::protocol::NonSteerableTurnKind::Review => (
                                "cannot steer a review turn".to_string(),
                                TurnSteerRequestError::NonSteerableReview,
                            ),
                            codex_protocol::protocol::NonSteerableTurnKind::Compact => (
                                "cannot steer a compact turn".to_string(),
                                TurnSteerRequestError::NonSteerableCompact,
                            ),
                        };
                        let error = TurnError {
                            message: message.clone(),
                            codex_error_info: Some(CodexErrorInfo::ActiveTurnNotSteerable {
                                turn_kind: turn_kind.into(),
                            }),
                            additional_details: None,
                        };
                        let data = match serde_json::to_value(error) {
                            Ok(data) => Some(data),
                            Err(error) => {
                                tracing::error!(
                                    ?error,
                                    "failed to serialize active-turn-not-steerable turn error"
                                );
                                None
                            }
                        };
                        (
                            message,
                            data,
                            Some(AnalyticsJsonRpcError::TurnSteer(turn_steer_error)),
                        )
                    }
                    SteerInputError::EmptyInput => (
                        "input must not be empty".to_string(),
                        None,
                        Some(AnalyticsJsonRpcError::Input(InputError::Empty)),
                    ),
                };
                let mut error = invalid_request(message);
                error.data = data;
                self.track_error_response(request_id, &error, error_type);
                return Err(error);
            }
        };
        Ok(TurnSteerResponse { turn_id })
    }

    async fn prepare_realtime_conversation_thread(
        &self,
        request_id: &ConnectionRequestId,
        thread_id: &str,
    ) -> Result<Option<(ThreadId, Arc<CodexThread>)>, JSONRPCErrorError> {
        let (thread_id, thread) = self.load_thread(thread_id).await?;

        match self
            .ensure_conversation_listener(
                thread_id,
                request_id.connection_id,
                /*raw_events_enabled*/ false,
            )
            .await
        {
            Ok(EnsureConversationListenerResult::Attached) => {}
            Ok(EnsureConversationListenerResult::ConnectionClosed) => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        }

        if !thread.enabled(Feature::RealtimeConversation) {
            return Err(invalid_request(format!(
                "thread {thread_id} does not support realtime conversation"
            )));
        }

        Ok(Some((thread_id, thread)))
    }

    async fn thread_realtime_start_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeStartParams,
    ) -> Result<Option<ThreadRealtimeStartResponse>, JSONRPCErrorError> {
        let Some((_, thread)) = self
            .prepare_realtime_conversation_thread(request_id, &params.thread_id)
            .await?
        else {
            return Ok(None);
        };
        self.submit_core_op(
            request_id,
            thread.as_ref(),
            Op::RealtimeConversationStart(ConversationStartParams {
                client_managed_handoffs: params.client_managed_handoffs.unwrap_or(false),
                flush_transcript_tail_on_session_end: params
                    .flush_transcript_tail_on_session_end
                    .unwrap_or(false),
                codex_responses_as_items: params.codex_responses_as_items.unwrap_or(false),
                codex_response_item_prefix: params.codex_response_item_prefix,
                codex_response_handoff_mode: params.codex_response_handoff_mode.unwrap_or_default(),
                codex_response_handoff_channel_prefixes: params
                    .codex_response_handoff_channel_prefixes,
                model: params.model,
                output_modality: params.output_modality,
                include_startup_context: params.include_startup_context.unwrap_or(true),
                initial_items: params
                    .initial_items
                    .unwrap_or_default()
                    .into_iter()
                    .map(|item| ConversationTextParams {
                        text: item.text,
                        role: item.role,
                    })
                    .collect(),
                prompt: params.prompt,
                realtime_session_id: params.realtime_session_id,
                transport: params.transport.map(|transport| match transport {
                    ThreadRealtimeStartTransport::Websocket => {
                        ConversationStartTransport::Websocket
                    }
                    ThreadRealtimeStartTransport::Webrtc { sdp } => {
                        ConversationStartTransport::Webrtc { sdp }
                    }
                }),
                version: params.version,
                voice: params.voice,
            }),
        )
        .await
        .map_err(|err| internal_error(format!("failed to start realtime conversation: {err}")))?;
        Ok(Some(ThreadRealtimeStartResponse::default()))
    }

    async fn thread_realtime_append_audio_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeAppendAudioParams,
    ) -> Result<Option<ThreadRealtimeAppendAudioResponse>, JSONRPCErrorError> {
        let Some((_, thread)) = self
            .prepare_realtime_conversation_thread(request_id, &params.thread_id)
            .await?
        else {
            return Ok(None);
        };
        self.submit_core_op(
            request_id,
            thread.as_ref(),
            Op::RealtimeConversationAudio(ConversationAudioParams {
                frame: params.audio.into(),
            }),
        )
        .await
        .map_err(|err| {
            internal_error(format!(
                "failed to append realtime conversation audio: {err}"
            ))
        })?;
        Ok(Some(ThreadRealtimeAppendAudioResponse::default()))
    }

    async fn thread_realtime_append_text_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeAppendTextParams,
    ) -> Result<Option<ThreadRealtimeAppendTextResponse>, JSONRPCErrorError> {
        let Some((_, thread)) = self
            .prepare_realtime_conversation_thread(request_id, &params.thread_id)
            .await?
        else {
            return Ok(None);
        };
        self.submit_core_op(
            request_id,
            thread.as_ref(),
            Op::RealtimeConversationText(ConversationTextParams {
                text: params.text,
                role: params.role,
            }),
        )
        .await
        .map_err(|err| {
            internal_error(format!(
                "failed to append realtime conversation text: {err}"
            ))
        })?;
        Ok(Some(ThreadRealtimeAppendTextResponse::default()))
    }

    async fn thread_realtime_append_speech_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeAppendSpeechParams,
    ) -> Result<Option<ThreadRealtimeAppendSpeechResponse>, JSONRPCErrorError> {
        let Some((_, thread)) = self
            .prepare_realtime_conversation_thread(request_id, &params.thread_id)
            .await?
        else {
            return Ok(None);
        };
        self.submit_core_op(
            request_id,
            thread.as_ref(),
            Op::RealtimeConversationSpeech(ConversationSpeechParams { text: params.text }),
        )
        .await
        .map_err(|err| {
            internal_error(format!(
                "failed to append realtime conversation speech: {err}"
            ))
        })?;
        Ok(Some(ThreadRealtimeAppendSpeechResponse::default()))
    }

    async fn thread_realtime_stop_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeStopParams,
    ) -> Result<Option<ThreadRealtimeStopResponse>, JSONRPCErrorError> {
        let Some((_, thread)) = self
            .prepare_realtime_conversation_thread(request_id, &params.thread_id)
            .await?
        else {
            return Ok(None);
        };
        self.submit_core_op(request_id, thread.as_ref(), Op::RealtimeConversationClose)
            .await
            .map_err(|err| {
                internal_error(format!("failed to stop realtime conversation: {err}"))
            })?;
        Ok(Some(ThreadRealtimeStopResponse::default()))
    }

    fn build_review_turn(turn_id: String, display_text: &str) -> Turn {
        let items = if display_text.is_empty() {
            Vec::new()
        } else {
            vec![ThreadItem::UserMessage {
                id: turn_id.clone(),
                client_id: None,
                content: vec![V2UserInput::Text {
                    text: display_text.to_string(),
                    // Review prompt display text is synthesized; no UI element ranges to preserve.
                    text_elements: Vec::new(),
                }],
            }]
        };

        Turn {
            id: turn_id,
            items,
            items_view: TurnItemsView::NotLoaded,
            error: None,
            status: TurnStatus::InProgress,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        }
    }

    async fn emit_review_started(
        &self,
        request_id: &ConnectionRequestId,
        turn: Turn,
        review_thread_id: String,
    ) {
        let response = ReviewStartResponse {
            turn,
            review_thread_id,
        };
        self.outgoing
            .send_response(request_id.clone(), response)
            .await;
    }

    async fn start_inline_review(
        &self,
        request_id: &ConnectionRequestId,
        parent_thread: Arc<CodexThread>,
        review_request: ReviewRequest,
        display_text: &str,
        parent_thread_id: String,
    ) -> std::result::Result<(), JSONRPCErrorError> {
        let turn_id = self
            .submit_core_op(
                request_id,
                parent_thread.as_ref(),
                Op::Review { review_request },
            )
            .await
            .map_err(|err| internal_error(format!("failed to start review: {err}")))?;
        let turn = Self::build_review_turn(turn_id, display_text);
        self.emit_review_started(request_id, turn, parent_thread_id)
            .await;
        Ok(())
    }

    async fn start_detached_review(
        &self,
        request_id: &ConnectionRequestId,
        parent_thread: Arc<CodexThread>,
        prompt: &str,
    ) -> std::result::Result<(), JSONRPCErrorError> {
        // AgentRunner::start still delegates to spawn_subagent, which forks from the parent's
        // full history. Paginated threads only allow bounded model-context reads, so keep this
        // closed until detached review has a bounded fork path.
        if matches!(
            parent_thread.config_snapshot().await.history_mode,
            codex_protocol::protocol::ThreadHistoryMode::Paginated
        ) {
            return Err(invalid_request(
                "paginated threads do not support detached review",
            ));
        }
        let mut config = self.config.as_ref().clone();
        if let Some(review_model) = &config.review_model {
            config.model = Some(review_model.clone());
        }

        let AgentRun {
            thread_id,
            thread: review_thread,
            turn_id,
        } = self
            .agent_runner
            .start(
                parent_thread.session_configured().thread_id,
                AgentInvocation {
                    config,
                    prompt: prompt.to_string(),
                    parent_trace: self.request_trace_context(request_id).await,
                },
            )
            .await
            .map_err(|err| internal_error(format!("failed to start detached review: {err}")))?;

        let fallback_provider = self.config.model_provider_id.as_str();
        let stored_thread = match review_thread
            .read_thread(
                /*include_archived*/ true, /*include_history*/ false,
            )
            .await
        {
            Ok(stored_thread) => {
                let (thread, _) =
                    thread_from_stored_thread(stored_thread, fallback_provider, &self.config.cwd);
                Some(thread)
            }
            Err(err) => {
                tracing::warn!("failed to load summary for review thread {thread_id}: {err}");
                None
            }
        };

        if let Some(mut thread) = stored_thread {
            thread.session_id = review_thread.session_configured().session_id.to_string();
            self.thread_watch_manager
                .upsert_thread_silently(&thread.id)
                .await;
            thread.status = resolve_thread_status(
                self.thread_watch_manager
                    .loaded_status_for_thread(&thread.id)
                    .await,
                /*has_in_progress_turn*/ false,
            );
            let notif = thread_started_notification(thread);
            self.outgoing
                .send_server_notification(ServerNotification::ThreadStarted(notif))
                .await;
        }

        log_listener_attach_result(
            self.ensure_conversation_listener(
                thread_id,
                request_id.connection_id,
                /*raw_events_enabled*/ false,
            )
            .await,
            thread_id,
            request_id.connection_id,
            "review thread",
        );

        let turn = Self::build_review_turn(turn_id, prompt);
        let review_thread_id = thread_id.to_string();
        self.emit_review_started(request_id, turn, review_thread_id)
            .await;

        Ok(())
    }

    async fn review_start_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: ReviewStartParams,
    ) -> Result<(), JSONRPCErrorError> {
        let ReviewStartParams {
            thread_id,
            target,
            delivery,
        } = params;

        let (_, parent_thread) = self.load_thread(&thread_id).await?;
        let (review_request, display_text, target_prompt) =
            Self::review_request_from_target(target)?;
        match delivery.unwrap_or(ApiReviewDelivery::Inline).to_core() {
            CoreReviewDelivery::Inline => {
                self.start_inline_review(
                    request_id,
                    parent_thread,
                    review_request,
                    &display_text,
                    thread_id,
                )
                .await?;
            }
            CoreReviewDelivery::Detached => {
                let review_skill_path = system_cache_root_dir(&self.config.codex_home)
                    .join("review-agent")
                    .join("SKILL.md");
                let prompt = format!(
                    "Use [$review-agent]({}) for this review.\n\n{target_prompt}",
                    review_skill_path.display()
                );
                let actual_chars = prompt.chars().count();
                if actual_chars > MAX_USER_INPUT_TEXT_CHARS {
                    return Err(Self::input_too_large_error(actual_chars));
                }
                self.start_detached_review(request_id, parent_thread, &prompt)
                    .await?;
            }
        }
        Ok(())
    }

    async fn turn_interrupt_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: TurnInterruptParams,
    ) -> Result<Option<TurnInterruptResponse>, JSONRPCErrorError> {
        let TurnInterruptParams { thread_id, turn_id } = params;
        let is_startup_interrupt = turn_id.is_empty();

        let (thread_uuid, thread) = self.load_thread(&thread_id).await?;

        // Record turn interrupts so we can reply when TurnAborted arrives. Startup
        // interrupts do not have a turn and are acknowledged after submission.
        if !is_startup_interrupt {
            let thread_state = self.thread_state_manager.thread_state(thread_uuid).await;
            let is_running = matches!(thread.agent_status().await, AgentStatus::Running);
            {
                let mut thread_state = thread_state.lock().await;
                if let Some(active_turn) = thread_state.active_turn_snapshot() {
                    if active_turn.id != turn_id {
                        return Err(invalid_request(format!(
                            "expected active turn id {turn_id} but found {}",
                            active_turn.id
                        )));
                    }
                } else if thread_state.last_terminal_turn_id.as_deref() == Some(turn_id.as_str())
                    || !is_running
                {
                    return Err(invalid_request("no active turn to interrupt"));
                }
                thread_state.pending_interrupts.push(request_id.clone());
            }

            self.outgoing
                .record_request_turn_id(request_id, &turn_id)
                .await;
        }

        // Submit the interrupt. Turn interrupts respond upon TurnAborted; startup
        // interrupts respond here because startup cancellation has no turn event.
        match self
            .submit_core_op(request_id, thread.as_ref(), Op::Interrupt)
            .await
        {
            Ok(_) if is_startup_interrupt => Ok(Some(TurnInterruptResponse {})),
            Ok(_) => Ok(None),
            Err(err) => {
                if !is_startup_interrupt {
                    let thread_state = self.thread_state_manager.thread_state(thread_uuid).await;
                    let mut thread_state = thread_state.lock().await;
                    thread_state
                        .pending_interrupts
                        .retain(|pending_request_id| pending_request_id != request_id);
                }
                let interrupt_target = if is_startup_interrupt {
                    "startup"
                } else {
                    "turn"
                };
                Err(internal_error(format!(
                    "failed to interrupt {interrupt_target}: {err}"
                )))
            }
        }
    }

    fn listener_task_context(&self) -> ListenerTaskContext {
        ListenerTaskContext {
            thread_manager: Arc::clone(&self.thread_manager),
            thread_state_manager: self.thread_state_manager.clone(),
            outgoing: Arc::clone(&self.outgoing),
            pending_thread_unloads: Arc::clone(&self.pending_thread_unloads),
            thread_watch_manager: self.thread_watch_manager.clone(),
            thread_list_state_permit: self.thread_list_state_permit.clone(),
            fallback_model_provider: self.config.model_provider_id.clone(),
            codex_home: self.config.codex_home.to_path_buf(),
            skills_watcher: Arc::clone(&self.skills_watcher),
        }
    }

    async fn ensure_conversation_listener(
        &self,
        conversation_id: ThreadId,
        connection_id: ConnectionId,
        raw_events_enabled: bool,
    ) -> Result<EnsureConversationListenerResult, JSONRPCErrorError> {
        super::thread_lifecycle::ensure_conversation_listener(
            self.listener_task_context(),
            conversation_id,
            connection_id,
            raw_events_enabled,
        )
        .await
    }
}

fn is_canonical_automatic_turn_input(input: &[V2UserInput]) -> bool {
    matches!(
        input,
        [V2UserInput::Text {
            text,
            text_elements,
        }] if text == "continue" && text_elements.is_empty()
    )
}

fn is_automatic_turn_capability(client_user_message_id: Option<&str>) -> bool {
    client_user_message_id.is_some_and(|client_id| {
        AutomaticTurnProvenance::decode_client_user_message_id(client_id).is_some()
    })
}

fn validate_automatic_turn_start_shape(
    params: &TurnStartParams,
) -> Result<bool, JSONRPCErrorError> {
    let Some(client_user_message_id) = params.client_user_message_id.as_deref() else {
        return Ok(false);
    };
    if AutomaticTurnProvenance::decode_client_user_message_id(client_user_message_id).is_none() {
        return Ok(false);
    }
    let canonical = is_canonical_automatic_turn_input(&params.input)
        && params.responsesapi_client_metadata.is_none()
        && params.additional_context.is_none()
        && params.output_schema.is_none()
        && params.multi_agent_mode.is_none();
    if !canonical {
        return Err(invalid_request(
            "automatic turn retry must use the server-canonical continue operation",
        ));
    }
    Ok(true)
}

fn automatic_turn_start_settings_match_current(
    params: &TurnStartParams,
    snapshot: &ThreadConfigSnapshot,
    processor: &TurnRequestProcessor,
) -> bool {
    let cwd_matches = params
        .cwd
        .as_ref()
        .is_none_or(|cwd| cwd.as_path() == snapshot.cwd().as_path());
    let roots_match = params
        .runtime_workspace_roots
        .as_ref()
        .is_none_or(|roots| roots == &snapshot.workspace_roots);
    let approval_matches = params
        .approval_policy
        .as_ref()
        .is_none_or(|approval| approval.clone().to_core() == snapshot.approval_policy);
    let reviewer_matches = params
        .approvals_reviewer
        .as_ref()
        .is_none_or(|reviewer| reviewer.to_core() == snapshot.approvals_reviewer);
    let sandbox_matches = params
        .sandbox_policy
        .as_ref()
        .is_none_or(|sandbox| sandbox.to_core() == snapshot.sandbox_policy());
    let permission_matches = params.permissions.as_ref().is_none_or(|permission| {
        snapshot
            .active_permission_profile
            .as_ref()
            .is_some_and(|active| active.id == *permission)
    });
    let model_matches = params
        .model
        .as_ref()
        .is_none_or(|model| model == &snapshot.model);
    let service_tier_matches = params
        .service_tier
        .as_ref()
        .is_none_or(|service_tier| service_tier == &snapshot.service_tier);
    let effort_matches = params
        .effort
        .as_ref()
        .is_none_or(|effort| snapshot.reasoning_effort.as_ref() == Some(effort));
    let summary_matches = params
        .summary
        .as_ref()
        .is_none_or(|summary| snapshot.reasoning_summary.as_ref() == Some(summary));
    let personality_matches = params
        .personality
        .as_ref()
        .is_none_or(|personality| snapshot.personality.as_ref() == Some(personality));
    let collaboration_matches = params.collaboration_mode.as_ref().is_none_or(|mode| {
        processor.normalize_collaboration_mode(mode.clone()) == snapshot.collaboration_mode
    });

    // Environment selections and arbitrary context/output changes are not part of the retry
    // contract. Omitted fields use the server's already-bound selections; supplied values are
    // rejected unless they are one of the directly comparable current settings above.
    params.environments.is_none()
        && params.responsesapi_client_metadata.is_none()
        && params.additional_context.is_none()
        && params.output_schema.is_none()
        && params.multi_agent_mode.is_none()
        && cwd_matches
        && roots_match
        && approval_matches
        && reviewer_matches
        && sandbox_matches
        && permission_matches
        && model_matches
        && service_tier_matches
        && effort_matches
        && summary_matches
        && personality_matches
        && collaboration_matches
}

#[cfg(test)]
mod automatic_turn_validation_tests {
    use super::*;
    use codex_protocol::ThreadId;

    fn client_user_message_id() -> String {
        AutomaticTurnProvenance::policy_retry(
            ThreadId::new(),
            "policy-turn",
            /*attempt*/ 1,
            /*max_attempts*/ 3,
            "capability",
        )
        .expect("valid provenance")
        .to_client_user_message_id()
        .expect("valid client message id")
    }

    #[test]
    fn automatic_turn_start_shape_allows_server_current_settings() {
        let mut params = TurnStartParams {
            client_user_message_id: Some(client_user_message_id()),
            input: vec![V2UserInput::Text {
                text: "continue".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        };
        assert!(validate_automatic_turn_start_shape(&params).is_ok_and(|automatic| automatic));

        params.model = Some("different-model".to_string());
        assert!(validate_automatic_turn_start_shape(&params).is_ok_and(|automatic| automatic));
    }

    #[test]
    fn automatic_turn_capability_is_not_accepted_by_steer() {
        assert!(is_automatic_turn_capability(
            Some(&client_user_message_id())
        ));
        assert!(!is_automatic_turn_capability(
            /*client_user_message_id*/ None
        ));
        assert!(!is_automatic_turn_capability(Some("ordinary-client-id")));
    }
}

fn xcode_26_4_mcp_elicitations_auto_deny(
    client_name: Option<&str>,
    client_version: Option<&str>,
) -> bool {
    // Xcode 26.4 shipped before app-server MCP elicitation requests were
    // client-visible. Keep elicitations auto-denied for that client line.
    // TODO: Remove this compatibility hack once Xcode 26.4 ages out.
    client_name == Some("Xcode")
        && client_version.is_some_and(|version| version.starts_with("26.4"))
}
