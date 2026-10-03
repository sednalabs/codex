//! Verifies observed Responses API usage is durably recorded in rollout history.

use anyhow::Result;
use codex_features::Feature;
use codex_history::RolloutItem;
use codex_protocol::SessionId;
use codex_protocol::protocol::TokenUsageRecord;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::json;
use tracing_test::traced_test;

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

fn ev_completed_with_response_usage(
    id: &str,
    model: Option<&str>,
    input_tokens: i64,
    cached_tokens: i64,
    cache_write_tokens: i64,
    output_tokens: i64,
    total_tokens: i64,
) -> serde_json::Value {
    let mut response = json!({
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
    });
    if let Some(model) = model {
        response["response"]["model"] = json!(model);
    }
    response
}

fn request_has_call_output(request: &wiremock::Request, call_id: &str) -> bool {
    serde_json::from_slice::<serde_json::Value>(&request.body)
        .ok()
        .and_then(|body| {
            body.get("input")
                .and_then(serde_json::Value::as_array)
                .cloned()
        })
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type").and_then(serde_json::Value::as_str) == Some("function_call_output")
                    && item.get("call_id").and_then(serde_json::Value::as_str) == Some(call_id)
            })
        })
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[traced_test]
async fn completed_response_usage_reaches_sqlite_lineage_and_credit_views_after_reopen()
-> Result<()> {
    skip_if_no_network!(Ok(()));

    const ROOT_PROMPT: &str = "spawn the first worker";
    const CHILD_TASK: &str = "first worker task";
    const COLLAB_NAMESPACE: &str = "collaboration";

    let server = start_mock_server().await;
    let root_response_id = "response-root-replay";
    let root_usage = ev_completed_with_response_usage(
        root_response_id,
        Some("gpt-6.1-sol"),
        100_000,
        10_000,
        0,
        1_000,
        101_000,
    );
    let spawn_arguments = json!({
        "message": CHILD_TASK,
        "task_name": "usage-proof-child",
        "fork_turns": "none",
    })
    .to_string();
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .is_ok_and(|body| body.to_string().contains(ROOT_PROMPT))
        },
        sse(vec![
            ev_response_created(root_response_id),
            ev_function_call_with_namespace(
                "spawn-usage-child",
                COLLAB_NAMESPACE,
                "spawn_agent",
                &spawn_arguments,
            ),
            root_usage.clone(),
        ]),
    )
    .await;

    let child_usage =
        ev_completed_with_response_usage("response-child", Some("gpt-6.1-sol"), 17, 4, 0, 6, 23);
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            let body = serde_json::from_slice::<serde_json::Value>(&request.body).ok();
            body.as_ref().is_some_and(|body| {
                body.to_string().contains(CHILD_TASK)
                    && !request_has_call_output(request, "spawn-usage-child")
            })
        },
        sse(vec![
            ev_response_created("response-child"),
            ev_assistant_message("message-child", "worker completed"),
            child_usage,
        ]),
    )
    .await;

    let replay_plan = json!({
        "plan": [{ "step": "same-turn replay", "status": "in_progress" }]
    })
    .to_string();
    let spawn_followup = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| request_has_call_output(request, "spawn-usage-child"),
        sse(vec![
            ev_response_created(root_response_id),
            ev_function_call("plan-usage-replay", "update_plan", &replay_plan),
            root_usage.clone(),
        ]),
    )
    .await;

    let conflict_plan = json!({
        "plan": [{ "step": "conflicting replay", "status": "completed" }]
    })
    .to_string();
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| request_has_call_output(request, "plan-usage-replay"),
        sse(vec![
            ev_response_created(root_response_id),
            ev_function_call("plan-usage-conflict", "update_plan", &conflict_plan),
            ev_completed_with_response_usage(
                root_response_id,
                Some("gpt-6.1-sol"),
                999,
                0,
                0,
                1,
                1_000,
            ),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| request_has_call_output(request, "plan-usage-conflict"),
        sse(vec![
            ev_response_created("response-root-final"),
            ev_assistant_message("message-root-final", "root completed"),
            ev_completed_with_response_usage(
                "response-root-final",
                Some("gpt-6.1-sol"),
                50,
                0,
                0,
                4,
                54,
            ),
        ]),
    )
    .await;

    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .is_ok_and(|body| body.to_string().contains("second turn with missing model"))
        },
        sse(vec![
            ev_response_created("response-unknown-model"),
            ev_assistant_message("message-unknown-model", "unknown model completed"),
            ev_completed_with_response_usage("response-unknown-model", None, 11, 3, 0, 2, 13),
        ]),
    )
    .await;

    let test = test_codex()
        .with_model("gpt-6-sol")
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
    test.submit_turn("second turn with missing model").await?;
    test.codex.shutdown_and_wait().await?;
    let sqlite = test.config.sqlite.clone();

    let records = token_usage_records(&rollout_path);
    let repeated_response_records = records
        .iter()
        .filter(|record| record.response_id == root_response_id)
        .collect::<Vec<_>>();
    assert_eq!(repeated_response_records.len(), 3);
    assert!(
        repeated_response_records
            .iter()
            .all(|record| record.turn_id == repeated_response_records[0].turn_id)
    );
    assert!(records.iter().any(|record| {
        record.response_id == "response-unknown-model"
            && record.turn_id != repeated_response_records[0].turn_id
    }));

    drop(test);
    let codex_state = codex_state::StateRuntime::init(sqlite, "openai".to_string()).await?;
    let pool = codex_state.usage_pool();
    let rows = sqlx::query_as::<
        _,
        (
            String,
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
        "SELECT request_id, turn_id, requested_model, actual_model_used, input_tokens_uncached, input_tokens_cached, input_tokens_cache_write, output_tokens, total_tokens, status FROM usage_provider_calls WHERE thread_id = ? ORDER BY request_id",
    )
    .bind(&root_thread_id)
    .fetch_all(pool.as_ref())
    .await?;
    assert_eq!(
        rows.len(),
        3,
        "exact replay must deduplicate and conflict must retain the first row"
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row.0 == root_response_id)
            .map(|row| (
                row.2.as_str(),
                row.3.as_deref(),
                row.4,
                row.5,
                row.6,
                row.7,
                row.8,
                row.9.as_str(),
            ))
            .collect::<Vec<_>>(),
        vec![(
            "gpt-6-sol",
            Some("gpt-6.1-sol"),
            Some(90_000),
            Some(10_000),
            Some(0),
            Some(1_000),
            Some(101_000),
            "ok",
        )]
    );
    let unknown = rows
        .iter()
        .find(|row| row.0 == "response-unknown-model")
        .expect("unknown-model response persisted");
    assert_eq!(unknown.2, "gpt-6-sol");
    assert_eq!(
        unknown.3, None,
        "requested model must not become observed identity"
    );
    assert_eq!(
        (
            unknown.4,
            unknown.5,
            unknown.6,
            unknown.7,
            unknown.8,
            unknown.9.as_str()
        ),
        (Some(8), Some(3), Some(0), Some(2), Some(13), "ok")
    );

    let lineage_rows = sqlx::query_as::<_, (String, Option<String>, Option<String>, String)>(
        "SELECT t.thread_id, t.parent_thread_id, t.root_thread_id, p.request_id FROM usage_threads t JOIN usage_provider_calls p ON p.thread_id = t.thread_id WHERE t.thread_id IN (?, ?) ORDER BY t.thread_id, p.request_id",
    )
    .bind(&root_thread_id)
    .bind(child_id.to_string())
    .fetch_all(pool.as_ref())
    .await?;
    assert_eq!(lineage_rows.len(), 4);
    assert!(lineage_rows.iter().any(|row| {
        row.0 == child_id.to_string()
            && row.1.as_deref() == Some(root_thread_id.as_str())
            && row.2.as_deref() == Some(root_thread_id.as_str())
            && row.3 == "response-child"
    }));
    assert!(lineage_rows.iter().any(|row| {
        row.0 == root_thread_id
            && row.1.is_none()
            && row.2.as_deref() == Some(root_thread_id.as_str())
            && row.3 == root_response_id
    }));

    let standard_rate = sqlx::query_as::<_, (String, Option<f64>)>(
        "SELECT scenario_status, estimated_total_credits FROM usage_provider_call_standard_rate_estimates WHERE provider_call_id = (SELECT provider_call_id FROM usage_provider_calls WHERE thread_id = ? AND request_id = ?)",
    )
    .bind(&root_thread_id)
    .bind(root_response_id)
    .fetch_one(pool.as_ref())
    .await?;
    assert_eq!(standard_rate.0, "priced_scenario_estimate");
    assert!(standard_rate.1.expect("supplied standard-rate estimate") > 0.0);
    let strict_rate = sqlx::query_as::<_, (String, Option<f64>)>(
        "SELECT pricing_status, estimated_total_credits FROM usage_provider_call_credit_estimates WHERE provider_call_id = (SELECT provider_call_id FROM usage_provider_calls WHERE thread_id = ? AND request_id = ?)",
    )
    .bind(&root_thread_id)
    .bind(root_response_id)
    .fetch_one(pool.as_ref())
    .await?;
    assert_eq!(strict_rate, ("actual_tier_missing".to_string(), None));

    let summary = sqlx::query_as::<_, (i64, i64, i64, i64)>(
        "SELECT provider_call_count, priced_call_count, unpriced_call_count, partial FROM usage_thread_credit_summary WHERE thread_id = ?",
    )
    .bind(&root_thread_id)
    .fetch_one(pool.as_ref())
    .await?;
    assert_eq!(summary, (3, 2, 1, 1));

    let root_and_child_summary = sqlx::query_as::<_, (i64, i64, i64, i64)>(
        "SELECT SUM(s.provider_call_count), SUM(s.priced_call_count), SUM(s.unpriced_call_count), MAX(s.partial) FROM usage_thread_credit_summary s JOIN usage_threads t ON t.thread_id = s.thread_id WHERE COALESCE(t.root_thread_id, t.thread_id) = ?",
    )
    .bind(&root_thread_id)
    .fetch_one(pool.as_ref())
    .await?;
    assert_eq!(root_and_child_summary, (4, 3, 1, 1));
    let unknown_price = sqlx::query_as::<_, (String, Option<f64>)>(
        "SELECT pricing_status, estimated_total_credits FROM usage_provider_call_credit_estimates WHERE provider_call_id = (SELECT provider_call_id FROM usage_provider_calls WHERE thread_id = ? AND request_id = ?)",
    )
    .bind(&root_thread_id)
    .bind("response-unknown-model")
    .fetch_one(pool.as_ref())
    .await?;
    assert_eq!(unknown_price, ("actual_model_missing".to_string(), None));

    codex_state.close().await;
    drop(home);
    logs_assert(|lines: &[&str]| {
        assert!(lines.iter().any(|line| {
            line.contains("failed to record per-response provider usage for response-root-replay")
                && line.contains("conflicting payload for an existing provider response identity")
        }));
        Ok(())
    });
    Ok(())
}
