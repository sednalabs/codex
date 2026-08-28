use core_test_support::test_codex::local_selections;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use codex_core::CodexThread;
use codex_core::config::CurrentTimeReminderConfig;
use codex_extension_items::ExtensionItem;
use codex_extension_items::sleep::SleepItem;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_protocol::AgentPath;
use codex_protocol::items::TurnItem;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HookEventName;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::user_input::UserInput;
use core_test_support::context_snapshot;
use core_test_support::context_snapshot::ContextSnapshotOptions;
use core_test_support::hooks::trust_discovered_hooks;
use core_test_support::responses;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_message_item_added;
use core_test_support::responses::ev_output_text_delta;
use core_test_support::responses::ev_reasoning_item;
use core_test_support::responses::ev_reasoning_item_added;
use core_test_support::responses::ev_response_created;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::StreamingSseServer;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::from_slice;
use serde_json::json;
use std::panic::AssertUnwindSafe;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::thread::ThreadId;
use std::time::Duration;
use test_case::test_case;
use tokio::sync::oneshot;
use tracing::Subscriber;
use tracing::span::Id;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;

const BOUNDARY_CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
const TURN_EVENT_TIMEOUT: Duration = Duration::from_secs(10);

fn ev_message_item_done(id: &str, text: &str) -> Value {
    serde_json::json!({
        "type": "response.output_item.done",
        "item": {
            "type": "message",
            "role": "assistant",
            "id": id,
            "content": [{"type": "output_text", "text": text}]
        }
    })
}

fn sse_event(event: Value) -> String {
    responses::sse(vec![event])
}

fn write_blocking_stop_hook(home: &Path) {
    let script_path = home.join("blocking_stop_hook.py");
    let started_path = home.join("started_stop_hook");
    let release_path = home.join("release_stop_hook");
    let script = format!(
        r#"from pathlib import Path
import time

Path(r"{}").touch()
release_path = Path(r"{}")
while not release_path.exists():
    time.sleep(0.01)
print("{{}}")
"#,
        started_path.display(),
        release_path.display(),
    );
    fs::write(&script_path, script).expect("write blocking stop hook");
    fs::write(
        home.join("hooks.json"),
        json!({
            "hooks": {
                "Stop": [{"hooks": [{
                    "type": "command",
                    "command": format!("python3 {}", script_path.display()),
                }]}]
            }
        })
        .to_string(),
    )
    .expect("write stop hook config");
}

fn message_input_texts(body: &Value, role: &str) -> Vec<String> {
    body.get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
        .filter(|item| item.get("role").and_then(Value::as_str) == Some(role))
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|span| span.get("type").and_then(Value::as_str) == Some("input_text"))
        .filter_map(|span| span.get("text").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

fn function_call_output_text<'a>(body: &'a Value, call_id: &str) -> Option<&'a str> {
    body.get("input")
        .and_then(Value::as_array)?
        .iter()
        .find(|item| {
            item.get("type").and_then(Value::as_str) == Some("function_call_output")
                && item.get("call_id").and_then(Value::as_str) == Some(call_id)
        })?
        .get("output")?
        .as_str()
}

fn assert_interrupted_sleep_output(output: Option<&str>) {
    let Some(output) = output else {
        panic!("sleep output missing");
    };
    let Some(wall_time) = output
        .strip_prefix("Wall time: ")
        .and_then(|output| output.strip_suffix(" seconds\nSleep interrupted by new input."))
    else {
        panic!("sleep output should include wall time");
    };
    assert!(
        wall_time.parse::<f64>().is_ok(),
        "sleep wall time should be a number"
    );
}

fn chunk(event: Value) -> StreamingSseChunk {
    StreamingSseChunk {
        gate: None,
        body: responses::sse(vec![event]),
    }
}

fn gated_chunk(gate: oneshot::Receiver<()>, events: Vec<Value>) -> StreamingSseChunk {
    StreamingSseChunk {
        gate: Some(gate),
        body: responses::sse(events),
    }
}

fn response_completed_chunks(response_id: &str) -> Vec<StreamingSseChunk> {
    vec![
        chunk(ev_response_created(response_id)),
        chunk(ev_completed(response_id)),
    ]
}

async fn build_codex(server: &StreamingSseServer) -> Arc<CodexThread> {
    test_codex()
        .with_model("gpt-5.4")
        .build_with_streaming_server(server)
        .await
        .expect("build streaming Codex test session")
        .codex
}

/// Observes the natural close of the task-run span and holds finalization until the test releases
/// it. The scoped subscriber is installed by each test, so this helper has no process-global state.
struct RegularTaskRunBoundaryObserver {
    armed: Arc<AtomicBool>,
    observed: Arc<AtomicUsize>,
    target_thread: ThreadId,
    reached: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
}

struct RegularTaskRunBoundaryControl {
    armed: Arc<AtomicBool>,
    reached: mpsc::Receiver<()>,
    release: mpsc::Sender<()>,
}

impl RegularTaskRunBoundaryObserver {
    fn new() -> (Self, RegularTaskRunBoundaryControl) {
        let armed = Arc::new(AtomicBool::new(false));
        let observed = Arc::new(AtomicUsize::new(0));
        let (reached, reached_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        (
            Self {
                armed: Arc::clone(&armed),
                observed: Arc::clone(&observed),
                target_thread: std::thread::current().id(),
                reached,
                release: Mutex::new(release_rx),
            },
            RegularTaskRunBoundaryControl {
                armed,
                reached: reached_rx,
                release,
            },
        )
    }
}

impl<S> Layer<S> for RegularTaskRunBoundaryObserver
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let is_regular_task_run = ctx
            .metadata(&id)
            .is_some_and(|metadata| metadata.name() == "session_task.run");
        if std::thread::current().id() != self.target_thread
            || !is_regular_task_run
            || !self.armed.swap(false, Ordering::SeqCst)
        {
            return;
        }

        self.observed.fetch_add(1, Ordering::SeqCst);
        self.reached.send(()).unwrap_or_else(|_| {
            panic!(
                "regular task-run boundary observer control thread disconnected before boundary notification"
            )
        });
        let release = self
            .release
            .lock()
            .expect("regular task-run boundary release mutex should not be poisoned");
        match release.recv_timeout(BOUNDARY_CONTROL_TIMEOUT) {
            Ok(()) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => panic!(
                "regular task-run boundary observer timed out after {BOUNDARY_CONTROL_TIMEOUT:?} waiting for finalization release"
            ),
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!(
                "regular task-run boundary observer control thread disconnected before finalization release"
            ),
        }
    }
}

impl RegularTaskRunBoundaryControl {
    fn arm(&self) {
        assert!(
            !self.armed.swap(true, Ordering::SeqCst),
            "boundary observer should be armed only once"
        );
    }

    fn spawn_releaser(self, after_boundary: impl FnOnce() + Send + 'static) -> JoinHandle<()> {
        std::thread::spawn(move || {
            match self.reached.recv_timeout(BOUNDARY_CONTROL_TIMEOUT) {
                Ok(()) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => panic!(
                    "regular task-run boundary control timed out after {BOUNDARY_CONTROL_TIMEOUT:?} waiting for task-run close"
                ),
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!(
                    "regular task-run boundary control disconnected before task-run close"
                ),
            }
            let result = std::panic::catch_unwind(AssertUnwindSafe(after_boundary));
            let release_result = self.release.send(());
            if let Err(payload) = result {
                // Always release the observer before propagating a callback panic. If the
                // observer has already failed, preserve the original callback panic rather than
                // replacing it with a second synchronization failure.
                std::mem::drop(release_result);
                std::panic::resume_unwind(payload);
            }
            match release_result {
                Ok(()) => {}
                Err(_) => panic!(
                    "regular task-run boundary observer disconnected before finalization release"
                ),
            }
        })
    }
}

struct ThreadIdleCounter {
    tx: tokio::sync::watch::Sender<usize>,
}

impl codex_extension_api::ThreadLifecycleContributor<codex_core::config::Config>
    for ThreadIdleCounter
{
    fn on_thread_idle<'a>(
        &'a self,
        _input: codex_extension_api::ThreadIdleInput<'a>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            self.tx.send_modify(|count| *count += 1);
        })
    }
}

struct TurnLifecycleObservation {
    turn_started: usize,
    turn_complete: usize,
    matching_user_messages: usize,
}

async fn observe_turn_completion(
    codex: &CodexThread,
    matching_user_message: Option<&str>,
) -> TurnLifecycleObservation {
    let mut observation = TurnLifecycleObservation {
        turn_started: 0,
        turn_complete: 0,
        matching_user_messages: 0,
    };
    loop {
        let event = tokio::time::timeout(TURN_EVENT_TIMEOUT, codex.next_event())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "timed out after {TURN_EVENT_TIMEOUT:?} waiting for TurnComplete; observed turn_started={}, turn_complete={}, matching_user_messages={}, expected_user_message={matching_user_message:?}",
                    observation.turn_started,
                    observation.turn_complete,
                    observation.matching_user_messages,
                )
            })
            .unwrap_or_else(|| {
                panic!(
                    "event stream closed before TurnComplete; observed turn_started={}, turn_complete={}, matching_user_messages={}, expected_user_message={matching_user_message:?}",
                    observation.turn_started,
                    observation.turn_complete,
                    observation.matching_user_messages,
                )
            });
        match event.msg {
            EventMsg::TurnStarted(_) => observation.turn_started += 1,
            EventMsg::UserMessage(message)
                if matching_user_message.is_some_and(|expected| message.message == expected) =>
            {
                observation.matching_user_messages += 1;
            }
            EventMsg::TurnComplete(_) => {
                observation.turn_complete += 1;
                break;
            }
            _ => {}
        }
    }
    observation
}

async fn wait_for_thread_idle_after(
    idle_rx: &mut tokio::sync::watch::Receiver<usize>,
    previous_count: usize,
) {
    tokio::time::timeout(TURN_EVENT_TIMEOUT, async {
        loop {
            if *idle_rx.borrow_and_update() > previous_count {
                return;
            }
            idle_rx
                .changed()
                .await
                .expect("thread idle counter sender should remain available");
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "timed out after {TURN_EVENT_TIMEOUT:?} waiting for thread idle after turn finalization; previous_idle_count={previous_count}"
        )
    });
}

#[test]
fn regular_task_run_boundary_observer_self_test() {
    let (observer, control) = RegularTaskRunBoundaryObserver::new();
    let observed = Arc::clone(&observer.observed);
    let subscriber = tracing_subscriber::registry().with(observer);
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    control.arm();
    let release_thread = control.spawn_releaser(|| {});
    let run_span = tracing::trace_span!("session_task.run");
    drop(run_span);
    release_thread
        .join()
        .expect("boundary self-test worker should exit cleanly");
    assert_eq!(observed.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn regular_task_run_boundary_observer_known_good_turn_finalization() {
    let (server, _completions) =
        start_streaming_sse_server(vec![response_completed_chunks("resp-1")]).await;
    let codex = build_codex(&server).await;
    let (observer, control) = RegularTaskRunBoundaryObserver::new();
    let observed = Arc::clone(&observer.observed);
    let subscriber = tracing_subscriber::registry().with(observer);
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    control.arm();
    let release_thread = control.spawn_releaser(|| {});
    submit_user_input(&codex, "observer control").await;
    let lifecycle = observe_turn_completion(&codex, None).await;
    release_thread
        .join()
        .expect("known-good boundary worker should exit cleanly");

    assert_eq!(observed.load(Ordering::SeqCst), 1);
    assert_eq!(lifecycle.turn_started, 1);
    assert_eq!(lifecycle.turn_complete, 1);
    assert_eq!(server.requests().await.len(), 1);
    server.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn base_turn_does_not_reopen_after_boundary_steer() {
    const INITIAL_PROMPT: &str = "boundary control";
    const STEER_PROMPT: &str = "boundary follow-up";
    const CLIENT_ID: &str = "boundary-client-id";

    let (server, _completions) = start_streaming_sse_server(vec![
        response_completed_chunks("resp-1"),
        response_completed_chunks("resp-2"),
    ])
    .await;
    let (thread_idle_tx, mut thread_idle_rx) = tokio::sync::watch::channel(0_usize);
    let mut extensions =
        codex_extension_api::ExtensionRegistryBuilder::<codex_core::config::Config>::new();
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdleCounter { tx: thread_idle_tx }));
    let codex = test_codex()
        .with_model("gpt-5.4")
        .with_extensions(Arc::new(extensions.build()))
        .build_with_streaming_server(&server)
        .await
        .expect("build streaming Codex test session")
        .codex;
    let idle_count_before = *thread_idle_rx.borrow();
    let (observer, control) = RegularTaskRunBoundaryObserver::new();
    let observed = Arc::clone(&observer.observed);
    let subscriber = tracing_subscriber::registry().with(observer);
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    let runtime_handle = tokio::runtime::Handle::current();
    let codex_for_steer = Arc::clone(&codex);
    control.arm();
    let release_thread = control.spawn_releaser(move || {
        let steer_result = runtime_handle.block_on(async {
            codex_for_steer
                .steer_input(
                    vec![UserInput::Text {
                        text: STEER_PROMPT.to_string(),
                        text_elements: Vec::new(),
                    }],
                    /*additional_context*/ Default::default(),
                    /*expected_turn_id*/ None,
                    /*client_user_message_id*/ Some(CLIENT_ID.to_string()),
                    /*responsesapi_client_metadata*/ None,
                )
                .await
        });
        assert!(
            steer_result.is_ok(),
            "boundary steer should be accepted while the task-run span is closing: {steer_result:?}"
        );
    });

    submit_user_input(&codex, INITIAL_PROMPT).await;
    let lifecycle = observe_turn_completion(&codex, Some(STEER_PROMPT)).await;
    release_thread
        .join()
        .expect("boundary steer worker should exit cleanly");

    assert_eq!(observed.load(Ordering::SeqCst), 1);
    assert_eq!(lifecycle.turn_started, 1);
    assert_eq!(lifecycle.turn_complete, 1);
    assert_eq!(lifecycle.matching_user_messages, 1);
    // TurnComplete is emitted before task finalization clears the active turn. The thread-idle
    // callback is the natural post-finalization boundary; with no mailbox work queued, the
    // subsequent pending-work admission must not produce a delayed second provider request.
    wait_for_thread_idle_after(&mut thread_idle_rx, idle_count_before).await;
    assert_eq!(server.requests().await.len(), 1);
    server.shutdown().await;
}

async fn submit_user_input(codex: &CodexThread, text: &str) {
    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: text.to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await
        .expect("submit user input");
}

async fn submit_danger_full_access_user_turn(test: &TestCodex, text: &str) {
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.config.cwd.as_path());
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: text.to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(local_selections(test.config.cwd.clone())),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: test.session_configured.model.clone(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await
        .expect("submit user turn");
}

async fn steer_user_input(codex: &CodexThread, text: &str) {
    codex
        .steer_input(
            vec![UserInput::Text {
                text: text.to_string(),
                text_elements: Vec::new(),
            }],
            /*additional_context*/ Default::default(),
            /*expected_turn_id*/ None,
            /*client_user_message_id*/ None,
            /*responsesapi_client_metadata*/ None,
        )
        .await
        .expect("steer user input");
}

async fn enqueue_queue_only_agent_mail(codex: &CodexThread, text: &str) {
    codex
        .submit(Op::InterAgentCommunication {
            communication: InterAgentCommunication::new(
                AgentPath::try_from("/root/worker").expect("worker path should parse"),
                AgentPath::root(),
                Vec::new(),
                text.to_string(),
                /*trigger_turn*/ false,
            ),
        })
        .await
        .expect("submit queue-only agent mail");
}

async fn submit_queue_only_agent_mail(codex: &CodexThread, text: &str) {
    enqueue_queue_only_agent_mail(codex, text).await;
    codex
        .submit(Op::RealtimeConversationListVoices)
        .await
        .expect("submit list-voices barrier");
    wait_for_event(codex, |event| {
        matches!(event, EventMsg::RealtimeConversationListVoicesResponse(_))
    })
    .await;
}

async fn wait_for_reasoning_item_started(codex: &CodexThread) {
    wait_for_event(codex, |event| {
        matches!(
            event,
            EventMsg::ItemStarted(item_started)
                if matches!(&item_started.item, TurnItem::Reasoning(_))
        )
    })
    .await;
}

async fn wait_for_agent_message(codex: &CodexThread, text: &str) {
    let final_message = wait_for_event(
        codex,
        |event| matches!(event, EventMsg::AgentMessage(message) if message.message == text),
    )
    .await;
    assert!(matches!(final_message, EventMsg::AgentMessage(_)));
}

async fn wait_for_turn_complete(codex: &CodexThread) {
    wait_for_event(codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
}

async fn wait_for_sleep_item_started(codex: &CodexThread, call_id: &str, duration_ms: u64) {
    let event = wait_for_event(codex, |event| {
        matches!(
            event,
            EventMsg::ItemStarted(started)
                if matches!(
                    &started.item,
                    TurnItem::Extension(ExtensionItem::Sleep(item)) if item.id == call_id
                )
        )
    })
    .await;
    let EventMsg::ItemStarted(started) = event else {
        unreachable!("wait predicate only accepts item/started events");
    };
    let TurnItem::Extension(ExtensionItem::Sleep(item)) = started.item else {
        unreachable!("wait predicate only accepts sleep items");
    };
    assert_eq!(
        item,
        SleepItem {
            id: call_id.to_string(),
            duration_ms,
        }
    );
}

async fn wait_for_sleep_item_completed(codex: &CodexThread, call_id: &str, duration_ms: u64) {
    let event = wait_for_event(codex, |event| {
        matches!(
            event,
            EventMsg::ItemCompleted(completed)
                if matches!(
                    &completed.item,
                    TurnItem::Extension(ExtensionItem::Sleep(item)) if item.id == call_id
                )
        )
    })
    .await;
    let EventMsg::ItemCompleted(completed) = event else {
        unreachable!("wait predicate only accepts item/completed events");
    };
    let TurnItem::Extension(ExtensionItem::Sleep(item)) = completed.item else {
        unreachable!("wait predicate only accepts sleep items");
    };
    assert_eq!(
        item,
        SleepItem {
            id: call_id.to_string(),
            duration_ms,
        }
    );
}

struct SleepingRootExtension;

impl codex_extension_api::ThreadLifecycleContributor<codex_core::config::Config>
    for SleepingRootExtension
{
    fn on_thread_start<'a>(
        &'a self,
        input: codex_extension_api::ThreadStartInput<'a, codex_core::config::Config>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            input.thread_store.insert(SleepItem {
                id: "clock-wait-1".to_string(),
                duration_ms: 60_000,
            });
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_only_agent_mail_wakes_sleeping_root_and_persists_message() {
    const CHILD_MESSAGE: &str = "worker completed";

    let (server, _completions) =
        start_streaming_sse_server(vec![response_completed_chunks("resp-1")]).await;
    let mut extensions =
        codex_extension_api::ExtensionRegistryBuilder::<codex_core::config::Config>::new();
    extensions.thread_lifecycle_contributor(Arc::new(SleepingRootExtension));
    let codex = test_codex()
        .with_model("gpt-5.4")
        .with_extensions(Arc::new(extensions.build()))
        .build_with_streaming_server(&server)
        .await
        .expect("build Codex test session")
        .codex;

    enqueue_queue_only_agent_mail(&codex, CHILD_MESSAGE).await;
    wait_for_turn_complete(&codex).await;

    assert_eq!(server.requests().await.len(), 1);
    let history = codex
        .load_history(/*include_archived*/ true)
        .await
        .expect("load persisted thread history");
    assert!(history.items.iter().any(|item| {
        matches!(
            item,
            RolloutItem::ResponseItem(codex_protocol::models::ResponseItem::AgentMessage {
                content,
                ..
            }) if content.iter().any(|content| matches!(
                content,
                codex_protocol::models::AgentMessageInputContent::InputText { text }
                    if text == CHILD_MESSAGE
            ))
        )
    }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steer_interrupts_wait_agent_and_is_sent_in_follow_up_request() {
    const WAIT_CALL_ID: &str = "wait-call";
    const INITIAL_PROMPT: &str = "wait for an agent";
    const STEER_PROMPT: &str = "stop waiting and continue";
    const MULTI_AGENT_V2_NAMESPACE: &str = "agents";

    let first_chunks = vec![
        chunk(ev_response_created("resp-1")),
        chunk(ev_function_call_with_namespace(
            WAIT_CALL_ID,
            MULTI_AGENT_V2_NAMESPACE,
            "wait_agent",
            r#"{"timeout_ms":10000}"#,
        )),
        chunk(ev_completed("resp-1")),
    ];
    let (server, _completions) =
        start_streaming_sse_server(vec![first_chunks, response_completed_chunks("resp-2")]).await;
    let codex = test_codex()
        .with_model("gpt-5.4")
        .with_config(|config| {
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("test config should allow feature update");
        })
        .build_with_streaming_server(&server)
        .await
        .expect("build Codex test session")
        .codex;

    submit_user_input(&codex, INITIAL_PROMPT).await;
    wait_for_event(&codex, |event| {
        matches!(event, EventMsg::CollabWaitingBegin(_))
    })
    .await;

    steer_user_input(&codex, STEER_PROMPT).await;
    wait_for_turn_complete(&codex).await;

    let requests = server.requests().await;
    assert_eq!(requests.len(), 2);
    let second: Value = from_slice(&requests[1]).expect("parse second request");
    let relevant_user_input = message_input_texts(&second, "user")
        .into_iter()
        .filter(|text| text == INITIAL_PROMPT || text == STEER_PROMPT)
        .collect::<Vec<_>>();
    assert_eq!(
        relevant_user_input,
        vec![INITIAL_PROMPT.to_string(), STEER_PROMPT.to_string()]
    );
    let wait_output = function_call_output_text(&second, WAIT_CALL_ID).expect("wait_agent output");
    let wait_output = serde_json::from_str::<Value>(wait_output).expect("parse wait_agent output");
    assert_eq!(wait_output.get("timed_out"), Some(&json!(false)));
    assert_eq!(
        wait_output.get("message"),
        Some(&json!("Wait woke due to mailbox activity."))
    );

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn any_new_input_interrupts_sleep() {
    const FIRST_SLEEP_CALL_ID: &str = "sleep-call-1";
    const SECOND_SLEEP_CALL_ID: &str = "sleep-call-2";
    const SLEEP_DURATION_MS: u64 = 3_600_000;
    const INITIAL_PROMPT: &str = "sleep for a while";
    const STEER_PROMPT: &str = "stop sleeping and continue";
    let sleep_arguments = json!({ "duration_ms": SLEEP_DURATION_MS }).to_string();

    let first_chunks = vec![
        chunk(ev_response_created("resp-1")),
        chunk(ev_function_call_with_namespace(
            FIRST_SLEEP_CALL_ID,
            "clock",
            "sleep",
            &sleep_arguments,
        )),
        chunk(ev_completed("resp-1")),
    ];
    let second_chunks = vec![
        chunk(ev_response_created("resp-2")),
        chunk(ev_function_call_with_namespace(
            SECOND_SLEEP_CALL_ID,
            "clock",
            "sleep",
            &sleep_arguments,
        )),
        chunk(ev_completed("resp-2")),
    ];
    let (server, _completions) = start_streaming_sse_server(vec![
        first_chunks,
        second_chunks,
        response_completed_chunks("resp-3"),
    ])
    .await;
    let codex = test_codex()
        .with_model("gpt-5.4")
        .with_config(|config| {
            config
                .features
                .enable(Feature::CurrentTimeReminder)
                .expect("test config should allow current-time reminders");
            config.current_time_reminder = Some(CurrentTimeReminderConfig {
                sleep_tool: true,
                ..CurrentTimeReminderConfig::default()
            });
        })
        .build_with_streaming_server(&server)
        .await
        .expect("build Codex test session")
        .codex;

    submit_user_input(&codex, INITIAL_PROMPT).await;
    wait_for_sleep_item_started(&codex, FIRST_SLEEP_CALL_ID, SLEEP_DURATION_MS).await;

    steer_user_input(&codex, STEER_PROMPT).await;
    wait_for_sleep_item_completed(&codex, FIRST_SLEEP_CALL_ID, SLEEP_DURATION_MS).await;
    wait_for_sleep_item_started(&codex, SECOND_SLEEP_CALL_ID, SLEEP_DURATION_MS).await;

    enqueue_queue_only_agent_mail(&codex, "new mailbox input").await;
    wait_for_sleep_item_completed(&codex, SECOND_SLEEP_CALL_ID, SLEEP_DURATION_MS).await;
    wait_for_turn_complete(&codex).await;

    let requests = server.requests().await;
    assert_eq!(requests.len(), 3);
    let second: Value = from_slice(&requests[1]).expect("parse second request");
    let relevant_user_input = message_input_texts(&second, "user")
        .into_iter()
        .filter(|text| text == INITIAL_PROMPT || text == STEER_PROMPT)
        .collect::<Vec<_>>();
    assert_eq!(
        relevant_user_input,
        vec![INITIAL_PROMPT.to_string(), STEER_PROMPT.to_string()]
    );
    assert_interrupted_sleep_output(function_call_output_text(&second, FIRST_SLEEP_CALL_ID));

    let third: Value = from_slice(&requests[2]).expect("parse third request");
    assert_interrupted_sleep_output(function_call_output_text(&third, SECOND_SLEEP_CALL_ID));

    codex.submit(Op::Shutdown).await.expect("shutdown session");
    wait_for_event(&codex, |event| matches!(event, EventMsg::ShutdownComplete)).await;

    let rollout_path = codex.rollout_path().expect("rollout path");
    let rollout = tokio::fs::read_to_string(rollout_path)
        .await
        .expect("read rollout");
    let persisted_sleep_items = rollout
        .lines()
        .filter_map(|line| serde_json::from_str::<RolloutLine>(line).ok())
        .filter_map(|line| match line.item {
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) => match event.item {
                TurnItem::Extension(ExtensionItem::Sleep(item)) => Some(item),
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        persisted_sleep_items,
        vec![
            SleepItem {
                id: FIRST_SLEEP_CALL_ID.to_string(),
                duration_ms: SLEEP_DURATION_MS,
            },
            SleepItem {
                id: SECOND_SLEEP_CALL_ID.to_string(),
                duration_ms: SLEEP_DURATION_MS,
            },
        ]
    );

    server.shutdown().await;
}

fn assert_two_responses_input_snapshot(snapshot_name: &str, requests: &[Vec<u8>]) {
    assert_eq!(requests.len(), 2);
    let options = ContextSnapshotOptions::default().strip_capability_instructions();
    let first: Value = from_slice(&requests[0]).expect("parse first request");
    let second: Value = from_slice(&requests[1]).expect("parse second request");
    let first_items = first["input"]
        .as_array()
        .expect("first request input")
        .clone();
    let second_items = second["input"]
        .as_array()
        .expect("second request input")
        .clone();
    let snapshot = context_snapshot::format_labeled_items_snapshot(
        "/responses POST bodies (input only, redacted like other suite snapshots)",
        &[
            ("First request", first_items.as_slice()),
            ("Second request", second_items.as_slice()),
        ],
        &options,
    );
    insta::assert_snapshot!(snapshot_name, snapshot);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "TODO(aibrahim): flaky"]
async fn injected_user_input_triggers_follow_up_request_with_deltas() {
    let (gate_completed_tx, gate_completed_rx) = oneshot::channel();

    let first_chunks = vec![
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_response_created("resp-1")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_message_item_added("msg-1", "")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_output_text_delta("first ")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_output_text_delta("turn")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_message_item_done("msg-1", "first turn")),
        },
        StreamingSseChunk {
            gate: Some(gate_completed_rx),
            body: sse_event(ev_completed("resp-1")),
        },
    ];

    let second_chunks = vec![
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_response_created("resp-2")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_completed("resp-2")),
        },
    ];

    let (server, _completions) =
        start_streaming_sse_server(vec![first_chunks, second_chunks]).await;

    let codex = test_codex()
        .with_model("gpt-5.4")
        .build_with_streaming_server(&server)
        .await
        .unwrap()
        .codex;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "first prompt".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await
        .unwrap();

    wait_for_event(&codex, |event| {
        matches!(event, EventMsg::AgentMessageContentDelta(_))
    })
    .await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "second prompt".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await
        .unwrap();

    let _ = gate_completed_tx.send(());

    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let requests = server.requests().await;
    assert_eq!(requests.len(), 2);

    let first_body: Value = serde_json::from_slice(&requests[0]).expect("parse first request");
    let second_body: Value = serde_json::from_slice(&requests[1]).expect("parse second request");

    let first_texts = message_input_texts(&first_body, "user");
    assert!(first_texts.iter().any(|text| text == "first prompt"));
    assert!(!first_texts.iter().any(|text| text == "second prompt"));

    let second_texts = message_input_texts(&second_body, "user");
    assert!(second_texts.iter().any(|text| text == "first prompt"));
    assert!(second_texts.iter().any(|text| text == "second prompt"));

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_inter_agent_mail_triggers_follow_up_after_reasoning_item() {
    let (gate_reasoning_done_tx, gate_reasoning_done_rx) = oneshot::channel();

    let first_chunks = vec![
        chunk(ev_response_created("resp-1")),
        chunk(ev_reasoning_item_added("reason-1", &["thinking"])),
        gated_chunk(
            gate_reasoning_done_rx,
            vec![
                ev_reasoning_item("reason-1", &["thinking"], &[]),
                ev_function_call(
                    "call-stale",
                    "shell",
                    r#"{"command":"echo stale tool call"}"#,
                ),
                ev_message_item_added("msg-stale", ""),
                ev_output_text_delta("stale final"),
                ev_message_item_done("msg-stale", "stale final"),
                ev_completed("resp-1"),
            ],
        ),
    ];

    let (server, _completions) =
        start_streaming_sse_server(vec![first_chunks, response_completed_chunks("resp-2")]).await;

    let codex = build_codex(&server).await;

    submit_user_input(&codex, "first prompt").await;

    wait_for_reasoning_item_started(&codex).await;

    submit_queue_only_agent_mail(&codex, "queued child update").await;

    let _ = gate_reasoning_done_tx.send(());

    wait_for_turn_complete(&codex).await;

    let requests = server.requests().await;
    assert_two_responses_input_snapshot("pending_input_queued_mail_after_reasoning", &requests);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_inter_agent_mail_triggers_follow_up_after_commentary_message_item() {
    let (gate_message_done_tx, gate_message_done_rx) = oneshot::channel();

    let first_chunks = vec![
        chunk(ev_response_created("resp-1")),
        chunk(ev_message_item_added("msg-1", "")),
        gated_chunk(
            gate_message_done_rx,
            vec![
                ev_output_text_delta("first answer"),
                json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "message",
                        "role": "assistant",
                        "id": "msg-1",
                        "content": [{"type": "output_text", "text": "first answer"}],
                        "phase": "commentary",
                    }
                }),
                ev_function_call(
                    "call-stale",
                    "shell",
                    r#"{"command":"echo stale tool call"}"#,
                ),
                ev_message_item_added("msg-stale", ""),
                ev_output_text_delta("stale final"),
                ev_message_item_done("msg-stale", "stale final"),
                ev_completed("resp-1"),
            ],
        ),
    ];

    let (server, _completions) =
        start_streaming_sse_server(vec![first_chunks, response_completed_chunks("resp-2")]).await;

    let codex = build_codex(&server).await;

    submit_user_input(&codex, "first prompt").await;

    wait_for_event(&codex, |event| {
        matches!(
            event,
            EventMsg::ItemStarted(item_started)
                if matches!(&item_started.item, TurnItem::AgentMessage(_))
        )
    })
    .await;

    submit_queue_only_agent_mail(&codex, "queued child update").await;

    let _ = gate_message_done_tx.send(());

    wait_for_agent_message(&codex, "first answer").await;

    wait_for_turn_complete(&codex).await;

    let requests = server.requests().await;
    assert_two_responses_input_snapshot("pending_input_queued_mail_after_commentary", &requests);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_inter_agent_mail_does_not_restart_after_final_answer() {
    let first_chunks = vec![
        chunk(ev_response_created("resp-1")),
        chunk(ev_message_item_added("msg-1", "")),
        chunk(ev_output_text_delta("first answer")),
        chunk(json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "role": "assistant",
                "id": "msg-1",
                "content": [{"type": "output_text", "text": "first answer"}],
                "phase": "final_answer",
            }
        })),
        chunk(ev_completed("resp-1")),
    ];

    let (server, _completions) = start_streaming_sse_server(vec![
        first_chunks,
        response_completed_chunks("unexpected-resp-2"),
    ])
    .await;
    let codex = build_codex(&server).await;

    submit_queue_only_agent_mail(&codex, "queued child update").await;
    submit_user_input(&codex, "first prompt").await;
    wait_for_turn_complete(&codex).await;

    let mut requests = server.requests().await;
    assert_eq!(requests.len(), 1);
    let request: Value = from_slice(&requests[0]).expect("parse request");
    assert!(
        request["input"]
            .as_array()
            .expect("request input")
            .iter()
            .all(|item| item.get("type").and_then(Value::as_str) != Some("agent_message"))
    );

    submit_user_input(&codex, "second prompt").await;
    wait_for_turn_complete(&codex).await;

    requests = server.requests().await;
    assert_eq!(requests.len(), 2);
    let request: Value = from_slice(&requests[1]).expect("parse request");
    let input = request["input"].as_array().expect("request input");
    let agent_message = input
        .iter()
        .find(|item| item.get("type").and_then(Value::as_str) == Some("agent_message"))
        .expect("queued child update should be included in the next turn");
    assert_eq!(
        agent_message["content"],
        json!([{"type": "input_text", "text": "queued child update"}])
    );
    let user_input = message_input_texts(&request, "user")
        .into_iter()
        .filter(|text| text == "second prompt")
        .collect::<Vec<_>>();
    assert_eq!(user_input, vec!["second prompt"]);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_response_item_reopens_turn_after_final_answer() {
    const INITIAL_PROMPT: &str = "first prompt";
    const INJECTED_CONTEXT: &str = "late injected context";
    let (gate_completed_tx, gate_completed_rx) = oneshot::channel();

    let first_chunks = vec![
        chunk(ev_response_created("resp-1")),
        chunk(ev_message_item_added("msg-1", "")),
        chunk(ev_output_text_delta("first answer")),
        chunk(json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "role": "assistant",
                "id": "msg-1",
                "content": [{"type": "output_text", "text": "first answer"}],
                "phase": "final_answer",
            }
        })),
        // Keep the response open past an observable event so the answer boundary is established
        // before the late context is injected.
        chunk(ev_reasoning_item_added("reason-after-final", &["done"])),
        gated_chunk(
            gate_completed_rx,
            vec![
                ev_reasoning_item("reason-after-final", &["done"], &[]),
                ev_completed("resp-1"),
            ],
        ),
    ];
    let (server, _completions) =
        start_streaming_sse_server(vec![first_chunks, response_completed_chunks("resp-2")]).await;
    let codex = build_codex(&server).await;

    submit_user_input(&codex, INITIAL_PROMPT).await;
    wait_for_reasoning_item_started(&codex).await;

    assert!(
        codex
            .inject_if_running(vec![responses::user_message_item(INJECTED_CONTEXT)])
            .await
            .is_ok()
    );
    let _ = gate_completed_tx.send(());

    wait_for_turn_complete(&codex).await;

    let requests = server.requests().await;
    assert_eq!(requests.len(), 2);
    let second: Value = from_slice(&requests[1]).expect("parse second request");
    let relevant_user_input = message_input_texts(&second, "user")
        .into_iter()
        .filter(|text| text == INITIAL_PROMPT || text == INJECTED_CONTEXT)
        .collect::<Vec<_>>();
    assert_eq!(relevant_user_input, vec![INITIAL_PROMPT, INJECTED_CONTEXT]);

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_input_does_not_preempt_after_reasoning_item() {
    let (gate_reasoning_done_tx, gate_reasoning_done_rx) = oneshot::channel();

    let first_chunks = vec![
        chunk(ev_response_created("resp-1")),
        chunk(ev_reasoning_item_added("reason-1", &["thinking"])),
        gated_chunk(
            gate_reasoning_done_rx,
            vec![
                ev_reasoning_item("reason-1", &["thinking"], &[]),
                ev_function_call(
                    "call-preserved",
                    "shell",
                    r#"{"command":"echo preserved tool call"}"#,
                ),
                ev_message_item_added("msg-1", ""),
                ev_output_text_delta("first answer"),
                ev_message_item_done("msg-1", "first answer"),
                ev_completed("resp-1"),
            ],
        ),
    ];

    let (server, _completions) =
        start_streaming_sse_server(vec![first_chunks, response_completed_chunks("resp-2")]).await;

    let codex = build_codex(&server).await;

    submit_user_input(&codex, "first prompt").await;

    wait_for_reasoning_item_started(&codex).await;

    steer_user_input(&codex, "second prompt").await;

    let _ = gate_reasoning_done_tx.send(());

    wait_for_agent_message(&codex, "first answer").await;

    wait_for_turn_complete(&codex).await;

    let requests = server.requests().await;
    assert_two_responses_input_snapshot(
        "pending_input_user_input_no_preempt_after_reasoning",
        &requests,
    );

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steered_user_input_waits_for_model_continuation_after_mid_turn_compact() {
    let first_chunks = vec![
        chunk(ev_response_created("resp-1")),
        chunk(ev_function_call("call-1", "test_tool", "{}")),
        chunk(ev_completed_with_tokens(
            "resp-1", /*total_tokens*/ 500,
        )),
    ];

    let compact_chunks = vec![
        chunk(ev_response_created("resp-compact")),
        chunk(ev_message_item_done("msg-compact", "AUTO_COMPACT_SUMMARY")),
        chunk(ev_completed_with_tokens(
            "resp-compact",
            /*total_tokens*/ 50,
        )),
    ];

    let post_compact_continuation_chunks = vec![
        chunk(ev_response_created("resp-post-compact")),
        chunk(ev_message_item_added("msg-post-compact", "")),
        chunk(ev_output_text_delta("resumed old task")),
        chunk(ev_message_item_done("msg-post-compact", "resumed old task")),
        chunk(ev_completed_with_tokens(
            "resp-post-compact",
            /*total_tokens*/ 60,
        )),
    ];

    let steered_follow_up_chunks = vec![
        chunk(ev_response_created("resp-steered")),
        chunk(ev_message_item_done(
            "msg-steered",
            "processed steered prompt",
        )),
        chunk(ev_completed_with_tokens(
            "resp-steered",
            /*total_tokens*/ 70,
        )),
    ];

    let (server, _completions) = start_streaming_sse_server(vec![
        first_chunks,
        compact_chunks,
        post_compact_continuation_chunks,
        steered_follow_up_chunks,
    ])
    .await;

    let codex = test_codex()
        .with_model("gpt-5.4")
        .with_config(|config| {
            config.model_provider.name = "OpenAI (test)".to_string();
            config.model_provider.supports_websockets = false;
            config.model_auto_compact_token_limit = Some(200);
        })
        .build_with_streaming_server(&server)
        .await
        .expect("build streaming Codex test session")
        .codex;

    submit_user_input(&codex, "first prompt").await;
    submit_user_input(&codex, "second prompt").await;

    wait_for_agent_message(&codex, "resumed old task").await;
    wait_for_turn_complete(&codex).await;

    let requests = server.requests().await;
    assert_eq!(requests.len(), 4);

    let post_compact_body: Value = from_slice(&requests[2]).expect("parse post-compact request");
    let steered_body: Value = from_slice(&requests[3]).expect("parse steered request");

    let post_compact_user_texts = message_input_texts(&post_compact_body, "user");
    assert!(
        !post_compact_user_texts
            .iter()
            .any(|text| text == "second prompt"),
        "steered input should stay pending until the model resumes after compaction"
    );

    let steered_user_texts = message_input_texts(&steered_body, "user");
    assert!(
        steered_user_texts
            .iter()
            .any(|text| text == "second prompt"),
        "steered input should be recorded on the request after the post-compact continuation"
    );

    server.shutdown().await;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CompactionFailurePoint {
    PreTurn,
    MidTurn,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingInputAfterFailure {
    Steer,
    QueuedMail,
    TriggeringMail,
}

#[test_case(CompactionFailurePoint::PreTurn, PendingInputAfterFailure::Steer; "pre_turn_steer")]
#[test_case(CompactionFailurePoint::PreTurn, PendingInputAfterFailure::QueuedMail; "pre_turn_mail")]
#[test_case(CompactionFailurePoint::PreTurn, PendingInputAfterFailure::TriggeringMail; "pre_turn_triggering_mail")]
#[test_case(CompactionFailurePoint::MidTurn, PendingInputAfterFailure::Steer; "mid_turn_steer")]
#[test_case(CompactionFailurePoint::MidTurn, PendingInputAfterFailure::QueuedMail; "mid_turn_mail")]
#[test_case(CompactionFailurePoint::MidTurn, PendingInputAfterFailure::TriggeringMail; "mid_turn_triggering_mail")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_compaction_error_does_not_retry_pending_input(
    failure_point: CompactionFailurePoint,
    pending_input: PendingInputAfterFailure,
) -> anyhow::Result<()> {
    const PENDING_MESSAGE: &str = "pending input must survive the failed compaction";
    let (release_failure, failure_gate) = oneshot::channel();
    let initial_output = match failure_point {
        CompactionFailurePoint::PreTurn => ev_message_item_done("initial", "first answer"),
        CompactionFailurePoint::MidTurn => ev_function_call("call-1", "test_tool", "{}"),
    };
    let failure = responses::sse_failed("failed-compact", "insufficient_quota", "quota exhausted");
    let mut streams = vec![
        vec![
            chunk(ev_response_created("initial")),
            chunk(initial_output),
            chunk(ev_completed_with_tokens(
                "initial", /*total_tokens*/ 500_000,
            )),
        ],
        vec![StreamingSseChunk {
            gate: Some(failure_gate),
            body: failure.clone(),
        }],
    ];
    // Mail arriving during a failed turn may start one fresh turn. That turn must also
    // stop on the terminal error, persist its mail, and not start another turn for it.
    let failed_turns = if pending_input == PendingInputAfterFailure::TriggeringMail {
        streams.push(vec![StreamingSseChunk {
            gate: None,
            body: failure,
        }]);
        2
    } else {
        1
    };
    streams.extend([
        vec![
            chunk(json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "compaction",
                    "encrypted_content": "RECOVERED_COMPACTION",
                }
            })),
            chunk(ev_completed_with_tokens(
                "recovered-compact",
                /*total_tokens*/ 50,
            )),
        ],
        vec![
            chunk(ev_message_item_done("recovered", "recovered answer")),
            chunk(ev_completed_with_tokens(
                "recovered",
                /*total_tokens*/ 60,
            )),
        ],
    ]);
    let (server, _completions) = start_streaming_sse_server(streams).await;
    let config_server = responses::start_mock_server().await;
    let base_url = format!("{}/v1", server.uri());
    let test = test_codex()
        .with_model("gpt-5.4")
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_auto_compact_token_limit = Some(100_000);
            let _ = config.features.enable(Feature::RemoteCompactionV2);
            // The streaming fixture records raw request bodies for JSON assertions.
            let _ = config.features.disable(Feature::EnableRequestCompression);
        })
        .build_with_auto_env(&config_server)
        .await?;
    let codex = &test.codex;

    if failure_point == CompactionFailurePoint::PreTurn {
        submit_user_input(codex, "initial prompt").await;
        wait_for_turn_complete(codex).await;
    }
    if pending_input == PendingInputAfterFailure::QueuedMail {
        submit_queue_only_agent_mail(codex, PENDING_MESSAGE).await;
    }
    submit_user_input(codex, "prompt that needs compaction").await;
    tokio::time::timeout(
        std::time::Duration::from_secs(/*secs*/ 10),
        server.wait_for_request_count(/*count*/ 2),
    )
    .await?;
    match pending_input {
        PendingInputAfterFailure::Steer => steer_user_input(codex, PENDING_MESSAGE).await,
        PendingInputAfterFailure::QueuedMail => {}
        PendingInputAfterFailure::TriggeringMail => {
            codex
                .submit(Op::InterAgentCommunication {
                    communication: InterAgentCommunication::new(
                        AgentPath::root().join("worker").expect("valid worker path"),
                        AgentPath::root(),
                        Vec::new(),
                        PENDING_MESSAGE.to_string(),
                        /*trigger_turn*/ true,
                    ),
                })
                .await?;
            codex.submit(Op::RealtimeConversationListVoices).await?;
            wait_for_event(codex, |event| {
                matches!(event, EventMsg::RealtimeConversationListVoicesResponse(_))
            })
            .await;
        }
    }
    release_failure.send(()).expect("release compact failure");

    let mut errors = Vec::new();
    let mut completed_turns = 0;
    wait_for_event(codex, |event| {
        match event {
            EventMsg::Error(error) => {
                assert_eq!(
                    error.codex_error_info,
                    Some(CodexErrorInfo::UsageLimitExceeded)
                );
                errors.push(error.clone());
            }
            EventMsg::TurnComplete(completed) => {
                assert_eq!(completed.error.as_ref(), errors.last());
                assert_eq!(completed.last_agent_message, None);
                completed_turns += 1;
            }
            _ => {}
        }
        completed_turns == failed_turns
    })
    .await;
    assert_eq!(errors.len(), failed_turns);
    let requests = server.requests().await;
    assert_eq!(requests.len(), 1 + failed_turns);
    for request in &requests[1..] {
        let body: Value = from_slice(request)?;
        assert!(
            body["input"]
                .as_array()
                .expect("compact input")
                .iter()
                .any(|item| { item["type"] == "compaction_trigger" })
        );
    }

    codex.flush_rollout().await?;
    let history = codex.load_history(/*include_archived*/ false).await?;
    let saved_pending_messages = history
        .items
        .iter()
        .filter_map(|item| {
            let RolloutItem::ResponseItem(response_item) = item else {
                return None;
            };
            match response_item {
                ResponseItem::Message { role, content, .. } if role == "user" => {
                    content.iter().find_map(|item| match item {
                        ContentItem::InputText { text } if text == PENDING_MESSAGE => {
                            Some(text.as_str())
                        }
                        _ => None,
                    })
                }
                ResponseItem::AgentMessage { content, .. } => {
                    content.iter().find_map(|item| match item {
                        AgentMessageInputContent::InputText { text } if text == PENDING_MESSAGE => {
                            Some(text.as_str())
                        }
                        _ => None,
                    })
                }
                _ => None,
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(saved_pending_messages, vec![PENDING_MESSAGE]);

    // The failed turn must not poison a later explicit retry once compaction can succeed.
    submit_user_input(codex, "retry after quota resets").await;
    wait_for_agent_message(codex, "recovered answer").await;
    let completed = wait_for_event(codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    let EventMsg::TurnComplete(completed) = completed else {
        unreachable!("expected turn completion");
    };
    assert_eq!(completed.error, None);
    assert_eq!(server.requests().await.len(), 3 + failed_turns);
    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steered_user_input_follows_compact_when_only_the_steer_needs_follow_up() {
    let (gate_first_completed_tx, gate_first_completed_rx) = oneshot::channel();

    let first_chunks = vec![
        chunk(ev_response_created("resp-1")),
        chunk(ev_message_item_added("msg-1", "")),
        chunk(ev_output_text_delta("first answer")),
        chunk(ev_message_item_done("msg-1", "first answer")),
        gated_chunk(
            gate_first_completed_rx,
            vec![ev_completed_with_tokens(
                "resp-1", /*total_tokens*/ 500,
            )],
        ),
    ];

    let compact_chunks = vec![
        chunk(ev_response_created("resp-compact")),
        chunk(ev_message_item_done("msg-compact", "AUTO_COMPACT_SUMMARY")),
        chunk(ev_completed_with_tokens(
            "resp-compact",
            /*total_tokens*/ 50,
        )),
    ];

    let steered_follow_up_chunks = vec![
        chunk(ev_response_created("resp-steered")),
        chunk(ev_message_item_done(
            "msg-steered",
            "processed steered prompt",
        )),
        chunk(ev_completed_with_tokens(
            "resp-steered",
            /*total_tokens*/ 70,
        )),
    ];

    let outer_reentry_chunks = vec![
        chunk(ev_response_created("resp-outer-reentry")),
        chunk(ev_message_item_done("msg-outer-reentry", "outer reentry")),
        chunk(ev_completed_with_tokens(
            "resp-outer-reentry",
            /*total_tokens*/ 30,
        )),
    ];

    let (server, _completions) = start_streaming_sse_server(vec![
        first_chunks,
        compact_chunks,
        steered_follow_up_chunks,
        outer_reentry_chunks,
    ])
    .await;

    let test = test_codex()
        .with_model("gpt-5.4")
        .with_pre_build_hook(write_blocking_stop_hook)
        .with_config(|config| {
            config.model_provider.name = "OpenAI (test)".to_string();
            config.model_provider.supports_websockets = false;
            config.model_auto_compact_token_limit = Some(200);
        })
        .with_config(trust_discovered_hooks)
        .build_with_streaming_server(&server)
        .await
        .expect("build streaming Codex test session");
    let codex = test.codex.clone();

    submit_user_input(&codex, "first prompt").await;
    wait_for_agent_message(&codex, "first answer").await;
    steer_user_input(&codex, "second prompt").await;
    let _ = gate_first_completed_tx.send(());

    wait_for_agent_message(&codex, "processed steered prompt").await;
    wait_for_event(&codex, |event| {
        matches!(
            event,
            EventMsg::HookStarted(started) if started.run.event_name == HookEventName::Stop
        )
    })
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !test.codex_home_path().join("started_stop_hook").exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("stop hook command did not start");
    steer_user_input(&codex, "third prompt").await;
    fs::write(test.codex_home_path().join("release_stop_hook"), b"release")
        .expect("release blocking stop hook");

    let mut stop_hook_starts = 1;
    let turn_complete = loop {
        match wait_for_event(&codex, |_| true).await {
            EventMsg::HookStarted(started) if started.run.event_name == HookEventName::Stop => {
                stop_hook_starts += 1;
            }
            EventMsg::TurnComplete(event) => break event,
            _ => {}
        }
    };
    assert_eq!(stop_hook_starts, 2, "steer should enter run_turn twice");
    assert_eq!(
        turn_complete.provider_usage,
        Some(TokenUsage {
            input_tokens: 650,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 0,
            reasoning_output_tokens: 0,
            total_tokens: 650,
        })
    );

    let requests = server.requests().await;
    assert_eq!(requests.len(), 4);

    let compact_body: Value = from_slice(&requests[1]).expect("parse compact request");
    let steered_body: Value = from_slice(&requests[2]).expect("parse steered request");
    let outer_reentry_body: Value = from_slice(&requests[3]).expect("parse outer reentry request");

    let compact_user_texts = message_input_texts(&compact_body, "user");
    assert!(
        !compact_user_texts
            .iter()
            .any(|text| text == "second prompt"),
        "steered input should not be included in the compaction request"
    );

    let steered_user_texts = message_input_texts(&steered_body, "user");
    assert!(
        steered_user_texts
            .iter()
            .any(|text| text == "second prompt"),
        "steered input should follow compaction without an empty resume request when the model was already done"
    );

    let outer_reentry_user_texts = message_input_texts(&outer_reentry_body, "user");
    assert!(
        outer_reentry_user_texts
            .iter()
            .any(|text| text == "third prompt"),
        "steer queued behind the stop hook should run after outer task-loop re-entry"
    );

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steered_user_input_waits_when_tool_output_triggers_compact_before_next_request() {
    let (gate_first_completed_tx, gate_first_completed_rx) = oneshot::channel();

    let large_output_command = if cfg!(windows) {
        "[Console]::Out.Write([string]::new([char]'0', 4000))"
    } else {
        "printf '%04000d' 0"
    };
    let large_output_args = json!({
        "command": large_output_command,
        "login": false,
        "timeout_ms": 2000,
    })
    .to_string();

    let first_chunks = vec![
        chunk(ev_response_created("resp-1")),
        chunk(ev_function_call(
            "call-1",
            "shell_command",
            &large_output_args,
        )),
        gated_chunk(
            gate_first_completed_rx,
            vec![ev_completed_with_tokens(
                "resp-1", /*total_tokens*/ 100,
            )],
        ),
    ];

    let compact_chunks = vec![
        chunk(ev_response_created("resp-compact")),
        chunk(ev_message_item_done("msg-compact", "TOOL_OUTPUT_SUMMARY")),
        chunk(ev_completed_with_tokens(
            "resp-compact",
            /*total_tokens*/ 50,
        )),
    ];

    let post_compact_continuation_chunks = vec![
        chunk(ev_response_created("resp-post-compact")),
        chunk(ev_message_item_done(
            "msg-post-compact",
            "resumed after compacting tool output",
        )),
        chunk(ev_completed_with_tokens(
            "resp-post-compact",
            /*total_tokens*/ 60,
        )),
    ];

    let steered_follow_up_chunks = vec![
        chunk(ev_response_created("resp-steered")),
        chunk(ev_message_item_done(
            "msg-steered",
            "processed steered prompt",
        )),
        chunk(ev_completed_with_tokens(
            "resp-steered",
            /*total_tokens*/ 70,
        )),
    ];

    let (server, _completions) = start_streaming_sse_server(vec![
        first_chunks,
        compact_chunks,
        post_compact_continuation_chunks,
        steered_follow_up_chunks,
    ])
    .await;

    let test = test_codex()
        .with_model("gpt-5.4")
        .with_config(|config| {
            config.model_provider.name = "OpenAI (test)".to_string();
            config.model_provider.supports_websockets = false;
            config.model_auto_compact_token_limit = Some(200);
        })
        .build_with_streaming_server(&server)
        .await
        .expect("build streaming Codex test session");
    let codex = test.codex.clone();

    submit_danger_full_access_user_turn(&test, "first prompt").await;
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnStarted(_))).await;
    steer_user_input(&codex, "second prompt").await;
    let _ = gate_first_completed_tx.send(());

    wait_for_turn_complete(&codex).await;

    let requests = server.requests().await;
    assert_eq!(requests.len(), 4);

    let compact_body: Value = from_slice(&requests[1]).expect("parse compact request");
    let post_compact_body: Value = from_slice(&requests[2]).expect("parse post-compact request");
    let steered_body: Value = from_slice(&requests[3]).expect("parse steered request");

    let compact_user_texts = message_input_texts(&compact_body, "user");
    assert!(
        !compact_user_texts
            .iter()
            .any(|text| text == "second prompt"),
        "steered input should not be included in the compaction request"
    );

    let post_compact_user_texts = message_input_texts(&post_compact_body, "user");
    assert!(
        !post_compact_user_texts
            .iter()
            .any(|text| text == "second prompt"),
        "steered input should stay pending until after the compacted continuation"
    );

    let steered_user_texts = message_input_texts(&steered_body, "user");
    assert!(
        steered_user_texts
            .iter()
            .any(|text| text == "second prompt"),
        "steered input should be recorded on the request after the post-compact continuation"
    );

    server.shutdown().await;
}
