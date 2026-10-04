use super::*;
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
{"timestamp":"2026-10-05T01:00:01Z","type":"response_item","payload":{"type":"function_call_output","call_id":"same-id","output":"{}"}}"#;
    let (observations, _) = super::super::envelopes::parse(input.as_bytes(), "same-namespace");
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
