use super::*;
use pretty_assertions::assert_eq;
use codex_diagnostics::control_plane::{
    UsageAccountScope, UsageJoinCall, UsageJoinKey, UsageJoinScope, join_control_plane_usage,
};

fn request() -> UsageRequestFile {
    UsageRequestFile {
        snapshot_path: "/tmp/selected-usage.sqlite".to_owned(),
        source_namespace: "selected-source".to_owned(),
        snapshot_identity: "snapshot-1".to_owned(),
        expected_snapshot_identity: Some("snapshot-1".to_owned()),
        source_revision: Some("fixture-revision".to_owned()),
        schema_evidence: Some("fixture-schema".to_owned()),
        independently_identified: true,
        quiescent: true,
        no_live_writer: true,
        no_pending_wal: true,
        read_only_source: true,
        thread_ids: vec!["thread-1".to_owned()],
        call_ids: vec!["call-1".to_owned()],
        turn_ids: vec!["turn-1".to_owned()],
        from_started_at: "2026-10-05T01:00:00Z".to_owned(),
        to_started_at: "2026-10-05T02:00:00Z".to_owned(),
        limit: 32,
        credit_scenario: Some(CreditScenarioInput::StrictObservedEvidence),
    }
}

#[test]
fn request_binds_exact_source_namespace_and_half_open_window() {
    let (request, claims) = build_state_request(request(), "selected-source").unwrap();
    assert_eq!(request.source_namespace, "selected-source");
    assert_eq!(request.snapshot_identity, "snapshot-1");
    assert_eq!(request.thread_ids, ["thread-1"]);
    assert_eq!(request.call_ids, ["call-1"]);
    assert!(claims.independently_identified && claims.quiescent && claims.no_live_writer);
    assert!(claims.no_pending_wal && claims.read_only_source);
}

#[test]
fn request_rejects_a_namespace_that_does_not_match_diagnostic_capture() {
    let error = build_state_request(request(), "other-source").unwrap_err();
    assert_eq!(error, UsageInputError::IdentityMismatch);
}

#[test]
fn false_custodian_claim_is_not_relabelled_as_reader_verification() {
    let mut request = request();
    request.no_pending_wal = false;
    let claims = claims_from(&request);
    assert!(!claims.no_pending_wal);
    assert_eq!(claims_json(&claims)["claimStatus"], "callerCustodianAssertionNotIndependentlyVerified");
}

#[test]
fn equal_host_tool_and_provider_call_ids_do_not_create_an_association() {
    let input = r#"{"timestamp":"2026-10-05T01:00:00Z","type":"response_item","payload":{"type":"function_call","name":"wait_agent","call_id":"same-id","arguments":"{\"targets\":[\"actor/path\"],\"return_when\":\"any\"}"}}
{"timestamp":"2026-10-05T01:00:01Z","type":"response_item","payload":{"type":"function_call_output","call_id":"same-id","output":"{\"timed_out\":false,\"reason\":\"target_terminal\",\"wake_cause\":\"target_status\",\"target_status\":{\"actor/path\":\"completed\"}}"}}"#;
    let (observations, _) = super::super::envelopes::parse(input.as_bytes(), "same-namespace");
    assert_eq!(observations.len(), 1);
    let recorder = codex_diagnostics::control_plane::ControlPlaneRecorder::new(
        codex_diagnostics::control_plane::CaptureMode::Session,
        "fixture-capture",
    )
    .unwrap();
    for observation in observations {
        let event = super::super::to_event(observation).unwrap();
        assert_eq!(event.external_operation.as_ref().unwrap().call_id, "same-id");
        recorder.record(event).unwrap();
    }
    let snapshot = recorder.snapshot().unwrap();
    assert_eq!(snapshot.events.len(), 1);
    let reduced = codex_diagnostics::control_plane::Summary::reduce_snapshot(&snapshot);
    assert!(super::super::summary::project(&reduced)["providerCallTimelines"]
        .as_array()
        .unwrap()
        .is_empty());
    let (capture, high_water, refs, diagnostic_complete) =
        diagnostic_references(Some(&snapshot));
    assert_eq!(refs.len(), 0);
    assert!(!diagnostic_complete);

    let scope = UsageJoinScope {
        usage_snapshot_id: "usage-snapshot".to_owned(),
        usage_source_namespace: "same-namespace".to_owned(),
        usage_from_started_at: "2026-10-05T00:00:00.000000000Z".to_owned(),
        usage_to_started_at: "2026-10-05T02:00:00.000000000Z".to_owned(),
        usage_snapshot_complete: true,
        diagnostic_capture_instance_id: capture,
        diagnostic_high_water: high_water,
        diagnostic_snapshot_complete: diagnostic_complete,
    };
    let call = UsageJoinCall {
        source_snapshot_id: "usage-snapshot".to_owned(),
        started_at: "2026-10-05T01:00:00.000000000Z".to_owned(),
        key: UsageJoinKey {
            source_namespace: "same-namespace".to_owned(),
            thread_id: "provider-thread".to_owned(),
            turn_id: Some("provider-turn".to_owned()),
            provider: None,
            account_scope: UsageAccountScope::Unknown,
            call_id: Some("same-id".to_owned()),
            response_id: None,
        },
        identity_quarantined: false,
        root_thread_id: None,
        configured_role: None,
        observed_model: None,
        input_tokens_uncached: None,
        input_tokens_cached: None,
        input_tokens_cache_write: None,
        output_tokens: None,
        total_tokens: None,
        strict_pricing_status: None,
        strict_credit_source: None,
        strict_estimated_credits: None,
        provider_reported_credits: None,
        standard_scenario_status: None,
        standard_scenario_credits: None,
    };
    let joined = join_control_plane_usage(&scope, &[call], &refs);
    assert_eq!(joined.associated_call_count, 0);
    assert_eq!(joined.unmatched_call_count, 1);
}

// Standard hosted Unix runners provide Python's SQLite fixture writer. It is
// used only before the explicitly selected synthetic source is frozen; the
// production consumer uses the real immutable/query-only state reader.
#[cfg(unix)]
struct FrozenUsageFixture {
    root: tempfile::TempDir,
    database: std::path::PathBuf,
    directory: std::path::PathBuf,
}

#[cfg(unix)]
impl FrozenUsageFixture {
    fn create(identity: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("frozen");
        std::fs::create_dir(&directory).unwrap();
        let database = directory.join("usage.sqlite");
        let status = std::process::Command::new("python3").arg("-c").arg(r#"
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
c.executescript('''
CREATE TABLE usage_provider_calls (provider_call_id TEXT PRIMARY KEY, thread_id TEXT, turn_id TEXT, spawn_request_id TEXT, tool_call_id TEXT, provider TEXT, requested_model TEXT, actual_model_used TEXT, request_id TEXT, requested_service_tier TEXT, actual_service_tier TEXT, actual_service_tier_source TEXT, fast_mode_requested INTEGER, fast_mode_used INTEGER, billing_surface TEXT, account_plan TEXT, started_at TEXT, completed_at TEXT, input_tokens_uncached INTEGER, input_tokens_cached INTEGER, input_tokens_cache_write INTEGER, output_tokens INTEGER, total_tokens INTEGER, status TEXT);
CREATE TABLE usage_threads (thread_id TEXT PRIMARY KEY, parent_thread_id TEXT, root_thread_id TEXT, fork_parent_thread_id TEXT, agent_role TEXT);
CREATE TABLE usage_response_idempotency (provider TEXT, account_scope TEXT, thread_id TEXT, response_id TEXT, provider_call_id TEXT);
CREATE VIEW usage_provider_call_credit_estimates AS SELECT provider_call_id,
 CASE WHEN actual_model_used IS NULL THEN 'provider_usage_missing' ELSE 'priced_estimate' END AS pricing_status,
 CASE WHEN actual_model_used IS NULL THEN NULL ELSE 'rate_card_estimate' END AS credit_source,
 CASE WHEN actual_model_used IS NULL THEN NULL ELSE 0.125 END AS estimated_total_credits,
 CASE WHEN actual_model_used IS NULL THEN NULL ELSE 0.125 END AS rate_card_estimated_total_credits,
 NULL AS provider_reported_credits, 'rate-1' AS rate_id, 'card-1' AS rate_card_kind, 'card-1' AS selected_rate_card_kind,
 '2026-01-01T00:00:00Z' AS rate_effective_from, NULL AS rate_effective_to, '2026-01-01T00:00:00Z' AS rate_source_observed_at,
 'https://openai.com/pricing' AS rate_source_url FROM usage_provider_calls;
CREATE VIEW usage_provider_call_standard_rate_estimates AS SELECT provider_call_id,
 'operator_supplied_standard_rate_card_scenario' AS estimate_scenario, 'operator_supplied_credit_rate_guide' AS assumption_source,
 'provider' AS assumed_rate_provider, 'codex_token_based' AS assumed_rate_card_kind, 'default' AS assumed_service_tier,
 'standard' AS assumed_speed_mode, actual_model_used AS observed_model, 'actual_model_used' AS model_evidence,
 CASE WHEN actual_model_used IS NULL THEN 'provider_usage_missing' ELSE 'priced_scenario_estimate' END AS scenario_status,
 CASE WHEN actual_model_used IS NULL THEN NULL ELSE 0.25 END AS estimated_total_credits,
 'scenario-rate' AS rate_id, 'observed-model' AS rate_model, '2026-01-01T00:00:00Z' AS rate_effective_from,
 NULL AS rate_effective_to, '2026-01-01T00:00:00Z' AS rate_source_observed_at, 'https://openai.com/pricing' AS rate_source_url,
 1 AS model_rate_count, 1 AS matching_rate_count FROM usage_provider_calls;
INSERT INTO usage_threads VALUES ('thread-1', NULL, 'root-1', NULL, 'worker');
INSERT INTO usage_provider_calls VALUES ('selected-call', 'thread-1', 'turn-1', NULL, NULL, 'provider', 'REQUESTED_MODEL_CANARY', 'observed-model', 'response-1', 'default', 'default', 'provider', 0, 0, 'chatgpt_credits', 'plus', '2026-10-05T01:00:00Z', '2026-10-05T01:00:01Z', 5, 3, NULL, 2, 10, 'ok');
INSERT INTO usage_provider_calls VALUES ('unknown-call', 'thread-unknown', NULL, NULL, NULL, 'provider', 'REQUESTED_MODEL_CANARY', NULL, 'response-unknown', NULL, NULL, NULL, NULL, NULL, NULL, NULL, '2026-10-05T01:00:00Z', NULL, NULL, NULL, NULL, NULL, NULL, 'ok');
''')
c.execute('INSERT INTO usage_response_idempotency VALUES (?, ?, ?, ?, ?)',
 ('provider', '', 'thread-1', sys.argv[2], 'selected-call'))
c.commit()
c.close()
"#).arg(&database).arg(identity).status().unwrap();
        assert!(status.success(), "hosted fixture creation failed");
        std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o444)).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o555)).unwrap();
        Self { root, database, directory }
    }

    fn request_json(&self) -> serde_json::Value {
        serde_json::json!({"snapshotPath": self.database, "sourceNamespace": "fixture-source",
            "snapshotIdentity": "fixture-usage", "expectedSnapshotIdentity": "fixture-usage",
            "sourceRevision": "fixture-revision", "schemaEvidence": "fixture-schema",
            "independentlyIdentified": true, "quiescent": true, "noLiveWriter": true, "noPendingWal": true,
            "readOnlySource": true, "threadIds": ["thread-1", "thread-unknown"], "callIds": [], "turnIds": [],
            "fromStartedAt": "2026-10-05T01:00:00Z", "toStartedAt": "2026-10-05T02:00:00Z", "limit": 32,
            "creditScenario": "operator_standard_rate_scenario"})
    }
}

#[cfg(unix)]
impl Drop for FrozenUsageFixture {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.directory, std::fs::Permissions::from_mode(0o700));
        let _ = std::fs::set_permissions(&self.database, std::fs::Permissions::from_mode(0o600));
    }
}

#[cfg(unix)]
fn expected_frozen_usage() -> serde_json::Value {
    serde_json::json!({
        "requested": true, "status": "readComplete",
        "source": {"snapshotIdentity": "fixture-usage", "sourceNamespace": "fixture-source", "sourcePlane": "ExistingUsageLedger",
            "sourceKind": "ExistingReadOnlySnapshot", "sourceRevision": "fixture-revision", "schemaEvidence": "fixture-schema",
            "observedAt": "READ_END", "transactionReadStartedAt": "READ_START", "transactionReadEndedAt": "READ_END",
            "window": {"fromInclusive": "2026-10-05T01:00:00+00:00", "toExclusive": "2026-10-05T02:00:00+00:00"},
            "selectors": {"threadIds": ["thread-1", "thread-unknown"], "callIds": [], "turnIds": []},
            "rowLimit": 32, "exportedCount": 2, "coverage": "Complete", "creditScenario": "OperatorStandardRateScenario",
            "standardRateScenarioRequested": true},
        "callerClaims": {"kind": "ExistingReadOnlySnapshot", "snapshotIdentity": "fixture-usage", "independentlyIdentified": true,
            "quiescent": true, "noLiveWriter": true, "noPendingWal": true, "readOnlySource": true,
            "claimStatus": "callerCustodianAssertionNotIndependentlyVerified"},
        "readerObservation": {"sqliteImmutableReadOnly": true, "transactionCommitted": true},
        "join": {"complete": false, "associationCoverage": "unknownProviderCallKeyNotExposedByExternalHostEnvelope",
            "inputCallRows": 2, "callRowsTruncated": false, "diagnosticReferencesTruncated": false, "invalidUsageCallRows": 0,
            "outOfScopeDiagnosticReferences": 0, "distinctCallCount": 2, "duplicateCallRows": 0, "conflictingCallIds": 0,
            "quarantinedCallCount": 0, "associationTruncatedCount": 0,
            "conflictingIdentityEventCount": 0, "unqualifiedResponseEventCount": 0, "associatedCallCount": 0, "unmatchedCallCount": 2,
            "partitions": [
                {"rootThreadId": null, "configuredRole": null, "observedModel": null, "callCount": 1, "usagePresentCallCount": 0,
                    "strictCoveredCallCount": 0, "strictUnpricedOrMissingCallCount": 1, "providerReportedCreditMicros": 0,
                    "strictEstimateCreditMicros": 0, "standardScenarioCoveredCallCount": 0,
                    "standardScenarioUnpricedOrMissingCallCount": 1, "standardScenarioCreditMicros": 0,
                    "inputTokensUncached": {"sum": null, "rows": 0}, "inputTokensCached": {"sum": null, "rows": 0},
                    "inputTokensCacheWrite": {"sum": null, "rows": 0}, "outputTokens": {"sum": null, "rows": 0}, "totalTokens": {"sum": null, "rows": 0}},
                {"rootThreadId": "root-1", "configuredRole": "worker", "observedModel": "observed-model", "callCount": 1, "usagePresentCallCount": 1,
                    "strictCoveredCallCount": 1, "strictUnpricedOrMissingCallCount": 0, "providerReportedCreditMicros": 0,
                    "strictEstimateCreditMicros": 125000, "standardScenarioCoveredCallCount": 1,
                    "standardScenarioUnpricedOrMissingCallCount": 0, "standardScenarioCreditMicros": 250000,
                    "inputTokensUncached": {"sum": 5, "rows": 1}, "inputTokensCached": {"sum": 3, "rows": 1},
                    "inputTokensCacheWrite": {"sum": null, "rows": 0}, "outputTokens": {"sum": 2, "rows": 1}, "totalTokens": {"sum": 10, "rows": 1}}
            ],
            "associations": [
                {"callId": "selected-call", "diagnosticEvents": [], "unmatched": true, "conflictingUsageIdentity": false,
                    "identityQuarantined": false, "conflictingIdentityEventCount": 0, "unqualifiedResponseEventCount": 0,
                    "associationTruncated": false, "causalWakeClaim": false},
                {"callId": "unknown-call", "diagnosticEvents": [], "unmatched": true, "conflictingUsageIdentity": false,
                    "identityQuarantined": false, "conflictingIdentityEventCount": 0, "unqualifiedResponseEventCount": 0,
                    "associationTruncated": false, "causalWakeClaim": false}
            ]}
    })
}

#[cfg(unix)]
#[tokio::test]
async fn actual_debug_consumer_reads_frozen_usage_and_compares_complete_export_with_nonzero_unknown_partitions() {
    for identity in ["response-1", "contradictory-response"] {
        let fixture = FrozenUsageFixture::create(identity);
        let before = std::fs::read(&fixture.database).unwrap();
        let input = super::super::tests::rollout_pair("list_agents", serde_json::json!({"path_prefix": "agent"}),
            serde_json::json!({"agents": [{"agent_id": "actor", "canonical_path": "agent/path", "agent_status": "completed"}],
                "completed": "BODY_CANARY"}));
        let input_path = fixture.root.path().join("selected.jsonl");
        let request_path = fixture.root.path().join("request.json");
        std::fs::write(&input_path, &input).unwrap();
        let request = fixture.request_json();
        std::fs::write(&request_path, request.to_string()).unwrap();
        let mut actual = super::super::execute(super::super::tests::command_for(&input_path, input.len(), Some(&request_path)))
            .await.unwrap();
        // Only the reader's genuinely runtime-dependent clock values are normalized.
        let start = DateTime::parse_from_rfc3339(actual["usage"]["source"]["transactionReadStartedAt"].as_str().unwrap()).unwrap();
        let end = DateTime::parse_from_rfc3339(actual["usage"]["source"]["transactionReadEndedAt"].as_str().unwrap()).unwrap();
        assert!(start <= end);
        assert_eq!(actual["usage"]["source"]["observedAt"], actual["usage"]["source"]["transactionReadEndedAt"]);
        actual["usage"]["source"]["transactionReadStartedAt"] = "READ_START".into();
        actual["usage"]["source"]["transactionReadEndedAt"] = "READ_END".into(); actual["usage"]["source"]["observedAt"] = "READ_END".into();
        let mut expected = super::super::tests::expected_status_export(input.len());
        expected["coverage"]["usageCoverage"] = "requested".into(); expected["usage"] = expected_frozen_usage();
        if identity != "response-1" {
            expected["usage"]["join"]["quarantinedCallCount"] = 1.into();
            expected["usage"]["join"]["associations"][0]["identityQuarantined"] = true.into();
        }
        assert_eq!(actual, expected);
        assert!(!actual.to_string().contains("BODY_CANARY"));
        assert!(!actual.to_string().contains("REQUESTED_MODEL_CANARY"));
        assert_eq!(std::fs::read(&fixture.database).unwrap(), before);
        let files = std::fs::read_dir(&fixture.directory).unwrap().map(|entry| entry.unwrap().file_name()).collect::<Vec<_>>();
        assert_eq!(files, vec![std::ffi::OsString::from("usage.sqlite")]);

        for field in ["sourceNamespace", "expectedSnapshotIdentity"] {
            let mut wrong = request.clone(); wrong[field] = "wrong-identity".into();
            std::fs::write(&request_path, wrong.to_string()).unwrap();
            let actual = super::super::execute(super::super::tests::command_for(&input_path, input.len(), Some(&request_path)))
                .await.unwrap();
            let mut expected = super::super::tests::expected_status_export(input.len());
            expected["coverage"]["usageCoverage"] = "requested".into();
            expected["usage"] = serde_json::json!({"requested": true, "status": "IdentityMismatch",
                "callerClaims": expected_frozen_usage()["callerClaims"], "readerObservation": null, "join": null});
            assert_eq!(actual, expected);
            assert_eq!(std::fs::read(&fixture.database).unwrap(), before);
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn frozen_reader_real_projection_and_typed_provider_mapper_join_compare_whole_public_result() {
    use codex_diagnostics::control_plane::{CaptureMode, ControlPlaneRecorder, CreditMicros,
        EventIdentity, EventKind, ObservationQuality, ProviderCallObservation, ProviderLedgerWriteOutcome,
        SourcePlane, UsageCallAssociation, UsageJoinSummary, UsagePartition, UsagePartitionKey,
        PROVIDER_COMPLETION_PRODUCER_BOUNDARY, map_provider_completion_reference};
    let fixture = FrozenUsageFixture::create("response-1");
    let file = serde_json::from_value(fixture.request_json()).unwrap();
    let (request, _) = build_state_request(file, "fixture-source").unwrap();
    let frozen = read_control_plane_usage_snapshot(request).await.unwrap();
    let calls = project_calls(&frozen);
    // Synthetic typed boundary control only: actual Session hook application
    // and emitted native-core proof remain the receiving integrator's outcome.
    let input = super::super::tests::rollout_pair("list_agents", serde_json::json!({}), serde_json::json!({"agents": []}));
    let (observations, _) = super::super::envelopes::parse(input.as_bytes(), "fixture-source");
    assert_eq!(observations.len(), 1);
    let mut provider = super::super::to_event(observations.into_iter().next().unwrap()).unwrap();
    provider.source_plane = SourcePlane::RustCollab;
    provider.producer_boundary = PROVIDER_COMPLETION_PRODUCER_BOUNDARY.into();
    provider.quality = ObservationQuality::Owned; provider.kind = EventKind::ProviderCompletionObserved;
    provider.thread_id = Some("thread-1".into()); provider.turn_id = Some("turn-1".into());
    provider.external_operation = None; provider.status_query = None; provider.queue = None; provider.field_coverage.clear();
    provider.provider_call = Some(ProviderCallObservation { provider: "provider".into(), response_id: "response-1".into(),
        persisted_provider_call_id: Some("selected-call".into()), ledger_response_scope: UsageAccountScope::WriterUnscoped,
        ledger_write_outcome: ProviderLedgerWriteOutcome::Inserted });
    let recorder = ControlPlaneRecorder::new(CaptureMode::Session, "fixture-capture").unwrap();
    recorder.record(provider).unwrap();
    let capture = recorder.snapshot().unwrap();
    let reference = map_provider_completion_reference(&capture.events[0], "fixture-source").unwrap().unwrap();
    let scope = UsageJoinScope { usage_snapshot_id: "fixture-usage".into(), usage_source_namespace: "fixture-source".into(),
        usage_from_started_at: "2026-10-05T01:00:00.000000000Z".into(), usage_to_started_at: "2026-10-05T02:00:00.000000000Z".into(),
        usage_snapshot_complete: true, diagnostic_capture_instance_id: "fixture-capture".into(),
        diagnostic_high_water: 1, diagnostic_snapshot_complete: true };
    let expected = UsageJoinSummary { scope: scope.clone(), complete: true, input_call_rows: 2, distinct_call_count: 2,
        associated_call_count: 1, unmatched_call_count: 1,
        partitions: std::collections::BTreeMap::from([
            (UsagePartitionKey { root_thread_id: None, configured_role: None, observed_model: None },
                UsagePartition { call_count: 1, strict_unpriced_or_missing_call_count: 1,
                    standard_scenario_unpriced_or_missing_call_count: 1, ..UsagePartition::default() }),
            (UsagePartitionKey { root_thread_id: Some("root-1".into()), configured_role: Some("worker".into()), observed_model: Some("observed-model".into()) },
                UsagePartition { call_count: 1, usage_present_call_count: 1, strict_covered_call_count: 1,
                    strict_rate_estimate_credit_micros: CreditMicros(125000), standard_scenario_covered_call_count: 1,
                    standard_scenario_credit_micros: CreditMicros(250000), input_tokens_uncached_sum: Some(5), input_tokens_uncached_rows: 1,
                    input_tokens_cached_sum: Some(3), input_tokens_cached_rows: 1, output_tokens_sum: Some(2), output_tokens_rows: 1,
                    total_tokens_sum: Some(10), total_tokens_rows: 1, ..UsagePartition::default() })
        ]),
        associations: vec![
            UsageCallAssociation { call_id: "selected-call".into(), diagnostic_events: vec![(SourcePlane::RustCollab,
                EventIdentity { capture_instance_id: "fixture-capture".into(), sequence: 1 })], unmatched: false,
                conflicting_usage_identity: false, identity_quarantined: false, association_truncated: false,
                conflicting_identity_event_count: 0, unqualified_response_event_count: 0 },
            UsageCallAssociation { call_id: "unknown-call".into(), diagnostic_events: vec![], unmatched: true,
                conflicting_usage_identity: false, identity_quarantined: false, association_truncated: false,
                conflicting_identity_event_count: 0, unqualified_response_event_count: 0 }
        ], ..UsageJoinSummary::default() };
    assert_eq!(join_control_plane_usage(&scope, &calls, &[reference.clone()]), expected);

    let mut wrong_thread = reference.clone(); wrong_thread.key.thread_id = "wrong-thread".into();
    let mut rejected = expected.clone(); rejected.complete = false; rejected.associated_call_count = 0;
    rejected.unmatched_call_count = 2; rejected.conflicting_identity_event_count = 1;
    rejected.associations[0].diagnostic_events.clear(); rejected.associations[0].unmatched = true;
    rejected.associations[0].conflicting_identity_event_count = 1;
    assert_eq!(join_control_plane_usage(&scope, &calls, &[wrong_thread]), rejected);

    let mut wrong_source = capture.events[0].clone(); wrong_source.input.source_plane = SourcePlane::ExternalHostEnvelope;
    wrong_source.input.quality = ObservationQuality::ObservedOnly;
    assert_eq!(map_provider_completion_reference(&wrong_source, "fixture-source").unwrap(), None);
    let unknown_scope = UsageJoinScope { diagnostic_snapshot_complete: false, ..scope.clone() };
    let mut unavailable = expected.clone(); unavailable.scope = unknown_scope.clone(); unavailable.complete = false;
    unavailable.associated_call_count = 0; unavailable.unmatched_call_count = 2;
    unavailable.associations[0].diagnostic_events.clear(); unavailable.associations[0].unmatched = true;
    assert_eq!(join_control_plane_usage(&unknown_scope, &calls, &[]), unavailable);

    let mut loss_scope = scope; loss_scope.diagnostic_snapshot_complete = false;
    let mut partial = expected; partial.scope = loss_scope.clone(); partial.complete = false;
    assert_eq!(join_control_plane_usage(&loss_scope, &calls, &[reference]), partial);
}
