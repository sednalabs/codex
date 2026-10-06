use super::StateRuntime;
use super::usage::UsageLogger;
use crate::SqliteConfig;
use anyhow::Result;
use codex_protocol::ThreadId;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TokenCountEvent;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageInfo;
use codex_utils_absolute_path::test_support::PathExt;
use sqlx::SqlitePool;
use tempfile::tempdir;

fn thread_id(value: &str) -> ThreadId {
    ThreadId::from_string(value).expect("fixture thread id is a UUID")
}

fn token_count_event(
    turn_id: &str,
    uncached_input_tokens: i64,
    cached_input_tokens: i64,
    output_tokens: i64,
    total_tokens: i64,
) -> Event {
    let usage = TokenUsage {
        input_tokens: uncached_input_tokens + cached_input_tokens,
        cached_input_tokens,
        cache_write_input_tokens: 0,
        output_tokens,
        reasoning_output_tokens: 0,
        total_tokens,
    };
    let info = TokenUsageInfo {
        total_token_usage: usage.clone(),
        last_token_usage: usage,
        model_context_window: Some(4096),
    };
    Event {
        id: turn_id.to_string(),
        msg: EventMsg::TokenCount(TokenCountEvent {
            info: Some(info),
            rate_limits: None,
            provider: Some("openai".to_string()),
            model_used: Some("gpt-6-luna".to_string()),
            requested_service_tier: Some("default".to_string()),
            actual_service_tier: Some("default".to_string()),
            actual_service_tier_source: Some("runtime_contract".to_string()),
            fast_mode_requested: Some(false),
            fast_mode_used: Some(false),
            billing_surface: Some("chatgpt_credits".to_string()),
            account_plan: Some("plus".to_string()),
        }),
    }
}

async fn start_logger(
    runtime: &std::sync::Arc<StateRuntime>,
    thread_id: ThreadId,
    source: SessionSource,
    thread_source: ThreadSource,
    forked_from_id: Option<ThreadId>,
) -> Result<UsageLogger> {
    UsageLogger::try_new_with_thread_source(
        runtime.clone(),
        thread_id,
        source,
        Some(thread_source),
        forked_from_id,
        /*agent_nickname*/ None,
        /*agent_role*/ None,
    )
    .await
}

async fn record_completed_response(
    logger: &mut UsageLogger,
    turn_id: &str,
    uncached_input_tokens: i64,
    cached_input_tokens: i64,
    output_tokens: i64,
    total_tokens: i64,
) {
    logger
        .record_event(&token_count_event(
            turn_id,
            uncached_input_tokens,
            cached_input_tokens,
            output_tokens,
            total_tokens,
        ))
        .await;
    logger
        .record_event(&Event {
            id: turn_id.to_string(),
            msg: EventMsg::TurnComplete(codex_protocol::protocol::TurnCompleteEvent {
                turn_id: turn_id.to_string(),
                started_at: None,
                last_agent_message: None,
                error: None,
                compaction_events_in_turn: 0,
                final_model: Some("gpt-6-luna".to_string()),
                model_snapshot: Some("gpt-6-luna".to_string()),
                provider_usage: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        })
        .await;
}

async fn set_call_window(
    pool: &SqlitePool,
    thread_id: &str,
    turn_id: &str,
    started_at: &str,
    completed_at: &str,
    actual_model: Option<&str>,
    reported_credits: Option<f64>,
) -> Result<()> {
    let result = sqlx::query(
        r#"UPDATE usage_provider_calls
SET requested_model = 'gpt-6-sol', actual_model_used = ?,
    requested_service_tier = 'default', actual_service_tier = 'default',
    fast_mode_used = 0, started_at = ?, completed_at = ?,
    provider_reported_credits = ?
WHERE thread_id = ? AND turn_id = ?"#,
    )
    .bind(actual_model)
    .bind(started_at)
    .bind(completed_at)
    .bind(reported_credits)
    .bind(thread_id)
    .bind(turn_id)
    .execute(pool)
    .await?;
    anyhow::ensure!(
        result.rows_affected() == 1,
        "expected one writer-created usage call for turn {turn_id}"
    );
    Ok(())
}

async fn provider_writer_batch_seconds(indexed_for_reporting: bool) -> Result<f64> {
    let home = tempdir()?;
    let sqlite = SqliteConfig::new_for_testing(home.path().abs());
    let runtime = StateRuntime::init(sqlite, "openai".to_string()).await?;
    let pool = runtime.usage_pool();
    if !indexed_for_reporting {
        for statement in [
            "DROP INDEX usage_provider_calls_started_at_idx",
            "DROP INDEX usage_provider_calls_thread_started_at_idx",
            "DROP INDEX usage_threads_reporting_parent_idx",
            "CREATE INDEX usage_provider_calls_thread_idx ON usage_provider_calls(thread_id)",
        ] {
            sqlx::query(statement).execute(pool.as_ref()).await?;
        }
    }
    let mut logger = UsageLogger::try_new(
        runtime.clone(),
        ThreadId::new(),
        SessionSource::Cli,
        /*forked_from_id*/ None,
        /*agent_nickname*/ None,
        /*agent_role*/ None,
    )
    .await?;
    let started = std::time::Instant::now();
    for index in 0..250 {
        let turn_id = format!("writer-turn-{index:04}");
        record_completed_response(
            &mut logger,
            &turn_id,
            /*uncached_input_tokens*/ 100,
            /*cached_input_tokens*/ 20,
            /*output_tokens*/ 10,
            /*total_tokens*/ 130,
        )
        .await;
    }
    let elapsed = started.elapsed().as_secs_f64();
    drop(logger);
    drop(pool);
    runtime.close().await;
    Ok(elapsed)
}

#[tokio::test]
async fn bounded_usage_report_cli_qualifies_real_writer_and_hosted_scale() -> Result<()> {
    let home = tempdir()?;
    let sqlite = SqliteConfig::new_for_testing(home.path().abs());
    let runtime = StateRuntime::init(sqlite.clone(), "openai".to_string()).await?;
    let pool = runtime.usage_pool();
    let root_id = thread_id("00000000-0000-4000-8000-000000000001");
    let child_id = thread_id("00000000-0000-4000-8000-000000000002");
    let grandchild_id = thread_id("00000000-0000-4000-8000-000000000003");
    let zero_call_id = thread_id("00000000-0000-4000-8000-000000000004");
    let forked_id = thread_id("00000000-0000-4000-8000-000000000005");

    let mut root_logger = start_logger(
        &runtime,
        root_id,
        SessionSource::Cli,
        ThreadSource::User,
        /*forked_from_id*/ None,
    )
    .await?;
    let mut child_logger = start_logger(
        &runtime,
        child_id,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: root_id,
            depth: 1,
            agent_nickname: Some("Child".to_string()),
            agent_role: Some("worker".to_string()),
            agent_path: None,
        }),
        ThreadSource::Subagent,
        /*forked_from_id*/ None,
    )
    .await?;
    let mut grandchild_logger = start_logger(
        &runtime,
        grandchild_id,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: child_id,
            depth: 2,
            agent_nickname: Some("Grandchild".to_string()),
            agent_role: Some("worker".to_string()),
            agent_path: None,
        }),
        ThreadSource::Subagent,
        /*forked_from_id*/ None,
    )
    .await?;
    let zero_call_logger = start_logger(
        &runtime,
        zero_call_id,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: child_id,
            depth: 2,
            agent_nickname: Some("Empty".to_string()),
            agent_role: Some("worker".to_string()),
            agent_path: None,
        }),
        ThreadSource::Subagent,
        /*forked_from_id*/ None,
    )
    .await?;
    let mut forked_logger = start_logger(
        &runtime,
        forked_id,
        SessionSource::Cli,
        ThreadSource::Side,
        Some(child_id),
    )
    .await?;

    for statement in [
        "DELETE FROM usage_codex_credit_policies WHERE policy_id = 'openai-chatgpt-plus-token-20260402'",
        "DELETE FROM usage_codex_credit_rates WHERE rate_id = 'openai-gpt-6-luna-standard-20260926'",
    ] {
        sqlx::query(statement).execute(pool.as_ref()).await?;
    }
    sqlx::query(
        "INSERT INTO usage_codex_credit_policies (policy_id, provider, billing_surface, account_plan, rate_card_kind, effective_from, effective_to, source_url, source_observed_at) VALUES ('policy-test', 'openai', 'chatgpt_credits', 'plus', 'codex_token_based', '2026-09-01T00:00:00Z', NULL, 'https://example.invalid/test-card', '2026-09-01T00:00:00Z')",
    )
    .execute(pool.as_ref())
    .await?;
    sqlx::query(
        "INSERT INTO usage_codex_credit_rates (rate_id, provider, model, service_tier, speed_mode, rate_card_kind, credits_per_1m_uncached_input, credits_per_1m_cached_input, credits_per_1m_output, effective_from, effective_to, source_url, source_observed_at) VALUES ('rate-before', 'openai', 'gpt-6-luna', 'default', 'standard', 'codex_token_based', 1000000.0, 2000000.0, 3000000.0, '2026-09-30T00:00:00Z', '2026-09-30T00:30:00Z', 'https://example.invalid/test-card', '2026-09-29T00:00:00Z'), ('rate-after', 'openai', 'gpt-6-luna', 'default', 'standard', 'codex_token_based', 1000000.0, 2000000.0, 3000000.0, '2026-09-30T00:30:00Z', NULL, 'https://example.invalid/test-card', '2026-09-29T00:00:00Z')",
    )
    .execute(pool.as_ref())
    .await?;

    record_completed_response(
        &mut root_logger,
        "root-call",
        /*uncached_input_tokens*/ 100,
        /*cached_input_tokens*/ 20,
        /*output_tokens*/ 10,
        /*total_tokens*/ 130,
    )
    .await;
    record_completed_response(
        &mut child_logger,
        "child-call",
        /*uncached_input_tokens*/ 50,
        /*cached_input_tokens*/ 10,
        /*output_tokens*/ 5,
        /*total_tokens*/ 65,
    )
    .await;
    record_completed_response(
        &mut grandchild_logger,
        "grandchild-call",
        /*uncached_input_tokens*/ 1,
        /*cached_input_tokens*/ 0,
        /*output_tokens*/ 1,
        /*total_tokens*/ 2,
    )
    .await;
    record_completed_response(
        &mut forked_logger,
        "forked-call",
        /*uncached_input_tokens*/ 1,
        /*cached_input_tokens*/ 0,
        /*output_tokens*/ 1,
        /*total_tokens*/ 2,
    )
    .await;
    record_completed_response(
        &mut child_logger,
        "end-boundary-call",
        /*uncached_input_tokens*/ 1,
        /*cached_input_tokens*/ 0,
        /*output_tokens*/ 1,
        /*total_tokens*/ 2,
    )
    .await;

    let root_id = root_id.to_string();
    let child_id = child_id.to_string();
    let grandchild_id = grandchild_id.to_string();
    let forked_id = forked_id.to_string();
    set_call_window(
        pool.as_ref(),
        &root_id,
        "root-call",
        "2026-09-30T00:00:00.000Z",
        "2026-09-30T00:00:01Z",
        Some("gpt-6-luna"),
        /*reported_credits*/ None,
    )
    .await?;
    set_call_window(
        pool.as_ref(),
        &child_id,
        "child-call",
        "2026-09-30T00:30:00.000+00:00",
        "2026-09-30T00:30:01Z",
        Some("gpt-6-luna"),
        Some(4.25),
    )
    .await?;
    set_call_window(
        pool.as_ref(),
        &grandchild_id,
        "grandchild-call",
        "2026-09-30T00:40:00Z",
        "2026-09-30T00:40:01Z",
        /*actual_model*/ None,
        /*reported_credits*/ None,
    )
    .await?;
    sqlx::query(
        "UPDATE usage_provider_calls SET input_tokens_uncached = NULL, input_tokens_cached = NULL, input_tokens_cache_write = NULL, output_tokens = NULL, total_tokens = NULL, status = 'provider_usage_missing' WHERE thread_id = ? AND turn_id = 'grandchild-call'",
    )
    .bind(&grandchild_id)
    .execute(pool.as_ref())
    .await?;
    set_call_window(
        pool.as_ref(),
        &forked_id,
        "forked-call",
        "2026-09-30T00:50:00Z",
        "2026-09-30T00:50:01Z",
        /*actual_model*/ None,
        /*reported_credits*/ None,
    )
    .await?;
    set_call_window(
        pool.as_ref(),
        &child_id,
        "end-boundary-call",
        "2026-09-30T01:00:00Z",
        "2026-09-30T01:00:01Z",
        Some("gpt-6-luna"),
        /*reported_credits*/ None,
    )
    .await?;
    drop(pool);
    drop(root_logger);
    drop(child_logger);
    drop(grandchild_logger);
    drop(zero_call_logger);
    drop(forked_logger);
    runtime.close().await;

    let harness =
        codex_utils_cargo_bin::find_resource!("../../scripts/test_codex_usage_report.py")?;
    let python_path = std::env::var_os("PATH").expect("hosted Python must be on PATH");
    let output = std::process::Command::new("python3")
        .arg(&harness)
        .arg("--database")
        .arg(sqlite.usage_db_path())
        .arg("--root-thread-id")
        .arg(&root_id)
        .arg("--child-thread-id")
        .arg(&child_id)
        .arg("--scale")
        .env_clear()
        .env("PATH", python_path)
        .env("PYTHONNOUSERSITE", "1")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("TZ", "UTC")
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "synthetic usage reporting qualification failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let evidence: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(evidence["consumer"]["correctness_status"], "passed");
    assert_eq!(evidence["scale"]["scale_status"], "passed");
    assert_eq!(
        evidence["scale"]["history_queries"]
            .as_array()
            .map(Vec::len),
        Some(3)
    );

    let indexed_writer_seconds =
        provider_writer_batch_seconds(/*indexed_for_reporting*/ true).await?;
    let legacy_writer_seconds =
        provider_writer_batch_seconds(/*indexed_for_reporting*/ false).await?;
    println!(
        "CODEX_USAGE_REAL_WRITER_COMPARISON={}",
        serde_json::json!({
            "rows_per_variant": 250,
            "legacy_index_seconds": legacy_writer_seconds,
            "reporting_indexes_seconds": indexed_writer_seconds,
            "measurement_kind": "actual UsageLogger::record_event batches on separate temporary SQLite databases"
        })
    );
    Ok(())
}
