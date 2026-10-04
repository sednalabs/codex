//! Explicit frozen-snapshot request, state read, exact-key join and safe output.

use chrono::{DateTime, SecondsFormat, Utc};
use codex_diagnostics::control_plane::RecorderSnapshot;
use codex_diagnostics::control_plane::{
    DiagnosticCallReference, UsageAccountScope, UsageJoinCall, UsageJoinKey, UsageJoinScope,
    join_control_plane_usage,
};
use codex_state::{
    UsageCreditScenario, UsageSnapshotReadError, UsageSnapshotReadRequest,
    UsageSnapshotSourceKind, UsageSnapshotSourceProvenance, read_control_plane_usage_snapshot,
};
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Deserialize;
use serde_json::json;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

const MAX_REQUEST_BYTES: u64 = 65_536;
const MAX_SELECTORS: usize = 2_048;
const MAX_ID_BYTES: usize = 256;

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct UsageRequestFile {
    snapshot_path: String,
    source_namespace: String,
    snapshot_identity: String,
    expected_snapshot_identity: Option<String>,
    source_revision: Option<String>,
    schema_evidence: Option<String>,
    independently_identified: bool,
    quiescent: bool,
    no_live_writer: bool,
    no_pending_wal: bool,
    read_only_source: bool,
    thread_ids: Vec<String>,
    call_ids: Vec<String>,
    turn_ids: Vec<String>,
    from_started_at: String,
    to_started_at: String,
    limit: usize,
    credit_scenario: Option<CreditScenarioInput>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CreditScenarioInput {
    StrictObservedEvidence,
    OperatorStandardRateScenario,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UsageInputError {
    RequestUnavailable,
    InvalidRequest,
    IdentityMismatch,
    SnapshotUnavailable,
    UnsupportedSchema,
    BoundExceeded,
}

pub(super) async fn run(
    request_path: &PathBuf,
    diagnostic_namespace: &str,
    diagnostic_snapshot: Option<&RecorderSnapshot>,
) -> serde_json::Value {
    let file = match read_request(request_path) {
        Ok(file) => file,
        Err(error) => return json!({"requested": true, "status": format!("{error:?}"), "join": null}),
    };
    let claims = claims_from(&file);
    if ![
        claims.independently_identified,
        claims.quiescent,
        claims.no_live_writer,
        claims.no_pending_wal,
        claims.read_only_source,
    ].into_iter().all(|asserted| asserted) {
        return json!({
            "requested": true,
            "status": "UnsafeSource",
            "callerClaims": claims_json(&claims),
            "readerObservation": null,
            "join": null
        });
    }
    let (read_request, claims) = match build_state_request(file, diagnostic_namespace) {
        Ok(value) => value,
        Err(error) => return json!({
            "requested": true,
            "status": format!("{error:?}"),
            "callerClaims": claims_json(&claims),
            "readerObservation": null,
            "join": null
        }),
    };
    match read_control_plane_usage_snapshot(read_request).await {
            Ok(snapshot) => project_snapshot(snapshot, diagnostic_snapshot),
            Err(error) => json!({
                "requested": true,
                "status": usage_error(error),
                "callerClaims": claims_json(&claims),
                "readerObservation": null,
                "join": null
            }),
    }
}

fn read_request(path: &PathBuf) -> Result<UsageRequestFile, UsageInputError> {
    let metadata = std::fs::metadata(path).map_err(|_| UsageInputError::RequestUnavailable)?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_REQUEST_BYTES {
        return Err(UsageInputError::BoundExceeded);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)
        .map_err(|_| UsageInputError::RequestUnavailable)?
        .take(MAX_REQUEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| UsageInputError::RequestUnavailable)?;
    if bytes.len() as u64 > MAX_REQUEST_BYTES {
        return Err(UsageInputError::BoundExceeded);
    }
    let request = serde_json::from_slice(&bytes).map_err(|_| UsageInputError::InvalidRequest);
    bytes.fill(0);
    request
}

fn build_state_request(
    file: UsageRequestFile,
    diagnostic_namespace: &str,
) -> Result<(UsageSnapshotReadRequest, UsageSnapshotSourceProvenance), UsageInputError> {
    if file.source_namespace != diagnostic_namespace {
        return Err(UsageInputError::IdentityMismatch);
    }
    let selector_count = file.thread_ids.len() + file.call_ids.len() + file.turn_ids.len();
    if selector_count > MAX_SELECTORS
        || file.thread_ids.is_empty() && file.call_ids.is_empty()
        || file.limit == 0
        || file.limit > MAX_SELECTORS
        || file.snapshot_identity.is_empty()
        || file.snapshot_identity.len() > MAX_ID_BYTES
        || file.source_namespace.is_empty()
        || file.source_namespace.len() > MAX_ID_BYTES
        || file.source_namespace.chars().any(char::is_control)
        || [&file.thread_ids, &file.call_ids, &file.turn_ids]
            .iter()
            .flat_map(|values| values.iter())
            .any(|value| value.is_empty() || value.len() > MAX_ID_BYTES)
    {
        return Err(UsageInputError::BoundExceeded);
    }
    let start = parse_time(&file.from_started_at)?;
    let end = parse_time(&file.to_started_at)?;
    if start >= end {
        return Err(UsageInputError::BoundExceeded);
    }
    let snapshot_path = AbsolutePathBuf::try_from(PathBuf::from(file.snapshot_path))
        .map_err(|_| UsageInputError::InvalidRequest)?;
    let claims = claims_from(&file);
    let request = UsageSnapshotReadRequest {
        snapshot_path,
        source_namespace: file.source_namespace,
        snapshot_identity: file.snapshot_identity,
        expected_snapshot_identity: file.expected_snapshot_identity,
        source_revision: file.source_revision,
        schema_evidence: file.schema_evidence,
        source_provenance: claims.clone(),
        thread_ids: file.thread_ids,
        call_ids: file.call_ids,
        turn_ids: file.turn_ids,
        from_started_at: start,
        to_started_at: end,
        limit: file.limit,
        credit_scenario: file.credit_scenario.map(|value| match value {
            CreditScenarioInput::StrictObservedEvidence => UsageCreditScenario::StrictObservedEvidence,
            CreditScenarioInput::OperatorStandardRateScenario => UsageCreditScenario::OperatorStandardRateScenario,
        }),
    };
    Ok((request, claims))
}

fn claims_from(file: &UsageRequestFile) -> UsageSnapshotSourceProvenance {
    UsageSnapshotSourceProvenance {
        kind: UsageSnapshotSourceKind::ExistingReadOnlySnapshot,
        snapshot_identity: file.snapshot_identity.clone(),
        independently_identified: file.independently_identified,
        quiescent: file.quiescent,
        no_live_writer: file.no_live_writer,
        no_pending_wal: file.no_pending_wal,
        read_only_source: file.read_only_source,
    }
}

fn parse_time(value: &str) -> Result<DateTime<Utc>, UsageInputError> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| UsageInputError::InvalidRequest)
}

fn project_snapshot(
    snapshot: codex_state::UsageSnapshot,
    diagnostic_snapshot: Option<&RecorderSnapshot>,
) -> serde_json::Value {
    let source = &snapshot.source_snapshot;
    let calls = snapshot.calls.iter().map(|row| {
        let strict = row.strict_credit.as_ref();
        let scenario = row.standard_rate_scenario.as_ref();
        let verified_response = (!row.response_identity_quarantined)
            .then_some(row.response_identity.as_ref())
            .flatten();
        UsageJoinCall {
            source_snapshot_id: source.source_snapshot_id.clone(),
            started_at: row.started_at.map_or_else(String::new, |time| {
                time.to_rfc3339_opts(SecondsFormat::Nanos, true)
            }),
            key: UsageJoinKey {
                source_namespace: source.source_namespace.clone(),
                thread_id: row.thread_id.clone(),
                turn_id: row.turn_id.clone(),
                provider: row.provider.clone().or_else(|| {
                    verified_response.map(|identity| identity.provider.clone())
                }),
                account_scope: verified_response.map_or(UsageAccountScope::Unknown, |identity| {
                    match &identity.account_scope {
                        codex_state::ResponseAccountScope::KnownScope(value) => {
                            UsageAccountScope::KnownScope(value.clone())
                        }
                        codex_state::ResponseAccountScope::WriterUnscoped => {
                            UsageAccountScope::WriterUnscoped
                        }
                        codex_state::ResponseAccountScope::Unknown => UsageAccountScope::Unknown,
                    }
                }),
                call_id: Some(row.call_id.clone()),
                response_id: verified_response.map(|identity| identity.response_id.clone()),
            },
            identity_quarantined: row.response_identity_quarantined,
            root_thread_id: row.lineage.as_ref().and_then(|lineage| lineage.root_thread_id.clone()),
            configured_role: row.lineage.as_ref().and_then(|lineage| lineage.configured_role.clone()),
            observed_model: row.observed_model.clone(),
            input_tokens_uncached: row.input_tokens_uncached,
            input_tokens_cached: row.input_tokens_cached,
            input_tokens_cache_write: row.input_tokens_cache_write,
            output_tokens: row.output_tokens,
            total_tokens: row.total_tokens,
            strict_pricing_status: strict.map(|value| value.pricing_status.clone()),
            strict_credit_source: strict.and_then(|value| value.credit_source.clone()),
            strict_estimated_credits: strict.and_then(|value| value.estimated_total_credits),
            provider_reported_credits: strict.and_then(|value| value.provider_reported_credits),
            standard_scenario_status: scenario.map(|value| value.scenario_status.clone()),
            standard_scenario_credits: scenario.and_then(|value| value.estimated_total_credits),
        }
    }).collect::<Vec<_>>();
    let (capture_id, high_water, refs, diagnostic_complete) =
        diagnostic_references(diagnostic_snapshot);
    let scope = UsageJoinScope {
        usage_snapshot_id: source.source_snapshot_id.clone(),
        usage_source_namespace: source.source_namespace.clone(),
        usage_from_started_at: source.from_started_at.to_rfc3339_opts(SecondsFormat::Nanos, true),
        usage_to_started_at: source.to_started_at.to_rfc3339_opts(SecondsFormat::Nanos, true),
        usage_snapshot_complete: snapshot.coverage == codex_state::UsageSnapshotCoverage::Complete,
        diagnostic_capture_instance_id: capture_id,
        diagnostic_high_water: high_water,
        diagnostic_snapshot_complete: diagnostic_complete,
    };
    let join = join_control_plane_usage(&scope, &calls, &refs);
    json!({
        "requested": true,
        "status": "readComplete",
        "source": {
            "snapshotIdentity": source.source_snapshot_id,
            "sourceNamespace": source.source_namespace,
            "sourcePlane": format!("{:?}", source.source_plane),
            "sourceKind": format!("{:?}", source.source_kind),
            "sourceRevision": source.source_revision,
            "schemaEvidence": source.schema_evidence,
            "observedAt": source.observed_at.to_rfc3339(),
            "transactionReadStartedAt": source.transaction_read_started_at.to_rfc3339(),
            "transactionReadEndedAt": source.transaction_read_ended_at.to_rfc3339(),
            "window": {"fromInclusive": source.from_started_at.to_rfc3339(), "toExclusive": source.to_started_at.to_rfc3339()},
            "selectors": {"threadIds": source.thread_ids, "callIds": source.call_ids, "turnIds": source.turn_ids},
            "rowLimit": source.row_limit,
            "exportedCount": source.exported_count,
            "coverage": format!("{:?}", source.coverage),
            "creditScenario": source.credit_scenario.map(|scenario| format!("{:?}", scenario)),
            "standardRateScenarioRequested": source.standard_rate_scenario_requested
        },
        "callerClaims": claims_json(&source.custodian_claims),
        "readerObservation": {
            "sqliteImmutableReadOnly": source.read_observation.sqlite_immutable_read_only,
            "transactionCommitted": source.read_observation.transaction_committed
        },
        "join": join_json(&join)
    })
}

fn diagnostic_references(
    snapshot: Option<&RecorderSnapshot>,
) -> (String, u64, Vec<DiagnosticCallReference>, bool) {
    let Some(snapshot) = snapshot else { return (String::new(), 0, Vec::new(), false) };
    // external_operation.call_id is a host-tool envelope ID, not the provider
    // usage ledger's provider_call_id. The two identifier domains must never
    // be equated, even when the strings and caller namespace happen to match.
    // The current CLI input contract exposes no provider-call key, so exact
    // diagnostic-to-ledger references are unknown regardless of capture loss.
    (snapshot.capture_instance_id.clone(), snapshot.high_water, Vec::new(), false)
}

fn claims_json(claims: &UsageSnapshotSourceProvenance) -> serde_json::Value {
    json!({
        "kind": format!("{:?}", claims.kind),
        "snapshotIdentity": claims.snapshot_identity,
        "independentlyIdentified": claims.independently_identified,
        "quiescent": claims.quiescent,
        "noLiveWriter": claims.no_live_writer,
        "noPendingWal": claims.no_pending_wal,
        "readOnlySource": claims.read_only_source,
        "claimStatus": "callerCustodianAssertionNotIndependentlyVerified"
    })
}

fn usage_error(error: UsageSnapshotReadError) -> &'static str {
    match error {
        UsageSnapshotReadError::Unavailable => "SnapshotUnavailable",
        UsageSnapshotReadError::UnsupportedSchema => "UnsupportedSchema",
        UsageSnapshotReadError::UnsafeSource => "UnsafeSource",
        UsageSnapshotReadError::BoundExceeded => "BoundExceeded",
        UsageSnapshotReadError::IdentityMismatch => "IdentityMismatch",
    }
}

fn join_json(summary: &codex_diagnostics::control_plane::UsageJoinSummary) -> serde_json::Value {
    json!({
        "complete": summary.complete,
        "associationCoverage": "unknownProviderCallKeyNotExposedByExternalHostEnvelope",
        "inputCallRows": summary.input_call_rows,
        "callRowsTruncated": summary.call_rows_truncated,
        "diagnosticReferencesTruncated": summary.diagnostic_references_truncated,
        "invalidUsageCallRows": summary.invalid_usage_call_rows,
        "outOfScopeDiagnosticReferences": summary.out_of_scope_diagnostic_references,
        "distinctCallCount": summary.distinct_call_count,
        "duplicateCallRows": summary.duplicate_call_rows,
        "conflictingCallIds": summary.conflicting_call_ids,
        "conflictingIdentityEventCount": summary.conflicting_identity_event_count,
        "unqualifiedResponseEventCount": summary.unqualified_response_event_count,
        "associatedCallCount": summary.associated_call_count,
        "unmatchedCallCount": summary.unmatched_call_count,
        "partitions": summary.partitions.iter().map(|(key, value)| json!({
            "rootThreadId": key.root_thread_id,
            "configuredRole": key.configured_role,
            "observedModel": key.observed_model,
            "callCount": value.call_count,
            "usagePresentCallCount": value.usage_present_call_count,
            "strictCoveredCallCount": value.strict_covered_call_count,
            "strictUnpricedOrMissingCallCount": value.strict_unpriced_or_missing_call_count,
            "providerReportedCreditMicros": value.provider_reported_credit_micros.0,
            "strictEstimateCreditMicros": value.strict_rate_estimate_credit_micros.0,
            "standardScenarioCoveredCallCount": value.standard_scenario_covered_call_count,
            "standardScenarioUnpricedOrMissingCallCount": value.standard_scenario_unpriced_or_missing_call_count,
            "standardScenarioCreditMicros": value.standard_scenario_credit_micros.0,
            "inputTokensUncached": {"sum": value.input_tokens_uncached_sum, "rows": value.input_tokens_uncached_rows},
            "inputTokensCached": {"sum": value.input_tokens_cached_sum, "rows": value.input_tokens_cached_rows},
            "inputTokensCacheWrite": {"sum": value.input_tokens_cache_write_sum, "rows": value.input_tokens_cache_write_rows},
            "outputTokens": {"sum": value.output_tokens_sum, "rows": value.output_tokens_rows},
            "totalTokens": {"sum": value.total_tokens_sum, "rows": value.total_tokens_rows}
        })).collect::<Vec<_>>(),
        "associations": summary.associations.iter().map(|row| json!({
            "callId": row.call_id,
            "diagnosticEvents": row.diagnostic_events.iter().map(|(plane, identity)| json!({
                "sourcePlane": format!("{:?}", plane),
                "identity": {"captureInstanceId": identity.capture_instance_id, "sequence": identity.sequence}
            })).collect::<Vec<_>>(),
            "unmatched": row.unmatched,
            "conflictingUsageIdentity": row.conflicting_usage_identity,
            "conflictingIdentityEventCount": row.conflicting_identity_event_count,
            "unqualifiedResponseEventCount": row.unqualified_response_event_count,
            "associationTruncated": row.association_truncated,
            "causalWakeClaim": false
        })).collect::<Vec<_>>()
    })
}

#[cfg(test)]
#[path = "usage_tests.rs"]
mod tests;
