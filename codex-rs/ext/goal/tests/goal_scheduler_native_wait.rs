//! Real goal scheduling through a counted mock provider and the V2 native wait.
//! The older core notification fixture intentionally injects a marker; these
//! witnesses install the goal extension into the actual thread manager.

#![recursion_limit = "256"]
#![allow(clippy::expect_used)]

use anyhow::Result;
use codex_analytics::AnalyticsEventsClient;
use codex_core::StateDbHandle;
use codex_core::TurnInputRequest;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::TurnLifecycleContributor;
use codex_extension_api::TurnStartInput;
use codex_features::Feature;
use codex_goal_extension::GoalExtensionConfig;
use codex_goal_extension::GoalService;
use codex_goal_extension::install_with_backend;
use codex_protocol::ThreadId;
use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use codex_state::ThreadGoalStatus;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::sse_failed;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::TestCodexBuilder;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event_with_timeout;
use serde_json::Value;
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const GOAL_OBJECTIVE: &str = "real-goal-scheduler-wait-fixture";
const CHILD_INITIAL: &str = "goal-worker-initial";
const PARENT_WAIT: &str = "wait for goal worker";
const SPAWN_CALL: &str = "goal-spawn-call";
const WAIT_CALL: &str = "goal-wait-call";

fn body_contains(request: &Request, needle: &str) -> bool {
    String::from_utf8_lossy(&decoded_body(request)).contains(needle)
}

fn decoded_body(request: &Request) -> Vec<u8> {
    let compressed = request
        .headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|part| part.trim() == "zstd"));
    if compressed {
        zstd::stream::decode_all(std::io::Cursor::new(&request.body))
            .expect("decode mock provider request")
    } else {
        request.body.clone()
    }
}

fn body_json(request: &Request) -> Value {
    serde_json::from_slice(&decoded_body(request)).expect("mock provider request JSON")
}

fn has_agent_message(request: &Request) -> bool {
    body_json(request)
        .get("input")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some("agent_message"))
        })
}

fn request_thread_id(request: &Request) -> Option<String> {
    body_json(request)["client_metadata"]["thread_id"]
        .as_str()
        .map(str::to_string)
}

fn turn_metadata(request: &Request) -> Option<Value> {
    let body = body_json(request);
    let raw = body["client_metadata"]["x-codex-turn-metadata"].as_str()?;
    serde_json::from_str(raw).ok()
}

fn is_goal_turn(request: &Request) -> bool {
    turn_metadata(request)
        .as_ref()
        .and_then(|value| value["turn_trigger"].as_str())
        == Some("goal")
}

struct GoalStartGate {
    state_db: Mutex<Option<StateDbHandle>>,
    entered: AtomicUsize,
    permits: Semaphore,
}

impl GoalStartGate {
    fn new() -> Self {
        Self {
            state_db: Mutex::new(None),
            entered: AtomicUsize::new(0),
            permits: Semaphore::new(0),
        }
    }

    fn release_one(&self) {
        self.permits.add_permits(1);
    }
}

impl TurnLifecycleContributor for GoalStartGate {
    fn on_turn_start<'a>(&'a self, input: TurnStartInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let Some(state_db) = self.state_db.lock().expect("gate state").clone() else {
                return;
            };
            let Ok(thread_id) = ThreadId::from_string(input.thread_store.level_id()) else {
                return;
            };
            let active = state_db
                .thread_goals()
                .get_thread_goal(thread_id)
                .await
                .expect("read persisted goal at turn start")
                .is_some_and(|goal| goal.status == ThreadGoalStatus::Active);
            if active {
                self.entered.fetch_add(1, Ordering::AcqRel);
                self.permits.acquire().await.expect("gate open").forget();
            }
        })
    }
}

fn builder_with_goal(
    state_slot: Arc<Mutex<Option<StateDbHandle>>>,
    gate: Arc<GoalStartGate>,
) -> TestCodexBuilder {
    test_codex()
        .with_model("koffing")
        .with_extension_builder(move |registry, state_db, manager| {
            *state_slot.lock().expect("state slot") = state_db.clone();
            *gate.state_db.lock().expect("gate state") = state_db.clone();
            install_with_backend(
                registry,
                state_db.expect("persistent state for goal fixture"),
                AnalyticsEventsClient::disabled(),
                None,
                manager,
                Arc::new(GoalService::new()),
                |_| GoalExtensionConfig {
                    enabled: true,
                    max_goal_token_budget: None,
                },
            );
            // Observe after the real goal contributor binds this turn.
            registry.turn_lifecycle_contributor(gate);
        })
        .with_config(|config| {
            config.features.enable(Feature::Collab).expect("collab");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("multi-agent v2");
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.supports_websockets = false;
        })
}

struct GoalReplies {
    index: AtomicUsize,
    outcome: FinalOutcome,
}

#[derive(Clone, Copy)]
enum FinalOutcome {
    Complete,
    Error,
    Abort,
    OperatorSteer,
    StaleGeneration,
}

impl Respond for GoalReplies {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let index = self.index.fetch_add(1, Ordering::AcqRel);
        // These requests are reached only after the turn-start contributor has
        // released the child into a registered task. Keep the provider pending
        // so interrupt exercises that real task, not an uninterruptible test hook.
        if (matches!(self.outcome, FinalOutcome::OperatorSteer) && index == 0)
            || (matches!(self.outcome, FinalOutcome::Abort) && index == 3)
        {
            return sse_response(sse(vec![
                ev_response_created("resp-goal-cancellable"),
                ev_assistant_message("msg-goal-cancellable", "unused after interrupt"),
                ev_completed("resp-goal-cancellable"),
            ]))
            .set_delay(Duration::from_secs(60));
        }
        if matches!(self.outcome, FinalOutcome::Error) && index == 3 {
            return sse_response(sse_failed(
                "resp-goal-final-error",
                "server_error",
                "fixture provider failure",
            ));
        }
        let events = match index {
            0 => vec![
                ev_response_created("resp-goal-continuation-1"),
                ev_assistant_message("msg-goal-continuation-1", "intermediate one"),
                ev_completed("resp-goal-continuation-1"),
            ],
            1 => vec![
                ev_response_created("resp-goal-continuation-2-call"),
                ev_function_call_with_namespace(
                    "goal-queue-only-call",
                    "collaboration",
                    "send_message",
                    r#"{"target":"/root","message":"queue-only goal progress"}"#,
                ),
                ev_completed("resp-goal-continuation-2-call"),
            ],
            2 => vec![
                ev_response_created("resp-goal-continuation-2-done"),
                ev_assistant_message("msg-goal-continuation-2", "intermediate two"),
                ev_completed("resp-goal-continuation-2-done"),
            ],
            3 if matches!(self.outcome, FinalOutcome::Complete) => vec![
                ev_response_created("resp-goal-final-call"),
                ev_function_call(
                    "goal-complete-call",
                    "update_goal",
                    r#"{"status":"complete"}"#,
                ),
                ev_completed("resp-goal-final-call"),
            ],
            4 if matches!(self.outcome, FinalOutcome::Complete) => vec![
                ev_response_created("resp-goal-final-done"),
                ev_assistant_message("msg-goal-final", "goal complete"),
                ev_completed("resp-goal-final-done"),
            ],
            _ => panic!("unexpected extra automatic goal provider request {index}"),
        };
        sse_response(sse(events))
    }
}

async fn mount_provider(server: &MockServer, resumed: bool, outcome: FinalOutcome) -> Result<()> {
    let spawn_args = serde_json::to_string(&json!({
        "message": CHILD_INITIAL,
        "task_name": "worker",
        "fork_turns": "none",
    }))?;
    mount_sse_once_match(
        server,
        |request: &Request| body_contains(request, "spawn goal worker"),
        sse(vec![
            ev_response_created("resp-goal-parent-spawn"),
            ev_function_call_with_namespace(
                SPAWN_CALL,
                "collaboration",
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-goal-parent-spawn"),
        ]),
    )
    .await;
    mount_sse_once_match(
        server,
        |request: &Request| body_contains(request, SPAWN_CALL) && !has_agent_message(request),
        sse(vec![
            ev_response_created("resp-goal-parent-spawn-done"),
            ev_assistant_message("msg-goal-parent-spawn-done", "worker started"),
            ev_completed("resp-goal-parent-spawn-done"),
        ]),
    )
    .await;
    if resumed {
        mount_sse_once_match(
            server,
            |request: &Request| body_contains(request, CHILD_INITIAL) && has_agent_message(request),
            sse(vec![
                ev_response_created("resp-goal-child-initial-resume"),
                ev_assistant_message("msg-goal-child-initial-resume", "ready for restart"),
                ev_completed("resp-goal-child-initial-resume"),
            ]),
        )
        .await;
    } else {
        mount_sse_once_match(
            server,
            |request: &Request| {
                body_contains(request, CHILD_INITIAL)
                    && has_agent_message(request)
                    && !body_contains(request, "goal-create-call")
            },
            sse(vec![
                ev_response_created("resp-goal-create-call"),
                ev_function_call(
                    "goal-create-call",
                    "create_goal",
                    &serde_json::to_string(&json!({"objective": GOAL_OBJECTIVE}))?,
                ),
                ev_completed("resp-goal-create-call"),
            ]),
        )
        .await;
        mount_sse_once_match(
            server,
            |request: &Request| {
                body_contains(request, "goal-create-call") && has_agent_message(request)
            },
            sse(vec![
                ev_response_created("resp-goal-created"),
                ev_assistant_message("msg-goal-created", "goal established"),
                ev_completed("resp-goal-created"),
            ]),
        )
        .await;
    }
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(|request: &Request| is_goal_turn(request))
        .respond_with(GoalReplies {
            index: AtomicUsize::new(0),
            outcome,
        })
        .expect(match outcome {
            FinalOutcome::Complete => 5,
            FinalOutcome::Error => 4,
            FinalOutcome::Abort => 4,
            FinalOutcome::OperatorSteer => 1,
            FinalOutcome::StaleGeneration => 3,
        })
        .mount(server)
        .await;
    mount_sse_once_match(
        server,
        |request: &Request| body_contains(request, PARENT_WAIT),
        sse(vec![
            ev_response_created("resp-goal-parent-wait"),
            ev_function_call_with_namespace(
                WAIT_CALL,
                "collaboration",
                "wait_agent",
                r#"{"targets":["/root/worker"],"return_when":"any","timeout_ms":15000}"#,
            ),
            ev_completed("resp-goal-parent-wait"),
        ]),
    )
    .await;
    mount_sse_once_match(
        server,
        |request: &Request| body_contains(request, WAIT_CALL) && !has_agent_message(request),
        sse(vec![
            ev_response_created("resp-goal-parent-wait-done"),
            ev_assistant_message("msg-goal-parent-wait-done", "goal worker finished"),
            ev_completed("resp-goal-parent-wait-done"),
        ]),
    )
    .await;
    Ok(())
}

async fn parent_request_count(server: &MockServer, parent_id: ThreadId) -> usize {
    server
        .received_requests()
        .await
        .expect("mock requests")
        .iter()
        .filter(|request| request_thread_id(request) == Some(parent_id.to_string()))
        .count()
}

async fn wait_for_goal_request_count(
    server: &MockServer,
    child_id: ThreadId,
    expected: usize,
) -> Result<()> {
    timeout(Duration::from_secs(10), async {
        loop {
            let requests = server.received_requests().await.expect("mock requests");
            if requests
                .iter()
                .filter(|request| {
                    request_thread_id(request) == Some(child_id.to_string())
                        && is_goal_turn(request)
                })
                .count()
                >= expected
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok(())
}

async fn wait_for_child_complete(child: &codex_core::CodexThread) {
    wait_for_event_with_timeout(
        child,
        |event| matches!(event, EventMsg::TurnComplete(_)),
        Duration::from_secs(10),
    )
    .await;
}

async fn wait_for_goal_start_count(gate: &GoalStartGate, expected: usize) -> Result<()> {
    timeout(Duration::from_secs(5), async {
        while gate.entered.load(Ordering::Acquire) < expected {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok(())
}

async fn wait_for_parent_wait_output(
    server: &MockServer,
    parent_id: ThreadId,
    baseline: usize,
) -> Result<String> {
    timeout(Duration::from_secs(10), async {
        while parent_request_count(server, parent_id).await <= baseline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let requests = server.received_requests().await.expect("mock requests");
    let resumed_parent = requests
        .iter()
        .find(|request| {
            request_thread_id(request) == Some(parent_id.to_string())
                && body_contains(request, WAIT_CALL)
        })
        .expect("native wait output reached parent provider");
    let body = body_json(resumed_parent);
    let output = body["input"]
        .as_array()
        .expect("parent request input")
        .iter()
        .find(|item| {
            item["type"] == json!("function_call_output") && item["call_id"] == json!(WAIT_CALL)
        })
        .and_then(|item| item["output"].as_str())
        .expect("native wait output text");
    Ok(output.to_string())
}

async fn run_goal_scheduler_wait(resumed: bool, outcome: FinalOutcome) -> Result<()> {
    let server = start_mock_server().await;
    mount_provider(&server, resumed, outcome).await?;
    let state_slot = Arc::new(Mutex::new(None));
    let gate = Arc::new(GoalStartGate::new());
    let test = builder_with_goal(Arc::clone(&state_slot), Arc::clone(&gate))
        .build(&server)
        .await?;
    let parent_id = test.session_configured.thread_id;
    let parent_key = parent_id.to_string();
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(move |request: &Request| request_thread_id(request) == Some(parent_key.clone()))
        .respond_with(sse_response(sse(vec![
            ev_response_created("resp-goal-parent-unbound-fallback"),
            ev_assistant_message("msg-goal-parent-unbound-fallback", "continue waiting"),
            ev_completed("resp-goal-parent-unbound-fallback"),
        ])))
        .with_priority(10)
        .mount(&server)
        .await;
    let mut created_threads = test.thread_manager.subscribe_thread_created();
    test.submit_turn("spawn goal worker").await?;
    let child_id = timeout(Duration::from_secs(5), created_threads.recv()).await??;
    let child = test.thread_manager.get_thread(child_id).await?;
    wait_for_child_complete(child.as_ref()).await;
    if resumed {
        // No goal exists in the first child turn of the resume case. Its
        // queue-only handback is intentionally silent; only wait for the
        // initial parent turn to become idle, not a third provider request.
        timeout(Duration::from_secs(5), async {
            while test.codex.agent_status().await == AgentStatus::Running {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        assert_eq!(parent_request_count(&server, parent_id).await, 2);
    } else {
        assert_eq!(
            parent_request_count(&server, parent_id).await,
            2,
            "create_goal made its own turn quiet before automatic continuation",
        );
    }

    let active = if resumed {
        let state_db = state_slot
            .lock()
            .expect("state slot")
            .clone()
            .expect("state db");
        let rollout_path = test
            .session_configured
            .rollout_path
            .clone()
            .expect("root rollout path");
        test.thread_manager
            .shutdown_all_threads_bounded(Duration::from_secs(5))
            .await;
        state_db
            .thread_goals()
            .insert_thread_goal(child_id, GOAL_OBJECTIVE, ThreadGoalStatus::Active, None)
            .await?
            .expect("resumed child goal inserted");
        let mut builder = builder_with_goal(Arc::clone(&state_slot), Arc::clone(&gate));
        let resumed_test = builder
            .resume(&server, Arc::clone(&test.home), rollout_path)
            .await?;
        assert_eq!(resumed_test.session_configured.thread_id, parent_id);
        resumed_test
            .thread_manager
            .ensure_multi_agent_v2_child_loaded(child_id)
            .await?;
        resumed_test
    } else {
        let state_db = state_slot
            .lock()
            .expect("state slot")
            .clone()
            .expect("state db");
        let goal = state_db
            .thread_goals()
            .get_thread_goal(child_id)
            .await?
            .expect("create_goal persisted child goal");
        assert_eq!(goal.status, ThreadGoalStatus::Active);
        test
    };
    let child = active.thread_manager.get_thread(child_id).await?;
    wait_for_goal_start_count(gate.as_ref(), 1).await?;
    timeout(Duration::from_secs(5), async {
        while active.codex.agent_status().await == AgentStatus::Running {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    active
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: PARENT_WAIT.to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    timeout(Duration::from_secs(10), async {
        loop {
            let event = active.codex.next_event().await.expect("parent events");
            if let EventMsg::ItemStarted(item) = event.msg
                && let TurnItem::CollabAgentToolCall(call) = item.item
                && call.tool == CollabAgentTool::Wait
                && call.status == CollabAgentToolCallStatus::InProgress
            {
                break;
            }
        }
    })
    .await?;
    let baseline = parent_request_count(&server, parent_id).await;
    assert!(
        baseline > 0,
        "real parent provider boundary was not observed"
    );

    if matches!(outcome, FinalOutcome::OperatorSteer) {
        active
            .codex
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: "operator changes the request".to_string(),
                text_elements: Vec::new(),
            }]))
            .await?;
        let output = wait_for_parent_wait_output(&server, parent_id, baseline).await?;
        let result: Value = serde_json::from_str(&output)?;
        assert_eq!(result["reason"], json!("steered"));
        assert_eq!(result["wake_cause"], json!("operator_steer"));
        assert_eq!(result["queued_update_count"], json!(0));
        assert_eq!(gate.entered.load(Ordering::Acquire), 1);
        gate.release_one();
        wait_for_goal_request_count(&server, child_id, 1).await?;
        child.submit(Op::Interrupt).await?;
        wait_for_event_with_timeout(
            child.as_ref(),
            |event| matches!(event, EventMsg::TurnAborted(_)),
            Duration::from_secs(10),
        )
        .await;
        return Ok(());
    }

    for intermediate in 1..=2 {
        if matches!(outcome, FinalOutcome::StaleGeneration) && intermediate == 2 {
            let state_db = state_slot
                .lock()
                .expect("state slot")
                .clone()
                .expect("state db");
            let prior = state_db
                .thread_goals()
                .get_thread_goal(child_id)
                .await?
                .expect("active prior generation");
            let replacement = state_db
                .thread_goals()
                .replace_thread_goal(
                    child_id,
                    "replacement goal generation",
                    ThreadGoalStatus::Active,
                    None,
                )
                .await?;
            assert_ne!(prior.goal_id, replacement.goal_id);
        }
        gate.release_one();
        wait_for_child_complete(child.as_ref()).await;
        if matches!(outcome, FinalOutcome::StaleGeneration) && intermediate == 2 {
            let output = wait_for_parent_wait_output(&server, parent_id, baseline).await?;
            let result: Value = serde_json::from_str(&output)?;
            assert_eq!(result["reason"], json!("target_terminal"));
            assert_eq!(result["wake_cause"], json!("target_status"));
            child.submit(Op::Interrupt).await?;
            return Ok(());
        }
        wait_for_goal_start_count(gate.as_ref(), intermediate + 1).await?;
        let requests = server.received_requests().await.expect("mock requests");
        assert!(
            !requests.iter().any(|request| {
                request_thread_id(request) == Some(parent_id.to_string())
                    && body_contains(request, WAIT_CALL)
            }),
            "intermediate {intermediate} released the pending native wait"
        );
        assert_eq!(
            parent_request_count(&server, parent_id).await,
            baseline,
            "intermediate {intermediate} caused an extra parent provider request"
        );
    }
    if matches!(outcome, FinalOutcome::Abort) {
        gate.release_one();
        wait_for_goal_request_count(&server, child_id, 4).await?;
        child.submit(Op::Interrupt).await?;
        wait_for_event_with_timeout(
            child.as_ref(),
            |event| matches!(event, EventMsg::TurnAborted(_)),
            Duration::from_secs(10),
        )
        .await;
    } else {
        gate.release_one();
        wait_for_child_complete(child.as_ref()).await;
    }
    let output = wait_for_parent_wait_output(&server, parent_id, baseline).await?;
    let result: Value = serde_json::from_str(&output)?;
    assert_eq!(result["reason"], json!("target_terminal"));
    assert_eq!(result["wake_cause"], json!("target_status"));
    assert_eq!(result["queued_update_count"], json!(1));
    if matches!(outcome, FinalOutcome::Abort) {
        assert!(output.contains("interrupted"));
        return Ok(());
    }
    if matches!(outcome, FinalOutcome::Error) {
        assert!(output.contains("errored"));
        let state_db = state_slot
            .lock()
            .expect("state slot")
            .clone()
            .expect("state db");
        let goal = state_db
            .thread_goals()
            .get_thread_goal(child_id)
            .await?
            .expect("persisted errored goal");
        assert_eq!(goal.status, ThreadGoalStatus::Blocked);
        return Ok(());
    }
    let requests = server.received_requests().await.expect("mock requests");
    let goal_turns: HashSet<String> = requests
        .iter()
        .filter(|request| request_thread_id(request) == Some(child_id.to_string()))
        .filter(|request| is_goal_turn(request))
        .filter_map(|request| {
            body_json(request)["client_metadata"]["turn_id"]
                .as_str()
                .map(str::to_string)
        })
        .collect();
    assert_eq!(goal_turns.len(), 3, "three real automatic goal turns");
    let state_db = state_slot
        .lock()
        .expect("state slot")
        .clone()
        .expect("state db");
    let goal = state_db
        .thread_goals()
        .get_thread_goal(child_id)
        .await?
        .expect("persisted final goal");
    assert_eq!(goal.status, ThreadGoalStatus::Complete);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_goal_scheduler_keeps_native_wait_quiet_until_complete() -> Result<()> {
    run_goal_scheduler_wait(false, FinalOutcome::Complete).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_goal_scheduler_keeps_native_wait_quiet_until_complete() -> Result<()> {
    run_goal_scheduler_wait(true, FinalOutcome::Complete).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goal_scheduler_error_wakes_native_wait_with_raw_error() -> Result<()> {
    run_goal_scheduler_wait(false, FinalOutcome::Error).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goal_scheduler_abort_wakes_native_wait_with_raw_interrupt() -> Result<()> {
    run_goal_scheduler_wait(false, FinalOutcome::Abort).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operator_steer_wakes_goal_native_wait_without_goal_completion() -> Result<()> {
    run_goal_scheduler_wait(false, FinalOutcome::OperatorSteer).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_goal_generation_fails_open_to_native_wait() -> Result<()> {
    run_goal_scheduler_wait(false, FinalOutcome::StaleGeneration).await
}
