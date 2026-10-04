use super::*;
use crate::control_plane_usage_reader::{bounded_metadata, project_account_scope, safe_url};
use crate::control_plane_usage_reader::validate_request;

use pretty_assertions::assert_eq;
use sqlx::ConnectOptions;
use sqlx::sqlite::SqliteConnectOptions;
use std::fs;
use std::path::PathBuf;

struct FrozenFixture {
    directory: PathBuf,
    path: PathBuf,
}

impl FrozenFixture {
    async fn create() -> Self {
        let directory = std::env::temp_dir().join(format!("control-plane-usage-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).expect("create isolated fixture directory");
        let path = directory.join("usage.sqlite");
        let options = SqliteConnectOptions::new().filename(&path).create_if_missing(true)
            .log_statements(LevelFilter::Off);
        let mut connection = SqliteConnection::connect_with(&options).await.expect("create fixture DB");
        for statement in [
            "CREATE TABLE usage_provider_calls (provider_call_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, turn_id TEXT, spawn_request_id TEXT, tool_call_id TEXT, provider TEXT, requested_model TEXT, actual_model_used TEXT, request_id TEXT, requested_service_tier TEXT, actual_service_tier TEXT, actual_service_tier_source TEXT, fast_mode_requested INTEGER, fast_mode_used INTEGER, billing_surface TEXT, account_plan TEXT, started_at TEXT NOT NULL, completed_at TEXT, input_tokens_uncached INTEGER, input_tokens_cached INTEGER, input_tokens_cache_write INTEGER, output_tokens INTEGER, total_tokens INTEGER, provider_reported_credits REAL, status TEXT)",
            "CREATE TABLE usage_threads (thread_id TEXT PRIMARY KEY, parent_thread_id TEXT, root_thread_id TEXT, fork_parent_thread_id TEXT, agent_role TEXT)",
            "CREATE TABLE usage_response_idempotency (provider TEXT NOT NULL, account_scope TEXT NOT NULL, thread_id TEXT NOT NULL, response_id TEXT NOT NULL, provider_call_id TEXT NOT NULL UNIQUE, PRIMARY KEY(provider, account_scope, thread_id, response_id))",
            "CREATE VIEW usage_provider_call_credit_estimates AS SELECT provider_call_id, 'priced_estimate' AS pricing_status, 'rate_card_estimate' AS credit_source, 0.125 AS estimated_total_credits, 0.125 AS rate_card_estimated_total_credits, NULL AS provider_reported_credits, 'rate-1' AS rate_id, 'card-1' AS rate_card_kind, 'card-1' AS selected_rate_card_kind, '2026-01-01T00:00:00Z' AS rate_effective_from, NULL AS rate_effective_to, '2026-01-01T00:00:00Z' AS rate_source_observed_at, 'https://openai.com/pricing' AS rate_source_url FROM usage_provider_calls",
            "CREATE VIEW usage_provider_call_standard_rate_estimates AS SELECT provider_call_id, 'operator_supplied_standard_rate_card_scenario' AS estimate_scenario, 'operator_supplied_credit_rate_guide' AS assumption_source, 'openai' AS assumed_rate_provider, 'codex_token_based' AS assumed_rate_card_kind, 'default' AS assumed_service_tier, 'standard' AS assumed_speed_mode, actual_model_used AS observed_model, 'actual_model_used' AS model_evidence, 'priced_scenario_estimate' AS scenario_status, 0.25 AS estimated_total_credits, 'scenario-rate-1' AS rate_id, 'gpt-6-sol' AS rate_model, '2026-01-01T00:00:00Z' AS rate_effective_from, NULL AS rate_effective_to, '2026-01-01T00:00:00Z' AS rate_source_observed_at, 'https://openai.com/pricing' AS rate_source_url, 1 AS model_rate_count, 1 AS matching_rate_count FROM usage_provider_calls",
        ] {
            sqlx::query(statement).execute(&mut connection).await.expect("seed read-only fixture schema");
        }
        sqlx::query("INSERT INTO usage_threads VALUES ('thread-1', NULL, 'root-1', NULL, 'worker')")
            .execute(&mut connection).await.expect("seed thread lineage");
        sqlx::query("INSERT INTO usage_response_idempotency VALUES ('openai', '', 'thread-1', 'conflicting-response', 'call-1')")
            .execute(&mut connection).await.expect("seed conflicting response identity");
        sqlx::query("INSERT INTO usage_provider_calls VALUES ('call-1', 'thread-1', 'turn-1', NULL, NULL, 'openai', 'requested-model', 'gpt-observed', 'response-1', 'default', 'default', 'provider', 0, 0, 'chatgpt_credits', 'plus', '2026-01-01T00:00:01Z', '2026-01-01T00:00:02Z', 10, 2, NULL, 3, 15, NULL, 'ok')")
            .execute(&mut connection).await.expect("seed usage row with unknown actual model");
        sqlx::query("INSERT INTO usage_provider_calls VALUES ('call-at-end', 'thread-1', 'turn-1', NULL, NULL, 'openai', 'requested-model', 'model', 'response-2', 'default', 'default', 'provider', 0, 0, 'chatgpt_credits', 'plus', '2026-01-01T00:00:10Z', NULL, 1, 0, 0, 1, 2, NULL, 'ok')")
            .execute(&mut connection).await.expect("seed exclusive upper-bound row");
        connection.close().await.expect("close fixture writer");
        let mut file_permissions = fs::metadata(&path).expect("fixture file metadata").permissions();
        file_permissions.set_readonly(true);
        fs::set_permissions(&path, file_permissions).expect("freeze fixture file");
        let mut directory_permissions = fs::metadata(&directory).expect("fixture dir metadata").permissions();
        directory_permissions.set_readonly(true);
        fs::set_permissions(&directory, directory_permissions).expect("freeze fixture directory");
        Self { directory, path }
    }

    fn request(&self, scenario: Option<UsageCreditScenario>) -> UsageSnapshotReadRequest {
        let snapshot_identity = "fixture-usage-v1".to_string();
        UsageSnapshotReadRequest {
            snapshot_path: AbsolutePathBuf::from_absolute_path_checked(&self.path).expect("absolute fixture path"),
            source_namespace: "fixture-session".to_string(),
            snapshot_identity: snapshot_identity.clone(),
            expected_snapshot_identity: Some(snapshot_identity.clone()),
            source_revision: Some("fixture-revision-1".to_string()),
            schema_evidence: Some("usage fixture schema v1".to_string()),
            source_provenance: UsageSnapshotSourceProvenance {
                kind: UsageSnapshotSourceKind::ExistingReadOnlySnapshot,
                snapshot_identity,
                independently_identified: true,
                quiescent: true,
                no_live_writer: true,
                no_pending_wal: true,
                read_only_source: true,
            },
            thread_ids: vec!["thread-1".to_string()],
            call_ids: Vec::new(),
            turn_ids: Vec::new(),
            from_started_at: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z").unwrap().with_timezone(&Utc),
            to_started_at: DateTime::parse_from_rfc3339("2026-01-01T00:00:10Z").unwrap().with_timezone(&Utc),
            limit: 32,
            credit_scenario: scenario,
        }
    }
}

impl Drop for FrozenFixture {
    fn drop(&mut self) {
        if let Ok(metadata) = fs::metadata(&self.directory) {
            let mut permissions = metadata.permissions();
            permissions.set_readonly(false);
            let _ = fs::set_permissions(&self.directory, permissions);
        }
        if let Ok(metadata) = fs::metadata(&self.path) {
            let mut permissions = metadata.permissions();
            permissions.set_readonly(false);
            let _ = fs::set_permissions(&self.path, permissions);
        }
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[tokio::test]
async fn frozen_read_only_projection_preserves_nullable_usage_and_separate_scenarios() {
    let fixture = FrozenFixture::create().await;
    let before = fs::read(&fixture.path).expect("read fixture bytes");
    let snapshot = read_control_plane_usage_snapshot(fixture.request(Some(
        UsageCreditScenario::OperatorStandardRateScenario,
    )))
    .await
    .expect("read selected frozen source");

    assert_eq!(snapshot.coverage, UsageSnapshotCoverage::Complete);
    assert_eq!(snapshot.source_snapshot.custodian_claims, fixture.request(None).source_provenance);
    assert_eq!(snapshot.source_snapshot.read_observation, UsageSnapshotReadObservation {
        sqlite_immutable_read_only: true,
        transaction_committed: true,
    });
    assert_eq!(snapshot.calls, vec![UsageCallRow {
        call_id: "call-1".to_string(),
        thread_id: "thread-1".to_string(),
        turn_id: Some("turn-1".to_string()),
        spawn_request_id: None,
        tool_call_id: None,
        provider: Some("openai".to_string()),
        response_id: Some("response-1".to_string()),
        response_identity: None,
        response_identity_quarantined: true,
        lineage: Some(UsageLineage {
            thread_id: "thread-1".to_string(),
            parent_thread_id: None,
            root_thread_id: Some("root-1".to_string()),
            fork_parent_thread_id: None,
            configured_role: Some("worker".to_string()),
        }),
        requested_model: Some("requested-model".to_string()),
        observed_model: Some("gpt-observed".to_string()),
        requested_service_tier: Some("default".to_string()),
        actual_service_tier: Some("default".to_string()),
        actual_service_tier_source: Some("provider".to_string()),
        fast_mode_requested: Some(false),
        fast_mode_used: Some(false),
        billing_surface: Some("chatgpt_credits".to_string()),
        account_plan: Some("plus".to_string()),
        started_at: Some(DateTime::parse_from_rfc3339("2026-01-01T00:00:01Z").unwrap().with_timezone(&Utc)),
        completed_at: Some(DateTime::parse_from_rfc3339("2026-01-01T00:00:02Z").unwrap().with_timezone(&Utc)),
        status: Some("ok".to_string()),
        input_tokens_uncached: Some(10),
        input_tokens_cached: Some(2),
        input_tokens_cache_write: None,
        output_tokens: Some(3),
        total_tokens: Some(15),
        strict_credit: Some(StrictCreditEstimate {
            pricing_status: "priced_estimate".to_string(),
            credit_source: Some("rate_card_estimate".to_string()),
            estimated_total_credits: Some(0.125),
            rate_card_estimated_total_credits: Some(0.125),
            provider_reported_credits: None,
            rate_id: Some("rate-1".to_string()),
            rate_card_kind: Some("card-1".to_string()),
            selected_rate_card_kind: Some("card-1".to_string()),
            rate_effective_from: Some("2026-01-01T00:00:00Z".to_string()),
            rate_effective_to: None,
            rate_source_observed_at: Some("2026-01-01T00:00:00Z".to_string()),
            rate_source_url: Some("https://openai.com/pricing".to_string()),
        }),
        standard_rate_scenario: Some(StandardRateScenarioEstimate {
            estimate_scenario: "operator_supplied_standard_rate_card_scenario".to_string(),
            assumption_source: "operator_supplied_credit_rate_guide".to_string(),
            assumed_rate_provider: "openai".to_string(),
            assumed_rate_card_kind: "codex_token_based".to_string(),
            assumed_service_tier: "default".to_string(),
            assumed_speed_mode: "standard".to_string(),
            observed_model: Some("gpt-observed".to_string()),
            model_evidence: Some("actual_model_used".to_string()),
            scenario_status: "priced_scenario_estimate".to_string(),
            estimated_total_credits: Some(0.25),
            rate_id: Some("scenario-rate-1".to_string()),
            rate_model: Some("gpt-6-sol".to_string()),
            rate_effective_from: Some("2026-01-01T00:00:00Z".to_string()),
            rate_effective_to: None,
            rate_source_observed_at: Some("2026-01-01T00:00:00Z".to_string()),
            rate_source_url: Some("https://openai.com/pricing".to_string()),
            model_rate_count: Some(1),
            matching_rate_count: Some(1),
        }),
    }]);

    assert_eq!(fs::read(&fixture.path).expect("read fixture bytes after projection"), before);
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(!fixture.path.with_extension(format!("sqlite{suffix}")).exists());
    }
}

#[test]
fn snapshot_provenance_mismatch_and_empty_selectors_fail_before_open() {
    let mut request = UsageSnapshotReadRequest {
        snapshot_path: AbsolutePathBuf::from_absolute_path_checked("/not-opened.sqlite").unwrap(),
        source_namespace: "n".to_string(),
        snapshot_identity: "expected".to_string(),
        expected_snapshot_identity: Some("different".to_string()),
        source_revision: None,
        schema_evidence: None,
        source_provenance: UsageSnapshotSourceProvenance {
            kind: UsageSnapshotSourceKind::ExistingReadOnlySnapshot,
            snapshot_identity: "expected".to_string(),
            independently_identified: true,
            quiescent: true,
            no_live_writer: true,
            no_pending_wal: true,
            read_only_source: true,
        },
        thread_ids: vec!["thread".to_string()],
        call_ids: Vec::new(),
        turn_ids: Vec::new(),
        from_started_at: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z").unwrap().with_timezone(&Utc),
        to_started_at: DateTime::parse_from_rfc3339("2026-01-02T00:00:00Z").unwrap().with_timezone(&Utc),
        limit: 1,
        credit_scenario: None,
    };
    assert_eq!(validate_request(&request), Err(UsageSnapshotReadError::IdentityMismatch));
    request.expected_snapshot_identity = None;
    request.thread_ids.clear();
    assert_eq!(validate_request(&request), Err(UsageSnapshotReadError::BoundExceeded));
}

#[test]
fn rate_metadata_is_bounded_and_unknown_when_over_limit_or_control_bearing() {
    assert_eq!(bounded_metadata(Some("2026-01-01T00:00:00Z".to_string())), Some("2026-01-01T00:00:00Z".to_string()));
    assert_eq!(bounded_metadata(Some("x".repeat(257))), None);
    assert_eq!(bounded_metadata(Some("bad\nvalue".to_string())), None);
    assert_eq!(bounded_metadata(None), None);
}

#[test]
fn only_allowlisted_static_public_https_rate_urls_are_exposed() {
    assert_eq!(safe_url(Some("https://openai.com/pricing".to_string())), Some("https://openai.com/pricing".to_string()));
    for unsafe_url in [
        "https://openai.com/pricing?api_key=private-credential-canary",
        "https://user:password@openai.com/pricing",
        "https://127.0.0.1/private",
        "https://openai%2ecom/pricing",
        "https://openai.com/pricing#credential-canary",
        "https://openai.com/private-credential-canary",
    ] {
        assert_eq!(safe_url(Some(unsafe_url.to_string())), None, "{unsafe_url}");
    }
}

#[test]
fn account_scope_keeps_writer_unscoped_distinct_from_unknown() {
    assert_eq!(project_account_scope(Some("account-1".to_string())), ResponseAccountScope::KnownScope("account-1".to_string()));
    assert_eq!(project_account_scope(Some(String::new())), ResponseAccountScope::WriterUnscoped);
    assert_eq!(project_account_scope(None), ResponseAccountScope::Unknown);
    assert_eq!(project_account_scope(Some("bad\nvalue".to_string())), ResponseAccountScope::Unknown);
}
