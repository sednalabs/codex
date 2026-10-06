//! Finite, read-only projection of an explicitly supplied frozen usage snapshot.
//!
//! This is deliberately independent of `StateRuntime`: constructing a runtime
//! can run migrations and initialize telemetry. Callers must supply a snapshot
//! with independently established quiescent/read-only provenance.

use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use crate::control_plane_usage_reader::{parse_optional_time, project_call, validate_request};
use codex_utils_absolute_path::AbsolutePathBuf;
use log::LevelFilter;
use sqlx::ConnectOptions;
use sqlx::Connection;
use sqlx::QueryBuilder;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection};

const READ_BOUND: Duration = Duration::from_secs(5);

/// The only supported database source. It is a caller-selected frozen snapshot,
/// never a discovered profile or live runtime database.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageSnapshotSourceKind {
    ExistingReadOnlySnapshot,
}

/// Caller-supplied source evidence. These flags are assertions from the
/// snapshot custodian, not properties inferred from a filename or SQLite flags.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageSnapshotSourceProvenance {
    pub kind: UsageSnapshotSourceKind,
    pub snapshot_identity: String,
    pub independently_identified: bool,
    pub quiescent: bool,
    pub no_live_writer: bool,
    pub no_pending_wal: bool,
    pub read_only_source: bool,
}

/// Operations actually performed by the projection. These observations do
/// not independently verify the custodian's frozen-source claims.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageSnapshotReadObservation {
    pub sqlite_immutable_read_only: bool,
    pub transaction_committed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageCreditScenario {
    StrictObservedEvidence,
    OperatorStandardRateScenario,
}

/// Explicit selectors for one bounded `[from_started_at, to_started_at)` read.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageSnapshotReadRequest {
    pub snapshot_path: AbsolutePathBuf,
    pub source_namespace: String,
    pub snapshot_identity: String,
    pub expected_snapshot_identity: Option<String>,
    pub source_revision: Option<String>,
    pub schema_evidence: Option<String>,
    pub source_provenance: UsageSnapshotSourceProvenance,
    pub thread_ids: Vec<String>,
    pub call_ids: Vec<String>,
    pub turn_ids: Vec<String>,
    pub from_started_at: DateTime<Utc>,
    pub to_started_at: DateTime<Utc>,
    pub limit: usize,
    pub credit_scenario: Option<UsageCreditScenario>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageSnapshotReadError {
    Unavailable,
    UnsupportedSchema,
    UnsafeSource,
    BoundExceeded,
    IdentityMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageSnapshotCoverage {
    Complete,
    Truncated,
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageSnapshot {
    pub source_snapshot: UsageSnapshotSource,
    pub calls: Vec<UsageCallRow>,
    pub coverage: UsageSnapshotCoverage,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageSnapshotSource {
    pub source_snapshot_id: String,
    pub source_namespace: String,
    pub source_plane: UsageSourcePlane,
    pub source_kind: UsageSnapshotSourceKind,
    /// Caller/custodian assertions, retained as claims rather than verification.
    pub custodian_claims: UsageSnapshotSourceProvenance,
    /// Reader behavior observed during this projection only.
    pub read_observation: UsageSnapshotReadObservation,
    pub source_revision: Option<String>,
    pub schema_evidence: Option<String>,
    pub observed_at: DateTime<Utc>,
    pub transaction_read_started_at: DateTime<Utc>,
    pub transaction_read_ended_at: DateTime<Utc>,
    pub from_started_at: DateTime<Utc>,
    pub to_started_at: DateTime<Utc>,
    pub thread_ids: Vec<String>,
    pub call_ids: Vec<String>,
    pub turn_ids: Vec<String>,
    pub row_limit: usize,
    pub exported_count: usize,
    pub coverage: UsageSnapshotCoverage,
    pub credit_scenario: Option<UsageCreditScenario>,
    pub standard_rate_scenario_requested: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageSourcePlane {
    ExistingUsageLedger,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageCallRow {
    pub call_id: String,
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub spawn_request_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub provider: Option<String>,
    pub response_id: Option<String>,
    pub response_identity: Option<ResponseIdentity>,
    pub response_identity_quarantined: bool,
    pub lineage: Option<UsageLineage>,
    pub requested_model: Option<String>,
    pub observed_model: Option<String>,
    pub requested_service_tier: Option<String>,
    pub actual_service_tier: Option<String>,
    pub actual_service_tier_source: Option<String>,
    pub fast_mode_requested: Option<bool>,
    pub fast_mode_used: Option<bool>,
    pub billing_surface: Option<String>,
    pub account_plan: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub status: Option<String>,
    pub input_tokens_uncached: Option<i64>,
    pub input_tokens_cached: Option<i64>,
    pub input_tokens_cache_write: Option<i64>,
    pub output_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub strict_credit: Option<StrictCreditEstimate>,
    pub standard_rate_scenario: Option<StandardRateScenarioEstimate>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResponseIdentity {
    pub provider: String,
    pub account_scope: ResponseAccountScope,
    pub thread_id: String,
    pub response_id: String,
    pub provider_call_id: String,
}

/// Distinguishes writer's persisted empty account scope from absent/unknown
/// scope; unknown is never treated as unscoped.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum ResponseAccountScope {
    KnownScope(String),
    WriterUnscoped,
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageLineage {
    pub thread_id: String,
    pub parent_thread_id: Option<String>,
    pub root_thread_id: Option<String>,
    pub fork_parent_thread_id: Option<String>,
    pub configured_role: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StrictCreditEstimate {
    pub pricing_status: String,
    pub credit_source: Option<String>,
    pub estimated_total_credits: Option<f64>,
    pub rate_card_estimated_total_credits: Option<f64>,
    pub provider_reported_credits: Option<f64>,
    pub rate_id: Option<String>,
    pub rate_card_kind: Option<String>,
    pub selected_rate_card_kind: Option<String>,
    pub rate_effective_from: Option<String>,
    pub rate_effective_to: Option<String>,
    pub rate_source_observed_at: Option<String>,
    /// Credential-bearing or otherwise unsafe URLs are never returned.
    pub rate_source_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StandardRateScenarioEstimate {
    pub estimate_scenario: String,
    pub assumption_source: String,
    pub assumed_rate_provider: String,
    pub assumed_rate_card_kind: String,
    pub assumed_service_tier: String,
    pub assumed_speed_mode: String,
    pub observed_model: Option<String>,
    pub model_evidence: Option<String>,
    pub scenario_status: String,
    pub estimated_total_credits: Option<f64>,
    pub rate_id: Option<String>,
    pub rate_model: Option<String>,
    pub rate_effective_from: Option<String>,
    pub rate_effective_to: Option<String>,
    pub rate_source_observed_at: Option<String>,
    pub rate_source_url: Option<String>,
    pub model_rate_count: Option<i64>,
    pub matching_rate_count: Option<i64>,
}

/// Read a bounded set of existing call rows from a caller-selected frozen
/// snapshot. No state runtime, profile lookup, migration, writer, or live ledger
/// fallback is involved. Standard-rate assumptions stay a separate scenario.
pub async fn read_control_plane_usage_snapshot(
    request: UsageSnapshotReadRequest,
) -> Result<UsageSnapshot, UsageSnapshotReadError> {
    validate_request(&request)?;

    let query_result = tokio::time::timeout(READ_BOUND, async {
        let options = SqliteConnectOptions::new()
            .filename(request.snapshot_path.as_path())
            .create_if_missing(false)
            .read_only(true)
            .immutable(true)
            .log_statements(LevelFilter::Off);
        let mut connection = SqliteConnection::connect_with(&options)
            .await
            .map_err(|_| UsageSnapshotReadError::Unavailable)?;
        sqlx::query("PRAGMA query_only = ON")
            .execute(&mut connection)
            .await
            .map_err(|_| UsageSnapshotReadError::Unavailable)?;
        let mut transaction = connection
            .begin()
            .await
            .map_err(|_| UsageSnapshotReadError::Unavailable)?;
        let transaction_read_started_at = Utc::now();
        let names = sqlx::query_as::<_, (String, String)>(
            "SELECT name, type FROM sqlite_schema WHERE name IN ('usage_provider_calls', 'usage_threads', 'usage_response_idempotency', 'usage_provider_call_credit_estimates', 'usage_provider_call_standard_rate_estimates')",
        )
        .fetch_all(&mut *transaction)
        .await
        .map_err(|_| UsageSnapshotReadError::Unavailable)?;
        let has = |name: &str, kind: &str| names.iter().any(|(found, found_kind)| found == name && found_kind == kind);
        if !has("usage_provider_calls", "table")
            || !has("usage_threads", "table")
            || !has("usage_response_idempotency", "table")
            || !has("usage_provider_call_credit_estimates", "view")
        {
            return Err(UsageSnapshotReadError::UnsupportedSchema);
        }
        let standard_view_available = has("usage_provider_call_standard_rate_estimates", "view");
        let include_standard_scenario = request.credit_scenario
            == Some(UsageCreditScenario::OperatorStandardRateScenario);
        if include_standard_scenario && !standard_view_available
        {
            return Err(UsageSnapshotReadError::UnsupportedSchema);
        }

        let started = request.from_started_at.to_rfc3339_opts(SecondsFormat::Nanos, true);
        let ended = request.to_started_at.to_rfc3339_opts(SecondsFormat::Nanos, true);
        // Query a small superset for SQLite date precision, then apply the exact
        // half-open interval with chrono before exposing any row.
        let standard_columns = if include_standard_scenario {
            "s.estimate_scenario, s.assumption_source, s.assumed_rate_provider,\
             s.assumed_rate_card_kind, s.assumed_service_tier, s.assumed_speed_mode,\
             s.observed_model AS scenario_observed_model, s.model_evidence,\
             s.scenario_status, s.estimated_total_credits AS scenario_credits,\
             s.rate_id AS scenario_rate_id, s.rate_model,\
             s.rate_effective_from AS scenario_rate_effective_from,\
             s.rate_effective_to AS scenario_rate_effective_to,\
             s.rate_source_observed_at AS scenario_rate_source_observed_at,\
             s.rate_source_url AS scenario_rate_source_url,\
             s.model_rate_count, s.matching_rate_count"
        } else {
            "NULL AS estimate_scenario, NULL AS assumption_source,\
             NULL AS assumed_rate_provider, NULL AS assumed_rate_card_kind,\
             NULL AS assumed_service_tier, NULL AS assumed_speed_mode,\
             NULL AS scenario_observed_model, NULL AS model_evidence,\
             NULL AS scenario_status, NULL AS scenario_credits,\
             NULL AS scenario_rate_id, NULL AS rate_model,\
             NULL AS scenario_rate_effective_from, NULL AS scenario_rate_effective_to,\
             NULL AS scenario_rate_source_observed_at, NULL AS scenario_rate_source_url,\
             NULL AS model_rate_count, NULL AS matching_rate_count"
        };
        let standard_join = if include_standard_scenario {
            "LEFT JOIN usage_provider_call_standard_rate_estimates AS s\
               ON s.provider_call_id = p.provider_call_id"
        } else {
            ""
        };
        // These optional SQL fragments are fixed literals; request data stays bound.
        let mut query = QueryBuilder::<Sqlite>::new(r#"
            SELECT
                p.provider_call_id, p.thread_id, p.turn_id, p.spawn_request_id,
                p.tool_call_id, p.provider, p.request_id,
                i.provider AS identity_provider, i.account_scope AS identity_account_scope,
                i.thread_id AS identity_thread_id, i.response_id AS identity_response_id,
                i.provider_call_id AS identity_call_id,
                t.thread_id AS lineage_thread_id, t.parent_thread_id,
                t.root_thread_id, t.fork_parent_thread_id, t.agent_role,
                p.requested_model, p.actual_model_used, p.requested_service_tier,
                p.actual_service_tier, p.actual_service_tier_source,
                p.fast_mode_requested, p.fast_mode_used, p.billing_surface,
                p.account_plan, p.started_at, p.completed_at, p.status,
                p.input_tokens_uncached, p.input_tokens_cached,
                p.input_tokens_cache_write, p.output_tokens, p.total_tokens,
                c.pricing_status, c.credit_source, c.estimated_total_credits,
                c.rate_card_estimated_total_credits, c.provider_reported_credits,
                c.rate_id, c.rate_card_kind, c.selected_rate_card_kind,
                c.rate_effective_from, c.rate_effective_to, c.rate_source_observed_at,
                c.rate_source_url,
            "#);
        query.push(standard_columns).push(r#"
            FROM usage_provider_calls AS p
            LEFT JOIN usage_response_idempotency AS i
              ON i.provider_call_id = p.provider_call_id
            LEFT JOIN usage_threads AS t ON t.thread_id = p.thread_id
            LEFT JOIN usage_provider_call_credit_estimates AS c
              ON c.provider_call_id = p.provider_call_id
            "#);
        query.push(standard_join).push(r#"
            WHERE julianday(p.started_at) >= julianday(?) - 0.00001
              AND julianday(p.started_at) < julianday(?) + 0.00001
              AND (? = '[]' OR p.thread_id IN (SELECT value FROM json_each(?)))
              AND (? = '[]' OR p.provider_call_id IN (SELECT value FROM json_each(?)))
              AND (? = '[]' OR p.turn_id IN (SELECT value FROM json_each(?)))
            ORDER BY p.provider_call_id
            LIMIT ?
        "#);
        let thread_json = serde_json::to_string(&request.thread_ids)
            .map_err(|_| UsageSnapshotReadError::Unavailable)?;
        let call_json = serde_json::to_string(&request.call_ids)
            .map_err(|_| UsageSnapshotReadError::Unavailable)?;
        let turn_json = serde_json::to_string(&request.turn_ids)
            .map_err(|_| UsageSnapshotReadError::Unavailable)?;
        let raw_rows = query
            .push_bind(started)
            .push_bind(ended)
            .push_bind(thread_json.clone())
            .push_bind(thread_json)
            .push_bind(call_json.clone())
            .push_bind(call_json)
            .push_bind(turn_json.clone())
            .push_bind(turn_json)
            .push_bind((request.limit + 1) as i64)
            .build()
            .fetch_all(&mut *transaction)
            .await
            .map_err(|_| UsageSnapshotReadError::Unavailable)?;
        let truncated = raw_rows.len() > request.limit;
        let rows = raw_rows.into_iter().take(request.limit).collect::<Vec<_>>();
        let mut calls = Vec::with_capacity(rows.len());
        for row in rows {
            let started_at = parse_optional_time(row.try_get("started_at").ok())?;
            if !started_at.is_some_and(|time| time >= request.from_started_at && time < request.to_started_at) {
                continue;
            }
            calls.push(project_call(row, include_standard_scenario)?);
        }
        transaction
            .commit()
            .await
            .map_err(|_| UsageSnapshotReadError::Unavailable)?;
        Ok::<_, UsageSnapshotReadError>((calls, truncated, transaction_read_started_at))
    })
    .await;
    let (calls, truncated, transaction_read_started_at) = match query_result {
        Ok(result) => result?,
        Err(_) => return Err(UsageSnapshotReadError::Unavailable),
    };
    let read_ended = Utc::now();
    let coverage = if truncated {
        UsageSnapshotCoverage::Truncated
    } else {
        UsageSnapshotCoverage::Complete
    };
    let standard_requested = request.credit_scenario
        == Some(UsageCreditScenario::OperatorStandardRateScenario);
    let source_snapshot = UsageSnapshotSource {
        source_snapshot_id: request.snapshot_identity,
        source_namespace: request.source_namespace,
        source_plane: UsageSourcePlane::ExistingUsageLedger,
        source_kind: request.source_provenance.kind,
        custodian_claims: request.source_provenance.clone(),
        read_observation: UsageSnapshotReadObservation {
            sqlite_immutable_read_only: true,
            transaction_committed: true,
        },
        source_revision: request.source_revision,
        schema_evidence: request.schema_evidence,
        observed_at: read_ended,
        transaction_read_started_at,
        transaction_read_ended_at: read_ended,
        from_started_at: request.from_started_at,
        to_started_at: request.to_started_at,
        thread_ids: request.thread_ids,
        call_ids: request.call_ids,
        turn_ids: request.turn_ids,
        row_limit: request.limit,
        exported_count: calls.len(),
        coverage,
        credit_scenario: request.credit_scenario,
        standard_rate_scenario_requested: standard_requested,
    };
    Ok(UsageSnapshot { source_snapshot, calls, coverage })
}

#[cfg(test)]
#[path = "control_plane_usage_tests.rs"]
mod tests;
