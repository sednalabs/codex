//! Exercises streamed remote compaction, including the retained checkpoint and continuing history.

use anyhow::Context;
use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_core::StartThreadOptions;
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_history::CodexHarnessMetadata;
use codex_history::InitialHistory;
use codex_history::RolloutItem;
use codex_login::CodexAuth;
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
use codex_login::auth::AgentIdentityAuth;
use codex_login::auth::AgentIdentityAuthRecord;
use codex_protocol::account::PlanType as AccountPlanType;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::items::TurnItem;
=======
use codex_login::auth::BedrockApiKeyAuth;
use codex_model_provider_info::AMAZON_BEDROCK_GPT_5_5_MODEL_ID;
use codex_model_provider_info::AMAZON_BEDROCK_PROVIDER_ID;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::AgentPath;
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
use codex_protocol::models::ContentItem;
use codex_protocol::models::ImageReference;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ConversationStartParams;
use codex_protocol::protocol::ConversationTextParams;
use codex_protocol::protocol::ConversationTextRole;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RealtimeConversationRealtimeEvent;
use codex_protocol::protocol::RealtimeConversationVersion;
use codex_protocol::protocol::RealtimeEvent;
use codex_protocol::protocol::RealtimeOutputModality;
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::TokenUsage;
=======
use codex_protocol::protocol::ThreadHistoryMode;
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::start_websocket_server;
use core_test_support::run_large_stack_test;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodexBuilder;
use core_test_support::test_codex::TestCodexHarness;
use core_test_support::test_codex::test_codex as base_test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
use core_test_support::wait_for_event_with_timeout;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::fs;
use std::path::Path;
use test_case::test_case;
use tokio::time::Duration;
use wiremock::ResponseTemplate;

#[path = "compact_remote_trimming.rs"]
mod trimming;

const DUMMY_FUNCTION_NAME: &str = "test_tool";
const TURN_STATE_HEADER: &str = "x-codex-turn-state";
const REMOTE_COMPACT_TURN_COMPLETE_TIMEOUT: Duration = Duration::from_secs(30);
const CONTEXT_WINDOW_TRUNCATED_OUTPUT_MESSAGE: &str =
    "Output exceeded the available model context and was truncated";

fn test_codex() -> TestCodexBuilder {
    base_test_codex().with_config(|config| {
        config.update_plan_enabled = true;
    })
}

fn remote_realtime_test_codex_builder(
    realtime_server: &responses::WebSocketTestServer,
) -> TestCodexBuilder {
    let realtime_base_url = realtime_server.uri().to_string();
    test_codex()
        .with_auth(CodexAuth::from_api_key("dummy"))
        .with_config(move |config| {
            config.experimental_realtime_ws_base_url = Some(realtime_base_url);
        })
}

async fn start_remote_realtime_server() -> responses::WebSocketTestServer {
    start_websocket_server(vec![vec![
        vec![json!({
            "type": "session.updated",
            "session": { "id": "sess_remote_compact", "instructions": "backend prompt" }
        })],
        // Keep the websocket open after startup so routed transcript items during the test do not
        // exhaust the scripted responses and mark realtime inactive before the assertions run.
        vec![],
        vec![],
        vec![],
        vec![],
        vec![],
        vec![],
        vec![],
        vec![],
    ]])
    .await
}

async fn start_realtime_conversation(codex: &codex_core::CodexThread) -> Result<()> {
    start_realtime_conversation_with_initial_items(codex, Vec::new()).await
}

async fn start_realtime_conversation_with_initial_items(
    codex: &codex_core::CodexThread,
    initial_items: Vec<ConversationTextParams>,
) -> Result<()> {
    let version = (!initial_items.is_empty()).then_some(RealtimeConversationVersion::V3);
    codex
        .submit(Op::RealtimeConversationStart(ConversationStartParams {
            client_managed_handoffs: false,
            delegation_ack_filler: None,
            flush_transcript_tail_on_session_end: false,
            codex_responses_as_items: false,
            codex_response_item_prefix: None,
            codex_response_handoff_mode:
                codex_protocol::protocol::CodexResponseHandoffMode::Thinking,
            codex_response_handoff_channel_prefixes: None,
            model: None,
            output_modality: RealtimeOutputModality::Audio,
            include_startup_context: true,
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
            initial_items,
=======
            initial_items: Vec::new(),
            realtime_start_instructions: None,
            realtime_end_instructions: None,
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
            prompt: Some(Some("backend prompt".to_string())),
            realtime_session_id: None,
            transport: None,
            version,
            voice: None,
        }))
        .await?;

    wait_for_event_match(codex, |msg| match msg {
        EventMsg::RealtimeConversationStarted(started) => Some(Ok(started.clone())),
        EventMsg::Error(err) => Some(Err(err.clone())),
        _ => None,
    })
    .await
    .expect("conversation start failed");

    wait_for_event_match(codex, |msg| match msg {
        EventMsg::RealtimeConversationRealtime(RealtimeConversationRealtimeEvent {
            payload:
                RealtimeEvent::SessionUpdated {
                    realtime_session_id: session_id,
                    ..
                },
        }) => Some(session_id.clone()),
        _ => None,
    })
    .await;

    Ok(())
}

async fn close_realtime_conversation(codex: &codex_core::CodexThread) -> Result<()> {
    codex.submit(Op::RealtimeConversationClose).await?;
    wait_for_event_match(codex, |msg| match msg {
        EventMsg::RealtimeConversationClosed(closed) => Some(closed.clone()),
        _ => None,
    })
    .await;
    Ok(())
}

fn assert_request_contains_custom_realtime_start(
    request: &responses::ResponsesRequest,
    instructions: &str,
) {
    let body = request.body_json().to_string();
    assert!(
        body.contains("<realtime_conversation>"),
        "expected request to preserve the realtime wrapper"
    );
    assert!(
        body.contains(instructions),
        "expected request to use custom realtime start instructions"
    );
    assert!(
        !body.contains("Realtime conversation started."),
        "expected request to replace the default realtime start instructions"
    );
}

async fn wait_for_turn_complete(codex: &codex_core::CodexThread) {
    wait_for_event_with_timeout(
        codex,
        |ev| matches!(ev, EventMsg::TurnComplete(_)),
        REMOTE_COMPACT_TURN_COMPLETE_TIMEOUT,
    )
    .await;
}

fn amazon_bedrock_test_codex() -> TestCodexBuilder {
    let auth = CodexAuth::BedrockApiKey(BedrockApiKeyAuth {
        api_key: "bedrock-test-api-key".to_string(),
        region: "us-east-1".to_string(),
    });
    test_codex()
        .with_auth(auth)
        .with_model(AMAZON_BEDROCK_GPT_5_5_MODEL_ID)
        .with_config(|config| {
            config.model_provider = ModelProviderInfo {
                base_url: config.model_provider.base_url.clone(),
                ..ModelProviderInfo::create_amazon_bedrock_provider(/*aws*/ None)
            };
            config.model_provider_id = AMAZON_BEDROCK_PROVIDER_ID.to_string();
        })
}

fn is_retained_user_message(item: &ResponseItem, retained_text: &str) -> bool {
    matches!(
        item,
        ResponseItem::Message { role, content, .. }
            if role == "user"
                && content.iter().any(|item| {
                    matches!(item, ContentItem::InputText { text } if text == retained_text)
                })
    )
}

fn annotate_retained_user_in_rollout(path: &Path, retained_text: &str) -> Result<()> {
    let mut rollout = fs::read_to_string(path)?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(codex_rollout::parse_rollout_line)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    rollout
        .iter_mut()
        .find_map(|line| match &mut line.item {
            RolloutItem::ResponseItem(envelope)
                if is_retained_user_message(&envelope.item, retained_text) =>
            {
                Some(envelope)
            }
            _ => None,
        })
        .context("persisted user response missing from rollout")?
        .metadata = Some(CodexHarnessMetadata::default());

    let contents = rollout
        .iter()
        .map(serde_json::to_string)
        .collect::<std::result::Result<Vec<_>, _>>()?
        .join("\n");
    fs::write(path, format!("{contents}\n"))?;
    Ok(())
}

fn assert_compacted_user_metadata(path: &Path, retained_text: &str) -> Result<()> {
    let replacement_history = fs::read_to_string(path)?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(codex_rollout::parse_rollout_line)
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .rev()
        .find_map(|line| match line.item {
            RolloutItem::Compacted(compacted) => compacted.replacement_history,
            _ => None,
        })
        .context("compacted replacement history missing")?;
    assert!(
        replacement_history.iter().any(|envelope| {
            is_retained_user_message(&envelope.item, retained_text) && envelope.metadata.is_some()
        }),
        "compacted user message should retain its aligned harness metadata"
    );
    Ok(())
}

fn assert_compact_request_omits_harness_metadata(request: &responses::ResponsesRequest) {
    for item in request.input() {
        assert!(
            item.get("metadata").is_none() && item.get("replacement_history_metadata").is_none(),
            "provider request must not receive harness history metadata: {item}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_compact_v2_retains_metadata_from_resumed_history() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = wiremock::MockServer::start().await;
    let retained_text = "annotated v2 user message";
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                responses::ev_assistant_message("message-1", "before compaction"),
                responses::ev_completed("response-1"),
            ]),
            sse(vec![
                json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "compaction",
                        "encrypted_content": "ANNOTATED_V2_COMPACTION_SUMMARY",
                    },
                }),
                responses::ev_completed("response-compact"),
            ]),
            sse(vec![responses::ev_completed("response-after")]),
        ],
    )
    .await;

    let builder = || test_codex().with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    let initial = builder()
        .with_pre_build_hook(|home| {
            fs::write(
                home.join("config.toml"),
                "[features]\nremote_compaction_v2 = false\n",
            )
            .expect("write saved config");
        })
        .build(&server)
        .await?;
    initial.submit_turn(retained_text).await?;
    let home = initial.home.clone();
    let rollout_path = initial
        .session_configured
        .rollout_path
        .clone()
        .context("rollout path")?;
    initial.codex.shutdown_and_wait().await?;
    annotate_retained_user_in_rollout(&rollout_path, retained_text)?;

    let resumed = builder()
        .resume(&server, home, rollout_path.clone())
        .await?;
    resumed.codex.submit(Op::Compact).await?;
    wait_for_turn_complete(&resumed.codex).await;
    resumed.submit_turn("continue after compaction").await?;
    resumed.codex.shutdown_and_wait().await?;

    let requests = response_mock.requests();
    let compact_request = &requests[1];
    assert_eq!(
        compact_request.inputs_of_type("compaction_trigger").len(),
        1
    );
    assert_compact_request_omits_harness_metadata(compact_request);
    assert_compacted_user_metadata(&rollout_path, retained_text)?;
    assert_eq!(
        requests[2].inputs_of_type("compaction")[0]["encrypted_content"],
        "ANNOTATED_V2_COMPACTION_SUMMARY"
    );
    assert!(requests[2].body_contains_text("continue after compaction"));

    Ok(())
}

#[test_case(false; "feature_disabled")]
#[test_case(true; "feature_enabled")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_compact_v2_retains_only_client_developer_messages_when_enabled(
    enabled: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = TestCodexHarness::with_auto_env_builder(
        test_codex()
            .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
            .with_config(move |config| {
                if enabled {
                    config
                        .features
                        .enable(Feature::RetainClientDeveloperMessages)
                        .expect("client developer retention should be configurable");
                }
            }),
    )
    .await?;
    let developer = |text: &str| ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let codex = &harness.test().codex;
    let rollout_path = codex.rollout_path().context("rollout path")?;
    codex
        .inject_response_items(vec![developer("INJECTED_CLIENT_DEVELOPER")])
        .await?;
    let response_mock = responses::mount_sse_sequence(
        harness.server(),
        vec![
            sse(vec![responses::ev_completed("before-compact")]),
            sse(vec![
                json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "compaction",
                        "encrypted_content": "CLIENT_RETENTION_SUMMARY",
                    },
                }),
                responses::ev_completed("compact"),
            ]),
            sse(vec![responses::ev_completed("after-compact")]),
        ],
    )
    .await;

    harness.test().submit_turn("before compact").await?;
    codex.submit(Op::Compact).await?;
    wait_for_turn_complete(codex).await;
    harness.test().submit_turn("after compact").await?;

    let requests = response_mock.requests();
    let follow_up = requests.last().context("follow-up request")?;
    assert_eq!(
        follow_up.body_contains_text("INJECTED_CLIENT_DEVELOPER"),
        enabled
    );
    requests
        .iter()
        .for_each(assert_compact_request_omits_harness_metadata);

    codex.shutdown_and_wait().await?;
    let replacement_history = fs::read_to_string(&rollout_path)?
        .lines()
        .filter_map(|line| codex_rollout::parse_rollout_line(line).ok())
        .filter_map(|line| match line.item {
            RolloutItem::Compacted(compacted) => compacted.replacement_history,
            _ => None,
        })
        .next_back()
        .context("remote compaction should persist a checkpoint")?;
    let retained_client_developers = replacement_history
        .iter()
        .filter(|item| {
            item.metadata
                .as_ref()
                .is_some_and(|metadata| metadata.client_authored)
        })
        .count();
    assert_eq!(retained_client_developers, usize::from(enabled));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_compact_v2_records_usage_before_output_validation() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = TestCodexHarness::with_auto_env_builder(
        test_codex()
            .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
            .with_config(|config| {
                config.model_auto_compact_token_limit = Some(200);
            }),
    )
    .await?;
    let codex = &harness.test().codex;
    let rollout_path = codex.rollout_path().context("rollout path")?;
    let responses_mock = responses::mount_sse_sequence(
        harness.server(),
        vec![
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
            responses::sse(vec![
                responses::ev_assistant_message("m1", "FIRST_REMOTE_REPLY"),
                responses::ev_completed_with_tokens("resp-1", /*total_tokens*/ 9),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("m2", "AFTER_COMPACT_REPLY"),
                responses::ev_completed("resp-2"),
=======
            sse(vec![responses::ev_completed_with_tokens(
                "before-compact",
                /*total_tokens*/ 500,
            )]),
            sse(vec![
                json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "compaction",
                        "encrypted_content": "FIRST_SUMMARY",
                    },
                }),
                json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "compaction",
                        "encrypted_content": "SECOND_SUMMARY",
                    },
                }),
                responses::ev_completed_with_tokens("invalid-compact", /*total_tokens*/ 8_200),
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
            ]),
        ],
    )
    .await;

    harness.test().submit_turn("before compact").await?;
    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "turn that triggers auto compact".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let mut preserved_prompts = Vec::new();
    wait_for_event(codex, |event| {
        if let EventMsg::UserMessage(message) = event {
            preserved_prompts.push(message.message.clone());
        }
        matches!(event, EventMsg::Error(_))
    })
    .await;
    assert_eq!(preserved_prompts, vec!["turn that triggers auto compact"]);
    wait_for_event(codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    assert_eq!(responses_mock.requests().len(), 2);

    codex.flush_rollout().await?;
    let history = codex.load_history(/*include_archived*/ false).await?;
    assert_eq!(
        history
            .items
            .iter()
            .filter(|item| matches!(
                item,
                RolloutItem::ResponseItem(envelope)
                    if is_retained_user_message(
                        &envelope.item,
                        "turn that triggers auto compact",
                    )
            ))
            .count(),
        1,
        "the accepted prompt should be saved exactly once after compaction fails"
    );
    codex.shutdown_and_wait().await?;

    let record = fs::read_to_string(&rollout_path)?
        .lines()
        .filter_map(|line| codex_rollout::parse_rollout_line(line).ok())
        .filter_map(|line| match line.item {
            RolloutItem::TokenUsageRecord(record) => Some(record),
            _ => None,
        })
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
        .await?;
    let first_turn_complete = wait_for_event_match(&codex, |event| match event {
        EventMsg::TurnComplete(event) => Some(event.clone()),
        _ => None,
    })
    .await;
    assert_eq!(
        first_turn_complete.provider_usage,
        Some(TokenUsage {
            input_tokens: 9,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 0,
            reasoning_output_tokens: 0,
            total_tokens: 9,
        })
    );

    codex.submit(Op::Compact).await?;
    wait_for_turn_complete(&codex).await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "after compact".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    let post_compact_turn_complete = wait_for_event_match(&codex, |event| match event {
        EventMsg::TurnComplete(event) => Some(event.clone()),
        _ => None,
    })
    .await;
    assert_eq!(
        post_compact_turn_complete.provider_usage,
        Some(TokenUsage::default())
    );

    let compact_request = compact_mock.single_request();
    assert_eq!(compact_request.path(), "/v1/responses/compact");
    assert_eq!(
        compact_request.header("chatgpt-account-id").as_deref(),
        Some("account_id")
    );
    assert_eq!(
        compact_request.header("authorization").as_deref(),
        Some("Bearer Access Token")
    );
    assert_eq!(
        compact_request.header("session-id").as_deref(),
        Some(session_id.as_str())
    );
    assert_eq!(
        compact_request.header("thread-id").as_deref(),
        Some(thread_id.as_str())
    );
    let compact_metadata: Value = serde_json::from_str(
        &compact_request
            .header("x-codex-turn-metadata")
            .expect("remote compact request should include turn metadata"),
    )
    .expect("remote compact turn metadata should be valid json");
    assert_eq!(
        compact_request.header("x-codex-installation-id").as_deref(),
        compact_metadata["installation_id"].as_str()
    );
    assert!(
        compact_metadata["turn_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "remote compact turn metadata should include its turn id"
    );
    assert_eq!(
        compact_metadata["request_kind"].as_str(),
        Some("compaction")
    );
    assert_eq!(
        compact_metadata["window_id"].as_str(),
        compact_request.header("x-codex-window-id").as_deref()
    );
    assert_eq!(
        compact_metadata["compaction"],
        json!({
            "trigger": "manual",
            "reason": "user_requested",
            "implementation": "responses_compact",
            "phase": "standalone_turn",
            "strategy": "memento",
        })
    );
    let compact_body = compact_request.body_json();
    assert_eq!(
        compact_body.get("model").and_then(|v| v.as_str()),
        Some(harness.test().session_configured.model.as_str())
    );
    let response_requests = responses_mock.requests();
    let first_response_request = response_requests.first().expect("initial request missing");
    let first_response_metadata: Value = serde_json::from_str(
        &first_response_request
            .header("x-codex-turn-metadata")
            .expect("initial request should include turn metadata"),
    )
    .expect("initial turn metadata should be valid json");
    assert_ne!(
        first_response_metadata["turn_id"], compact_metadata["turn_id"],
        "manual compaction should use its own turn id"
    );
    assert_eq!(
        compact_body["tools"],
        first_response_request.body_json()["tools"],
        "compact requests should send the same tools payload as /v1/responses"
    );
    assert_eq!(
        compact_body["parallel_tool_calls"],
        first_response_request.body_json()["parallel_tool_calls"],
        "compact requests should match /v1/responses parallel_tool_calls"
    );
    assert_eq!(
        compact_body["reasoning"],
        first_response_request.body_json()["reasoning"],
        "compact requests should match /v1/responses reasoning"
    );
    assert_eq!(
        compact_body["text"],
        first_response_request.body_json()["text"],
        "compact requests should match /v1/responses text controls"
    );
    let compact_body_text = compact_body.to_string();
    assert!(
        compact_body_text.contains("hello remote compact"),
        "expected compact request to include user history"
    );
    assert!(
        compact_body_text.contains("FIRST_REMOTE_REPLY"),
        "expected compact request to include assistant history"
    );

    let response_requests = responses_mock.requests();
    let follow_up_request = response_requests.last().expect("follow-up request missing");
    let follow_up_metadata: Value = serde_json::from_str(
        &follow_up_request
            .header("x-codex-turn-metadata")
            .expect("follow-up request should include turn metadata"),
    )
    .expect("follow-up turn metadata should be valid json");
    assert_eq!(
        follow_up_metadata["request_kind"].as_str(),
        Some("turn"),
        "regular requests after compaction should remain turn requests"
    );
    assert!(
        follow_up_metadata.get("compaction").is_none(),
        "regular requests after compaction should not be marked as compact requests"
    );
    assert_ne!(
        follow_up_metadata["turn_id"], compact_metadata["turn_id"],
        "the following user turn should not reuse a manual compact turn id"
    );
    assert_eq!(
        follow_up_metadata["window_id"].as_str(),
        follow_up_request.header("x-codex-window-id").as_deref()
    );
    assert_ne!(
        follow_up_metadata["window_id"], compact_metadata["window_id"],
        "the following user turn should use the new compacted context window"
    );
    let follow_up_body = follow_up_request.body_json().to_string();
    assert!(
        follow_up_body.contains("\"type\":\"compaction\""),
        "expected follow-up request to use compacted history"
    );
    assert!(
        follow_up_body.contains("ENCRYPTED_COMPACTION_SUMMARY"),
        "expected follow-up request to include compaction summary item"
    );
    assert!(
        !follow_up_body.contains("FIRST_REMOTE_REPLY"),
        "expected follow-up request to drop pre-compaction assistant messages"
    );
    assert!(
        !follow_up_body.contains("hello remote compact"),
        "expected follow-up request to drop compacted-away user turns when remote output omits them"
    );

    insta::assert_snapshot!(
        "remote_manual_compact_with_history_shapes",
        format_labeled_requests_snapshot(
            "Remote manual /compact where remote compact output is compaction-only: follow-up layout uses the returned compaction item plus new user message.",
            &[
                ("Remote Compaction Request", &compact_request),
                ("Remote Post-Compaction History Layout", follow_up_request),
            ]
        )
    );
=======
        .find(|record| record.response_id == "invalid-compact")
        .context("remote compaction usage record")?;
    assert_eq!(record.usage.total_tokens, 8_200);
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn amazon_bedrock_automatic_compaction_uses_v2_responses_endpoint() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = TestCodexHarness::with_auto_env_builder(amazon_bedrock_test_codex().with_config(
        |config| {
            config.model_auto_compact_token_limit = Some(200);
        },
    ))
    .await?;
    let response_mock = responses::mount_sse_sequence(
        harness.server(),
        vec![
            sse(vec![
                responses::ev_assistant_message("message-1", "before automatic compaction"),
                responses::ev_completed_with_tokens("response-1", /*total_tokens*/ 500),
            ]),
            sse(vec![
                json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "compaction",
                        "encrypted_content": "BEDROCK_AUTOMATIC_REMOTE_COMPACTED_SUMMARY",
                    }
                }),
                responses::ev_completed("response-compact"),
            ]),
            sse(vec![
                responses::ev_assistant_message("message-2", "after automatic compaction"),
                responses::ev_completed("response-2"),
            ]),
        ],
    )
    .await;

    harness
        .test()
        .submit_turn("before automatic compact")
        .await?;
    harness
        .test()
        .submit_turn("after automatic compact")
        .await?;

    let response_requests = response_mock.requests();
    assert_eq!(response_requests.len(), 3);
    assert!(
        response_requests
            .iter()
            .all(|request| request.path() == "/v1/responses")
    );
    let compact_request = &response_requests[1];
    assert_eq!(
        compact_request.header("authorization").as_deref(),
        Some("Bearer bedrock-test-api-key")
    );
    assert_eq!(
        compact_request
            .header("x-amzn-mantle-client-agent")
            .as_deref(),
        Some("codex")
    );
    assert_eq!(
        compact_request.body_json()["model"],
        AMAZON_BEDROCK_GPT_5_5_MODEL_ID
    );
    assert_eq!(
        compact_request.inputs_of_type("compaction_trigger").len(),
        1
    );
    assert!(response_requests[2].input().iter().any(|item| {
        item["type"] == "compaction"
            && item["encrypted_content"] == "BEDROCK_AUTOMATIC_REMOTE_COMPACTED_SUMMARY"
    }));

    Ok(())
}

#[test_case(None; "default_trims_images")]
#[test_case(Some(false); "disabled_preserves_images")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_compact_v2_charges_retained_images_to_token_budget(
    image_budget_enabled: Option<bool>,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = TestCodexHarness::with_auto_env_builder(
        test_codex()
            .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
            .with_config(move |config| {
                let _ = config.features.enable(Feature::UnifiedImageBudget);
                if let Some(enabled) = image_budget_enabled {
                    let _ = config
                        .features
                        .set_enabled(Feature::CompactionImageBudget, enabled);
                }
            }),
    )
    .await?;
    let codex = &harness.test().codex;
    // Each original-detail image costs 10,000 estimated patch tokens.
    let image_inputs = (1..=8)
        .map(|number| {
            let image = image::ImageBuffer::from_pixel(
                /*width*/ 3200,
                /*height*/ 3200,
                image::Luma([number as u8]),
            );
            let mut bytes = std::io::Cursor::new(Vec::new());
            image.write_to(&mut bytes, image::ImageFormat::Png)?;
            Ok(UserInput::Image {
                image: ImageReference::Inline {
                    image_url: format!(
                        "data:image/png;base64,{}",
                        BASE64_STANDARD.encode(bytes.get_ref())
                    ),
                },
                detail: Some(codex_protocol::models::ImageDetail::Original),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut input = image_inputs[..7].to_vec();
    input.push(UserInput::Text {
        text: "Compare these images".to_string(),
        text_elements: Vec::new(),
    });
    let initial_mock = mount_sse_once(
        harness.server(),
        sse(vec![
            responses::ev_assistant_message("initial", "done"),
            responses::ev_completed("initial"),
        ]),
    )
    .await;
    codex
        .start_or_steer_turn(TurnInputRequest::user_input(input))
        .await?;
    wait_for_turn_complete(codex).await;
    let initial_request = initial_mock.single_request();
    let prepared_images = initial_request.message_input_image_urls("user");
    assert_eq!(prepared_images.len(), 7);

    for cycle in 1..=2 {
        let compact_mock = mount_sse_once(
            harness.server(),
            sse(vec![
                json!({
                    "type": "response.output_item.done",
                    "item": { "type": "compaction", "encrypted_content": "IMAGE_BUDGET_SUMMARY" },
                }),
                responses::ev_completed("compact-images"),
            ]),
        )
        .await;
        codex.submit(Op::Compact).await?;
        wait_for_turn_complete(codex).await;
        let compact_request = compact_mock.single_request();
        assert_eq!(compact_request.path(), "/v1/responses");
        assert_eq!(
            compact_request.inputs_of_type("compaction_trigger").len(),
            1
        );

        let follow_up_mock = mount_sse_once(
            harness.server(),
            sse(vec![
                responses::ev_assistant_message("after", "done"),
                responses::ev_completed("after"),
            ]),
        )
        .await;
        codex
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: "after compact".to_string(),
                text_elements: Vec::new(),
            }]))
            .await?;
        wait_for_turn_complete(codex).await;
        let follow_up = follow_up_mock.single_request();
        assert_eq!(
            follow_up.inputs_of_type("compaction")[0]["encrypted_content"],
            "IMAGE_BUDGET_SUMMARY"
        );
        let dropped = if image_budget_enabled.unwrap_or(true) {
            cycle
        } else {
            0
        };
        let mut expected_images = prepared_images[dropped..].to_vec();
        if cycle == 2 {
            let UserInput::Image {
                image: ImageReference::Inline { image_url },
                ..
            } = &image_inputs[7]
            else {
                unreachable!()
            };
            expected_images.push(image_url.clone());
        }
        assert_eq!(follow_up.message_input_image_urls("user"), expected_images);
        assert!(
            follow_up
                .message_input_texts("user")
                .iter()
                .any(|text| text == "Compare these images")
        );

        if cycle == 1 {
            let append_mock = mount_sse_once(
                harness.server(),
                sse(vec![
                    responses::ev_assistant_message("append", "done"),
                    responses::ev_completed("append"),
                ]),
            )
            .await;
            codex
                .start_or_steer_turn(TurnInputRequest::user_input(vec![image_inputs[7].clone()]))
                .await?;
            wait_for_turn_complete(codex).await;
            let _ = append_mock.single_request();
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_compact_v2_reuses_compaction_trigger_for_followups() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = TestCodexHarness::with_builder(
        test_codex().with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing()),
    )
    .await?;
    let image_url = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR4nGNgYAAAAAMAASsJTYQAAAAASUVORK5CYII=";
    let user_notice = "<image_resize_notice>retained user image</image_resize_notice>";
    let tool_notice = "<image_resize_notice>discarded tool image</image_resize_notice>";
    let unlisted_notice = "<unlisted_notice>discarded developer notice</unlisted_notice>";
    let developer_message = |text| {
        json!({
            "type": "message",
            "role": "developer",
            "content": [{ "type": "input_text", "text": text }]
        })
    };
    let initial_history = [
        json!({
            "type": "message",
            "role": "user",
            "content": [
                { "type": "input_text", "text": "retained image source" },
                { "type": "input_image", "image_url": image_url }
            ]
        }),
        developer_message(user_notice),
        developer_message(unlisted_notice),
        json!({
            "type": "function_call",
            "name": "view_image",
            "arguments": "{}",
            "call_id": "image-call"
        }),
        json!({
            "type": "function_call_output",
            "call_id": "image-call",
            "output": [{ "type": "input_image", "image_url": image_url }]
        }),
        developer_message(tool_notice),
    ]
    .into_iter()
    .map(|item| {
        serde_json::from_value::<ResponseItem>(item)
            .map(|item| RolloutItem::ResponseItem(item.into()))
    })
    .collect::<serde_json::Result<Vec<_>>>()?;
    let codex = harness
        .test()
        .thread_manager
        .start_thread(StartThreadOptions {
            initial_history: InitialHistory::Forked(initial_history),
            ..StartThreadOptions::new(harness.test().config.clone())
        })
        .await?
        .thread;

    let responses_mock = responses::mount_sse_sequence(
        harness.server(),
        vec![
            responses::sse(vec![
                responses::ev_assistant_message("m1", "FIRST_REMOTE_REPLY"),
                responses::ev_completed("resp-1"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("m-agent", "DELEGATED_TASK_REPLY"),
                responses::ev_completed("resp-agent"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("m-descendant", "DESCENDANT_FOLLOWUP_REPLY"),
                responses::ev_completed("resp-descendant"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("m-compact-noise", "IGNORED_COMPACT_REPLY"),
                serde_json::json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "compaction",
                        "encrypted_content": "ENCRYPTED_CONTEXT_COMPACTION_SUMMARY",
                    }
                }),
                responses::ev_completed_with_tokens("resp-compact", /*total_tokens*/ 17),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("m2", "AFTER_COMPACT_REPLY"),
                responses::ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello remote compact".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_turn_complete(&codex).await;

    codex
        .submit(Op::InterAgentCommunication {
            communication: InterAgentCommunication::new(
                AgentPath::root().join("child").expect("valid child path"),
                AgentPath::root(),
                Vec::new(),
                "Message Type: MESSAGE\nTask name: /root\nSender: /root/child\nPayload:\nchild progress"
                    .to_string(),
                /*trigger_turn*/ false,
            ),
            start_options: Default::default(),
        })
        .await?;
    codex
        .submit(Op::InterAgentCommunication {
            communication: InterAgentCommunication::new(
                AgentPath::root().join("child").expect("valid child path"),
                AgentPath::root(),
                Vec::new(),
                "Message Type: FINAL_ANSWER\nTask name: /root\nSender: /root/child\nPayload:\nchild completion".to_string(),
                /*trigger_turn*/ false,
            ),
            start_options: Default::default(),
        })
        .await?;
    let delegated_task_ciphertext = format!("delegated compact task{}", "x".repeat(40_000));
    codex
        .submit(Op::InterAgentCommunication {
            communication: InterAgentCommunication::new_encrypted(
                AgentPath::root(),
                AgentPath::root().join("worker").expect("valid worker path"),
                Vec::new(),
                delegated_task_ciphertext.clone(),
                /*trigger_turn*/ true,
            ),
            start_options: Default::default(),
        })
        .await?;
    wait_for_turn_complete(&codex).await;

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
    codex.submit(Op::Compact).await?;
    let compact_turn_complete = wait_for_event_match(&codex, |event| match event {
        EventMsg::TurnComplete(event) => Some(event.clone()),
        _ => None,
    })
    .await;
    assert_eq!(
        compact_turn_complete.provider_usage,
        Some(TokenUsage {
            input_tokens: 17,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 0,
            reasoning_output_tokens: 0,
            total_tokens: 17,
        })
    );
=======
    let descendant_followup_ciphertext = "descendant follow-up task";
    let worker_path = AgentPath::root().join("worker").expect("valid worker path");
    codex
        .submit(Op::InterAgentCommunication {
            communication: InterAgentCommunication::new_encrypted(
                worker_path.join("child").expect("valid grandchild path"),
                worker_path,
                Vec::new(),
                descendant_followup_ciphertext.to_string(),
                /*trigger_turn*/ true,
            ),
            start_options: Default::default(),
        })
        .await?;
    wait_for_turn_complete(&codex).await;

    let compact_turn_id = codex.submit(Op::Compact).await?;
    wait_for_turn_complete(&codex).await;
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360

    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "after compact".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_turn_complete(&codex).await;

    let response_requests = responses_mock.requests();
    let compact_request = &response_requests[3];
    let item_create_time = |request: &responses::ResponsesRequest, text: &str| {
        request
            .input()
            .into_iter()
            .find(|item| {
                item["content"].as_array().is_some_and(|content| {
                    content.iter().any(|part| {
                        part["text"].as_str() == Some(text)
                            || part["encrypted_content"].as_str() == Some(text)
                    })
                })
            })
            .and_then(|item| {
                item.pointer("/internal_chat_message_metadata_passthrough/create_time")
                    .cloned()
            })
            .expect("matching message should include a creation timestamp")
    };
    let original_user_create_time = item_create_time(&response_requests[0], "hello remote compact");
    let delegated_task_create_time =
        item_create_time(&response_requests[1], &delegated_task_ciphertext);
    assert!(
        compact_request
            .inputs_of_type("agent_message")
            .iter()
            .any(|item| item["content"][1]["encrypted_content"].as_str()
                == Some(delegated_task_ciphertext.as_str())),
        "expected v2 compaction input to include the encrypted delegated task"
    );
    assert!(
        compact_request
            .inputs_of_type("agent_message")
            .iter()
            .any(|item| item["content"][1]["encrypted_content"].as_str()
                == Some(descendant_followup_ciphertext)),
        "expected v2 compaction input to include the descendant-authored follow-up task"
    );
    assert!(
        compact_request
            .inputs_of_type("agent_message")
            .iter()
            .any(|item| item.to_string().contains("child progress")),
        "expected v2 compaction input to include the child progress update"
    );
    assert!(
        compact_request
            .inputs_of_type("agent_message")
            .iter()
            .any(|item| item.to_string().contains("child completion")),
        "expected v2 compaction input to include the child completion"
    );
    assert!(
        compact_request
            .header("x-codex-beta-features")
            .as_deref()
            .is_some_and(|value| value
                .split(',')
                .any(|feature| feature == "remote_compaction_v2")),
        "expected compact request to advertise the remote_compaction_v2 beta feature"
    );
    assert_eq!(compact_request.path(), "/v1/responses");
    let compact_metadata: Value = serde_json::from_str(
        &compact_request
            .header("x-codex-turn-metadata")
            .expect("v2 compact request should include turn metadata"),
    )
    .expect("v2 compact turn metadata should be valid json");
    assert_eq!(compact_metadata["turn_id"], compact_turn_id);
    assert_eq!(compact_metadata["root_turn_id"], compact_turn_id);
    assert_eq!(
        compact_metadata["request_kind"].as_str(),
        Some("compaction")
    );
    assert_eq!(
        compact_metadata["window_id"].as_str(),
        compact_request.header("x-codex-window-id").as_deref()
    );
    assert_eq!(
        compact_request.body_json()["client_metadata"]["x-codex-window-id"].as_str(),
        compact_metadata["window_id"].as_str()
    );
    assert_eq!(
        compact_metadata["compaction"],
        json!({
            "trigger": "manual",
            "reason": "user_requested",
            "implementation": "responses_compaction_v2",
            "phase": "standalone_turn",
            "strategy": "memento",
        })
    );
    let compact_body = compact_request.body_json().to_string();
    assert!(
        compact_body.contains("\"type\":\"compaction_trigger\""),
        "expected v2 compaction request to include the compaction_trigger item"
    );
    assert!(
        !compact_body.contains("ENCRYPTED_CONTEXT_COMPACTION_SUMMARY"),
        "expected v2 compaction trigger item to omit encrypted_content"
    );

    let follow_up_request = response_requests.last().expect("follow-up request missing");
    assert_eq!(
        item_create_time(follow_up_request, "hello remote compact"),
        original_user_create_time
    );
    assert_eq!(
        item_create_time(follow_up_request, &delegated_task_ciphertext),
        delegated_task_create_time
    );
    assert!(
        item_create_time(follow_up_request, "after compact")
            .as_f64()
            .is_some_and(|create_time| create_time > 0.0)
    );
    assert!(
        follow_up_request
            .inputs_of_type("agent_message")
            .iter()
            .any(|item| item["content"][1]["encrypted_content"].as_str()
                == Some(delegated_task_ciphertext.as_str())),
        "expected v2 follow-up request to retain the encrypted delegated task"
    );
    assert!(
        follow_up_request
            .inputs_of_type("agent_message")
            .iter()
            .any(|item| item["content"][1]["encrypted_content"].as_str()
                == Some(descendant_followup_ciphertext)),
        "expected v2 follow-up request to retain the descendant-authored follow-up task"
    );
    assert!(
        follow_up_request
            .inputs_of_type("agent_message")
            .iter()
            .all(|item| !item.to_string().contains("child progress")),
        "expected v2 follow-up request to omit the child progress update"
    );
    assert!(
        follow_up_request
            .inputs_of_type("agent_message")
            .iter()
            .all(|item| !item.to_string().contains("child completion")),
        "expected v2 follow-up request to omit the child completion"
    );
    let follow_up_body = follow_up_request.body_json().to_string();
    assert!(
        follow_up_body.contains("\"type\":\"compaction\""),
        "expected follow-up request to preserve the compaction item"
    );
    assert!(
        follow_up_body.contains("ENCRYPTED_CONTEXT_COMPACTION_SUMMARY"),
        "expected follow-up request to include the compaction payload"
    );
    assert!(!follow_up_body.contains("IGNORED_COMPACT_REPLY"));
    assert!(
        follow_up_body.contains("hello remote compact"),
        "expected v2 follow-up request to preserve retained original user messages"
    );
    assert!(
        follow_up_request.input().windows(2).any(|items| {
            items[0]["role"] == "user"
                && items[0]["content"][0]["text"] == "retained image source"
                && items[1]["role"] == "developer"
                && items[1]["content"][0]["text"] == user_notice
        }),
        "expected v2 compaction to retain the user image and its adjacent resize notice"
    );
    assert!(
        !follow_up_body.contains(unlisted_notice),
        "expected v2 compaction to drop unlisted developer notices"
    );
    assert!(
        !follow_up_body.contains(tool_notice),
        "expected v2 compaction to drop the resize notice with its tool output"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_compact_v2_retries_failures_with_stream_retry_budget() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = TestCodexHarness::with_builder(
        test_codex()
            .with_history_mode(ThreadHistoryMode::Paginated)
            .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
            .with_config(|config| {
                config.model_provider.request_max_retries = Some(0);
                config.model_provider.stream_max_retries = Some(2);
            }),
    )
    .await?;
    let codex = harness.test().codex.clone();

    let responses_mock = responses::mount_response_sequence(
        harness.server(),
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_assistant_message("m1", "FIRST_REMOTE_REPLY"),
                responses::ev_completed("resp-1"),
            ])),
            ResponseTemplate::new(500).set_body_string("first compact open failed"),
            responses::sse_response(responses::sse(vec![serde_json::json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "compaction",
                    "encrypted_content": "FAILED_COMPACT_SUMMARY",
                }
            })])),
            responses::sse_response(responses::sse(vec![
                serde_json::json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "compaction",
                        "encrypted_content": "RETRIED_COMPACT_SUMMARY",
                    }
                }),
                responses::ev_completed("resp-compact-retry"),
            ])),
            responses::sse_response(responses::sse(vec![
                responses::ev_assistant_message("m2", "AFTER_COMPACT_REPLY"),
                responses::ev_completed("resp-2"),
            ])),
        ],
    )
    .await;

    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello remote compact".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_turn_complete(&codex).await;

    codex.submit(Op::Compact).await?;
    wait_for_turn_complete(&codex).await;

    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "after compact".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_turn_complete(&codex).await;

    let response_requests = responses_mock.requests();
    assert_eq!(
        5,
        response_requests.len(),
        "expected initial turn, failed open, failed stream, compact retry, and follow-up turn"
    );
    for compact_request in &response_requests[1..=3] {
        assert_eq!("/v1/responses", compact_request.path());
        let compact_metadata: Value = serde_json::from_str(
            &compact_request
                .header("x-codex-turn-metadata")
                .expect("v2 compact request should include turn metadata"),
        )?;
        assert_eq!(compact_metadata["window_number"].as_u64(), Some(0));
        assert!(compact_metadata["context_window_id"].as_str().is_some());
        assert!(
            compact_request
                .body_json()
                .to_string()
                .contains("\"type\":\"compaction_trigger\""),
            "expected v2 compaction request to include the compaction_trigger item"
        );
    }

    let follow_up_request = response_requests.last().expect("follow-up request missing");
    let follow_up_body = follow_up_request.body_json().to_string();
    assert!(
        follow_up_body.contains("RETRIED_COMPACT_SUMMARY"),
        "expected follow-up request to include the retried compaction payload"
    );
    assert!(
        !follow_up_body.contains("FAILED_COMPACT_SUMMARY"),
        "expected failed compaction attempt output to be discarded"
    );

    Ok(())
}

#[test_case(false; "manual")]
#[test_case(true; "automatic")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_compact_v2_rewrites_multiple_trailing_function_call_outputs(
    automatic: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
    let harness = TestCodexHarness::with_builder(
        test_codex()
            .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
            .with_config(|config| {
                let _ = config.features.enable(Feature::RemoteCompactionV2);
            }),
    )
    .await?;
    let codex = harness.test().codex.clone();

    let responses_mock = responses::mount_sse_sequence(
        harness.server(),
        vec![
            responses::sse(vec![
                responses::ev_assistant_message("m1", "FIRST_REMOTE_REPLY"),
                responses::ev_completed("resp-1"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("m-compact-noise", "IGNORED_COMPACT_REPLY"),
                serde_json::json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "compaction",
                        "encrypted_content": "ENCRYPTED_CONTEXT_COMPACTION_SUMMARY",
                    }
                }),
                responses::ev_completed("resp-compact"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("m2", "AFTER_COMPACT_REPLY"),
                responses::ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello remote compact".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_turn_complete(&codex).await;

    codex.submit(Op::Compact).await?;
    wait_for_turn_complete(&codex).await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "after compact".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_turn_complete(&codex).await;

    let response_requests = responses_mock.requests();
    let follow_up_request = response_requests.last().expect("follow-up request missing");
    let follow_up_body = follow_up_request.body_json().to_string();
    assert!(
        follow_up_body.contains("\"type\":\"compaction\""),
        "expected follow-up request to preserve the compaction item"
    );
    assert!(
        follow_up_body.contains("ENCRYPTED_CONTEXT_COMPACTION_SUMMARY"),
        "expected follow-up request to include the compaction payload"
    );
    assert!(
        !follow_up_body.contains("IGNORED_COMPACT_REPLY"),
        "expected follow-up request to ignore unrelated output items from the compaction stream"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_compact_filters_deferred_dynamic_tools() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let mut builder = test_codex().with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    let mut test = builder.build(&server).await?;
    let hidden_tool = "hidden_dynamic_tool";
    let visible_tool = "visible_dynamic_tool";
    let input_schema = json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false,
    });
    let dynamic_tools = vec![
        DynamicToolSpec {
            namespace: Some("codex_app".to_string()),
            name: hidden_tool.to_string(),
            description: "Hidden until discovered.".to_string(),
            input_schema: input_schema.clone(),
            defer_loading: true,
            persist_on_resume: true,
            capability: None,
        },
        DynamicToolSpec {
            namespace: Some("codex_app".to_string()),
            name: visible_tool.to_string(),
            description: "Visible immediately.".to_string(),
            input_schema,
            defer_loading: false,
            persist_on_resume: true,
            capability: None,
        },
    ];
    let new_thread = test
        .thread_manager
        .start_thread(StartThreadOptions {
            dynamic_tools,
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?;
    test.codex = new_thread.thread;
    test.session_configured = new_thread.session_configured;
    let codex = test.codex.clone();

    let responses_mock = mount_sse_once(
        &server,
        sse(vec![
            responses::ev_assistant_message("m1", "FIRST_REMOTE_REPLY"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let compact_mock = responses::mount_compact_json_once(
        &server,
        serde_json::json!({
            "output": compacted_summary_only_output("compact summary"),
        }),
    )
    .await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello remote compact".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_turn_complete(&codex).await;

    codex.submit(Op::Compact).await?;
    wait_for_turn_complete(&codex).await;

    let first_response_body = responses_mock.single_request().body_json();
    let compact_body = compact_mock.single_request().body_json();
    assert_eq!(
        compact_body["tools"], first_response_body["tools"],
        "compact requests should send the same model-visible tools payload as /v1/responses"
    );
    assert_tools_payload_does_not_defer(&first_response_body);
    assert_tools_payload_does_not_defer(&compact_body);
    assert_eq!(
        namespace_child_tool_names(&first_response_body, "codex_app"),
        vec![visible_tool.to_string()]
    );
    assert_eq!(
        namespace_child_tool_names(&compact_body, "codex_app"),
        vec![visible_tool.to_string()]
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_compact_does_not_charge_inline_audio_payload_as_text() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let call_id = "audio-call";
    let tool_name = "recording";
    let audio_url = pcm_wav_data_url(/*sample_count*/ 300_000);
    let responses_mock = responses::mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                responses::ev_response_created("resp-1"),
                responses::ev_function_call_with_namespace(call_id, "codex_app", tool_name, "{}"),
                responses::ev_completed_with_tokens("resp-1", /*total_tokens*/ 100),
            ]),
            sse(vec![
                responses::ev_response_created("resp-2"),
                responses::ev_assistant_message("msg-1", "done"),
                responses::ev_completed_with_tokens("resp-2", /*total_tokens*/ 200),
            ]),
        ],
    )
    .await;
    let compact_mock = responses::mount_compact_json_once(
        &server,
        json!({ "output": compacted_summary_only_output("compact summary") }),
    )
    .await;

    let mut builder = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_model_info_override("gpt-5.5", |model_info| {
            model_info.input_modalities.push(InputModality::Audio);
        })
        .with_config(|config| {
            config.model_context_window = Some(50_000);
        });
    let mut test = builder.build(&server).await?;
    let dynamic_tool = DynamicToolSpec {
        namespace: Some("codex_app".to_string()),
        name: tool_name.to_string(),
        description: "Returns a recording.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        }),
        defer_loading: false,
        persist_on_resume: true,
        capability: None,
    };
    let new_thread = test
        .thread_manager
        .start_thread(StartThreadOptions {
            dynamic_tools: vec![dynamic_tool],
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?;
    test.codex = new_thread.thread;
    test.session_configured = new_thread.session_configured;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Return a recording".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    let EventMsg::DynamicToolCallRequest(request) = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::DynamicToolCallRequest(_))
    })
    .await
    else {
        unreachable!("event guard guarantees DynamicToolCallRequest");
    };
    test.codex
        .submit(Op::DynamicToolResponse {
            id: request.call_id,
            response: DynamicToolResponse {
                content_items: vec![DynamicToolCallOutputContentItem::InputAudio {
                    audio_url: audio_url.clone(),
                }],
                success: true,
            },
        })
        .await?;
    wait_for_turn_complete(&test.codex).await;

    test.codex.submit(Op::Compact).await?;
    wait_for_turn_complete(&test.codex).await;

    assert_eq!(responses_mock.requests().len(), 2);
    let output = compact_mock
        .single_request()
        .function_call_output(call_id)
        .get("output")
        .cloned()
        .expect("compact request should retain the dynamic tool output");
    assert_eq!(
        serde_json::from_value::<FunctionCallOutputPayload>(output)?,
        FunctionCallOutputPayload {
            body: FunctionCallOutputBody::ContentItems(vec![
                FunctionCallOutputContentItem::InputAudio { audio_url },
            ]),
            success: None,
        }
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_compact_runs_automatically() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = TestCodexHarness::with_builder(
        test_codex().with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing()),
    )
    .await?;
    let codex = harness.test().codex.clone();
    let session_id = harness.test().session_configured.session_id.to_string();
    let thread_id = harness.test().session_configured.thread_id.to_string();

    let initial_request = mount_sse_once(
        harness.server(),
        sse(vec![
            responses::ev_shell_command_call("m1", "echo 'hi'"),
            responses::ev_completed_with_tokens("resp-1", /*total_tokens*/ 100000000), // over token limit
        ]),
    )
    .await;
    let responses_mock = mount_sse_once(
        harness.server(),
        responses::sse(vec![
            responses::ev_assistant_message("m2", "AFTER_COMPACT_REPLY"),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    let compact_mock = responses::mount_compact_user_history_with_summary_once(
        harness.server(),
        "REMOTE_COMPACTED_SUMMARY",
    )
    .await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello remote compact".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let message = wait_for_event_match(&codex, |event| match event {
        EventMsg::ContextCompacted(_) => Some(true),
        _ => None,
    })
    .await;
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    assert!(message);
    assert_eq!(compact_mock.requests().len(), 1);
    assert_eq!(
        compact_mock
            .single_request()
            .header("session-id")
            .as_deref(),
        Some(session_id.as_str())
    );
    assert_eq!(
        compact_mock.single_request().header("thread-id").as_deref(),
        Some(thread_id.as_str())
    );
    let compact_metadata: Value = serde_json::from_str(
        &compact_mock
            .single_request()
            .header("x-codex-turn-metadata")
            .expect("auto remote compact request should include turn metadata"),
    )
    .expect("auto remote compact turn metadata should be valid json");
    assert_eq!(
        compact_metadata["request_kind"].as_str(),
        Some("compaction")
    );
    assert_eq!(
        compact_metadata["compaction"],
        json!({
            "trigger": "auto",
            "reason": "context_limit",
            "implementation": "responses_compact",
            "phase": "mid_turn",
            "strategy": "memento",
        })
    );
    let initial_metadata: Value = serde_json::from_str(
        &initial_request
            .single_request()
            .header("x-codex-turn-metadata")
            .expect("initial request should include turn metadata"),
    )
    .expect("initial turn metadata should be valid json");
    assert_eq!(
        initial_metadata["turn_id"], compact_metadata["turn_id"],
        "automatic mid-turn compaction should keep the current turn id"
    );
    assert_eq!(
        initial_metadata["window_id"], compact_metadata["window_id"],
        "automatic mid-turn compaction summarizes the current context window"
    );
    let follow_up_request = responses_mock.single_request();
    let follow_up_metadata: Value = serde_json::from_str(
        &follow_up_request
            .header("x-codex-turn-metadata")
            .expect("post-compaction continuation should include turn metadata"),
    )
    .expect("post-compaction turn metadata should be valid json");
    assert_eq!(
        follow_up_metadata["request_kind"].as_str(),
        Some("turn"),
        "post-compaction continuation should be a regular request"
    );
    assert!(follow_up_metadata.get("compaction").is_none());
    assert_eq!(
        follow_up_metadata["turn_id"], compact_metadata["turn_id"],
        "automatic mid-turn continuation should keep the current turn id"
    );
    assert_ne!(
        follow_up_metadata["window_id"], compact_metadata["window_id"],
        "post-compaction continuation should use the next context window"
    );
    let follow_up_body = follow_up_request.body_json().to_string();
    assert!(follow_up_body.contains("REMOTE_COMPACTED_SUMMARY"));

    Ok(())
}

#[test]
fn auto_remote_compact_retries_server_overloaded() -> Result<()> {
    run_large_stack_test("auto-remote-compact-capacity-retry-test", async {
        skip_if_no_network!(Ok(()));

        let harness = TestCodexHarness::with_builder(
            test_codex()
                .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
                .with_config(|config| {
                    config.model_provider.request_max_retries = Some(0);
                    config.model_provider.stream_max_retries = Some(0);
                }),
        )
        .await?;
        let codex = harness.test().codex.clone();

        mount_sse_once(
            harness.server(),
            sse(vec![
                responses::ev_shell_command_call("m1", "echo 'hi'"),
                responses::ev_completed_with_tokens("resp-1", /*total_tokens*/ 100000000),
            ]),
        )
        .await;
        let responses_mock = mount_sse_once(
            harness.server(),
            responses::sse(vec![
                responses::ev_assistant_message("m2", "AFTER_CAPACITY_COMPACT_REPLY"),
                responses::ev_completed("resp-2"),
            ]),
        )
        .await;
        let compact_mock = responses::mount_compact_response_sequence(
            harness.server(),
            vec![
                ResponseTemplate::new(503).set_body_json(json!({
                    "error": {
                        "code": "server_is_overloaded",
                        "message": "Selected model is at capacity."
                    }
                })),
                ResponseTemplate::new(200).set_body_json(json!({
                    "output": [{
                        "type": "compaction",
                        "encrypted_content": "REMOTE_COMPACTED_AFTER_CAPACITY"
                    }]
                })),
            ],
        )
        .await;

        codex
            .submit(Op::UserInput {
                items: vec![UserInput::Text {
                    text: "hello remote compact".into(),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
                responsesapi_client_metadata: None,
                additional_context: Default::default(),
                thread_settings: Default::default(),
            })
            .await?;

        let retry_message = wait_for_event_match(&codex, |event| match event {
            EventMsg::StreamError(error) => Some(error.message.clone()),
            _ => None,
        })
        .await;
        assert!(retry_message.starts_with("Model at capacity; retrying in "));
        let compacted = wait_for_event_match(&codex, |event| match event {
            EventMsg::ContextCompacted(_) => Some(true),
            _ => None,
        })
        .await;
        wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

        assert!(compacted);
        assert_eq!(compact_mock.requests().len(), 2);
        assert!(
            responses_mock
                .single_request()
                .body_json()
                .to_string()
                .contains("REMOTE_COMPACTED_AFTER_CAPACITY")
        );

        Ok(())
    })
}

#[cfg_attr(target_os = "windows", ignore)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_compact_trims_function_call_history_to_fit_context_window() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let first_user_message = "turn with retained shell call";
    let second_user_message = "turn with trimmed shell call";
=======
    let first_user_message = "turn with retained tool call";
    let second_user_message = "turn with parallel tool calls";
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
    let retained_call_id = "retained-call";
    let first_trimmed_call_id = "first-trimmed-call";
    let second_trimmed_call_id = "second-trimmed-call";
    let retained_output = "retained tool output";

    let harness = TestCodexHarness::with_builder(
        test_codex()
            .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
            .with_config(|config| {
                config.model_context_window = Some(2_000);
                config.model_auto_compact_token_limit = Some(200_000);
            }),
    )
    .await?;
    let codex = harness.test().codex.clone();

    let initial_mock = mount_sse_once(
        harness.server(),
        sse(vec![responses::ev_completed_with_tokens(
            "initial-response",
            if automatic { 500_000 } else { 100 },
        )]),
    )
    .await;
    harness.test().submit_turn("initial turn").await?;
    let _ = initial_mock.single_request();
    let history = [
        json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": first_user_message}]}),
        json!({"type": "function_call", "call_id": retained_call_id, "name": "exec_command", "arguments": "{}"}),
        json!({"type": "function_call_output", "call_id": retained_call_id, "output": retained_output}),
        json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": second_user_message}]}),
        json!({"type": "function_call", "call_id": first_trimmed_call_id, "name": "exec_command", "arguments": "{}"}),
        json!({"type": "function_call", "call_id": second_trimmed_call_id, "name": "exec_command", "arguments": "{}"}),
        json!({"type": "function_call_output", "call_id": first_trimmed_call_id, "output": "x".repeat(12_000)}),
        json!({"type": "function_call_output", "call_id": second_trimmed_call_id, "output": "y".repeat(12_000)}),
    ]
    .into_iter()
    .map(serde_json::from_value)
    .collect::<serde_json::Result<Vec<ResponseItem>>>()?;
    codex.inject_response_items(history).await?;

    let mut response_bodies = vec![sse(vec![
        json!({
            "type": "response.output_item.done",
            "item": {"type": "compaction", "encrypted_content": "REMOTE_COMPACT_SUMMARY"},
        }),
        responses::ev_completed("response-compact"),
    ])];
    if automatic {
        response_bodies.push(responses::sse_completed("after-compact"));
    }
    let compact_mock = responses::mount_sse_sequence(harness.server(), response_bodies).await;

    if automatic {
        harness.test().submit_text_turn("after compact").await?;
    } else {
        codex.submit(Op::Compact).await?;
        wait_for_turn_complete(&codex).await;
    }

    let requests = compact_mock.requests();
    assert_eq!(requests.len(), if automatic { 2 } else { 1 });
    let compact_request = &requests[0];
    assert_eq!(
        compact_request.inputs_of_type("compaction_trigger").len(),
        1
    );
    let user_messages = compact_request.message_input_texts("user");
    assert!(
        user_messages
            .iter()
            .any(|message| message == first_user_message)
    );
    assert!(
        user_messages
            .iter()
            .any(|message| message == second_user_message)
    );
    assert!(
        !user_messages
            .iter()
            .any(|message| message == "after compact")
    );
    if automatic {
        let followup_metadata: Value = serde_json::from_str(
            &requests[1]
                .header("x-codex-turn-metadata")
                .context("follow-up request metadata")?,
        )?;
        assert_eq!(followup_metadata["request_kind"], "turn");
    }
    assert!(compact_request.has_function_call(retained_call_id));
    assert_eq!(
        compact_request
            .function_call_output_text(retained_call_id)
            .as_deref(),
        Some(retained_output),
        "expected compact request to keep the older function output unchanged"
    );
    assert!(
        compact_request.has_function_call(first_trimmed_call_id)
            && compact_request.has_function_call(second_trimmed_call_id),
        "expected compact request to retain both trailing parallel function calls"
    );
    assert_eq!(
        compact_request.function_call_output_text(first_trimmed_call_id),
        Some(CONTEXT_WINDOW_TRUNCATED_OUTPUT_MESSAGE.to_string()),
        "expected compact request to rewrite the first trailing function call output"
    );
    assert_eq!(
        compact_request.function_call_output_text(second_trimmed_call_id),
        Some(CONTEXT_WINDOW_TRUNCATED_OUTPUT_MESSAGE.to_string()),
        "expected compact request to rewrite the second trailing function call output"
    );

    assert_eq!(
        compact_request.inputs_of_type("function_call").len(),
        3,
        "expected all function calls after rewriting trailing outputs"
    );
    assert_eq!(
        compact_request.inputs_of_type("function_call_output").len(),
        3,
        "expected all function call outputs after rewriting trailing outputs"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
async fn auto_remote_compact_trims_function_call_history_to_fit_context_window() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let first_user_message = "turn with retained shell call";
    let second_user_message = "turn with trimmed shell call";
    let retained_call_id = "retained-call";
    let trimmed_call_id = "trimmed-call";
    let retained_command = "echo retained-shell-output";
    let trimmed_command = "yes x | head -n 3000";
    let harness = TestCodexHarness::with_builder(
        test_codex()
            .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
            .with_config(|config| {
                config.model_context_window = Some(2_000);
                config.model_auto_compact_token_limit = Some(200_000);
            }),
    )
    .await?;
    let codex = harness.test().codex.clone();

    responses::mount_sse_sequence(
        harness.server(),
        vec![
            sse(vec![
                responses::ev_shell_command_call(retained_call_id, retained_command),
                responses::ev_completed_with_tokens(
                    "retained-call-response",
                    /*total_tokens*/ 100,
                ),
            ]),
            sse(vec![
                responses::ev_assistant_message("retained-assistant", "retained complete"),
                responses::ev_completed("retained-final-response"),
            ]),
            sse(vec![
                responses::ev_shell_command_call(trimmed_call_id, trimmed_command),
                responses::ev_completed_with_tokens(
                    "trimmed-call-response",
                    /*total_tokens*/ 100,
                ),
            ]),
            sse(vec![responses::ev_completed_with_tokens(
                "trimmed-final-response",
                /*total_tokens*/ 500_000,
            )]),
        ],
    )
    .await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: first_user_message.into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: second_user_message.into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let compact_mock = responses::mount_compact_user_history_with_summary_once(
        harness.server(),
        "REMOTE_AUTO_COMPACT_SUMMARY",
    )
    .await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "turn that triggers auto compact".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    assert_eq!(
        compact_mock.requests().len(),
        1,
        "expected exactly one remote compact request"
    );

    let compact_request = compact_mock.single_request();
    let user_messages = compact_request.message_input_texts("user");
    assert!(
        user_messages
            .iter()
            .any(|message| message == first_user_message),
        "expected compact request to retain earlier user history"
    );
    assert!(
        user_messages
            .iter()
            .any(|message| message == second_user_message),
        "expected compact request to retain the user boundary message"
    );

    assert!(
        compact_request.has_function_call(retained_call_id)
            && compact_request
                .function_call_output_text(retained_call_id)
                .is_some(),
        "expected compact request to keep the older function call/result pair"
    );
    assert!(
        compact_request.has_function_call(trimmed_call_id),
        "expected compact request to retain the trailing function call"
    );
    assert_eq!(
        compact_request.function_call_output_text(trimmed_call_id),
        Some(CONTEXT_WINDOW_TRUNCATED_OUTPUT_MESSAGE.to_string()),
        "expected compact request to rewrite the trailing function call output past the boundary"
    );

    assert_eq!(
        compact_request.inputs_of_type("function_call").len(),
        2,
        "expected both function calls after rewriting the trailing output"
    );
    assert_eq!(
        compact_request.inputs_of_type("function_call_output").len(),
        2,
        "expected both function call outputs after rewriting the trailing output"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_compact_trims_tool_search_output_to_empty_tools_array() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let search_call_id = "tool-search-1";
    let tool_name = "oversized_dynamic_tool";
    let tool_description = format!(
        "Oversized deferred tool for remote compaction. {}",
        "x".repeat(20_000)
    );
    let _responses_mock = mount_sse_once(
        &server,
        sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_tool_search_call(
                search_call_id,
                &json!({
                    "query": "oversized deferred tool",
                    "limit": 8,
                }),
            ),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;

    let input_schema = json!({
        "type": "object",
        "properties": {
            "mode": { "type": "string" },
        },
        "required": ["mode"],
        "additionalProperties": false,
    });
    let dynamic_tool = DynamicToolSpec {
        namespace: Some("codex_app".to_string()),
        name: tool_name.to_string(),
        description: tool_description,
        input_schema,
        defer_loading: true,
        persist_on_resume: true,
        capability: None,
    };

    let mut builder = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(|config| {
            configure_search_capable_model(config);
            config.model_context_window = Some(2_000);
        });
    let mut test = builder.build(&server).await?;
    let new_thread = test
        .thread_manager
        .start_thread(StartThreadOptions {
            dynamic_tools: vec![dynamic_tool],
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?;
    test.codex = new_thread.thread;
    test.session_configured = new_thread.session_configured;
    let codex = test.codex.clone();

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Find the oversized deferred tool".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_turn_complete(&codex).await;

    let compact_mock =
        responses::mount_compact_user_history_with_summary_once(&server, "REMOTE_COMPACT_SUMMARY")
            .await;

    codex.submit(Op::Compact).await?;
    wait_for_turn_complete(&codex).await;

    let compact_request = compact_mock.single_request();
    let compact_tools = compact_request
        .tool_search_output(search_call_id)
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(
        compact_request
            .inputs_of_type("tool_search_output")
            .iter()
            .any(|item| item.get("call_id").and_then(Value::as_str) == Some(search_call_id)),
        "expected compact request to retain the tool_search_output item"
    );
    assert!(
        compact_tools.is_empty(),
        "expected compact request to rewrite trailing tool_search output to an empty tools array"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_remote_compact_failure_stops_agent_loop() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = TestCodexHarness::with_builder(
        test_codex()
            .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
            .with_config(|config| {
                config.model_auto_compact_token_limit = Some(120);
            }),
    )
    .await?;
    let codex = harness.test().codex.clone();

    mount_sse_once(
        harness.server(),
        sse(vec![
            responses::ev_assistant_message("initial-assistant", "initial turn complete"),
            responses::ev_completed_with_tokens("initial-response", /*total_tokens*/ 500_000),
        ]),
    )
    .await;

    let first_compact_mock = responses::mount_compact_json_once(
        harness.server(),
        serde_json::json!({ "output": "invalid compact payload shape" }),
    )
    .await;
    let post_compact_turn_mock = mount_sse_once(
        harness.server(),
        sse(vec![
            responses::ev_assistant_message("post-compact-assistant", "should not run"),
            responses::ev_completed("post-compact-response"),
        ]),
    )
    .await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "turn that exceeds token threshold".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "turn that triggers auto compact".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let error_message = wait_for_event_match(&codex, |event| match event {
        EventMsg::Error(err) => Some(err.message.clone()),
        _ => None,
    })
    .await;
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    assert!(
        error_message.contains("Error running remote compact task"),
        "expected remote compact task error prefix, got {error_message}"
    );
    assert_eq!(
        first_compact_mock.requests().len(),
        1,
        "expected first remote compact attempt with incoming items"
    );
    assert!(
        post_compact_turn_mock.requests().is_empty(),
        "expected agent loop to stop after compaction failure"
    );

    insta::assert_snapshot!(
        "remote_pre_turn_compaction_failure_shapes",
        format_labeled_requests_snapshot(
            "Remote pre-turn auto-compaction parse failure: compaction request excludes the incoming user message and the turn stops.",
            &[(
                "Remote Compaction Request (Incoming User Excluded)",
                &first_compact_mock.single_request()
            ),]
        )
    );

    Ok(())
}

#[cfg_attr(target_os = "windows", ignore)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_compact_trim_estimate_uses_session_base_instructions() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let first_user_message = "turn with baseline shell call";
    let second_user_message = "turn with trailing shell call";
    let baseline_retained_call_id = "baseline-retained-call";
    let baseline_trailing_call_id = "baseline-trailing-call";
    let override_retained_call_id = "override-retained-call";
    let override_trailing_call_id = "override-trailing-call";
    let retained_command = "printf retained-shell-output";
    let trailing_command = "printf '%020000d' 0";

    let baseline_harness = TestCodexHarness::with_builder(
        test_codex()
            .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
            .with_config(|config| {
                config.model_context_window = Some(200_000);
            }),
    )
    .await?;
    let baseline_codex = baseline_harness.test().codex.clone();

    responses::mount_sse_sequence(
        baseline_harness.server(),
        vec![
            sse(vec![
                responses::ev_shell_command_call(baseline_retained_call_id, retained_command),
                responses::ev_completed("baseline-retained-call-response"),
            ]),
            sse(vec![
                responses::ev_assistant_message("baseline-retained-assistant", "retained complete"),
                responses::ev_completed("baseline-retained-final-response"),
            ]),
            sse(vec![
                responses::ev_shell_command_call(baseline_trailing_call_id, trailing_command),
                responses::ev_completed("baseline-trailing-call-response"),
            ]),
            sse(vec![responses::ev_completed(
                "baseline-trailing-final-response",
            )]),
        ],
    )
    .await;

    baseline_codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: first_user_message.into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&baseline_codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    baseline_codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: second_user_message.into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&baseline_codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let baseline_compact_mock = responses::mount_compact_user_history_with_summary_once(
        baseline_harness.server(),
        "REMOTE_BASELINE_SUMMARY",
    )
    .await;

    baseline_codex.submit(Op::Compact).await?;
    wait_for_event(&baseline_codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let baseline_compact_request = baseline_compact_mock.single_request();
    assert!(
        baseline_compact_request.has_function_call(baseline_retained_call_id),
        "expected baseline compact request to retain older function call history"
    );
    assert!(
        baseline_compact_request.has_function_call(baseline_trailing_call_id),
        "expected baseline compact request to retain trailing function call history"
    );

    let baseline_input_tokens = estimate_compact_input_tokens(&baseline_compact_request);
    let baseline_payload_tokens = estimate_compact_payload_tokens(&baseline_compact_request);

    let override_base_instructions = format!(
        "{}\nREMOTE_BASE_INSTRUCTIONS_OVERRIDE {}",
        baseline_compact_request.instructions_text(),
        "x".repeat(8_000)
    );
    let override_context_window = baseline_payload_tokens.saturating_add(500);
    let pretrim_override_estimate =
        baseline_input_tokens.saturating_add(approx_token_count(&override_base_instructions));
    assert!(
        pretrim_override_estimate > override_context_window,
        "expected override instructions to push pre-trim estimate past the context window"
    );

    let override_harness = TestCodexHarness::with_builder(
        test_codex()
            .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
            .with_config({
                let override_base_instructions = override_base_instructions.clone();
                move |config| {
                    config.model_context_window = Some(override_context_window);
                    config.base_instructions = Some(override_base_instructions);
                }
            }),
    )
    .await?;
    let override_codex = override_harness.test().codex.clone();

    responses::mount_sse_sequence(
        override_harness.server(),
        vec![
            sse(vec![
                responses::ev_shell_command_call(override_retained_call_id, retained_command),
                responses::ev_completed("override-retained-call-response"),
            ]),
            sse(vec![
                responses::ev_assistant_message("override-retained-assistant", "retained complete"),
                responses::ev_completed("override-retained-final-response"),
            ]),
            sse(vec![
                responses::ev_shell_command_call(override_trailing_call_id, trailing_command),
                responses::ev_completed("override-trailing-call-response"),
            ]),
            sse(vec![responses::ev_completed(
                "override-trailing-final-response",
            )]),
        ],
    )
    .await;

    override_codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: first_user_message.into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&override_codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    override_codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: second_user_message.into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&override_codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let override_compact_mock = responses::mount_compact_user_history_with_summary_once(
        override_harness.server(),
        "REMOTE_OVERRIDE_SUMMARY",
    )
    .await;

    override_codex.submit(Op::Compact).await?;
    wait_for_event(&override_codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let override_compact_request = override_compact_mock.single_request();
    assert_eq!(
        override_compact_request.instructions_text(),
        override_base_instructions
    );
    assert!(
        override_compact_request.has_function_call(override_retained_call_id),
        "expected remote compact request to preserve older function call history"
    );
    assert!(
        override_compact_request.has_function_call(override_trailing_call_id),
        "expected remote compact request to preserve trailing function call history with override instructions"
    );
    assert_eq!(
        override_compact_request.function_call_output_text(override_trailing_call_id),
        Some(CONTEXT_WINDOW_TRUNCATED_OUTPUT_MESSAGE.to_string()),
        "expected remote compact request to rewrite trailing function call output with override instructions"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_manual_compact_emits_context_compaction_items() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = TestCodexHarness::with_builder(
        test_codex().with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing()),
    )
    .await?;
    let codex = harness.test().codex.clone();

    mount_sse_once(
        harness.server(),
        sse(vec![
            responses::ev_assistant_message("m1", "REMOTE_REPLY"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;

    let compact_mock = responses::mount_compact_user_history_with_summary_once(
        harness.server(),
        "REMOTE_COMPACTED_SUMMARY",
    )
    .await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "manual remote compact".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    codex.submit(Op::Compact).await?;

    let mut started_item = None;
    let mut completed_item = None;
    let mut legacy_event = false;
    let mut saw_turn_complete = false;

    while !saw_turn_complete || started_item.is_none() || completed_item.is_none() || !legacy_event
    {
        let event = codex.next_event().await.unwrap();
        match event.msg {
            EventMsg::ItemStarted(ItemStartedEvent {
                item: TurnItem::ContextCompaction(item),
                ..
            }) => {
                started_item = Some(item);
            }
            EventMsg::ItemCompleted(ItemCompletedEvent {
                item: TurnItem::ContextCompaction(item),
                ..
            }) => {
                completed_item = Some(item);
            }
            EventMsg::ContextCompacted(_) => {
                legacy_event = true;
            }
            EventMsg::TurnComplete(_) => {
                saw_turn_complete = true;
            }
            _ => {}
        }
    }

    let started_item = started_item.expect("context compaction item started");
    let completed_item = completed_item.expect("context compaction item completed");
    assert_eq!(started_item.id, completed_item.id);
    assert!(legacy_event);
    assert_eq!(compact_mock.requests().len(), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_manual_compact_failure_emits_task_error_event() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = TestCodexHarness::with_builder(
        test_codex().with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing()),
    )
    .await?;
    let codex = harness.test().codex.clone();

    mount_sse_once(
        harness.server(),
        sse(vec![
            responses::ev_assistant_message("m1", "REMOTE_REPLY"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;

    let compact_mock = responses::mount_compact_json_once(
        harness.server(),
        serde_json::json!({ "output": "invalid compact payload shape" }),
    )
    .await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "manual remote compact".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    codex.submit(Op::Compact).await?;

    let error_message = wait_for_event_match(&codex, |event| match event {
        EventMsg::Error(err) => Some(err.message.clone()),
        _ => None,
    })
    .await;
    assert!(
        error_message.contains("Error running remote compact task"),
        "expected remote compact task error prefix, got {error_message}"
    );
    assert!(
        error_message.contains("invalid compact payload shape")
            || error_message.contains("invalid type: string"),
        "expected invalid compact payload details, got {error_message}"
    );
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    assert_eq!(compact_mock.requests().len(), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
// TODO(ccunningham): Re-enable after the follow-up compaction behavior PR lands.
// Current main behavior for rollout replacement-history persistence is known-incorrect.
#[ignore = "behavior change covered in follow-up compaction PR"]
async fn remote_compact_persists_replacement_history_in_rollout() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = TestCodexHarness::with_builder(
        test_codex().with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing()),
    )
    .await?;
    let codex = harness.test().codex.clone();
    let rollout_path = harness
        .test()
        .session_configured
        .rollout_path
        .clone()
        .expect("rollout path");

    let responses_mock = responses::mount_sse_once(
        harness.server(),
        responses::sse(vec![
            responses::ev_assistant_message("m1", "COMPACT_BASELINE_REPLY"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;

    let compacted_history = vec![
        ResponseItem::Compaction {
            id: None,
            encrypted_content: "ENCRYPTED_COMPACTION_SUMMARY".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "COMPACTED_ASSISTANT_NOTE".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    let compact_mock = responses::mount_compact_json_once(
        harness.server(),
        serde_json::json!({ "output": compacted_history.clone() }),
    )
    .await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "needs compaction".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    codex.submit(Op::Compact).await?;
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    codex.submit(Op::Shutdown).await?;
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::ShutdownComplete)).await;

    assert_eq!(responses_mock.requests().len(), 1);
    assert_eq!(compact_mock.requests().len(), 1);

    let rollout_text = fs::read_to_string(&rollout_path)?;
    let mut saw_compacted_history = false;
    for line in rollout_text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
    {
        let Ok(entry) = serde_json::from_str::<RolloutLine>(line) else {
            continue;
        };
        if let RolloutItem::Compacted(compacted) = entry.item
            && compacted.message.is_empty()
            && let Some(replacement_history) = compacted.replacement_history.as_ref()
        {
            let has_compaction_item = replacement_history.iter().any(|item| {
                matches!(
                    item,
                    ResponseItem::Compaction {
                        encrypted_content, ..
                    }
                        if encrypted_content == "ENCRYPTED_COMPACTION_SUMMARY"
                )
            });
            let has_compacted_assistant_note = replacement_history.iter().any(|item| {
                matches!(
                    item,
                    ResponseItem::Message { role, content, .. }
                        if role == "assistant"
                            && content.iter().any(|part| matches!(
                                part,
                                ContentItem::OutputText { text } if text == "COMPACTED_ASSISTANT_NOTE"
                            ))
                )
            });
            let has_permissions_developer_message = replacement_history.iter().any(|item| {
                matches!(
                    item,
                    ResponseItem::Message { role, content, .. }
                        if role == "developer"
                            && content.iter().any(|part| matches!(
                                part,
                                ContentItem::InputText { text }
                                    if text.contains("<permissions instructions>")
                            ))
                )
            });

            if has_compaction_item && has_compacted_assistant_note {
                assert!(
                    !has_permissions_developer_message,
                    "manual remote compact rollout replacement history should not inject permissions context"
                );
                saw_compacted_history = true;
                break;
            }
        }
    }

    assert!(
        saw_compacted_history,
        "expected rollout to persist remote compaction history"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_compact_and_resume_refresh_stale_developer_instructions() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = wiremock::MockServer::start().await;
    let stale_developer_message = "STALE_DEVELOPER_INSTRUCTIONS_SHOULD_BE_REMOVED";

    let mut start_builder =
        test_codex().with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    let initial = start_builder.build(&server).await?;
    let home = initial.home.clone();
    let rollout_path = initial
        .session_configured
        .rollout_path
        .clone()
        .expect("rollout path");

    let responses_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_assistant_message("m1", "BASELINE_REPLY"),
                responses::ev_completed("resp-1"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("m2", "AFTER_COMPACT_REPLY"),
                responses::ev_completed("resp-2"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("m3", "AFTER_RESUME_REPLY"),
                responses::ev_completed("resp-3"),
            ]),
        ],
    )
    .await;

    let compacted_history = vec![
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: stale_developer_message.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Compaction {
            id: None,
            encrypted_content: "ENCRYPTED_COMPACTION_SUMMARY".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    let compact_mock = responses::mount_compact_json_once(
        &server,
        serde_json::json!({ "output": compacted_history }),
    )
    .await;

    initial
        .codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "start remote compact flow".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&initial.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    initial.codex.submit(Op::Compact).await?;
    wait_for_event(&initial.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    initial
        .codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "after compact in same session".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&initial.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    initial.codex.submit(Op::Shutdown).await?;
    wait_for_event(&initial.codex, |ev| {
        matches!(ev, EventMsg::ShutdownComplete)
    })
    .await;

    let mut resume_builder =
        test_codex().with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    let resumed = resume_builder.resume(&server, home, rollout_path).await?;

    resumed
        .codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "after resume".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&resumed.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    assert_eq!(compact_mock.requests().len(), 1);
    let requests = responses_mock.requests();
    assert_eq!(requests.len(), 3, "expected three model requests");

    let after_compact_request = &requests[1];
    let after_resume_request = &requests[2];

    let after_compact_body = after_compact_request.body_json().to_string();
    assert!(
        !after_compact_body.contains(stale_developer_message),
        "stale developer instructions should be removed immediately after compaction"
    );
    assert!(
        after_compact_body.contains("<permissions instructions>"),
        "fresh developer instructions should be present after compaction"
    );
    assert!(
        after_compact_body.contains("ENCRYPTED_COMPACTION_SUMMARY"),
        "compaction item should be present after compaction"
    );

    let after_resume_body = after_resume_request.body_json().to_string();
    assert!(
        !after_resume_body.contains(stale_developer_message),
        "stale developer instructions should be removed after resume"
    );
    assert!(
        after_resume_body.contains("<permissions instructions>"),
        "fresh developer instructions should be present after resume"
    );
    assert!(
        after_resume_body.contains("ENCRYPTED_COMPACTION_SUMMARY"),
        "compaction item should persist after resume"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_compact_refreshes_stale_developer_instructions_without_resume() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = wiremock::MockServer::start().await;
    let stale_developer_message = "STALE_DEVELOPER_INSTRUCTIONS_SHOULD_BE_REMOVED";

    let mut builder = test_codex().with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    let test = builder.build(&server).await?;

    let responses_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_assistant_message("m1", "BASELINE_REPLY"),
                responses::ev_completed("resp-1"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("m2", "AFTER_COMPACT_REPLY"),
                responses::ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    let compacted_history = vec![
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: stale_developer_message.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Compaction {
            id: None,
            encrypted_content: "ENCRYPTED_COMPACTION_SUMMARY".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    let compact_mock = responses::mount_compact_json_once(
        &server,
        serde_json::json!({ "output": compacted_history }),
    )
    .await;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "start remote compact flow".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    test.codex.submit(Op::Compact).await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "after compact in same session".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    assert_eq!(compact_mock.requests().len(), 1);
    let requests = responses_mock.requests();
    assert_eq!(requests.len(), 2, "expected two model requests");

    let after_compact_body = requests[1].body_json().to_string();
    assert!(
        !after_compact_body.contains(stale_developer_message),
        "stale developer instructions should be removed immediately after compaction"
    );
    assert!(
        after_compact_body.contains("<permissions instructions>"),
        "fresh developer instructions should be present after compaction"
    );
    assert!(
        after_compact_body.contains("ENCRYPTED_COMPACTION_SUMMARY"),
        "compaction item should be present after compaction"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_request_shape_remote_pre_turn_compaction_restates_realtime_start() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = wiremock::MockServer::start().await;
    let realtime_server = start_remote_realtime_server().await;
    let mut builder = remote_realtime_test_codex_builder(&realtime_server).with_config(|config| {
        config.model_auto_compact_token_limit = Some(200);
    });
    let test = builder.build(&server).await?;

    let responses_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_assistant_message("m1", "REMOTE_FIRST_REPLY"),
                responses::ev_completed_with_tokens("r1", /*total_tokens*/ 500),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("m2", "REMOTE_SECOND_REPLY"),
                responses::ev_completed_with_tokens("r2", /*total_tokens*/ 80),
            ]),
        ],
    )
    .await;
    let compact_mock = responses::mount_compact_json_once(
        &server,
        serde_json::json!({
            "output": compacted_summary_only_output(
                "REMOTE_PRETURN_REALTIME_STILL_ACTIVE_SUMMARY"
            )
        }),
    )
    .await;

    start_realtime_conversation(test.codex.as_ref()).await?;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "USER_ONE".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "USER_TWO".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    assert_eq!(compact_mock.requests().len(), 1);
    let requests = responses_mock.requests();
    assert_eq!(requests.len(), 2, "expected two model requests");

    let compact_request = compact_mock.single_request();
    let post_compact_request = &requests[1];
    assert_request_contains_realtime_start(post_compact_request);

    insta::assert_snapshot!(
        "remote_pre_turn_compaction_restates_realtime_start_shapes",
        format_labeled_requests_snapshot(
            "Remote pre-turn auto-compaction while realtime remains active: compaction clears the reference baseline, so the follow-up request restates realtime-start instructions.",
            &[
                ("Remote Compaction Request", &compact_request),
                (
                    "Remote Post-Compaction History Layout",
                    post_compact_request
                ),
            ]
        )
    );

    close_realtime_conversation(test.codex.as_ref()).await?;
    realtime_server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_request_with_v3_initial_items_uses_custom_experimental_realtime_start_instructions()
-> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = wiremock::MockServer::start().await;
    let realtime_server = start_remote_realtime_server().await;
    let custom_instructions = "custom realtime start instructions";
    let mut builder = remote_realtime_test_codex_builder(&realtime_server).with_config({
        let custom_instructions = custom_instructions.to_string();
        move |config| {
            config.experimental_realtime_start_instructions = Some(custom_instructions);
        }
    });
    let test = builder.build(&server).await?;

    let responses_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("m1", "REMOTE_FIRST_REPLY"),
            responses::ev_completed("r1"),
        ]),
    )
    .await;

    start_realtime_conversation_with_initial_items(
        test.codex.as_ref(),
        vec![ConversationTextParams {
            text: "Seeded realtime history".to_string(),
            role: ConversationTextRole::User,
        }],
    )
    .await?;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "USER_ONE".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    assert_request_contains_custom_realtime_start(
        &responses_mock.single_request(),
        custom_instructions,
    );

    close_realtime_conversation(test.codex.as_ref()).await?;
    realtime_server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_realtime_does_not_diff_changed_start_instructions_after_resume() -> Result<()> {
=======
async fn active_realtime_refreshes_changed_start_instructions_only_after_compaction() -> Result<()>
{
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
    skip_if_no_network!(Ok(()));

    let server = wiremock::MockServer::start().await;
    let initial_realtime_server = start_remote_realtime_server().await;
    let initial_instructions = "initial custom realtime start instructions";
    let mut initial_builder = remote_realtime_test_codex_builder(&initial_realtime_server)
        .with_config({
            let initial_instructions = initial_instructions.to_string();
            move |config| {
                config.experimental_realtime_start_instructions = Some(initial_instructions);
            }
        });
    let initial = initial_builder.build(&server).await?;
    let home = initial.home.clone();
    let rollout_path = initial
        .session_configured
        .rollout_path
        .clone()
        .expect("rollout path");
    let responses_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![responses::ev_completed("r1")]),
            responses::sse(vec![responses::ev_completed("r2")]),
            responses::sse(vec![
                json!({
                    "type": "response.output_item.done",
                    "item": {"type": "compaction", "encrypted_content": "realtime-summary"},
                }),
                responses::ev_completed("r-compact"),
            ]),
            responses::sse(vec![responses::ev_completed("r3")]),
        ],
    )
    .await;

    start_realtime_conversation(initial.codex.as_ref()).await?;
    initial.submit_turn("USER_ONE").await?;
    close_realtime_conversation(initial.codex.as_ref()).await?;
    initial.codex.submit(Op::Shutdown).await?;
    wait_for_event(&initial.codex, |ev| {
        matches!(ev, EventMsg::ShutdownComplete)
    })
    .await;
    initial_realtime_server.shutdown().await;

    let resumed_realtime_server = start_remote_realtime_server().await;
    let changed_instructions = "changed custom realtime start instructions";
    let mut resume_builder = remote_realtime_test_codex_builder(&resumed_realtime_server)
        .with_config({
            let changed_instructions = changed_instructions.to_string();
            move |config| {
                config.experimental_realtime_start_instructions = Some(changed_instructions);
            }
        });
    let resumed = resume_builder.resume(&server, home, rollout_path).await?;

    start_realtime_conversation(resumed.codex.as_ref()).await?;
    resumed.submit_turn("USER_TWO").await?;
    resumed.codex.submit(Op::Compact).await?;
    wait_for_turn_complete(&resumed.codex).await;
    resumed.submit_turn("USER_THREE").await?;

    let requests = responses_mock.requests();
    assert_eq!(requests.len(), 4);
    assert_request_contains_custom_realtime_start(&requests[0], initial_instructions);
    let resumed_body = requests[1].body_json().to_string();
    assert!(
        resumed_body.contains(initial_instructions),
        "expected resumed history to retain the original realtime instructions"
    );
    assert!(
        !resumed_body.contains(changed_instructions),
        "did not expect an active-to-active instruction change to emit a diff"
    );
    assert_eq!(requests[2].inputs_of_type("compaction_trigger").len(), 1);
    assert_eq!(
        requests[3].inputs_of_type("compaction")[0]["encrypted_content"],
        "realtime-summary"
    );
    assert_request_contains_custom_realtime_start(&requests[3], changed_instructions);
    assert!(!requests[3].body_contains_text(initial_instructions));

    close_realtime_conversation(resumed.codex.as_ref()).await?;
    resumed_realtime_server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_mid_turn_compact_v2_sends_turn_state_over_http() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = TestCodexHarness::with_builder(
        test_codex()
            .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
            .with_config(|config| {
                config.model_auto_compact_token_limit = Some(200);
            }),
    )
    .await?;
    let codex = harness.test().codex.clone();
    let responses_mock = responses::mount_response_sequence(
        harness.server(),
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_function_call("call-before-compact", DUMMY_FUNCTION_NAME, "{}"),
                responses::ev_completed_with_tokens("r1", /*total_tokens*/ 500),
            ]))
            .insert_header(TURN_STATE_HEADER, "sampling-state"),
            responses::sse_response(responses::sse(vec![
                json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "compaction",
                        "encrypted_content": "V2_COMPACT_SUMMARY",
                    }
                }),
                responses::ev_completed("r-compact"),
            ]))
            .insert_header(TURN_STATE_HEADER, "compact-state"),
            responses::sse_response(responses::sse(vec![
                responses::ev_function_call("call-after-compact", DUMMY_FUNCTION_NAME, "{}"),
                responses::ev_completed_with_tokens("r2", /*total_tokens*/ 80),
            ]))
            .insert_header(TURN_STATE_HEADER, "continuation-state"),
            responses::sse_response(responses::sse(vec![
                responses::ev_assistant_message("m1", "FINAL_REPLY"),
                responses::ev_completed_with_tokens("r3", /*total_tokens*/ 80),
            ])),
        ],
    )
    .await;

    // Phase 1: sampling mints state and schedules inline v2 compaction.
    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "RUN_WITH_MID_TURN_COMPACT_V2".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_turn_complete(&codex).await;

    let requests = responses_mock.requests();
    assert_eq!(requests.len(), 4);
    assert!(
        requests
            .iter()
            .all(|request| request.path() == "/v1/responses")
    );
    assert_eq!(requests[0].header(TURN_STATE_HEADER), None);

    // Phase 2: the v2 compaction request replays the state already established by sampling.
    assert!(
        requests[1]
            .body_json()
            .to_string()
            .contains("\"type\":\"compaction_trigger\"")
    );
    assert_eq!(
        requests[1].header(TURN_STATE_HEADER).as_deref(),
        Some("sampling-state")
    );

    // Phase 3: later response headers do not replace the first value in the OnceLock.
    assert_eq!(
        requests[2].header(TURN_STATE_HEADER).as_deref(),
        Some("sampling-state")
    );
    assert_eq!(
        requests[3].header(TURN_STATE_HEADER).as_deref(),
        Some("sampling-state")
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_mid_turn_compact_v2_sends_turn_state_over_websocket() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_websocket_server(vec![vec![
        vec![
            responses::ev_response_created("warm-1"),
            responses::ev_completed("warm-1"),
        ],
        vec![
            json!({
                "type": "response.metadata",
                "headers": {(TURN_STATE_HEADER): "sampling-state"},
            }),
            responses::ev_function_call("call-before-compact", DUMMY_FUNCTION_NAME, "{}"),
            responses::ev_completed_with_tokens("r1", /*total_tokens*/ 500),
        ],
        vec![
            json!({
                "type": "response.metadata",
                "headers": {(TURN_STATE_HEADER): "compact-state"},
            }),
            json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "compaction",
                    "encrypted_content": "V2_WS_COMPACT_SUMMARY",
                }
            }),
            responses::ev_completed("r-compact"),
        ],
        vec![
            json!({
                "type": "response.metadata",
                "headers": {(TURN_STATE_HEADER): "continuation-state"},
            }),
            responses::ev_function_call("call-after-compact", DUMMY_FUNCTION_NAME, "{}"),
            responses::ev_completed_with_tokens("r2", /*total_tokens*/ 80),
        ],
        vec![
            responses::ev_assistant_message("m1", "FINAL_REPLY"),
            responses::ev_completed_with_tokens("r3", /*total_tokens*/ 80),
        ],
    ]])
    .await;
    let mut builder = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(|config| {
            config.model_auto_compact_token_limit = Some(200);
        });
    let test = builder.build_with_websocket_server(&server).await?;

    // Phase 1: startup prewarm stays empty, then WebSocket sampling mints state and schedules
    // inline v2 compaction.
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "RUN_WITH_WS_MID_TURN_COMPACT_V2".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_turn_complete(&test.codex).await;

    let requests = server.single_connection();
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[0].body_json()["generate"].as_bool(), Some(false));
    // Phase 2: the v2 compact request replays the state already established by sampling.
    assert!(
        requests[2]
            .body_json()
            .to_string()
            .contains("\"type\":\"compaction_trigger\"")
    );
    // Phase 3: both post-compact requests keep replaying that first value.
    assert_eq!(
        requests
            .iter()
            .map(|request| request.body_json()["client_metadata"][TURN_STATE_HEADER].clone())
            .collect::<Vec<_>>(),
        vec![
            json!(null),
            json!(null),
            json!("sampling-state"),
            json!("sampling-state"),
            json!("sampling-state"),
        ]
    );

    server.shutdown().await;
    Ok(())
}
