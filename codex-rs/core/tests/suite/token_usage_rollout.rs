//! Verifies observed Responses API usage is durably recorded in rollout history.

use anyhow::Context;
use anyhow::Result;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_features::Feature;
use codex_history::RolloutItem;
use codex_protocol::SessionId;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::openai_models::ModelServiceTier;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TokenUsageRecord;
use core_test_support::ThreadIdle;
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
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
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
    let mut event = json!({
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
        event["response"]["model"] = json!(model);
    }
    event
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

    let server = start_mock_server().await;
    let root_response_id = "response-root-replay";
    let root_usage = ev_completed_with_response_usage(
        root_response_id,
        Some("gpt-6.1-sol"),
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
    let root_request = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .is_ok_and(|body| body.to_string().contains(ROOT_PROMPT))
        },
        sse(vec![
            ev_response_created(root_response_id),
            ev_function_call_with_namespace(
                "spawn-usage-child",
                "collaboration",
                "spawn_agent",
                &spawn_arguments,
            ),
            root_usage.clone(),
        ]),
    )
    .await;

    let child_usage = ev_completed_with_response_usage(
        "response-child",
        Some("gpt-6.1-sol"),
        /*input_tokens*/ 17,
        /*cached_tokens*/ 4,
        /*cache_write_tokens*/ 0,
        /*output_tokens*/ 6,
        /*total_tokens*/ 23,
    );
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
                /*input_tokens*/ 999,
                /*cached_tokens*/ 0,
                /*cache_write_tokens*/ 0,
                /*output_tokens*/ 1,
                /*total_tokens*/ 1_000,
            ),
        ]),
    )
    .await;
    let mut final_usage = ev_completed_with_response_usage(
        "response-root-final",
        Some("gpt-6.1-sol"),
        /*input_tokens*/ 50,
        /*cached_tokens*/ 0,
        /*cache_write_tokens*/ 0,
        /*output_tokens*/ 4,
        /*total_tokens*/ 54,
    );
    final_usage["response"]["service_tier"] = json!("provider-tier-unpriced");
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| request_has_call_output(request, "plan-usage-conflict"),
        sse(vec![
            ev_response_created("response-root-final"),
            ev_assistant_message("message-root-final", "root completed"),
            final_usage,
        ]),
    )
    .await;

    let mut unknown_usage = ev_completed_with_response_usage(
        "response-unknown-model",
        /*model*/ None,
        /*input_tokens*/ 11,
        /*cached_tokens*/ 3,
        /*cache_write_tokens*/ 0,
        /*output_tokens*/ 2,
        /*total_tokens*/ 13,
    );
    unknown_usage["response"]["service_tier"] = serde_json::Value::Null;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .is_ok_and(|body| body.to_string().contains("second turn with missing model"))
        },
        sse(vec![
            ev_response_created("response-unknown-model"),
            ev_assistant_message("message-unknown-model", "unknown model completed"),
            unknown_usage,
        ]),
    )
    .await;

    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let test = test_codex()
        .with_model("gpt-6-sol")
        .with_model_info_override("gpt-6-sol", |model| {
            model.service_tiers = vec![ModelServiceTier {
                id: ServiceTier::Fast.request_value().to_string(),
                name: "Fast".to_string(),
                description: "Priority processing".to_string(),
            }];
        })
        .with_extensions(Arc::new(extensions.build()))
        .with_config(|config| {
            config.features.enable(Feature::Sqlite).unwrap();
            config.features.enable(Feature::Collab).unwrap();
            config.features.enable(Feature::MultiAgentV2).unwrap();
            config.features.enable(Feature::FastMode).unwrap();
            config.service_tier = Some(ServiceTier::Fast.request_value().to_string());
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
    assert_eq!(
        root_request.single_request().body_json()["service_tier"],
        json!(ServiceTier::Fast.request_value())
    );

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
    assert_eq!(
        repeated_response_records
            .iter()
            .map(|record| (
                record.usage.input_tokens,
                record.usage.cached_input_tokens,
                record.usage.cache_write_input_tokens,
                record.usage.output_tokens,
                record.usage.total_tokens,
            ))
            .collect::<Vec<_>>(),
        vec![
            (100_000, 10_000, 0, 1_000, 101_000),
            (100_000, 10_000, 0, 1_000, 101_000),
            (999, 0, 0, 1, 1_000),
        ],
        "rollout must retain the exact replay and conflicting response-local payloads"
    );
    assert!(records.iter().any(|record| {
        record.response_id == "response-unknown-model"
            && record.turn_id != repeated_response_records[0].turn_id
    }));

    drop(child);
    drop(test);
    drop(writer_state);
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
            Option<String>,
            Option<String>,
            Option<String>,
            String,
            String,
        ),
    >(
        "SELECT request_id, turn_id, requested_model, actual_model_used, input_tokens_uncached, input_tokens_cached, input_tokens_cache_write, output_tokens, total_tokens, status, requested_service_tier, actual_service_tier, actual_service_tier_source, started_at, completed_at FROM usage_provider_calls WHERE thread_id = ? ORDER BY request_id",
    )
    .bind(&root_thread_id)
    .fetch_all(pool.as_ref())
    .await?;
    assert_eq!(
        rows.len(),
        3,
        "exact replay must deduplicate and conflict must retain the first row"
    );
    let replay = rows
        .iter()
        .find(|row| row.0 == root_response_id)
        .context("first response retained")?;
    assert_eq!(
        (
            replay.2.as_str(),
            replay.3.as_deref(),
            replay.4,
            replay.5,
            replay.6,
            replay.7,
            replay.8,
            replay.9.as_str(),
        ),
        (
            "gpt-6-sol",
            Some("gpt-6.1-sol"),
            Some(90_000),
            Some(10_000),
            Some(0),
            Some(1_000),
            Some(101_000),
            "ok",
        )
    );
    assert_eq!(
        replay.10.as_deref(),
        Some(ServiceTier::Fast.request_value())
    );
    assert_eq!((replay.11.as_deref(), replay.12.as_deref()), (None, None));
    let final_row = rows
        .iter()
        .find(|row| row.0 == "response-root-final")
        .context("unknown-tier response persisted")?;
    assert_eq!(
        (final_row.11.as_deref(), final_row.12.as_deref()),
        (Some("provider-tier-unpriced"), Some("provider_response"))
    );
    let unknown = rows
        .iter()
        .find(|row| row.0 == "response-unknown-model")
        .context("unknown-model response persisted")?;
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
            unknown.9.as_str(),
        ),
        (Some(8), Some(3), Some(0), Some(2), Some(13), "ok")
    );
    assert_eq!((unknown.11.as_deref(), unknown.12.as_deref()), (None, None));
    for row in &rows {
        assert_eq!(row.10.as_deref(), Some(ServiceTier::Fast.request_value()));
        let started_at = chrono::DateTime::parse_from_rfc3339(&row.13)?;
        let completed_at = chrono::DateTime::parse_from_rfc3339(&row.14)?;
        assert!(completed_at >= started_at);
    }

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
    assert!(standard_rate.1.context("supplied standard-rate estimate")? > 0.0);
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
    assert_eq!(summary, (3, 0, 3, 1));
    let root_and_child_summary = sqlx::query_as::<_, (i64, i64, i64, i64)>(
        "SELECT SUM(s.provider_call_count), SUM(s.priced_call_count), SUM(s.unpriced_call_count), MAX(s.partial) FROM usage_thread_credit_summary s JOIN usage_threads t ON t.thread_id = s.thread_id WHERE COALESCE(t.root_thread_id, t.thread_id) = ?",
    )
    .bind(&root_thread_id)
    .fetch_one(pool.as_ref())
    .await?;
    assert_eq!(root_and_child_summary, (4, 0, 4, 1));
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
    // Session tasks can emit the conflict warning outside this test's span.
    let global_logs = tracing_test::internal::global_buf()
        .lock()
        .expect("tracing-test global log buffer")
        .clone();
    let global_logs = String::from_utf8(global_logs).expect("tracing-test logs are UTF-8");
    logs_assert(|lines: &[&str]| {
        let has_expected_warning = |line: &&str| {
            line.contains(
                "failed to persist completed provider usage; continuing response handling",
            ) && line.contains("response-root-replay")
                && line.contains("conflicting payload for an existing provider response identity")
        };
        let global_response_logs = global_logs
            .lines()
            .filter(|line| line.contains("response-root-replay"))
            .collect::<Vec<_>>();
        if lines.iter().any(has_expected_warning)
            || global_response_logs.iter().any(has_expected_warning)
        {
            return Ok(());
        }
        let related_lines = lines
            .iter()
            .filter(|line| {
                line.contains("provider usage")
                    || line.contains("response-root-replay")
                    || line.contains("conflicting payload")
            })
            .take(12)
            .map(|line| line.chars().take(512).collect::<String>())
            .collect::<Vec<_>>();
        Err(format!(
            "expected conflict warning was not captured in test scope or global buffer; observed {} scoped lines and {} global lines for the unique response ID; scoped related logs (up to 12 x 512 chars): {related_lines:?}",
            lines.len(),
            global_response_logs.len()
        ))
    });
    Ok(())
}
