//! Verifies observed Responses API usage is durably recorded in rollout history.

use anyhow::Context;
use anyhow::Result;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_features::Feature;
use codex_history::RolloutItem;
use codex_protocol::SessionId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TokenUsageRecord;
use core_test_support::ThreadIdle;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_response_once_match;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

fn token_usage_records(path: &std::path::Path) -> Vec<TokenUsageRecord> {
    std::fs::read_to_string(path)
        .expect("read rollout")
        .lines()
        .filter_map(|line| codex_rollout::parse_rollout_line(line).ok())
        .filter_map(|line| match line.item {
            RolloutItem::TokenUsageRecord(record) => Some(record),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observed_response_usage_accumulates_per_turn_and_thread() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let plan_args = json!({
        "plan": [{
            "step": "keep sampling",
            "status": "in_progress"
        }]
    })
    .to_string();
    mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("response-a"),
                ev_function_call("call-a", "update_plan", &plan_args),
                ev_completed_with_tokens("response-a", /*total_tokens*/ 120),
            ]),
            sse(vec![
                ev_response_created("response-b"),
                ev_assistant_message("message-b", "done"),
                ev_completed_with_tokens("response-b", /*total_tokens*/ 80),
            ]),
            sse(vec![
                ev_response_created("response-c"),
                ev_assistant_message("message-c", "next"),
                ev_completed_with_tokens("response-c", /*total_tokens*/ 30),
            ]),
            sse(vec![
                ev_response_created("response-without-usage"),
                ev_assistant_message("message-d", "no usage"),
                json!({
                    "type": "response.completed",
                    "response": {
                        "id": "response-without-usage"
                    }
                }),
            ]),
        ],
    )
    .await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let rollout_path = test.codex.rollout_path().expect("rollout path");
    let home = test.home.clone();

    test.submit_turn("first").await?;
    test.codex.shutdown_and_wait().await?;

    let resumed = test_codex()
        .resume(&server, home, rollout_path.clone())
        .await?;
    for prompt in ["second", "third"] {
        resumed.submit_turn(prompt).await?;
    }
    resumed.codex.shutdown_and_wait().await?;

    let records = token_usage_records(&rollout_path);
    assert_eq!(records.len(), 3);
    assert_eq!(
        records
            .iter()
            .map(|record| {
                (
                    record.response_id.as_str(),
                    record.turn_token_usage.total_tokens,
                    record.thread_token_usage.total_tokens,
                )
            })
            .collect::<Vec<_>>(),
        vec![
            ("response-a", 120, 120),
            ("response-b", 200, 200),
            ("response-c", 30, 230),
        ]
    );
    assert_eq!(records[0].turn_id, records[1].turn_id);
    assert_ne!(records[1].turn_id, records[2].turn_id);
    assert!(records.iter().all(|record| {
        record.session_id == SessionId::from(record.thread_id)
            && record.root_turn_id == record.turn_id
    }));

    Ok(())
}

fn ev_completed_with_response_usage(
    id: &str,
    input_tokens: i64,
    cached_tokens: i64,
    cache_write_tokens: i64,
    output_tokens: i64,
    total_tokens: i64,
) -> serde_json::Value {
    json!({
        "type": "response.completed",
        "response": {
            "id": id,
            "usage": {
                "input_tokens": input_tokens,
                "input_tokens_details": {
                    "cached_tokens": cached_tokens,
                    "cache_write_tokens": cache_write_tokens,
                },
                "output_tokens": output_tokens,
                "output_tokens_details": null,
                "total_tokens": total_tokens,
            }
        }
    })
}

fn request_has_call_output(request: &wiremock::Request, call_id: &str) -> bool {
    serde_json::from_slice::<serde_json::Value>(&request.body)
        .ok()
        .and_then(|body| body.get("input").and_then(serde_json::Value::as_array).cloned())
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type").and_then(serde_json::Value::as_str)
                    == Some("function_call_output")
                    && item.get("call_id").and_then(serde_json::Value::as_str) == Some(call_id)
            })
        })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_response_usage_reopens_with_root_child_lineage_and_replay_deduplication()
-> Result<()> {
    skip_if_no_network!(Ok(()));

    const ROOT_PROMPT: &str = "spawn the first worker";
    const CHILD_TASK: &str = "first worker task";
    const ROOT_RESPONSE_ID: &str = "response-root-replay";
    const ACTUAL_MODEL: &str = "gpt-6.1-sol";

    let server = start_mock_server().await;
    let root_usage = ev_completed_with_response_usage(
        ROOT_RESPONSE_ID,
        /*input_tokens*/ 100_000,
        /*cached_tokens*/ 10_000,
        /*cache_write_tokens*/ 0,
        /*output_tokens*/ 1_000,
        /*total_tokens*/ 101_000,
    );
    let spawn_arguments = json!({
        "message": CHILD_TASK,
        "task_name": "usage_proof_child",
        "fork_turns": "none",
    })
    .to_string();
    let _root_request = mount_response_once_match(
        &server,
        |request: &wiremock::Request| {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .is_ok_and(|body| body.to_string().contains(ROOT_PROMPT))
        },
        sse_response(sse(vec![
            ev_response_created(ROOT_RESPONSE_ID),
            ev_function_call_with_namespace(
                "spawn-usage-child",
                "collaboration",
                "spawn_agent",
                &spawn_arguments,
            ),
            root_usage.clone(),
        ]))
        .insert_header("openai-model", ACTUAL_MODEL),
    )
    .await;

    mount_response_once_match(
        &server,
        |request: &wiremock::Request| {
            let body = serde_json::from_slice::<serde_json::Value>(&request.body).ok();
            body.as_ref().is_some_and(|body| {
                body.to_string().contains(CHILD_TASK)
                    && !request_has_call_output(request, "spawn-usage-child")
            })
        },
        sse_response(sse(vec![
            ev_response_created("response-child"),
            ev_assistant_message("message-child", "worker completed"),
            ev_completed_with_response_usage(
                "response-child",
                /*input_tokens*/ 17,
                /*cached_tokens*/ 4,
                /*cache_write_tokens*/ 0,
                /*output_tokens*/ 6,
                /*total_tokens*/ 23,
            ),
        ]))
        .insert_header("openai-model", ACTUAL_MODEL),
    )
    .await;

    let replay_plan = json!({
        "plan": [{ "step": "same-turn replay", "status": "in_progress" }]
    })
    .to_string();
    let spawn_followup = mount_response_once_match(
        &server,
        |request: &wiremock::Request| request_has_call_output(request, "spawn-usage-child"),
        sse_response(sse(vec![
            ev_response_created(ROOT_RESPONSE_ID),
            ev_function_call("plan-usage-replay", "update_plan", &replay_plan),
            root_usage.clone(),
        ]))
        .insert_header("openai-model", ACTUAL_MODEL),
    )
    .await;

    let conflict_plan = json!({
        "plan": [{ "step": "conflicting replay", "status": "completed" }]
    })
    .to_string();
    mount_response_once_match(
        &server,
        |request: &wiremock::Request| request_has_call_output(request, "plan-usage-replay"),
        sse_response(sse(vec![
            ev_response_created(ROOT_RESPONSE_ID),
            ev_function_call("plan-usage-conflict", "update_plan", &conflict_plan),
            ev_completed_with_response_usage(
                ROOT_RESPONSE_ID,
                /*input_tokens*/ 999,
                /*cached_tokens*/ 0,
                /*cache_write_tokens*/ 0,
                /*output_tokens*/ 1,
                /*total_tokens*/ 1_000,
            ),
        ]))
        .insert_header("openai-model", ACTUAL_MODEL),
    )
    .await;
    mount_response_once_match(
        &server,
        |request: &wiremock::Request| request_has_call_output(request, "plan-usage-conflict"),
        sse_response(sse(vec![
            ev_response_created("response-root-final"),
            ev_assistant_message("message-root-final", "root completed"),
            ev_completed_with_response_usage(
                "response-root-final",
                /*input_tokens*/ 50,
                /*cached_tokens*/ 0,
                /*cache_write_tokens*/ 0,
                /*output_tokens*/ 4,
                /*total_tokens*/ 54,
            ),
        ]))
        .insert_header("openai-model", ACTUAL_MODEL),
    )
    .await;
    mount_response_once_match(
        &server,
        |request: &wiremock::Request| {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .is_ok_and(|body| body.to_string().contains("second turn with missing model"))
        },
        sse_response(sse(vec![
            ev_response_created("response-unknown-model"),
            ev_assistant_message("message-unknown-model", "unknown model completed"),
            ev_completed_with_response_usage(
                "response-unknown-model",
                /*input_tokens*/ 11,
                /*cached_tokens*/ 3,
                /*cache_write_tokens*/ 0,
                /*output_tokens*/ 2,
                /*total_tokens*/ 13,
            ),
        ])),
    )
    .await;

    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let test = test_codex()
        .with_model("gpt-6-sol")
        .with_extensions(Arc::new(extensions.build()))
        .with_config(|config| {
            config.features.enable(Feature::Sqlite).unwrap();
            config.features.enable(Feature::Collab).unwrap();
            config.features.enable(Feature::MultiAgentV2).unwrap();
        })
        .build_with_auto_env(&server)
        .await?;
    let root_thread_id = test.session_configured.thread_id.to_string();
    let rollout_path = test.codex.rollout_path().expect("root rollout path");
    let home = test.home.clone();
    test.submit_turn(ROOT_PROMPT).await?;
    ThreadIdle::wait(&test.codex).await;
    let spawn_output = spawn_followup
        .function_call_output_text("spawn-usage-child")
        .expect("root should receive the spawn_agent result");
    let child_id = test
        .thread_manager
        .list_thread_ids()
        .await
        .into_iter()
        .find(|thread_id| thread_id.to_string() != root_thread_id)
        .unwrap_or_else(|| panic!("spawn_agent did not register a child: {spawn_output}"));
    let child = test.thread_manager.get_thread(child_id).await?;
    wait_for_event(&child, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    ThreadIdle::wait(&child).await;
    test.submit_turn("second turn with missing model").await?;
    ThreadIdle::wait(&test.codex).await;

    let sqlite = test.config.sqlite.clone();
    let writer_state = test
        .thread_store
        .as_any()
        .downcast_ref::<codex_thread_store::LocalThreadStore>()
        .context("local thread store")?
        .state_db()
        .await
        .context("actual session writer state")?;
    let shutdown = test
        .thread_manager
        .shutdown_all_threads_bounded(Duration::from_secs(10))
        .await;
    assert!(shutdown.submit_failed.is_empty());
    assert!(shutdown.timed_out.is_empty());
    writer_state.close().await;
    assert!(writer_state.usage_pool().is_closed());

    let replay_records = token_usage_records(&rollout_path)
        .into_iter()
        .filter(|record| record.response_id == ROOT_RESPONSE_ID)
        .map(|record| record.usage.total_tokens)
        .collect::<Vec<_>>();
    assert_eq!(replay_records, vec![101_000, 101_000, 1_000]);

    drop(child);
    drop(test);
    drop(writer_state);
    let reopened = codex_state::StateRuntime::init(sqlite, "openai".to_string()).await?;
    let pool = reopened.usage_pool();
    let root_rows = sqlx::query_as::<
        _,
        (
            String,
            String,
            Option<String>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            String,
        ),
    >(
        "SELECT request_id, requested_model, actual_model_used, input_tokens_uncached, input_tokens_cached, input_tokens_cache_write, output_tokens, total_tokens, status FROM usage_provider_calls WHERE thread_id = ? ORDER BY request_id",
    )
    .bind(&root_thread_id)
    .fetch_all(pool.as_ref())
    .await?;
    assert_eq!(root_rows.len(), 3, "replay deduplicates and conflict keeps first write");
    let replay = root_rows
        .iter()
        .find(|row| row.0 == ROOT_RESPONSE_ID)
        .context("first response row retained")?;
    assert_eq!(
        (
            replay.1.as_str(),
            replay.2.as_deref(),
            replay.3,
            replay.4,
            replay.5,
            replay.6,
            replay.7,
            replay.8.as_str(),
        ),
        (
            "gpt-6-sol",
            Some(ACTUAL_MODEL),
            Some(90_000),
            Some(10_000),
            Some(0),
            Some(1_000),
            Some(101_000),
            "ok",
        )
    );
    let unknown = root_rows
        .iter()
        .find(|row| row.0 == "response-unknown-model")
        .context("unknown-model response persisted")?;
    assert_eq!(unknown.1, "gpt-6-sol");
    assert_eq!(unknown.2, None, "requested model is not actual-model evidence");
    let root_total = root_rows.iter().filter_map(|row| row.7).sum::<i64>();
    assert_eq!(root_total, 101_067);

    let child_rows = sqlx::query_as::<_, (Option<String>, Option<i64>)>(
        "SELECT actual_model_used, total_tokens FROM usage_provider_calls WHERE thread_id = ?",
    )
    .bind(child_id.to_string())
    .fetch_all(pool.as_ref())
    .await?;
    assert_eq!(child_rows, vec![(Some(ACTUAL_MODEL.to_string()), Some(23))]);
    let root_and_child_total = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT SUM(p.total_tokens) FROM usage_provider_calls p JOIN usage_threads t ON t.thread_id = p.thread_id WHERE COALESCE(t.root_thread_id, t.thread_id) = ?",
    )
    .bind(&root_thread_id)
    .fetch_one(pool.as_ref())
    .await?;
    assert_eq!(root_and_child_total, Some(101_090));

    let root_lineage = sqlx::query_as::<_, (Option<String>, Option<String>)>(
        "SELECT parent_thread_id, root_thread_id FROM usage_threads WHERE thread_id = ?",
    )
    .bind(&root_thread_id)
    .fetch_one(pool.as_ref())
    .await?;
    let child_lineage = sqlx::query_as::<_, (Option<String>, Option<String>)>(
        "SELECT parent_thread_id, root_thread_id FROM usage_threads WHERE thread_id = ?",
    )
    .bind(child_id.to_string())
    .fetch_one(pool.as_ref())
    .await?;
    assert_eq!(root_lineage, (None, Some(root_thread_id.clone())));
    assert_eq!(
        child_lineage,
        (Some(root_thread_id.clone()), Some(root_thread_id))
    );

    reopened.close().await;
    drop(home);
    Ok(())
}
