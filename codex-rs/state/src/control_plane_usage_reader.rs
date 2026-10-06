//! Bounded SQL projection implementation, separated from its public DTOs.

use chrono::{DateTime, Utc};
use sqlx::Row;

use crate::control_plane_usage::{
    ResponseIdentity, StandardRateScenarioEstimate, StrictCreditEstimate, UsageCallRow,
    ResponseAccountScope, UsageLineage, UsageSnapshotReadError, UsageSnapshotReadRequest,
};

const MAX_USAGE_ROWS: usize = 2048;
const MAX_SELECTOR_BYTES: usize = 256;
const MAX_NAMESPACE_BYTES: usize = 256;

pub(crate) fn validate_request(request: &UsageSnapshotReadRequest) -> Result<(), UsageSnapshotReadError> {
    let provenance = &request.source_provenance;
    if !provenance.independently_identified
        || !provenance.quiescent
        || !provenance.no_live_writer
        || !provenance.no_pending_wal
        || !provenance.read_only_source
    {
        return Err(UsageSnapshotReadError::UnsafeSource);
    }
    if provenance.snapshot_identity.is_empty()
        || request.snapshot_identity != provenance.snapshot_identity
        || request
            .expected_snapshot_identity
            .as_ref()
            .is_some_and(|expected| expected != &provenance.snapshot_identity)
    {
        return Err(UsageSnapshotReadError::IdentityMismatch);
    }
    if request.source_namespace.is_empty()
        || request.source_namespace.len() > MAX_NAMESPACE_BYTES
        || request.snapshot_identity.is_empty()
        || request.snapshot_identity.len() > MAX_SELECTOR_BYTES
        || request.thread_ids.is_empty() && request.call_ids.is_empty()
        || request.limit == 0
        || request.limit > MAX_USAGE_ROWS
        || request.from_started_at >= request.to_started_at
    {
        return Err(UsageSnapshotReadError::BoundExceeded);
    }
    let selector_count = request.thread_ids.len() + request.call_ids.len() + request.turn_ids.len();
    if selector_count == 0 || selector_count > MAX_USAGE_ROWS {
        return Err(UsageSnapshotReadError::BoundExceeded);
    }
    for selector in request.thread_ids.iter().chain(&request.call_ids).chain(&request.turn_ids) {
        if selector.is_empty() || selector.len() > MAX_SELECTOR_BYTES {
            return Err(UsageSnapshotReadError::BoundExceeded);
        }
    }
    for value in request.source_revision.iter().chain(&request.schema_evidence) {
        if value.len() > MAX_SELECTOR_BYTES {
            return Err(UsageSnapshotReadError::BoundExceeded);
        }
    }
    if request
        .expected_snapshot_identity
        .as_ref()
        .is_some_and(|value| value.len() > MAX_SELECTOR_BYTES)
    {
        return Err(UsageSnapshotReadError::BoundExceeded);
    }
    Ok(())
}

pub(crate) fn parse_optional_time(value: Option<String>) -> Result<Option<DateTime<Utc>>, UsageSnapshotReadError> {
    value
        .map(|text| {
            DateTime::parse_from_rfc3339(&text)
                .map(|time| time.with_timezone(&Utc))
                .map_err(|_| UsageSnapshotReadError::UnsupportedSchema)
        })
        .transpose()
}

pub(crate) fn bounded_metadata(value: Option<String>) -> Option<String> {
    value.filter(|value| {
        value.len() <= MAX_SELECTOR_BYTES && !value.chars().any(char::is_control)
    })
}

pub(crate) fn safe_url(value: Option<String>) -> Option<String> {
    value.filter(|url| {
        if url.len() > MAX_SELECTOR_BYTES
            || !url.is_ascii()
            || url.chars().any(char::is_control)
            || url.chars().any(|character| matches!(character, '?' | '#' | '%' | '\\'))
        {
            return false;
        }
        let Some(rest) = url.strip_prefix("https://") else {
            return false;
        };
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        let public_host = matches!(host, "openai.com" | "platform.openai.com" | "github.com");
        let static_path = path.is_empty()
            || path.split('/').all(|segment| {
                !segment.is_empty()
                    && segment.len() <= 32
                    && segment.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
                    && !segment.contains("..")
                    && !["token", "key", "secret", "credential", "auth"]
                        .iter()
                        .any(|sensitive| segment.to_ascii_lowercase().contains(sensitive))
            });
        public_host && static_path && !host.contains('@') && !host.contains(':')
    })
}

pub(crate) fn project_call(row: sqlx::sqlite::SqliteRow, include_standard_scenario: bool) -> Result<UsageCallRow, UsageSnapshotReadError> {
    let call_id: String = row.try_get("provider_call_id").map_err(|_| UsageSnapshotReadError::UnsupportedSchema)?;
    let thread_id: String = row.try_get("thread_id").map_err(|_| UsageSnapshotReadError::UnsupportedSchema)?;
    if !valid_identifier(&call_id) || !valid_identifier(&thread_id) {
        return Err(UsageSnapshotReadError::BoundExceeded);
    }
    let response_id: Option<String> = row
        .try_get::<Option<String>, _>("request_id")
        .map_err(|_| UsageSnapshotReadError::UnsupportedSchema)?
        .and_then(bounded_identifier);
    let provider: Option<String> = row
        .try_get::<Option<String>, _>("provider")
        .map_err(|_| UsageSnapshotReadError::UnsupportedSchema)?
        .and_then(bounded_identifier);
    let identity = read_response_identity(&row, &call_id, &thread_id, provider.as_deref(), response_id.as_deref())?;
    let lineage_thread: Option<String> = row.try_get("lineage_thread_id").map_err(|_| UsageSnapshotReadError::UnsupportedSchema)?;
    let lineage = lineage_thread.filter(|value| valid_identifier(value)).map(|thread_id| UsageLineage {
        thread_id,
        parent_thread_id: row.try_get("parent_thread_id").ok().flatten().and_then(bounded_identifier),
        root_thread_id: row.try_get("root_thread_id").ok().flatten().and_then(bounded_identifier),
        fork_parent_thread_id: row.try_get("fork_parent_thread_id").ok().flatten().and_then(bounded_identifier),
        configured_role: row.try_get("agent_role").ok().flatten().and_then(bounded_identifier),
    });
    let strict_credit = if row.try_get::<Option<String>, _>("pricing_status").ok().flatten().is_some() {
        Some(StrictCreditEstimate {
            pricing_status: required_metadata(&row, "pricing_status")?,
            credit_source: row.try_get("credit_source").ok().flatten().and_then(bounded_identifier),
            estimated_total_credits: finite(row.try_get("estimated_total_credits").ok().flatten()),
            rate_card_estimated_total_credits: finite(row.try_get("rate_card_estimated_total_credits").ok().flatten()),
            provider_reported_credits: finite(row.try_get("provider_reported_credits").ok().flatten()),
            rate_id: row.try_get("rate_id").ok().flatten().and_then(bounded_identifier),
            rate_card_kind: row.try_get("rate_card_kind").ok().flatten().and_then(bounded_identifier),
            selected_rate_card_kind: row.try_get("selected_rate_card_kind").ok().flatten().and_then(bounded_identifier),
            rate_effective_from: bounded_metadata(row.try_get("rate_effective_from").ok().flatten()),
            rate_effective_to: bounded_metadata(row.try_get("rate_effective_to").ok().flatten()),
            rate_source_observed_at: bounded_metadata(row.try_get("rate_source_observed_at").ok().flatten()),
            rate_source_url: safe_url(row.try_get("rate_source_url").ok().flatten()),
        })
    } else { None };
    let standard_rate_scenario = if include_standard_scenario && row.try_get::<Option<String>, _>("estimate_scenario").ok().flatten().is_some() {
        Some(StandardRateScenarioEstimate {
            estimate_scenario: required_metadata(&row, "estimate_scenario")?,
            assumption_source: required_metadata(&row, "assumption_source")?,
            assumed_rate_provider: required_metadata(&row, "assumed_rate_provider")?,
            assumed_rate_card_kind: required_metadata(&row, "assumed_rate_card_kind")?,
            assumed_service_tier: required_metadata(&row, "assumed_service_tier")?,
            assumed_speed_mode: required_metadata(&row, "assumed_speed_mode")?,
            observed_model: row.try_get("scenario_observed_model").ok().flatten().and_then(bounded_identifier),
            model_evidence: row.try_get("model_evidence").ok().flatten().and_then(bounded_identifier),
            scenario_status: required_metadata(&row, "scenario_status")?,
            estimated_total_credits: finite(row.try_get("scenario_credits").ok().flatten()),
            rate_id: row.try_get("scenario_rate_id").ok().flatten().and_then(bounded_identifier),
            rate_model: row.try_get("rate_model").ok().flatten().and_then(bounded_identifier),
            rate_effective_from: bounded_metadata(row.try_get("scenario_rate_effective_from").ok().flatten()),
            rate_effective_to: bounded_metadata(row.try_get("scenario_rate_effective_to").ok().flatten()),
            rate_source_observed_at: bounded_metadata(row.try_get("scenario_rate_source_observed_at").ok().flatten()),
            rate_source_url: safe_url(row.try_get("scenario_rate_source_url").ok().flatten()),
            model_rate_count: row.try_get("model_rate_count").ok().flatten(),
            matching_rate_count: row.try_get("matching_rate_count").ok().flatten(),
        })
    } else { None };
    Ok(UsageCallRow {
        call_id,
        thread_id,
        turn_id: row.try_get("turn_id").ok().flatten().and_then(bounded_identifier),
        spawn_request_id: row.try_get("spawn_request_id").ok().flatten().and_then(bounded_identifier),
        tool_call_id: row.try_get("tool_call_id").ok().flatten().and_then(bounded_identifier),
        provider,
        response_id,
        response_identity: identity.0,
        response_identity_quarantined: identity.1,
        lineage,
        requested_model: row.try_get("requested_model").ok().flatten().and_then(bounded_identifier),
        observed_model: row.try_get("actual_model_used").ok().flatten().and_then(bounded_identifier),
        requested_service_tier: row.try_get("requested_service_tier").ok().flatten().and_then(bounded_identifier),
        actual_service_tier: row.try_get("actual_service_tier").ok().flatten().and_then(bounded_identifier),
        actual_service_tier_source: row.try_get("actual_service_tier_source").ok().flatten().and_then(bounded_identifier),
        fast_mode_requested: row.try_get::<Option<i64>, _>("fast_mode_requested").ok().flatten().map(|value| value == 1),
        fast_mode_used: row.try_get::<Option<i64>, _>("fast_mode_used").ok().flatten().map(|value| value == 1),
        billing_surface: row.try_get("billing_surface").ok().flatten().and_then(bounded_identifier),
        account_plan: row.try_get("account_plan").ok().flatten().and_then(bounded_identifier),
        started_at: parse_optional_time(row.try_get("started_at").ok())?,
        completed_at: parse_optional_time(row.try_get("completed_at").ok())?,
        status: row.try_get("status").ok().flatten().and_then(bounded_identifier),
        input_tokens_uncached: row.try_get("input_tokens_uncached").ok().flatten(),
        input_tokens_cached: row.try_get("input_tokens_cached").ok().flatten(),
        input_tokens_cache_write: row.try_get("input_tokens_cache_write").ok().flatten(),
        output_tokens: row.try_get("output_tokens").ok().flatten(),
        total_tokens: row.try_get("total_tokens").ok().flatten(),
        strict_credit,
        standard_rate_scenario,
    })
}

fn read_response_identity(
    row: &sqlx::sqlite::SqliteRow,
    call_id: &str,
    thread_id: &str,
    provider: Option<&str>,
    response_id: Option<&str>,
) -> Result<(Option<ResponseIdentity>, bool), UsageSnapshotReadError> {
    let identity_call_id: Option<String> = row.try_get("identity_call_id").map_err(|_| UsageSnapshotReadError::UnsupportedSchema)?;
    if identity_call_id.is_none() {
        return Ok((None, false));
    }
    let identity_provider: Option<String> = row.try_get("identity_provider").ok().flatten().and_then(bounded_identifier);
    let identity_thread_id: Option<String> = row.try_get("identity_thread_id").ok().flatten().and_then(bounded_identifier);
    let identity_response_id: Option<String> = row.try_get("identity_response_id").ok().flatten().and_then(bounded_identifier);
    let conflict = identity_call_id.as_deref() != Some(call_id)
        || identity_provider.as_deref() != provider
        || identity_thread_id.as_deref() != Some(thread_id)
        || identity_response_id.as_deref() != response_id;
    if conflict {
        return Ok((None, true));
    }
    let provider = identity_provider.ok_or(UsageSnapshotReadError::UnsupportedSchema)?;
    let thread_id = identity_thread_id.ok_or(UsageSnapshotReadError::UnsupportedSchema)?;
    let response_id = identity_response_id.ok_or(UsageSnapshotReadError::UnsupportedSchema)?;
    let raw_account_scope: Option<String> = row.try_get("identity_account_scope").map_err(|_| UsageSnapshotReadError::UnsupportedSchema)?;
    Ok((Some(ResponseIdentity {
        provider,
        account_scope: project_account_scope(raw_account_scope),
        thread_id,
        response_id,
        provider_call_id: call_id.to_string(),
    }), false))
}

fn finite(value: Option<f64>) -> Option<f64> {
    value.filter(|value| value.is_finite())
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_SELECTOR_BYTES && !value.chars().any(char::is_control)
}

fn bounded_identifier(value: String) -> Option<String> {
    valid_identifier(&value).then_some(value)
}

pub(crate) fn project_account_scope(value: Option<String>) -> ResponseAccountScope {
    match value {
        Some(value) if value.is_empty() => ResponseAccountScope::WriterUnscoped,
        Some(value) if valid_identifier(&value) => ResponseAccountScope::KnownScope(value),
        _ => ResponseAccountScope::Unknown,
    }
}

fn required_metadata(row: &sqlx::sqlite::SqliteRow, name: &str) -> Result<String, UsageSnapshotReadError> {
    let value: String = row.try_get(name).map_err(|_| UsageSnapshotReadError::UnsupportedSchema)?;
    bounded_identifier(value).ok_or(UsageSnapshotReadError::UnsupportedSchema)
}
