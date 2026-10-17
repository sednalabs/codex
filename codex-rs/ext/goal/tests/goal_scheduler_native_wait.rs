//! Real goal readiness and addressed updates join the same exact-target native wait.

#![recursion_limit = "256"]
#![allow(clippy::expect_used)]

use anyhow::Context;
use anyhow::Result;
use codex_analytics::AnalyticsEventsClient;
use codex_core::CodexThread;
use codex_core::GoalTurnMarker;
use codex_core::GoalTurnReadiness;
use codex_core::StateDbHandle;
use codex_core::TurnInputRequest;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::TurnLifecycleContributor;
use codex_extension_api::TurnStartInput;
use codex_extension_api::TurnStartPhase;
use codex_extension_api::TurnStopInput;
use codex_features::Feature;
use codex_goal_extension::GoalExtensionConfig;
use codex_goal_extension::GoalService;
use codex_goal_extension::install_with_backend;
use codex_protocol::ThreadId;
use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::CollabAgentToolCallItem;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_protocol::items::SubAgentActivityItem;
use codex_protocol::items::TurnItem;
use codex_protocol::items::WaitAgentOutcome;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::user_input::UserInput;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutRecorder;
use codex_state::ThreadGoalStatus;
use core_test_support::ThreadIdle;
use core_test_support::responses::ResponseMock;
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
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const CHILD_INITIAL: &str = "goal-worker-initial";
const WAIT_PROMPT: &str = "wait for goal worker";
const OPERATOR_PROMPT: &str = "operator changes the request";
const DRAIN_PROMPT: &str = "record queued goal handbacks";
const REPORT_PROMPT: &str = "report the final goal handback";
const SPAWN_CALL: &str = "goal-spawn-call";
const WAIT_CALL: &str = "goal-wait-call";
const PROGRESS_CALL: &str = "goal-progress-call";
const UPDATE_CALL: &str = "goal-actionable-call";
const UPDATE_BODY: &str = "addressed goal update";

#[derive(Clone, Copy)]
enum Scenario {
    FreshComplete,
    ResumedComplete,
    Error,
    OperatorSteer,
    ActionRequired,
    ResultReady,
}

fn body_json(request: &Request) -> Value {
    let compressed = request.headers.get("content-encoding").is_some_and(|value| {
        value
            .to_str()
            .is_ok_and(|value| value.split(',').any(|part| part.trim() == "zstd"))
    });
    let body = if compressed {
        zstd::stream::decode_all(std::io::Cursor::new(&request.body)).expect("decode request")
    } else {
        request.body.clone()
    };
    serde_json::from_slice(&body).expect("provider request JSON")
}

fn body_contains(request: &Request, text: &str) -> bool {
    body_json(request).to_string().contains(text)
}

fn is_goal_turn(request: &Request) -> bool {
    body_json(request)["client_metadata"]["x-codex-turn-metadata"]
        .as_str()
        .and_then(|metadata| serde_json::from_str::<Value>(metadata).ok())
        .is_some_and(|metadata| metadata["turn_trigger"] == "goal")
}

#[derive(Debug)]
struct GoalStart {
    thread_id: ThreadId,
    goal_id: String,
    turn_id: String,
}

struct GoalGate {
    state: Arc<Mutex<Option<StateDbHandle>>>,
    starts: mpsc::UnboundedSender<GoalStart>,
    markers: mpsc::UnboundedSender<GoalTurnMarker>,
    permits: Semaphore,
}

impl TurnLifecycleContributor for GoalGate {
    fn turn_start_phase(&self, _thread_store: &ExtensionData) -> TurnStartPhase {
        TurnStartPhase::RegularTaskStart
    }

    fn on_turn_start<'a>(&'a self, input: TurnStartInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let state = self
                .state
                .lock()
                .expect("goal state")
                .clone()
                .expect("state DB");
            let thread_id =
                ThreadId::from_string(input.thread_store.level_id()).expect("thread ID");
            if let Some(goal) = state
                .thread_goals()
                .get_thread_goal(thread_id)
                .await
                .expect("goal")
                && goal.status == ThreadGoalStatus::Active
            {
                self.starts
                    .send(GoalStart {
                        thread_id,
                        goal_id: goal.goal_id,
                        turn_id: input.turn_id.to_string(),
                    })
                    .expect("goal-start observer");
                self.permits.acquire().await.expect("goal gate").forget();
            }
        })
    }

    fn on_turn_stop<'a>(&'a self, input: TurnStopInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(marker) = input.turn_store.get::<GoalTurnMarker>() {
                self.markers
                    .send(marker.as_ref().clone())
                    .expect("real marker observer");
            }
        })
    }
}

fn builder_with_goal(
    state: Arc<Mutex<Option<StateDbHandle>>>,
    gate: Arc<GoalGate>,
) -> TestCodexBuilder {
    test_codex()
        .with_model_info_override("koffing", |model| {
            model.multi_agent_version = Some(MultiAgentVersion::V2);
        })
        .with_extension_builder(move |registry, state_db, manager| {
            *state.lock().expect("goal state") = state_db.clone();
            install_with_backend(
                registry,
                state_db.expect("persistent goal state"),
                AnalyticsEventsClient::disabled(),
                /*metrics_client*/ None,
                manager,
                Arc::new(GoalService::new()),
                |_| GoalExtensionConfig {
                    enabled: true,
                    max_goal_token_budget: None,
                },
            );
            registry.turn_lifecycle_contributor(gate);
            registry.thread_lifecycle_contributor(Arc::new(ThreadIdle));
        })
        .with_config(|config| {
            config.features.enable(Feature::Collab).expect("collab");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("V2");
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.supports_websockets = false;
        })
}

struct GoalReplies {
    index: AtomicUsize,
    scenario: Scenario,
}

impl Respond for GoalReplies {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let index = self.index.fetch_add(1, Ordering::AcqRel);
        let addressed = matches!(
            self.scenario,
            Scenario::ActionRequired | Scenario::ResultReady
        );
        let second_turn_end = if addressed { 3 } else { 2 };
        if index == 3 && matches!(self.scenario, Scenario::Error) {
            return sse_response(sse_failed(
                "goal-error",
                "server_error",
                "fixture goal failure",
            ));
        }
        let events = match index {
            0 => vec![
                ev_function_call_with_namespace(
                    PROGRESS_CALL,
                    "collaboration",
                    "send_message",
                    &json!({
                        "target": "/root",
                        "message": "quiet goal progress",
                        "intent": "progress",
                        "idempotency_key": "goal-progress",
                    })
                    .to_string(),
                ),
                ev_completed("goal-progress"),
            ],
            1 => vec![
                ev_assistant_message("goal-intermediate-one", "intermediate one"),
                ev_completed("goal-one"),
            ],
            2 if addressed => vec![
                ev_function_call_with_namespace(
                    UPDATE_CALL,
                    "collaboration",
                    "send_message",
                    &json!({
                        "target": "/root",
                        "message": UPDATE_BODY,
                        "intent": if matches!(self.scenario, Scenario::ActionRequired) {
                            "action_required"
                        } else {
                            "result_ready"
                        },
                        "idempotency_key": "goal-actionable",
                    })
                    .to_string(),
                ),
                ev_completed("goal-actionable"),
            ],
            index if index == second_turn_end => vec![
                ev_assistant_message("goal-intermediate-two", "intermediate two"),
                ev_completed("goal-two"),
            ],
            index if index == second_turn_end + 1 => vec![
                ev_function_call(
                    "goal-complete-call",
                    "update_goal",
                    r#"{"status":"complete"}"#,
                ),
                ev_completed("goal-complete-call"),
            ],
            index if index == second_turn_end + 2 => vec![
                ev_assistant_message("goal-final-message", "goal complete"),
                ev_completed("goal-final"),
            ],
            _ => panic!("unexpected goal provider request {index}"),
        };
        sse_response(sse(events))
    }
}

async fn mount_provider(
    server: &MockServer,
    scenario: Scenario,
) -> Result<(ResponseMock, ResponseMock)> {
    for (needle, events) in [
        (
            "spawn goal worker",
            vec![
                ev_function_call_with_namespace(
                    SPAWN_CALL,
                    "collaboration",
                    "spawn_agent",
                    &json!({
                        "task_name": "worker",
                        "fork_turns": "none",
                        "message": CHILD_INITIAL,
                    })
                    .to_string(),
                ),
                ev_completed("parent-spawn"),
            ],
        ),
        (
            SPAWN_CALL,
            vec![
                ev_assistant_message("parent-spawned", "worker started"),
                ev_completed("parent-spawned"),
            ],
        ),
    ] {
        mount_sse_once_match(
            server,
            move |request: &Request| body_contains(request, needle),
            sse(events),
        )
        .await;
    }
    let initial = if matches!(scenario, Scenario::ResumedComplete) {
        vec![
            ev_assistant_message("child-initial", "ready for resume"),
            ev_completed("child-initial"),
        ]
    } else {
        vec![
            ev_function_call(
                "goal-create-call",
                "create_goal",
                r#"{"objective":"real goal native wait"}"#,
            ),
            ev_completed("goal-created"),
        ]
    };
    mount_sse_once_match(
        server,
        |request: &Request| body_contains(request, CHILD_INITIAL),
        sse(initial),
    )
    .await;
    if !matches!(scenario, Scenario::ResumedComplete) {
        mount_sse_once_match(
            server,
            |request: &Request| body_contains(request, "goal-create-call"),
            sse(vec![
                ev_assistant_message("goal-established", "goal established"),
                ev_completed("goal-established"),
            ]),
        )
        .await;
    }
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(is_goal_turn)
        .respond_with(GoalReplies {
            index: AtomicUsize::new(0),
            scenario,
        })
        .expect(match scenario {
            Scenario::Error => 4,
            Scenario::ActionRequired | Scenario::ResultReady => 6,
            Scenario::FreshComplete | Scenario::ResumedComplete | Scenario::OperatorSteer => 5,
        })
        .mount(server)
        .await;
    mount_sse_once_match(
        server,
        |request: &Request| body_contains(request, WAIT_PROMPT),
        sse(vec![
            ev_response_created("parent-wait"),
            ev_function_call_with_namespace(
                WAIT_CALL,
                "collaboration",
                "wait_agent",
                &json!({
                    "targets": ["/root/worker"],
                    "return_when": if matches!(scenario, Scenario::ResumedComplete) {
                        "all"
                    } else {
                        "any"
                    },
                    "timeout_ms": 15000,
                })
                .to_string(),
            ),
            ev_completed("parent-wait"),
        ]),
    )
    .await;
    let output = mount_sse_once_match(
        server,
        |request: &Request| body_contains(request, WAIT_CALL),
        sse(vec![
            ev_assistant_message("parent-wait-finished", "wait returned"),
            ev_completed("parent-wait-finished"),
        ]),
    )
    .await;
    mount_sse_once_match(
        server,
        |request: &Request| body_contains(request, DRAIN_PROMPT),
        sse(vec![
            ev_assistant_message("parent-drain", "ready to report"),
            ev_completed("parent-drain"),
        ]),
    )
    .await;
    let report = mount_sse_once_match(
        server,
        |request: &Request| body_contains(request, REPORT_PROMPT),
        sse(vec![
            ev_assistant_message("parent-report", "reported"),
            ev_completed("parent-report"),
        ]),
    )
    .await;
    Ok((output, report))
}

async fn child_complete(
    child: &CodexThread,
    child_id: ThreadId,
    activities: &mut Vec<SubAgentActivityItem>,
) -> Result<String> {
    let turn_id = timeout(Duration::from_secs(10), async {
        loop {
            match child.next_event().await.context("child event stream ended")?.msg {
                EventMsg::ItemCompleted(event) => {
                    if let TurnItem::SubAgentActivity(activity) = event.item {
                        assert_eq!(event.thread_id, child_id);
                        activities.push(activity);
                    }
                }
                EventMsg::TurnComplete(event) => return Ok::<_, anyhow::Error>(event.turn_id),
                _ => {}
            }
        }
    })
    .await??;
    // Consume every idle permit, so the final permit cannot be from an earlier turn.
    timeout(Duration::from_secs(10), ThreadIdle::wait(child)).await?;
    Ok(turn_id)
}

async fn completed_wait(parent: &CodexThread) -> Result<CollabAgentToolCallItem> {
    loop {
        if let EventMsg::ItemCompleted(event) = parent
            .next_event()
            .await
            .context("parent event stream ended")?
            .msg
            && let TurnItem::CollabAgentToolCall(call) = event.item
            && call.tool == CollabAgentTool::Wait
        {
            return Ok(call);
        }
    }
}

async fn parent_request_count(server: &MockServer, parent: ThreadId) -> usize {
    server
        .received_requests()
        .await
        .expect("requests")
        .iter()
        .filter(|request| {
            body_json(request)["client_metadata"]["thread_id"] == parent.to_string()
        })
        .count()
}

async fn assert_quiet(
    parent: &CodexThread,
    server: &MockServer,
    parent_id: ThreadId,
    baseline: usize,
) -> Result<()> {
    // One deadline bounds the entire observer; unrelated events cannot reset it.
    assert!(
        timeout(Duration::from_millis(100), completed_wait(parent))
            .await
            .is_err(),
        "goal progress completed the native wait"
    );
    assert_eq!(parent_request_count(server, parent_id).await, baseline);
    Ok(())
}

fn set_resumed_phase(phase: &mut Option<&mut &'static str>, next: &'static str) {
    if let Some(phase) = phase.as_deref_mut() {
        *phase = next;
    }
}

async fn run_goal_scheduler_wait(
    scenario: Scenario,
    mut resumed_phase: Option<&mut &'static str>,
) -> Result<()> {
    let server = start_mock_server().await;
    let (output_mock, report_mock) = mount_provider(&server, scenario).await?;
    let state = Arc::new(Mutex::new(None));
    let (starts, mut start_rx) = mpsc::unbounded_channel();
    let (markers, mut marker_rx) = mpsc::unbounded_channel();
    let gate = Arc::new(GoalGate {
        state: Arc::clone(&state),
        starts,
        markers,
        permits: Semaphore::new(0),
    });
    let test = builder_with_goal(Arc::clone(&state), Arc::clone(&gate))
        .build_with_auto_env(&server)
        .await?;
    let parent_id = test.session_configured.thread_id;
    let mut created = test.thread_manager.subscribe_thread_created();
    set_resumed_phase(&mut resumed_phase, "spawn and initial child completion");
    test.submit_turn("spawn goal worker").await?;
    timeout(Duration::from_secs(10), ThreadIdle::wait(&test.codex)).await?;
    let child_id = timeout(Duration::from_secs(10), created.recv()).await??;
    let child = test.thread_manager.get_thread(child_id).await?;
    let mut activities = Vec::new();
    let initial_turn = child_complete(&child, child_id, &mut activities).await?;
    let (test, initial_goal_id) = if matches!(scenario, Scenario::ResumedComplete) {
        set_resumed_phase(&mut resumed_phase, "shutdown and seed persisted goal");
        let db = state.lock().expect("state").clone().expect("DB");
        test.thread_manager
            .shutdown_all_threads_bounded(Duration::from_secs(5))
            .await;
        let seeded_goal = db
            .thread_goals()
            .insert_thread_goal(
                child_id,
                "resumed real goal",
                ThreadGoalStatus::Active,
                None,
            )
            .await?
            .context("insert resumed goal")?;
        let mut builder = builder_with_goal(Arc::clone(&state), Arc::clone(&gate));
        set_resumed_phase(&mut resumed_phase, "resume root");
        let resumed = builder
            .resume(
                &server,
                Arc::clone(&test.home),
                test.session_configured
                    .rollout_path
                    .clone()
                    .context("rollout")?,
            )
            .await?;
        assert_eq!(resumed.session_configured.thread_id, parent_id);
        set_resumed_phase(&mut resumed_phase, "reload selected child");
        resumed
            .thread_manager
            .ensure_multi_agent_v2_child_loaded(child_id)
            .await?;
        (resumed, seeded_goal.goal_id)
    } else {
        let marker = timeout(Duration::from_secs(10), marker_rx.recv())
            .await?
            .context("initial marker")?;
        assert_eq!(marker.turn_id, initial_turn);
        assert_eq!(marker.readiness, GoalTurnReadiness::Continuing);
        (test, marker.goal_id)
    };
    let child = test.thread_manager.get_thread(child_id).await?;
    set_resumed_phase(&mut resumed_phase, "first automatic goal start");
    let first = timeout(Duration::from_secs(10), start_rx.recv())
        .await?
        .context("first automatic goal turn")?;
    assert_eq!(first.thread_id, child_id);
    assert_eq!(first.goal_id, initial_goal_id);
    let db = state.lock().expect("state").clone().expect("DB");
    assert_eq!(
        db.thread_goals()
            .get_thread_goal(child_id)
            .await?
            .context("active goal")?
            .goal_id,
        first.goal_id
    );
    set_resumed_phase(&mut resumed_phase, "start exact target native wait");
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: WAIT_PROMPT.to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    timeout(Duration::from_secs(10), async {
        loop {
            if let EventMsg::ItemStarted(event) = test
                .codex
                .next_event()
                .await
                .context("parent events")?
                .msg
                && let TurnItem::CollabAgentToolCall(call) = event.item
                && call.tool == CollabAgentTool::Wait
            {
                assert_eq!(event.thread_id, parent_id);
                assert_eq!(call.id, WAIT_CALL);
                assert_eq!(call.status, CollabAgentToolCallStatus::InProgress);
                assert_eq!(call.sender_thread_id, parent_id);
                assert_eq!(call.receiver_thread_ids, vec![child_id]);
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await??;
    let baseline = parent_request_count(&server, parent_id).await;
    assert_eq!(baseline, 3);
    set_resumed_phase(&mut resumed_phase, "initial quiet observation");
    assert_quiet(&test.codex, &server, parent_id, baseline).await?;

    let mut completed = None;
    if matches!(scenario, Scenario::OperatorSteer) {
        test.codex
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: OPERATOR_PROMPT.to_string(),
                text_elements: Vec::new(),
            }]))
            .await?;
        completed = Some(timeout(Duration::from_secs(10), completed_wait(&test.codex)).await??);
    }
    let mut previous = first;
    set_resumed_phase(&mut resumed_phase, "intermediate goal turns and quiet observations");
    for intermediate in 0..2 {
        gate.permits.add_permits(1);
        let turn_id = child_complete(&child, child_id, &mut activities).await?;
        let marker = timeout(Duration::from_secs(10), marker_rx.recv())
            .await?
            .context("real continuing marker")?;
        assert_eq!(
            (marker.goal_id.as_str(), marker.turn_id.as_str(), marker.readiness),
            (previous.goal_id.as_str(), turn_id.as_str(), GoalTurnReadiness::Continuing)
        );
        assert_eq!(turn_id, previous.turn_id);
        let next = timeout(Duration::from_secs(10), start_rx.recv())
            .await?
            .context("next automatic goal turn")?;
        assert_eq!(next.thread_id, child_id);
        assert_eq!(next.goal_id, previous.goal_id);
        assert_ne!(next.turn_id, previous.turn_id);
        previous = next;
        if intermediate == 1
            && matches!(scenario, Scenario::ActionRequired | Scenario::ResultReady)
        {
            completed = Some(timeout(Duration::from_secs(10), completed_wait(&test.codex)).await??);
            let goal = db
                .thread_goals()
                .get_thread_goal(child_id)
                .await?
                .context("goal remains active")?;
            assert_eq!(goal.status, ThreadGoalStatus::Active);
            assert_eq!(goal.goal_id, previous.goal_id);
        } else if completed.is_none() {
            assert_quiet(&test.codex, &server, parent_id, baseline).await?;
        }
    }
    set_resumed_phase(&mut resumed_phase, "final goal turn");
    gate.permits.add_permits(1);
    child_complete(&child, child_id, &mut activities).await?;
    let final_goal = db
        .thread_goals()
        .get_thread_goal(child_id)
        .await?
        .context("final goal")?;
    assert_eq!(final_goal.goal_id, previous.goal_id);
    assert_eq!(
        final_goal.status,
        if matches!(scenario, Scenario::Error) {
            ThreadGoalStatus::Blocked
        } else {
            ThreadGoalStatus::Complete
        }
    );
    set_resumed_phase(&mut resumed_phase, "native wait completion and output");
    let call = match completed {
        Some(call) => call,
        None => timeout(Duration::from_secs(10), completed_wait(&test.codex)).await??,
    };
    let expected = match scenario {
        Scenario::OperatorSteer => WaitAgentOutcome::OperatorSteer,
        Scenario::ActionRequired | Scenario::ResultReady => {
            WaitAgentOutcome::ExactTargetActionableMessage
        }
        Scenario::ResumedComplete => WaitAgentOutcome::TargetTerminalAll,
        Scenario::FreshComplete | Scenario::Error => WaitAgentOutcome::TargetTerminalAny,
    };
    assert_eq!(
        (
            call.id.as_str(),
            call.sender_thread_id,
            call.receiver_thread_ids,
            call.status,
            call.wait_outcome,
        ),
        (WAIT_CALL, parent_id, vec![child_id], CollabAgentToolCallStatus::Completed, Some(expected))
    );
    if matches!(scenario, Scenario::Error) {
        assert!(
            matches!(call.agents_states.get(&child_id), Some(AgentStatus::Errored(message))
                if message.contains("fixture goal failure"))
        );
    } else if matches!(scenario, Scenario::FreshComplete | Scenario::ResumedComplete) {
        assert!(matches!(
            call.agents_states.get(&child_id),
            Some(AgentStatus::Completed(_))
        ));
    } else {
        assert!(call.agents_states.is_empty());
    }
    let EventMsg::TurnComplete(wait_turn) =
        wait_for_event(&test.codex, |event| matches!(event, EventMsg::TurnComplete(_))).await
    else {
        unreachable!("the matcher selects the root wait turn completion");
    };
    timeout(Duration::from_secs(10), ThreadIdle::wait(&test.codex)).await?;
    // ResponseMock also records requests rejected by the caller's matcher.
    // Select the actual root continuation, independently of its output value.
    let output_prompt = if matches!(scenario, Scenario::OperatorSteer) {
        OPERATOR_PROMPT
    } else {
        WAIT_PROMPT
    };
    let root_id = parent_id.to_string();
    let outputs = output_mock
        .requests()
        .into_iter()
        .filter(|request| {
            request.body_json()["client_metadata"]["thread_id"].as_str() == Some(root_id.as_str())
                && request.message_input_texts("user").last().map(String::as_str)
                    == Some(output_prompt)
                && request.function_call_output_text(WAIT_CALL).is_some()
        })
        .collect::<Vec<_>>();
    assert_eq!(outputs.len(), 1);
    let output = outputs[0]
        .function_call_output_text(WAIT_CALL)
        .context("native wait output")?;
    let result: Value = serde_json::from_str(&output)?;
    assert_eq!(result["outcome"], serde_json::to_value(expected)?);
    assert_eq!(result["timed_out"], false);

    // A fresh nonempty turn samples its prompt before queued mail. Its terminal
    // completion records deferred mail, so observe the following report only
    // after that recording boundary. All child notification idle boundaries and
    // the root wait idle boundary have already been consumed above.
    set_resumed_phase(&mut resumed_phase, "deferred handback recording turn");
    test.submit_turn(DRAIN_PROMPT).await?;
    timeout(Duration::from_secs(10), ThreadIdle::wait(&test.codex)).await?;
    set_resumed_phase(&mut resumed_phase, "final report and handback identity");
    test.submit_turn(REPORT_PROMPT).await?;
    let reports = report_mock
        .requests()
        .into_iter()
        .filter(|request| {
            request.body_json()["client_metadata"]["thread_id"].as_str() == Some(root_id.as_str())
                && request.message_input_texts("user").last().map(String::as_str)
                    == Some(REPORT_PROMPT)
        })
        .collect::<Vec<_>>();
    assert_eq!(reports.len(), 1);
    let report = &reports[0];
    let messages = report.inputs_of_type("agent_message");
    let handbacks = messages
        .iter()
        .filter(|message| message.to_string().contains("Message Type: FINAL_ANSWER"))
        .collect::<Vec<_>>();
    let mut payloads = Vec::new();
    for message in handbacks {
        assert_eq!(message["author"], "/root/worker");
        assert_eq!(message["recipient"], "/root");
        assert_eq!(
            message["content"].as_array().context("handback content")?.len(),
            1
        );
        let text = message["content"][0]["text"]
            .as_str()
            .context("handback text")?;
        payloads.push(
            text.strip_prefix(
                "Message Type: FINAL_ANSWER\nTask name: /root\nSender: /root/worker\nPayload:\n",
            )
            .context("exact child handback envelope")?,
        );
    }
    match scenario {
        Scenario::Error => {
            assert_eq!(payloads.len(), 1);
            let Some(AgentStatus::Errored(error)) = call.agents_states.get(&child_id) else {
                unreachable!("the selected child's raw error was asserted above");
            };
            assert!(payloads[0].starts_with(&format!("Agent errored: {error}\n\n")));
        }
        // Preserve and distinguish the pre-goal completion across the restart.
        Scenario::ResumedComplete => assert_eq!(payloads, vec!["ready for resume", "goal complete"]),
        Scenario::FreshComplete
        | Scenario::OperatorSteer
        | Scenario::ActionRequired
        | Scenario::ResultReady => assert_eq!(payloads, vec!["goal complete"]),
    }
    set_resumed_phase(&mut resumed_phase, "progress delivery identity");
    let progress = activities
        .iter()
        .filter(|activity| activity.id == PROGRESS_CALL)
        .collect::<Vec<_>>();
    assert_eq!(progress.len(), 1);
    assert_eq!(progress[0].agent_thread_id, parent_id);
    assert_eq!(progress[0].agent_path.as_str(), "/root");
    let progress_messages = messages
        .iter()
        .filter(|message| message.to_string().contains("quiet goal progress"))
        .collect::<Vec<_>>();
    assert_eq!(progress_messages.len(), 1);
    assert_eq!(progress_messages[0]["author"], "/root/worker");
    assert_eq!(progress_messages[0]["recipient"], "/root");
    if matches!(scenario, Scenario::ActionRequired | Scenario::ResultReady) {
        let update_activities = activities
            .iter()
            .filter(|activity| activity.id == UPDATE_CALL)
            .collect::<Vec<_>>();
        assert_eq!(update_activities.len(), 1);
        let activity = update_activities[0];
        assert_eq!(activity.agent_thread_id, parent_id);
        assert_eq!(activity.agent_path.as_str(), "/root");
        let updates = messages
            .iter()
            .filter(|message| message.to_string().contains(UPDATE_BODY))
            .collect::<Vec<_>>();
        assert_eq!(updates.len(), 1);
        let update = updates[0];
        assert_eq!(update["author"], "/root/worker");
        assert_eq!(update["recipient"], "/root");
        test.codex.flush_rollout().await?;
        let (items, stored_thread, parse_errors) =
            RolloutRecorder::load_rollout_items(&test.codex.rollout_path().context("root rollout")?)
                .await?;
        assert_eq!((stored_thread, parse_errors), (Some(parent_id), 0));
        let admissions = items
            .iter()
            .filter_map(|item| match item {
                RolloutItem::InterAgentCommunicationMetadata {
                    admission: Some(admission),
                    ..
                } if admission.idempotency_key == "goal-actionable" => Some(admission),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(admissions.len(), 1);
        let admission = admissions[0];
        assert_eq!(
            (admission.root_thread_id, admission.sender_thread_id, admission.recipient_thread_id),
            (parent_id, child_id, parent_id)
        );
        assert_eq!(
            (admission.sender_path.as_str(), admission.recipient_path.as_str()),
            ("/root/worker", "/root")
        );
        assert_eq!(
            admission.original_communication.id.as_ref(),
            Some(&admission.update_id)
        );
        assert_eq!(update["id"], admission.update_id.as_str());
        let recorded_updates = items
            .iter()
            .filter_map(|item| match item {
                RolloutItem::ResponseItem(response)
                    if response
                        .metadata
                        .as_ref()
                        .and_then(|metadata| metadata.inter_agent_update_id.as_ref())
                        == Some(&admission.update_id) => Some(response),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(recorded_updates.len(), 1);
        let recorded_update = &recorded_updates[0].item;
        assert_eq!(recorded_update.turn_id(), Some(wait_turn.turn_id.as_str()));
        let create_time = recorded_update
            .executed_tool_call_metadata()
            .and_then(|metadata| metadata.create_time.clone())
            .context("recorded addressed update creation time")?;
        assert!(create_time.as_f64().is_some_and(|seconds| seconds > 0.0));
        let mut expected_update = admission.original_communication.to_model_input_item();
        expected_update.set_turn_id_if_missing(&wait_turn.turn_id);
        expected_update.set_create_time_if_missing(create_time);
        assert_eq!(recorded_update, &expected_update);
        assert_eq!(
            *update,
            serde_json::to_value(expected_update)?
        );
        assert_eq!(admission.original_communication.author, admission.sender_path);
        assert_eq!(
            admission.original_communication.recipient,
            admission.recipient_path
        );
        assert!(admission.original_communication.content.is_empty());
        assert_eq!(
            admission.original_communication.encrypted_content.as_deref(),
            Some(UPDATE_BODY)
        );
        assert!(!admission.original_communication.trigger_turn);
        assert!(admission.original_communication.other_recipients.is_empty());
        assert_eq!(
            serde_json::to_value(admission.intent)?,
            if matches!(scenario, Scenario::ActionRequired) {
                json!("action_required")
            } else {
                json!("result_ready")
            }
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_goal_scheduler_keeps_native_wait_quiet_until_complete() -> Result<()> {
    run_goal_scheduler_wait(Scenario::FreshComplete, None).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_goal_scheduler_keeps_native_wait_quiet_until_complete(
) -> std::result::Result<(), &'static str> {
    struct PanicPhase(&'static str);

    impl Drop for PanicPhase {
        fn drop(&mut self) {
            if std::thread::panicking() {
                use std::io::Write as _;

                // A failed diagnostic write must not panic during unwinding.
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "resumed goal native wait panic phase: {}",
                    self.0
                );
            }
        }
    }

    let mut phase = PanicPhase("setup");
    // Keep this diagnostic finite and payload-free instead of returning an
    // anyhow backtrace that can displace the failure origin from hosted tails.
    run_goal_scheduler_wait(Scenario::ResumedComplete, Some(&mut phase.0))
        .await
        .map_err(|_| phase.0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goal_scheduler_error_wakes_native_wait_with_raw_error() -> Result<()> {
    run_goal_scheduler_wait(Scenario::Error, None).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operator_steer_wakes_goal_native_wait_without_goal_completion() -> Result<()> {
    run_goal_scheduler_wait(Scenario::OperatorSteer, None).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goal_action_required_wakes_same_pending_native_wait() -> Result<()> {
    run_goal_scheduler_wait(Scenario::ActionRequired, None).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goal_result_ready_wakes_same_pending_native_wait() -> Result<()> {
    run_goal_scheduler_wait(Scenario::ResultReady, None).await
}
