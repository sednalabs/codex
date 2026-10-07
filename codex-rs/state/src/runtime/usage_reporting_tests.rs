use super::StateRuntime;
use super::usage::UsageLogger;
use crate::migrations::USAGE_MIGRATOR;
use crate::migrations::runtime_usage_migrator;
use crate::migrations::validate_usage_reporting_indexes;
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
use sqlx::migrate::Migration;
use sqlx::migrate::Migrator;
use sqlx::SqlitePool;
use std::borrow::Cow;
use tempfile::tempdir;

static T10_USAGE_HISTORY_MIGRATOR: Migrator =
    sqlx::migrate!("./test_data/usage_migrations_t10");

type UsageMigrationRow = (i64, String, bool, Vec<u8>);

fn test_usage_migrator(
    migrations: Vec<Migration>,
    base: &Migrator,
    ignore_missing: bool,
) -> Migrator {
    Migrator {
        migrations: Cow::Owned(migrations),
        ignore_missing,
        locking: base.locking,
        no_tx: base.no_tx,
        table_name: base.table_name.clone(),
        create_schemas: base.create_schemas.clone(),
    }
}

fn current_main_usage_history_migrator() -> Migrator {
    test_usage_migrator(
        USAGE_MIGRATOR
            .iter()
            .filter(|migration| migration.version != 20)
            .cloned()
            .collect(),
        &USAGE_MIGRATOR,
        /*ignore_missing*/ false,
    )
}

fn t10_usage_history_migrator() -> Migrator {
    let mut migrations = USAGE_MIGRATOR
        .iter()
        .filter(|migration| {
            !matches!(migration.version, 1 | 5 | 6 | 8..=12 | 15 | 20)
        })
        .cloned()
        .collect::<Vec<_>>();
    migrations.extend(T10_USAGE_HISTORY_MIGRATOR.iter().cloned());
    migrations.sort_by_key(|migration| migration.version);
    test_usage_migrator(migrations, &USAGE_MIGRATOR, /*ignore_missing*/ false)
}

async fn usage_migration_rows(pool: &SqlitePool) -> Result<Vec<UsageMigrationRow>> {
    Ok(sqlx::query_as::<_, UsageMigrationRow>(
        "SELECT version, description, success, checksum FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(pool)
    .await?)
}

async fn assert_preserved_migration_rows(
    pool: &SqlitePool,
    before: &[UsageMigrationRow],
) -> Result<()> {
    let after = usage_migration_rows(pool).await?;
    let preserved = after
        .into_iter()
        .filter(|row| before.iter().any(|prior| prior.0 == row.0))
        .collect::<Vec<_>>();
    assert_eq!(preserved, before);
    Ok(())
}

async fn assert_table_column(pool: &SqlitePool, table: &str, column: &str) -> Result<()> {
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM pragma_table_info(?) WHERE name = ?",
    )
    .bind(table)
    .bind(column)
    .fetch_one(pool)
    .await?;
    assert_eq!(count, 1, "missing {table}.{column}");
    Ok(())
}

async fn assert_schema_object(pool: &SqlitePool, kind: &str, name: &str) -> Result<()> {
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = ? AND name = ?",
    )
    .bind(kind)
    .bind(name)
    .fetch_one(pool)
    .await?;
    assert_eq!(count, 1, "missing {kind} {name}");
    Ok(())
}

async fn assert_t10_pending_current_main_migrations(pool: &SqlitePool) -> Result<()> {
    let rows = usage_migration_rows(pool).await?;
    for version in [6, 8, 9, 10, 11, 12] {
        assert!(
            rows.iter()
                .any(|row| row.0 == version && row.2),
            "current-main usage migration {version} was not successfully applied"
        );
    }

    for column in ["lineage_edge_kind", "spawn_request_id"] {
        assert_table_column(pool, "usage_threads", column).await?;
    }
    for table in [
        "usage_automatic_turn_chains",
        "usage_automatic_turn_triggers",
        "usage_automatic_turn_eligibility",
        "usage_automatic_turns",
    ] {
        assert_schema_object(pool, "table", table).await?;
    }
    for (table, column) in [
        ("usage_automatic_turn_triggers", "event_occurrence_id"),
        ("usage_automatic_turn_eligibility", "event_occurrence_id"),
        ("usage_automatic_turn_eligibility", "connection_principal"),
        (
            "usage_automatic_turn_eligibility",
            "admitted_client_user_message_id",
        ),
        ("usage_automatic_turn_eligibility", "admitted_operation_kind"),
        ("usage_automatic_turn_eligibility", "admitted_expected_turn_id"),
        ("usage_automatic_turn_eligibility", "allowed_operation_kind"),
        ("usage_automatic_turn_eligibility", "allowed_expected_turn_id"),
        (
            "usage_automatic_turn_eligibility",
            "trigger_context_fingerprint",
        ),
        ("usage_automatic_turn_eligibility", "settings_generation"),
        ("usage_automatic_turn_eligibility", "auth_generation"),
        ("usage_automatic_turn_chains", "settings_generation"),
        ("usage_automatic_turns", "event_occurrence_id"),
        ("usage_automatic_turns", "connection_principal"),
        ("usage_automatic_turns", "abort_event_occurrence_id"),
    ] {
        assert_table_column(pool, table, column).await?;
    }
    for index in [
        "usage_automatic_turns_thread_turn_idx",
        "usage_automatic_turns_trigger_idx",
        "usage_automatic_turns_origin_idx",
        "usage_automatic_turn_triggers_occurrence_idx",
        "usage_automatic_turn_triggers_thread_idx",
        "usage_automatic_turn_eligibility_admission_idx",
    ] {
        assert_schema_object(pool, "index", index).await?;
    }
    Ok(())
}

async fn seed_t10_usage_history(sqlite: &SqliteConfig) -> Result<SqlitePool> {
    let pool = sqlite
        .open_read_write_pool(&sqlite.usage_db_path())
        .await?;
    t10_usage_history_migrator().run(&pool).await?;
    Ok(pool)
}

async fn assert_usage_version_absent(pool: &SqlitePool, version: i64) -> Result<()> {
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM _sqlx_migrations WHERE version = ?",
    )
    .bind(version)
    .fetch_one(pool)
    .await?;
    assert_eq!(count, 0, "unexpected usage migration version {version}");
    Ok(())
}

async fn qualify_usage_migration_histories() -> Result<()> {
    let fresh_home = tempdir()?;
    let fresh_sqlite = SqliteConfig::new_for_testing(fresh_home.path().abs());
    let fresh = fresh_sqlite
        .open_usage_db(&runtime_usage_migrator(), None)
        .await?;
    validate_usage_reporting_indexes(&fresh, /*require_all*/ true).await?;
    let fresh_rows = usage_migration_rows(&fresh).await?;
    assert!(fresh_rows.iter().any(|row| row.0 == 15 && row.2));
    assert!(fresh_rows.iter().any(|row| row.0 == 20 && row.2));
    let fresh_v15 = fresh_rows
        .iter()
        .find(|row| row.0 == 15)
        .expect("fresh usage history includes main version 15")
        .clone();
    fresh.close().await;
    let fresh_reopened = fresh_sqlite
        .open_usage_db(&runtime_usage_migrator(), None)
        .await?;
    validate_usage_reporting_indexes(&fresh_reopened, /*require_all*/ true).await?;
    assert_eq!(
        usage_migration_rows(&fresh_reopened)
            .await?
            .into_iter()
            .find(|row| row.0 == 15),
        Some(fresh_v15)
    );
    fresh_reopened.close().await;

    let main_home = tempdir()?;
    let main_sqlite = SqliteConfig::new_for_testing(main_home.path().abs());
    let main_fixture = main_sqlite
        .open_read_write_pool(&main_sqlite.usage_db_path())
        .await?;
    current_main_usage_history_migrator()
        .run(&main_fixture)
        .await?;
    let main_before = usage_migration_rows(&main_fixture).await?;
    let main_v15 = main_before
        .iter()
        .find(|row| row.0 == 15)
        .expect("indexed main fixture includes version 15")
        .clone();
    validate_usage_reporting_indexes(&main_fixture, /*require_all*/ true).await?;
    main_fixture.close().await;
    let main_upgraded = main_sqlite
        .open_usage_db(&runtime_usage_migrator(), None)
        .await?;
    assert_preserved_migration_rows(&main_upgraded, &main_before).await?;
    assert_eq!(
        usage_migration_rows(&main_upgraded)
            .await?
            .into_iter()
            .find(|row| row.0 == 15),
        Some(main_v15.clone())
    );
    validate_usage_reporting_indexes(&main_upgraded, /*require_all*/ true).await?;
    main_upgraded.close().await;
    let main_reopened = main_sqlite
        .open_usage_db(&runtime_usage_migrator(), None)
        .await?;
    assert_eq!(
        usage_migration_rows(&main_reopened)
            .await?
            .into_iter()
            .find(|row| row.0 == 15),
        Some(main_v15)
    );
    validate_usage_reporting_indexes(&main_reopened, /*require_all*/ true).await?;
    main_reopened.close().await;

    let t10_home = tempdir()?;
    let t10_sqlite = SqliteConfig::new_for_testing(t10_home.path().abs());
    let t10_fixture = seed_t10_usage_history(&t10_sqlite).await?;
    let t10_before = usage_migration_rows(&t10_fixture).await?;
    let t10_view_before = sqlx::query_scalar::<_, String>(
        "SELECT sql FROM sqlite_master WHERE type = 'view' AND name = 'usage_provider_call_credit_estimates'",
    )
    .fetch_one(&t10_fixture)
    .await?;
    t10_fixture.close().await;
    let t10_upgraded = t10_sqlite
        .open_usage_db(&runtime_usage_migrator(), None)
        .await?;
    assert_preserved_migration_rows(&t10_upgraded, &t10_before).await?;
    assert_t10_pending_current_main_migrations(&t10_upgraded).await?;
    validate_usage_reporting_indexes(&t10_upgraded, /*require_all*/ true).await?;
    let t10_view_after = sqlx::query_scalar::<_, String>(
        "SELECT sql FROM sqlite_master WHERE type = 'view' AND name = 'usage_provider_call_credit_estimates'",
    )
    .fetch_one(&t10_upgraded)
    .await?;
    assert_eq!(t10_view_after, t10_view_before);

    let thread_id = "00000000-0000-4000-8000-000000000010";
    sqlx::query(
        "INSERT INTO usage_threads (thread_id, root_thread_id, source) VALUES (?, ?, 'user')",
    )
    .bind(thread_id)
    .bind(thread_id)
    .execute(&t10_upgraded)
    .await?;
    sqlx::query(
        "INSERT INTO usage_provider_calls (provider_call_id, thread_id, provider, requested_model, actual_model_used, actual_service_tier, actual_service_tier_source, fast_mode_used, billing_surface, account_plan, started_at, completed_at, input_tokens_uncached, input_tokens_cached, input_tokens_cache_write, output_tokens, total_tokens, provider_reported_credits, status) VALUES ('t10-status-call', ?, 'openai', 'gpt-6-sol', 'gpt-6-luna', 'default', 'runtime_contract', 0, 'chatgpt_credits', 'plus', '2026-10-01T00:30:00Z', '2026-10-01T00:30:01Z', 0, 0, 0, 0, 0, NULL, 'provider_usage_missing')",
    )
    .bind(thread_id)
    .execute(&t10_upgraded)
    .await?;
    let legacy_estimate = sqlx::query_as::<_, (String, Option<f64>)>(
        "SELECT pricing_status, rate_card_estimated_total_credits FROM usage_provider_call_credit_estimates WHERE provider_call_id = 't10-status-call'",
    )
    .fetch_one(&t10_upgraded)
    .await?;
    assert_eq!(legacy_estimate, ("provider_usage_missing".to_string(), None));
    t10_upgraded.close().await;

    let t10_reopened = t10_sqlite
        .open_usage_db(&runtime_usage_migrator(), None)
        .await?;
    assert_preserved_migration_rows(&t10_reopened, &t10_before).await?;
    assert_t10_pending_current_main_migrations(&t10_reopened).await?;
    validate_usage_reporting_indexes(&t10_reopened, /*require_all*/ true).await?;
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT sql FROM sqlite_master WHERE type = 'view' AND name = 'usage_provider_call_credit_estimates'",
        )
        .fetch_one(&t10_reopened)
        .await?,
        t10_view_before
    );
    t10_reopened.close().await;

    let report = codex_utils_cargo_bin::find_resource!("../../scripts/codex_usage_report.py")?;
    let python_path = std::env::var_os("PATH").expect("hosted Python must be on PATH");
    let report_output = std::process::Command::new("python3")
        .arg(&report)
        .arg("--database")
        .arg(t10_sqlite.usage_db_path())
        .arg("--start-utc")
        .arg("2026-10-01T00:00:00Z")
        .arg("--end-utc")
        .arg("2026-10-02T00:00:00Z")
        .arg("--timezone")
        .arg("UTC")
        .arg("--scope")
        .arg("all")
        .arg("--credit-mode")
        .arg("observed_or_effective_rate")
        .env_clear()
        .env("PATH", python_path)
        .env("PYTHONNOUSERSITE", "1")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("TZ", "UTC")
        .output()?;
    anyhow::ensure!(
        report_output.status.success(),
        "T10 usage reporter failed: stdout={} stderr={}",
        String::from_utf8_lossy(&report_output.stdout),
        String::from_utf8_lossy(&report_output.stderr)
    );
    let report_json: serde_json::Value = serde_json::from_slice(&report_output.stdout)?;
    assert_eq!(report_json["status"], "incomplete");
    assert_eq!(report_json["summary"]["provider_call_count"], 1);
    assert_eq!(report_json["summary"]["actual_mode"]["uncovered_call_count"], 1);
    assert_eq!(
        report_json["summary"]["actual_mode"]["uncovered_reasons"]["provider_usage_missing"],
        1
    );

    for corrupt_version in [1, 19] {
        let corrupt_home = tempdir()?;
        let corrupt_sqlite = SqliteConfig::new_for_testing(corrupt_home.path().abs());
        let corrupt_fixture = seed_t10_usage_history(&corrupt_sqlite).await?;
        sqlx::query("UPDATE _sqlx_migrations SET checksum = zeroblob(length(checksum)) WHERE version = ?")
            .bind(corrupt_version)
            .execute(&corrupt_fixture)
            .await?;
        let corrupt_rows = usage_migration_rows(&corrupt_fixture).await?;
        corrupt_fixture.close().await;
        assert!(
            corrupt_sqlite
                .open_usage_db(&runtime_usage_migrator(), None)
                .await
                .is_err(),
            "conflicting T10 migration {corrupt_version} must fail before migration effects"
        );
        let readback = corrupt_sqlite
            .open_read_only_pool(&corrupt_sqlite.usage_db_path())
            .await?;
        assert_eq!(usage_migration_rows(&readback).await?, corrupt_rows);
        assert_usage_version_absent(&readback, 20).await?;
        assert_usage_version_absent(&readback, 6).await?;
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM pragma_table_info('usage_threads') WHERE name = 'lineage_edge_kind'",
            )
            .fetch_one(&readback)
            .await?,
            0
        );
        readback.close().await;
    }

    let incomplete_home = tempdir()?;
    let incomplete_sqlite = SqliteConfig::new_for_testing(incomplete_home.path().abs());
    let incomplete_fixture = seed_t10_usage_history(&incomplete_sqlite).await?;
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 15")
        .execute(&incomplete_fixture)
        .await?;
    let incomplete_rows = usage_migration_rows(&incomplete_fixture).await?;
    incomplete_fixture.close().await;
    assert!(
        incomplete_sqlite
            .open_usage_db(&runtime_usage_migrator(), None)
            .await
            .is_err(),
        "T10 v16-v19 history without its exact v15 variant must fail closed"
    );
    let incomplete_readback = incomplete_sqlite
        .open_read_only_pool(&incomplete_sqlite.usage_db_path())
        .await?;
    assert_eq!(
        usage_migration_rows(&incomplete_readback).await?,
        incomplete_rows
    );
    assert_usage_version_absent(&incomplete_readback, 20).await?;
    assert_usage_version_absent(&incomplete_readback, 6).await?;
    incomplete_readback.close().await;

    let index_home = tempdir()?;
    let index_sqlite = SqliteConfig::new_for_testing(index_home.path().abs());
    let index_fixture = seed_t10_usage_history(&index_sqlite).await?;
    let index_before = usage_migration_rows(&index_fixture).await?;
    sqlx::query(
        "CREATE INDEX usage_provider_calls_started_at_idx ON usage_provider_calls(thread_id)",
    )
    .execute(&index_fixture)
    .await?;
    index_fixture.close().await;
    assert!(
        index_sqlite
            .open_usage_db(&runtime_usage_migrator(), None)
            .await
            .is_err(),
        "an unexpected same-name reporting index must fail before migration effects"
    );
    let index_readback = index_sqlite
        .open_read_only_pool(&index_sqlite.usage_db_path())
        .await?;
    assert_eq!(usage_migration_rows(&index_readback).await?, index_before);
    assert_usage_version_absent(&index_readback, 20).await?;
    assert_usage_version_absent(&index_readback, 6).await?;
    index_readback.close().await;

    let future_home = tempdir()?;
    let future_sqlite = SqliteConfig::new_for_testing(future_home.path().abs());
    let future_fixture = future_sqlite
        .open_read_write_pool(&future_sqlite.usage_db_path())
        .await?;
    current_main_usage_history_migrator()
        .run(&future_fixture)
        .await?;
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, installed_on, success, checksum, execution_time) VALUES (21, 'future usage migration marker', CURRENT_TIMESTAMP, TRUE, zeroblob(48), 0)",
    )
    .execute(&future_fixture)
    .await?;
    let future_before = usage_migration_rows(&future_fixture)
        .await?
        .into_iter()
        .find(|row| row.0 == 21)
        .expect("source-absent future migration row was seeded");
    future_fixture.close().await;
    let future_upgraded = future_sqlite
        .open_usage_db(&runtime_usage_migrator(), None)
        .await?;
    assert_eq!(
        usage_migration_rows(&future_upgraded)
            .await?
            .into_iter()
            .find(|row| row.0 == 21),
        Some(future_before.clone())
    );
    validate_usage_reporting_indexes(&future_upgraded, /*require_all*/ true).await?;
    future_upgraded.close().await;
    let future_reopened = future_sqlite
        .open_usage_db(&runtime_usage_migrator(), None)
        .await?;
    assert_eq!(
        usage_migration_rows(&future_reopened)
            .await?
            .into_iter()
            .find(|row| row.0 == 21),
        Some(future_before)
    );
    validate_usage_reporting_indexes(&future_reopened, /*require_all*/ true).await?;
    future_reopened.close().await;

    Ok(())
}

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
    qualify_usage_migration_histories().await?;

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
