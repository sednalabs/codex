//! Integrated V2 continuity coverage for a provider-limited ThreadSpawn owner.
//!
//! The fixture uses a test-only lifecycle contributor to exercise the same typed owner
//! terminal-deferral and idle-start seam used by the goal extension. It deliberately keeps
//! provider identity and quota scope in the mock response; no independence is inferred.

use anyhow::Result;
use codex_core::ThreadManager;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::OwnerContinuationPending;
use codex_extension_api::ThreadIdleInput;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::TurnErrorInput;
use codex_extension_api::TurnLifecycleContributor;
use codex_features::Feature;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_response_once_match;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::oneshot;
use wiremock::Match;
use wiremock::Mock;
use wiremock::Request;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

const ROOT_PROMPT: &str = "root starts orchestrator";
const ORCHESTRATOR_TASK: &str = "orchestrator starts sub-orchestrator";
const SUB_ORCHESTRATOR_TASK: &str = "sub-orchestrator remains live";
const LIMIT_PROMPT: &str = "orchestrator provider limit";

#[derive(Clone, Debug)]
struct PendingOwnerContinuation {
    turn_id: String,
}

#[derive(Clone)]
struct ContinuityFixture {
    thread_manager: Arc<Mutex<Option<Weak<ThreadManager>>>>,
    continuation_starts: Arc<std::sync::atomic::AtomicUsize>,
    owner_idle_tx: tokio::sync::mpsc::UnboundedSender<ThreadId>,
    continuation_started_tx: tokio::sync::mpsc::UnboundedSender<ThreadId>,
}

impl ContinuityFixture {
    fn new(
        owner_idle_tx: tokio::sync::mpsc::UnboundedSender<ThreadId>,
        continuation_started_tx: tokio::sync::mpsc::UnboundedSender<ThreadId>,
    ) -> Self {
        Self {
            thread_manager: Arc::new(Mutex::new(None)),
            continuation_starts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            owner_idle_tx,
            continuation_started_tx,
        }
    }

    fn set_thread_manager(&self, thread_manager: &Arc<ThreadManager>) {
        *self.thread_manager.lock().expect("continuity fixture lock") =
            Some(Arc::downgrade(thread_manager));
    }
}

impl ThreadLifecycleContributor<codex_core::config::Config> for ContinuityFixture {
    fn on_thread_start<'a>(
        &'a self,
        input: ThreadStartInput<'a, codex_core::config::Config>,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if matches!(
                input.session_source,
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
            ) {
                input.thread_store.insert(ThreadSpawnOwner);
            }
        })
    }

    fn on_thread_idle<'a>(&'a self, input: ThreadIdleInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let Ok(thread_id) = ThreadId::from_string(input.thread_store.level_id()) else {
                return;
            };
            let _ = self.owner_idle_tx.send(thread_id);
            let Some(pending) = input.thread_store.remove::<PendingOwnerContinuation>() else {
                return;
            };
            debug_assert!(!pending.turn_id.is_empty());
            let Some(manager) = self
                .thread_manager
                .lock()
                .expect("continuity fixture lock")
                .as_ref()
                .and_then(Weak::upgrade)
            else {
                input.thread_store.insert((*pending).to_owned());
                return;
            };
            let Ok(thread) = manager.get_thread(thread_id).await else {
                input.thread_store.insert((*pending).to_owned());
                return;
            };
            if thread.try_start_turn_if_idle(Vec::new()).await.is_ok() {
                self.continuation_starts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = self.continuation_started_tx.send(thread_id);
            } else {
                input.thread_store.insert((*pending).to_owned());
            }
        })
    }
}

impl TurnLifecycleContributor for ContinuityFixture {
    fn on_turn_error<'a>(&'a self, input: TurnErrorInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if !matches!(input.error, CodexErrorInfo::UsageLimitExceeded)
                || input.thread_store.get::<ThreadSpawnOwner>().is_none()
            {
                return;
            }
            input.turn_store.insert(OwnerContinuationPending);
            input.thread_store.insert(PendingOwnerContinuation {
                turn_id: input.turn_id.to_string(),
            });
        })
    }
}

#[derive(Debug)]
struct ThreadSpawnOwner;

fn submit_user_turn(prompt: &str) -> Op {
    Op::UserInput {
        items: vec![UserInput::Text {
            text: prompt.to_string(),
            text_elements: Vec::new(),
        }],
        final_output_json_schema: None,
        responsesapi_client_metadata: None,
        additional_context: Default::default(),
        thread_settings: Default::default(),
    }
}

fn body_contains(request: &wiremock::Request, text: &str) -> bool {
    serde_json::from_slice::<Value>(&decoded_body(request))
        .is_ok_and(|body| body.to_string().contains(text))
}

fn decoded_body(request: &wiremock::Request) -> Vec<u8> {
    let is_zstd = request
        .headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|entry| entry.trim().eq_ignore_ascii_case("zstd"))
        });
    if is_zstd {
        zstd::stream::decode_all(std::io::Cursor::new(&request.body))
            .expect("failed to decode zstd request body")
    } else {
        request.body.clone()
    }
}

fn request_has_input_item(request: &wiremock::Request, input_type: &str, text: &str) -> bool {
    serde_json::from_slice::<Value>(&decoded_body(request))
        .ok()
        .and_then(|body| body["input"].as_array().cloned())
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item["type"] == input_type && item.to_string().contains(text))
        })
}

#[derive(Debug)]
struct LimitPromptMatcher;

impl Match for LimitPromptMatcher {
    fn matches(&self, request: &Request) -> bool {
        body_contains(request, LIMIT_PROMPT)
    }
}

#[derive(Debug)]
struct LimitSequenceResponder {
    calls: AtomicUsize,
    limit_response: ResponseTemplate,
    continuation_response: ResponseTemplate,
}

impl Respond for LimitSequenceResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        match self.calls.fetch_add(1, Ordering::SeqCst) {
            0 => self.limit_response.clone(),
            1 => self.continuation_response.clone(),
            call => panic!("unexpected provider-limit request {call}"),
        }
    }
}

async fn wait_for_thread_spawn(
    manager: &Arc<ThreadManager>,
    created_threads: &mut tokio::sync::broadcast::Receiver<ThreadId>,
    expected_parent_thread_id: ThreadId,
) -> Result<ThreadId> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let thread_id = created_threads.recv().await?;
            let Ok(thread) = manager.get_thread(thread_id).await else {
                continue;
            };
            let source = thread.config_snapshot().await.session_source;
            if matches!(
                source,
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn { parent_thread_id, .. })
                    if parent_thread_id == expected_parent_thread_id
            ) {
                return Ok(thread_id);
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("timed out waiting for ThreadSpawn publication"))?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_limit_defers_v2_owner_and_preserves_nested_descendant_identity() -> Result<()> {
    let server = start_mock_server().await;
    let root_spawn_args = serde_json::to_string(&json!({
        "message": ORCHESTRATOR_TASK,
        "task_name": "orchestrator",
        "fork_turns": "none"
    }))?;
    let sub_spawn_args = serde_json::to_string(&json!({
        "message": SUB_ORCHESTRATOR_TASK,
        "task_name": "sub_orchestrator",
        "fork_turns": "none"
    }))?;

    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, ROOT_PROMPT),
        sse(vec![
            ev_response_created("root-spawn"),
            ev_function_call_with_namespace(
                "root-spawn-call",
                "agents",
                "spawn_agent",
                &root_spawn_args,
            ),
            ev_completed("root-spawn"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, ORCHESTRATOR_TASK),
        sse(vec![
            ev_response_created("orchestrator-spawn"),
            ev_function_call_with_namespace(
                "sub-spawn-call",
                "agents",
                "spawn_agent",
                &sub_spawn_args,
            ),
            ev_completed("orchestrator-spawn"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "root-spawn-call"),
        sse(vec![
            ev_response_created("root-followup"),
            ev_assistant_message("root-done", "orchestrator started"),
            ev_completed("root-followup"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "sub-spawn-call"),
        sse(vec![
            ev_response_created("orchestrator-followup"),
            ev_assistant_message("orchestrator-ready", "sub-orchestrator started"),
            ev_completed("orchestrator-followup"),
        ]),
    )
    .await;
    let (child_release_tx, child_release_rx) = oneshot::channel();
    let child_chunks = vec![
        StreamingSseChunk {
            gate: None,
            body: sse(vec![ev_response_created("sub-live")]),
        },
        StreamingSseChunk {
            gate: Some(child_release_rx),
            body: sse(vec![
                ev_assistant_message("sub-complete", "sub-orchestrator complete"),
                ev_completed("sub-live"),
            ]),
        },
    ];
    let (child_stream_server, mut child_stream_completions) =
        start_streaming_sse_server(vec![child_chunks]).await;
    let child_stream_completion = child_stream_completions.remove(0);
    let sub_request = mount_response_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_input_item(request, "agent_message", SUB_ORCHESTRATOR_TASK)
        },
        ResponseTemplate::new(307).insert_header(
            "Location",
            format!("{}/v1/responses", child_stream_server.uri()),
        ),
    )
    .await;

    let limit_response = ResponseTemplate::new(429)
        .insert_header("content-type", "application/json")
        .set_body_json(json!({
            "error": {
                "type": "usage_limit_reached",
                "message": "provider limit",
                "resets_at": null,
                "plan_type": null
            }
        }));
    let continuation_response = sse_response(sse(vec![
        ev_response_created("orchestrator-continuation"),
        ev_assistant_message("orchestrator-resumed", "owner resumed"),
        ev_completed("orchestrator-continuation"),
    ]));
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .and(LimitPromptMatcher)
        .respond_with(LimitSequenceResponder {
            calls: AtomicUsize::new(0),
            limit_response,
            continuation_response,
        })
        .up_to_n_times(2)
        .expect(2)
        .mount(&server)
        .await;

    let (owner_idle_tx, mut owner_idle_rx) = tokio::sync::mpsc::unbounded_channel();
    let (continuation_started_tx, mut continuation_started_rx) =
        tokio::sync::mpsc::unbounded_channel();
    let continuity = Arc::new(ContinuityFixture::new(
        owner_idle_tx,
        continuation_started_tx,
    ));
    let thread_lifecycle_contributor: Arc<
        dyn ThreadLifecycleContributor<codex_core::config::Config>,
    > = continuity.clone();
    let turn_lifecycle_contributor: Arc<dyn TurnLifecycleContributor> = continuity.clone();
    let mut extensions = ExtensionRegistryBuilder::<codex_core::config::Config>::new();
    extensions.thread_lifecycle_contributor(thread_lifecycle_contributor);
    extensions.turn_lifecycle_contributor(turn_lifecycle_contributor);
    let mut builder = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("collab enabled");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("V2 enabled");
        });
    let test = builder.build(&server).await?;
    continuity.set_thread_manager(&test.thread_manager);
    let mut created_threads = test.thread_manager.subscribe_thread_created();
    let root_thread_id = test.session_configured.thread_id;
    test.submit_turn(ROOT_PROMPT).await?;
    let orchestrator_id =
        wait_for_thread_spawn(&test.thread_manager, &mut created_threads, root_thread_id).await?;
    let orchestrator = test.thread_manager.get_thread(orchestrator_id).await?;
    let initial_spawn_turn_id = loop {
        let event = wait_for_event(&orchestrator, |event| {
            matches!(event, EventMsg::TurnStarted(_))
        })
        .await;
        if let EventMsg::TurnStarted(started) = event {
            break started.turn_id;
        }
    };
    wait_for_event(&orchestrator, |event| {
        matches!(event, EventMsg::TurnComplete(complete) if complete.turn_id == initial_spawn_turn_id)
    })
    .await;
    let owner_idle_thread_id = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let thread_id = owner_idle_rx
                .recv()
                .await
                .expect("owner-idle signal channel should remain open");
            if thread_id == orchestrator_id {
                return thread_id;
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("timed out waiting for orchestrator owner-idle signal"))?;
    assert_eq!(owner_idle_thread_id, orchestrator_id);

    let sub_thread_id =
        wait_for_thread_spawn(&test.thread_manager, &mut created_threads, orchestrator_id).await?;
    let child_thread = test.thread_manager.get_thread(sub_thread_id).await?;
    let child_turn_id = loop {
        let event = wait_for_event(&child_thread, |event| {
            matches!(event, EventMsg::TurnStarted(_))
        })
        .await;
        if let EventMsg::TurnStarted(started) = event {
            break started.turn_id;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), child_stream_server.wait_for_request_count(1))
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for nested sidecar stream"))?;
    assert_eq!(
        1,
        sub_request.requests().len(),
        "one child initial delivery"
    );
    orchestrator.submit(submit_user_turn(LIMIT_PROMPT)).await?;
    let limit_turn_id = loop {
        let event = wait_for_event(&orchestrator, |event| {
            matches!(event, EventMsg::TurnStarted(_))
        })
        .await;
        if let EventMsg::TurnStarted(started) = event {
            break started.turn_id;
        }
    };
    wait_for_event(
        &orchestrator,
        |event| matches!(event, EventMsg::UserMessage(message) if message.message == LIMIT_PROMPT),
    )
    .await;
    let limit_turn_complete = loop {
        let event = wait_for_event(&orchestrator, |event| {
            matches!(event, EventMsg::TurnComplete(complete) if complete.turn_id == limit_turn_id)
        })
        .await;
        if let EventMsg::TurnComplete(complete) = event {
            break complete;
        }
    };
    assert_eq!(limit_turn_complete.turn_id, limit_turn_id, "provider turn must remain observable");
    assert!(matches!(
        limit_turn_complete
            .error
            .as_ref()
            .and_then(|error| error.codex_error_info.as_ref()),
        Some(CodexErrorInfo::UsageLimitExceeded)
    ));
    assert!(
        !matches!(orchestrator.agent_status().await, AgentStatus::Errored(_)),
        "owner status must stay non-terminal while continuation is admitted"
    );

    child_release_tx.send(()).expect("release child stream");
    tokio::time::timeout(Duration::from_secs(5), child_stream_completion)
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for child stream completion"))?
        .expect("child stream completion timestamp");
    wait_for_event(&child_thread, |event| {
        matches!(event, EventMsg::TurnComplete(complete) if complete.turn_id == child_turn_id)
    })
    .await;
    assert_eq!(
        child_thread.agent_status().await,
        AgentStatus::Completed(Some("sub-orchestrator complete".to_string()))
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let thread_id = continuation_started_rx
                .recv()
                .await
                .expect("continuation signal channel should remain open");
            if thread_id == orchestrator_id {
                return;
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("timed out waiting for owner continuation"))?;
    assert!(
        test.thread_manager
            .list_thread_ids()
            .await
            .contains(&orchestrator_id),
        "owner thread identity must remain stable"
    );
    let child_body = sub_request.requests()[0].body_json();
    let expected_child_thread_id = sub_thread_id.to_string();
    assert_eq!(
        child_body
            .pointer("/client_metadata/thread_id")
            .and_then(Value::as_str),
        Some(expected_child_thread_id.as_str())
    );
    assert_eq!(
        1,
        sub_request.requests().len(),
        "nested child must not replay"
    );
    assert_eq!(1, child_stream_server.requests().await.len());
    child_stream_server.shutdown().await;
    Ok(())
}
