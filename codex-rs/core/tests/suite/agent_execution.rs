use anyhow::Result;
use codex_features::Feature;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::AgentStatus;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;

const FIRST_PROMPT: &str = "spawn the first worker";
const FIRST_TASK: &str = "first worker task";
const SECOND_PROMPT: &str = "spawn the second worker";
const SECOND_TASK: &str = "second worker task";
const QUEUE_PROMPT: &str = "queue a note for the first worker";
const QUEUED_MESSAGE: &str = "remember the cold mailbox note";
const FOLLOWUP_PROMPT: &str = "resume the first worker";
const FOLLOWUP_TASK: &str = "continue after the cold mailbox note";
const MULTI_AGENT_V2_NAMESPACE: &str = "agents";

fn body_contains(request: &wiremock::Request, text: &str) -> bool {
    serde_json::from_slice::<serde_json::Value>(&request.body)
        .is_ok_and(|body| body.to_string().contains(text))
}

fn has_function_call_output(request: &wiremock::Request, call_id: &str) -> bool {
    serde_json::from_slice::<serde_json::Value>(&request.body).is_ok_and(|body| {
        body.get("input")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item.get("type").and_then(serde_json::Value::as_str)
                        == Some("function_call_output")
                        && item.get("call_id").and_then(serde_json::Value::as_str) == Some(call_id)
                })
            })
    })
}

fn has_input_type(request: &wiremock::Request, input_type: &str) -> bool {
    serde_json::from_slice::<serde_json::Value>(&request.body).is_ok_and(|body| {
        body.get("input")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item.get("type").and_then(serde_json::Value::as_str) == Some(input_type)
                })
            })
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_nested_spawn_checks_shared_active_execution_capacity() -> Result<()> {
    let server = start_mock_server().await;
    let first_args = serde_json::to_string(&json!({
        "message": FIRST_TASK,
        "task_name": "first",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, FIRST_PROMPT),
        sse(vec![
            ev_response_created("first-response"),
            ev_function_call_with_namespace(
                "first-call",
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &first_args,
            ),
            ev_completed("first-response"),
        ]),
    )
    .await;
    let second_args = serde_json::to_string(&json!({
        "message": SECOND_TASK,
        "task_name": "second",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, FIRST_TASK) && !has_function_call_output(request, "first-call")
        },
        sse(vec![
            ev_response_created("first-worker-response"),
            ev_function_call_with_namespace(
                "second-call",
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &second_args,
            ),
            ev_completed("first-worker-response"),
        ]),
    )
    .await;
    let second_followup = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, "second-call"),
        sse(vec![
            ev_response_created("second-followup-response"),
            ev_assistant_message("second-followup-message", "blocked"),
            ev_completed("second-followup-response"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, "first-call"),
        sse(vec![
            ev_response_created("first-followup-response"),
            ev_assistant_message("first-followup-message", "spawned"),
            ev_completed("first-followup-response"),
        ]),
    )
    .await;

    let mut builder = test_codex().with_model("koffing").with_config(|config| {
        config
            .features
            .enable(Feature::Collab)
            .expect("test config should allow feature update");
        config
            .features
            .enable(Feature::MultiAgentV2)
            .expect("test config should allow feature update");
        config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    });
    let test = builder.build(&server).await?;
    test.submit_turn(FIRST_PROMPT).await?;

    let second_output = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(output) = second_followup.function_call_output_text("second-call") {
                return output;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    assert_eq!(
        second_output,
        "collab spawn failed: agent thread limit reached"
    );
    assert_eq!(test.thread_manager.list_thread_ids().await.len(), 2);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_cold_mailbox_allows_eviction_and_replays_on_followup() -> Result<()> {
    let server = start_mock_server().await;
    let first_args = serde_json::to_string(&json!({
        "message": FIRST_TASK,
        "task_name": "first",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, FIRST_PROMPT),
        sse(vec![
            ev_response_created("first-response"),
            ev_function_call_with_namespace(
                "first-call",
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &first_args,
            ),
            ev_completed("first-response"),
        ]),
    )
    .await;

    let queue_args = serde_json::to_string(&json!({
        "target": "first",
        "items": [{"type": "text", "text": QUEUED_MESSAGE}],
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, QUEUE_PROMPT),
        sse(vec![
            ev_response_created("queue-response"),
            ev_function_call_with_namespace(
                "queue-call",
                MULTI_AGENT_V2_NAMESPACE,
                "send_message",
                &queue_args,
            ),
            ev_completed("queue-response"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, "queue-call"),
        sse(vec![
            ev_response_created("queue-followup-response"),
            ev_assistant_message("queue-followup-message", "queued"),
            ev_completed("queue-followup-response"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, FIRST_TASK) && !has_function_call_output(request, "first-call")
        },
        sse(vec![
            ev_response_created("first-worker-response"),
            ev_assistant_message("first-worker-message", "first worker done"),
            ev_completed("first-worker-response"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, "first-call"),
        sse(vec![
            ev_response_created("first-followup-response"),
            ev_assistant_message("first-followup-message", "spawned"),
            ev_completed("first-followup-response"),
        ]),
    )
    .await;

    let second_args = serde_json::to_string(&json!({
        "message": SECOND_TASK,
        "task_name": "second",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, SECOND_PROMPT),
        sse(vec![
            ev_response_created("second-response"),
            ev_function_call_with_namespace(
                "second-call",
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &second_args,
            ),
            ev_completed("second-response"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, SECOND_TASK) && !has_function_call_output(request, "second-call")
        },
        sse(vec![
            ev_response_created("second-worker-response"),
            ev_assistant_message("second-worker-message", "second worker done"),
            ev_completed("second-worker-response"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, "second-call"),
        sse(vec![
            ev_response_created("second-followup-response"),
            ev_assistant_message("second-followup-message", "spawned"),
            ev_completed("second-followup-response"),
        ]),
    )
    .await;

    let followup_args = serde_json::to_string(&json!({
        "target": "first",
        "message": FOLLOWUP_TASK,
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, FOLLOWUP_PROMPT),
        sse(vec![
            ev_response_created("followup-response"),
            ev_function_call_with_namespace(
                "followup-call",
                MULTI_AGENT_V2_NAMESPACE,
                "followup_task",
                &followup_args,
            ),
            ev_completed("followup-response"),
        ]),
    )
    .await;
    let followup_child = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            has_input_type(request, "agent_message")
                && body_contains(request, QUEUED_MESSAGE)
                && body_contains(request, FOLLOWUP_TASK)
        },
        sse(vec![
            ev_response_created("followup-worker-response"),
            ev_assistant_message("followup-worker-message", "first worker resumed"),
            ev_completed("followup-worker-response"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, "followup-call"),
        sse(vec![
            ev_response_created("followup-result-response"),
            ev_assistant_message("followup-result-message", "resumed"),
            ev_completed("followup-result-response"),
        ]),
    )
    .await;

    let mut builder = test_codex().with_model("koffing").with_config(|config| {
        config
            .features
            .enable(Feature::Collab)
            .expect("test config should allow feature update");
        config
            .features
            .enable(Feature::MultiAgentV2)
            .expect("test config should allow feature update");
        config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    });
    let test = builder.build_with_auto_env(&server).await?;
    test.submit_turn(FIRST_PROMPT).await?;

    let first_id = test
        .thread_manager
        .list_thread_ids()
        .await
        .into_iter()
        .find(|id| *id != test.session_configured.thread_id)
        .expect("first worker should be resident");
    let first_thread = test.thread_manager.get_thread(first_id).await?;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if first_thread.agent_status().await
                == AgentStatus::Completed(Some("first worker done".to_string()))
                && first_thread.inject_if_running(Vec::new()).await.is_err()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;

    test.submit_turn(QUEUE_PROMPT).await?;
    assert!(test.thread_manager.get_thread(first_id).await.is_ok());

    test.submit_turn(SECOND_PROMPT).await?;

    match test.thread_manager.get_thread(first_id).await {
        Err(CodexErr::ThreadNotFound(thread_id)) => assert_eq!(thread_id, first_id),
        Err(err) => panic!("expected evicted thread to be missing, got {err:?}"),
        Ok(_) => panic!("expected evicted thread to be missing"),
    }
    let second_id = test
        .thread_manager
        .list_thread_ids()
        .await
        .into_iter()
        .find(|id| *id != test.session_configured.thread_id)
        .expect("second worker should be resident");
    let second_thread = test.thread_manager.get_thread(second_id).await?;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if second_thread.agent_status().await
                == AgentStatus::Completed(Some("second worker done".to_string()))
                && second_thread.inject_if_running(Vec::new()).await.is_err()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;

    test.submit_turn(FOLLOWUP_PROMPT).await?;
    assert!(test.thread_manager.get_thread(first_id).await.is_ok());
    let replay_request = followup_child
        .requests()
        .into_iter()
        .find(|request| {
            request.body_contains_text(QUEUED_MESSAGE)
                && request.body_contains_text(FOLLOWUP_TASK)
        })
        .expect("follow-up request should contain cold and trigger mail");
    let replay_body = replay_request.body_json().to_string();
    let queued_position = replay_body
        .find(QUEUED_MESSAGE)
        .expect("cold mail should be present");
    let followup_position = replay_body
        .find(FOLLOWUP_TASK)
        .expect("triggering follow-up should be present");
    assert!(
        queued_position < followup_position,
        "cold mail should precede the triggering follow-up"
    );

    Ok(())
}
