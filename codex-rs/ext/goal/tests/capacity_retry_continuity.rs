//! Capacity recovery crosses the actual goal scheduler, not a synthetic marker.

#![recursion_limit = "256"]
#![allow(clippy::expect_used)]

use anyhow::Result;
use codex_analytics::AnalyticsEventsClient;
use codex_core::StateDbHandle;
use codex_features::Feature;
use codex_goal_extension::GoalExtensionConfig;
use codex_goal_extension::GoalService;
use codex_goal_extension::install_with_backend;
use codex_protocol::protocol::EventMsg;
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
use core_test_support::test_codex::test_codex;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::Request;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const CHILD_INITIAL: &str = "capacity-goal-child-initial";
const SPAWN_CALL: &str = "capacity-goal-spawn-call";
const GOAL_CREATE_CALL: &str = "capacity-goal-create-call";

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
    serde_json::from_slice(&decoded_body(request)).expect("provider request JSON")
}

fn body_contains(request: &Request, needle: &str) -> bool {
    String::from_utf8_lossy(&decoded_body(request)).contains(needle)
}

fn request_thread_id(request: &Request) -> Option<String> {
    body_json(request)["client_metadata"]["thread_id"]
        .as_str()
        .map(str::to_string)
}

fn has_agent_message(request: &Request) -> bool {
    body_json(request)["input"].as_array().is_some_and(|items| {
        items
            .iter()
            .any(|item| item["type"] == json!("agent_message"))
    })
}

fn is_goal_turn(request: &Request) -> bool {
    let body = body_json(request);
    let Some(raw) = body["client_metadata"]["x-codex-turn-metadata"].as_str() else {
        return false;
    };
    serde_json::from_str::<Value>(raw)
        .ok()
        .as_ref()
        .and_then(|metadata| metadata["turn_trigger"].as_str())
        == Some("goal")
}

struct GoalCapacityReplies {
    index: std::sync::atomic::AtomicUsize,
}

impl Respond for GoalCapacityReplies {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let index = self.index.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        match index {
            0..=2 => sse_response(sse_failed(
                &format!("goal-capacity-{index}"),
                "server_is_overloaded",
                "Selected model is at capacity.",
            )),
            3 => sse_response(sse(vec![
                ev_response_created("goal-capacity-recovered"),
                ev_function_call(
                    "goal-capacity-complete-call",
                    "update_goal",
                    r#"{"status":"complete"}"#,
                ),
                ev_completed("goal-capacity-recovered"),
            ])),
            4 => sse_response(sse(vec![
                ev_response_created("goal-capacity-finished"),
                ev_assistant_message("goal-capacity-finished-msg", "goal completed"),
                ev_completed("goal-capacity-finished"),
            ])),
            _ => panic!("unexpected extra goal model request {index}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goal_stays_active_through_capacity_retries_then_completes() -> Result<()> {
    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": CHILD_INITIAL,
        "task_name": "worker",
        "fork_turns": "none",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &Request| body_contains(request, "spawn capacity goal worker"),
        sse(vec![
            ev_response_created("capacity-parent-spawn"),
            ev_function_call_with_namespace(
                SPAWN_CALL,
                "collaboration",
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("capacity-parent-spawn"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &Request| body_contains(request, SPAWN_CALL) && !has_agent_message(request),
        sse(vec![
            ev_response_created("capacity-parent-done"),
            ev_assistant_message("capacity-parent-done-msg", "worker started"),
            ev_completed("capacity-parent-done"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &Request| {
            body_contains(request, CHILD_INITIAL)
                && has_agent_message(request)
                && !body_contains(request, GOAL_CREATE_CALL)
        },
        sse(vec![
            ev_response_created("capacity-goal-create"),
            ev_function_call(
                GOAL_CREATE_CALL,
                "create_goal",
                r#"{"objective":"complete after transient capacity"}"#,
            ),
            ev_completed("capacity-goal-create"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &Request| body_contains(request, GOAL_CREATE_CALL) && has_agent_message(request),
        sse(vec![
            ev_response_created("capacity-goal-created"),
            ev_assistant_message("capacity-goal-created-msg", "goal established"),
            ev_completed("capacity-goal-created"),
        ]),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(|request: &Request| is_goal_turn(request))
        .respond_with(GoalCapacityReplies {
            index: std::sync::atomic::AtomicUsize::new(0),
        })
        .expect(5)
        .mount(&server)
        .await;

    let state_slot: Arc<Mutex<Option<StateDbHandle>>> = Arc::new(Mutex::new(None));
    let state_for_builder = Arc::clone(&state_slot);
    let test = test_codex()
        .with_model("koffing")
        .with_extension_builder(move |registry, state_db, manager| {
            *state_for_builder.lock().expect("state slot") = state_db.clone();
            install_with_backend(
                registry,
                state_db.expect("persistent goal state"),
                AnalyticsEventsClient::disabled(),
                None,
                manager,
                Arc::new(GoalService::new()),
                |_| GoalExtensionConfig {
                    enabled: true,
                    max_goal_token_budget: None,
                },
            );
        })
        .with_config(|config| {
            config.features.enable(Feature::Collab).expect("collab");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("multi-agent v2");
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
            config.model_provider.supports_websockets = false;
        })
        .build(&server)
        .await?;
    let parent_id = test.session_configured.thread_id;
    let mut created = test.thread_manager.subscribe_thread_created();
    test.submit_turn("spawn capacity goal worker").await?;
    let child_id = timeout(Duration::from_secs(5), created.recv()).await??;
    let child = test.thread_manager.get_thread(child_id).await?;
    let state_db = state_slot
        .lock()
        .expect("state slot")
        .clone()
        .expect("state db");

    let mut recovery_events = 0;
    timeout(Duration::from_secs(15), async {
        loop {
            let event = child.next_event().await.expect("child event");
            match event.msg {
                EventMsg::StreamError(progress) => {
                    assert!(progress.message.contains("Model at capacity; retrying"));
                    recovery_events += 1;
                    let goal = state_db
                        .thread_goals()
                        .get_thread_goal(child_id)
                        .await
                        .expect("read goal")
                        .expect("goal exists during retry");
                    assert_eq!(goal.status, ThreadGoalStatus::Active);
                }
                EventMsg::Error(error) => panic!("capacity terminalized child: {error:?}"),
                EventMsg::TurnComplete(_) if recovery_events == 3 => {
                    let goal = state_db
                        .thread_goals()
                        .get_thread_goal(child_id)
                        .await
                        .expect("read completed goal")
                        .expect("goal remains stored");
                    if goal.status == ThreadGoalStatus::Complete {
                        break;
                    }
                }
                _ => {}
            }
        }
    })
    .await?;
    assert_eq!(recovery_events, 3);

    let requests = server.received_requests().await.expect("mock requests");
    let goal_requests = requests
        .iter()
        .filter(|request| request_thread_id(request) == Some(child_id.to_string()))
        .filter(|request| is_goal_turn(request))
        .collect::<Vec<_>>();
    assert_eq!(goal_requests.len(), 5);
    let retry_turn = body_json(goal_requests[0])["client_metadata"]["turn_id"]
        .as_str()
        .expect("goal turn id")
        .to_string();
    assert!(goal_requests[..4].iter().all(|request| {
        body_json(request)["client_metadata"]["turn_id"].as_str() == Some(retry_turn.as_str())
    }));
    assert_eq!(
        goal_requests
            .iter()
            .filter(|request| body_contains(request, "goal-capacity-complete-call"))
            .count(),
        1,
        "the update_goal tool result is consumed once after the retries",
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request_thread_id(request) == Some(parent_id.to_string()))
            .count(),
        2,
        "the parent made no model call to observe child retry progress",
    );
    Ok(())
}
