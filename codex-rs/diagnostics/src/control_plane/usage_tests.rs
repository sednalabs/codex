use super::*;
use super::super::types::{
    EventInput, ProviderCallObservation, ProviderLedgerWriteOutcome, RecordedEvent,
};
use std::time::SystemTime;

fn call(call_id: &str, namespace: &str) -> UsageJoinCall {
    UsageJoinCall {
        source_snapshot_id: "usage-snapshot-1".to_string(),
        started_at: "2026-01-01T12:00:00.000000000Z".to_string(),
        key: UsageJoinKey {
            source_namespace: namespace.to_string(),
            thread_id: "thread-1".to_string(),
            turn_id: Some("turn-1".to_string()),
            provider: Some("openai".to_string()),
            account_scope: UsageAccountScope::KnownScope("account-1".to_string()),
            call_id: Some(call_id.to_string()),
            response_id: Some(format!("response-{call_id}")),
        },
        identity_quarantined: false,
        root_thread_id: Some("root-1".to_string()),
        configured_role: Some("worker".to_string()),
        observed_model: Some("gpt-observed".to_string()),
        input_tokens_uncached: Some(5),
        input_tokens_cached: Some(3),
        input_tokens_cache_write: None,
        output_tokens: Some(2),
        total_tokens: Some(10),
        strict_pricing_status: Some("priced_estimate".to_string()),
        strict_credit_source: Some("rate_card_estimate".to_string()),
        strict_estimated_credits: Some(1.2345678),
        provider_reported_credits: None,
        standard_scenario_status: Some("priced_scenario_estimate".to_string()),
        standard_scenario_credits: Some(2.5),
    }
}

fn reference(call: &UsageJoinCall, sequence: u64) -> DiagnosticCallReference {
    DiagnosticCallReference {
        key: call.key.clone(),
        source_plane: SourcePlane::RustCollab,
        event: EventIdentity {
            capture_instance_id: "capture-1".to_string(),
            sequence,
        },
    }
}

fn reference_with_key(key: UsageJoinKey, sequence: u64) -> DiagnosticCallReference {
    DiagnosticCallReference {
        key,
        source_plane: SourcePlane::RustCollab,
        event: EventIdentity {
            capture_instance_id: "capture-1".to_string(),
            sequence,
        },
    }
}

fn scope() -> UsageJoinScope {
    UsageJoinScope {
        usage_snapshot_id: "usage-snapshot-1".to_string(),
        usage_source_namespace: "scope-1".to_string(),
        usage_from_started_at: "2026-01-01T00:00:00.000000000Z".to_string(),
        usage_to_started_at: "2026-01-02T00:00:00.000000000Z".to_string(),
        usage_snapshot_complete: true,
        diagnostic_capture_instance_id: "capture-1".to_string(),
        diagnostic_high_water: 100,
        diagnostic_snapshot_complete: true,
    }
}

fn provider_event(
    outcome: ProviderLedgerWriteOutcome,
    persisted_provider_call_id: Option<&str>,
    ledger_response_scope: UsageAccountScope,
) -> RecordedEvent {
    RecordedEvent {
        identity: EventIdentity { capture_instance_id: "capture-1".to_string(), sequence: 17 },
        input: EventInput {
            source_plane: SourcePlane::RustCollab,
            producer_boundary: PROVIDER_COMPLETION_PRODUCER_BOUNDARY.to_string(),
            producer_version: Some("source-version".to_string()),
            kind: EventKind::ProviderCompletionObserved,
            quality: ObservationQuality::Owned,
            wall_correlation: SystemTime::UNIX_EPOCH,
            monotonic_offset_ns: Some(10),
            operation_id: Some("same-string-as-provider-pk".to_string()),
            thread_id: Some("native-thread".to_string()),
            turn_id: Some("native-turn".to_string()),
            root_thread_id: None,
            parent_thread_id: None,
            fork_parent_thread_id: None,
            window_id: None,
            window_number: None,
            previous_window_id: None,
            wait: None,
            sleep: None,
            readiness: None,
            status_query: None,
            queue: None,
            field_coverage: Vec::new(),
            message: None,
            scheduler: None,
            external_operation: None,
            provider_call: Some(ProviderCallObservation {
                provider: "openai".to_string(),
                response_id: "native-response".to_string(),
                persisted_provider_call_id: persisted_provider_call_id.map(str::to_string),
                ledger_response_scope,
                ledger_write_outcome: outcome,
            }),
        },
        truncated: false,
    }
}

#[test]
fn exact_keys_associate_once_and_partition_distinct_calls() {
    let first = call("call-1", "scope-1");
    let second = call("call-2", "scope-1");
    let other_scope = reference(&UsageJoinCall {
        key: UsageJoinKey { source_namespace: "foreign".to_string(), ..first.key.clone() },
        ..first.clone()
    }, 9);
    let summary = join_control_plane_usage(
        &scope(),
        &[first.clone(), first.clone(), second.clone()],
        &[reference(&first, 1), reference(&first, 1), other_scope],
    );

    assert_eq!(summary.scope.usage_snapshot_id, "usage-snapshot-1");
    assert!(!summary.complete);
    assert_eq!(summary.input_call_rows, 3);
    assert_eq!(summary.distinct_call_count, 2);
    assert_eq!(summary.duplicate_call_rows, 1);
    assert_eq!(summary.conflicting_call_ids, 0);
    assert_eq!(summary.associated_call_count, 1);
    assert_eq!(summary.unmatched_call_count, 1);
    assert_eq!(summary.associations[0], UsageCallAssociation {
        call_id: "call-1".to_string(),
        diagnostic_events: vec![(SourcePlane::RustCollab, EventIdentity {
            capture_instance_id: "capture-1".to_string(), sequence: 1,
        })],
        unmatched: false,
        conflicting_usage_identity: false,
        identity_quarantined: false,
        association_truncated: false,
        conflicting_identity_event_count: 0,
        unqualified_response_event_count: 0,
    });
    assert_eq!(summary.partitions, BTreeMap::from([(
        UsagePartitionKey {
            root_thread_id: Some("root-1".to_string()),
            configured_role: Some("worker".to_string()),
            observed_model: Some("gpt-observed".to_string()),
        },
        UsagePartition {
            call_count: 2,
            usage_present_call_count: 2,
            strict_covered_call_count: 2,
            strict_unpriced_or_missing_call_count: 0,
            provider_reported_credit_micros: CreditMicros::default(),
            strict_rate_estimate_credit_micros: CreditMicros(2_469_136),
            standard_scenario_covered_call_count: 2,
            standard_scenario_unpriced_or_missing_call_count: 0,
            standard_scenario_credit_micros: CreditMicros(5_000_000),
            input_tokens_uncached_sum: Some(10),
            input_tokens_uncached_rows: 2,
            input_tokens_cached_sum: Some(6),
            input_tokens_cached_rows: 2,
            input_tokens_cache_write_sum: None,
            input_tokens_cache_write_rows: 0,
            output_tokens_sum: Some(4),
            output_tokens_rows: 2,
            total_tokens_sum: Some(20),
            total_tokens_rows: 2,
        },
    )]));
}

#[test]
fn conflicting_duplicate_is_quarantined_into_unknown_partition() {
    let first = call("call-1", "scope-1");
    let mut conflict = first.clone();
    conflict.observed_model = Some("different-model".to_string());
    let summary = join_control_plane_usage(&scope(), &[first, conflict], &[]);
    assert_eq!(summary.conflicting_call_ids, 1);
    assert_eq!(summary.distinct_call_count, 1);
    assert_eq!(summary.associations, vec![UsageCallAssociation {
        call_id: "call-1".to_string(),
        diagnostic_events: Vec::new(),
        unmatched: true,
        conflicting_usage_identity: true,
        identity_quarantined: false,
        association_truncated: false,
        conflicting_identity_event_count: 0,
        unqualified_response_event_count: 0,
    }]);
    assert_eq!(summary.partitions, BTreeMap::from([(
        UsagePartitionKey {
            root_thread_id: None,
            configured_role: None,
            observed_model: None,
        },
        UsagePartition {
            call_count: 1,
            strict_unpriced_or_missing_call_count: 1,
            standard_scenario_unpriced_or_missing_call_count: 1,
            ..UsagePartition::default()
        },
    )]));
}

#[test]
fn unknown_actual_model_and_missing_usage_are_not_filled_or_zero_priced() {
    let mut unknown = call("call-unknown", "scope-1");
    unknown.observed_model = None;
    unknown.input_tokens_uncached = None;
    unknown.input_tokens_cached = None;
    unknown.input_tokens_cache_write = None;
    unknown.output_tokens = None;
    unknown.total_tokens = None;
    unknown.strict_pricing_status = Some("provider_usage_missing".to_string());
    unknown.strict_credit_source = None;
    unknown.strict_estimated_credits = None;
    unknown.standard_scenario_status = Some("provider_usage_missing".to_string());
    unknown.standard_scenario_credits = None;
    let summary = join_control_plane_usage(&scope(), &[unknown], &[]);
    assert_eq!(summary.distinct_call_count, 1);
    assert_eq!(summary.unmatched_call_count, 1);
    assert_eq!(summary.partitions, BTreeMap::from([(
        UsagePartitionKey {
            root_thread_id: Some("root-1".to_string()),
            configured_role: Some("worker".to_string()),
            observed_model: None,
        },
        UsagePartition {
            call_count: 1,
            usage_present_call_count: 0,
            strict_covered_call_count: 0,
            strict_unpriced_or_missing_call_count: 1,
            input_tokens_uncached_sum: None,
            input_tokens_uncached_rows: 0,
            input_tokens_cached_sum: None,
            input_tokens_cached_rows: 0,
            input_tokens_cache_write_sum: None,
            input_tokens_cache_write_rows: 0,
            output_tokens_sum: None,
            output_tokens_rows: 0,
            total_tokens_sum: None,
            total_tokens_rows: 0,
            standard_scenario_unpriced_or_missing_call_count: 1,
            ..UsagePartition::default()
        },
    )]));
}

#[test]
fn response_key_may_join_only_with_exact_thread_and_same_namespace() {
    let usage = call("call-1", "scope-1");
    let mut diagnostic = reference(&usage, 3);
    diagnostic.key.call_id = None;
    let summary = join_control_plane_usage(&scope(), &[usage.clone()], &[diagnostic.clone()]);
    assert_eq!(summary.associated_call_count, 1);

    diagnostic.key.source_namespace = "foreign".to_string();
    let summary = join_control_plane_usage(&scope(), &[usage], &[diagnostic]);
    assert_eq!(summary.associated_call_count, 0);
    assert_eq!(summary.unmatched_call_count, 1);
}

#[test]
fn provider_reported_and_operator_scenario_credits_remain_separate() {
    let mut call = call("call-credits", "scope-1");
    call.strict_pricing_status = Some("provider_reported".to_string());
    call.strict_credit_source = Some("provider_reported".to_string());
    call.strict_estimated_credits = Some(0.75);
    call.provider_reported_credits = Some(0.75);
    let summary = join_control_plane_usage(&scope(), &[call], &[]);
    assert_eq!(summary.partitions.len(), 1);
    assert_eq!(summary.partitions.values().next(), Some(&UsagePartition {
        call_count: 1,
        usage_present_call_count: 1,
        strict_covered_call_count: 1,
        strict_unpriced_or_missing_call_count: 0,
        provider_reported_credit_micros: CreditMicros(750_000),
        strict_rate_estimate_credit_micros: CreditMicros::default(),
        standard_scenario_covered_call_count: 1,
        standard_scenario_unpriced_or_missing_call_count: 0,
        standard_scenario_credit_micros: CreditMicros(2_500_000),
        input_tokens_uncached_sum: Some(5),
        input_tokens_uncached_rows: 1,
        input_tokens_cached_sum: Some(3),
        input_tokens_cached_rows: 1,
        input_tokens_cache_write_sum: None,
        input_tokens_cache_write_rows: 0,
        output_tokens_sum: Some(2),
        output_tokens_rows: 1,
        total_tokens_sum: Some(10),
        total_tokens_rows: 1,
    }));
}

#[test]
fn late_usage_snapshot_is_not_joined_into_the_selected_as_of_window() {
    let mut late = call("call-1", "scope-1");
    late.source_snapshot_id = "usage-snapshot-2".to_string();
    let diagnostic = reference(&late, 1);
    let summary = join_control_plane_usage(&scope(), &[late], &[diagnostic]);
    assert_eq!(summary.associated_call_count, 0);
    assert_eq!(summary.invalid_usage_call_rows, 1);
    assert!(!summary.complete);
}

#[test]
fn usage_outside_half_open_window_is_rejected() {
    let mut outside = call("call-outside", "scope-1");
    outside.started_at = "2026-01-02T00:00:00.000000000Z".to_string();
    let summary = join_control_plane_usage(&scope(), &[outside], &[]);
    assert_eq!(summary.distinct_call_count, 0);
    assert_eq!(summary.invalid_usage_call_rows, 1);
    assert!(!summary.complete);
}

#[test]
fn primary_call_key_rejects_a_contradictory_response_without_double_counting() {
    let usage = call("call-primary", "scope-1");
    let mut contradictory = usage.key.clone();
    contradictory.response_id = Some("different-response".to_string());
    let summary = join_control_plane_usage(&scope(), &[usage], &[reference_with_key(contradictory, 1)]);
    assert_eq!(summary.associated_call_count, 0);
    assert_eq!(summary.unmatched_call_count, 1);
    assert_eq!(summary.conflicting_identity_event_count, 1);
    assert_eq!(summary.associations[0].conflicting_identity_event_count, 1);
    assert_eq!(summary.partitions.values().map(|partition| partition.call_count).sum::<usize>(), 1);
}

#[test]
fn primary_call_key_rejects_each_other_supplied_identity_contradiction() {
    let usage = call("call-primary", "scope-1");
    let mut contradictions = Vec::new();
    let mut provider = usage.key.clone();
    provider.provider = Some("other-provider".to_string());
    contradictions.push(provider);
    let mut account = usage.key.clone();
    account.account_scope = UsageAccountScope::KnownScope("other-account".to_string());
    contradictions.push(account);
    let mut turn = usage.key.clone();
    turn.turn_id = Some("other-turn".to_string());
    contradictions.push(turn);
    let mut thread = usage.key.clone();
    thread.thread_id = "other-thread".to_string();
    contradictions.push(thread);

    for (sequence, contradictory) in contradictions.into_iter().enumerate() {
        let summary = join_control_plane_usage(
            &scope(),
            &[usage.clone()],
            &[reference_with_key(contradictory, 20 + sequence as u64)],
        );
        assert_eq!(summary.associated_call_count, 0);
        assert_eq!(summary.conflicting_identity_event_count, 1);
        assert_eq!(summary.distinct_call_count, 1);
        assert_eq!(summary.partitions.values().map(|partition| partition.call_count).sum::<usize>(), 1);
    }
}

#[test]
fn response_key_rejects_a_contradictory_provider_call_key() {
    let mut usage = call("call-response", "scope-1");
    let mut contradictory = usage.key.clone();
    contradictory.call_id = Some("different-call".to_string());
    let summary = join_control_plane_usage(&scope(), &[usage], &[reference_with_key(contradictory, 2)]);
    assert_eq!(summary.associated_call_count, 0);
    assert_eq!(summary.conflicting_identity_event_count, 1);
    assert_eq!(summary.distinct_call_count, 1);
    assert_eq!(summary.partitions.values().map(|partition| partition.call_count).sum::<usize>(), 1);
}

#[test]
fn response_key_requires_matching_provider_and_qualified_scope() {
    let mut usage = call("call-response", "scope-1");
    let mut wrong_provider = usage.key.clone();
    wrong_provider.call_id = None;
    wrong_provider.provider = Some("other-provider".to_string());
    let summary = join_control_plane_usage(&scope(), &[usage.clone()], &[reference_with_key(wrong_provider, 3)]);
    assert_eq!(summary.associated_call_count, 0);
    assert_eq!(summary.conflicting_identity_event_count, 0);

    let mut unknown_scope = usage.key.clone();
    unknown_scope.call_id = None;
    unknown_scope.account_scope = UsageAccountScope::Unknown;
    let summary = join_control_plane_usage(&scope(), &[usage.clone()], &[reference_with_key(unknown_scope, 4)]);
    assert_eq!(summary.associated_call_count, 0);
    assert_eq!(summary.unqualified_response_event_count, 1);
    assert!(!summary.complete);

    usage.key.account_scope = UsageAccountScope::WriterUnscoped;
    let mut omitted_scope = usage.key.clone();
    omitted_scope.call_id = None;
    omitted_scope.account_scope = UsageAccountScope::Unknown;
    let summary = join_control_plane_usage(&scope(), &[usage.clone()], &[reference_with_key(omitted_scope, 5)]);
    assert_eq!(summary.associated_call_count, 0);
    assert_eq!(summary.unqualified_response_event_count, 1);

    let mut known_scope = usage.key.clone();
    known_scope.call_id = None;
    known_scope.account_scope = UsageAccountScope::KnownScope("account-1".to_string());
    let summary = join_control_plane_usage(&scope(), &[usage], &[reference_with_key(known_scope, 10)]);
    assert_eq!(summary.associated_call_count, 0);
    assert_eq!(summary.conflicting_identity_event_count, 0);
}

#[test]
fn identical_response_ids_across_accounts_do_not_cross_associate() {
    let mut account_a = call("call-a", "scope-1");
    account_a.key.response_id = Some("shared-response".to_string());
    let mut account_b = call("call-b", "scope-1");
    account_b.key.response_id = Some("shared-response".to_string());
    account_b.key.account_scope = UsageAccountScope::KnownScope("account-2".to_string());
    let mut ref_a = account_a.key.clone();
    ref_a.call_id = None;
    let mut ref_b = account_b.key.clone();
    ref_b.call_id = None;
    let summary = join_control_plane_usage(
        &scope(),
        &[account_a, account_b],
        &[reference_with_key(ref_a, 6), reference_with_key(ref_b, 7)],
    );
    assert_eq!(summary.associated_call_count, 2);
    assert_eq!(summary.conflicting_identity_event_count, 0);
    assert_eq!(summary.distinct_call_count, 2);
    assert_eq!(summary.partitions.values().map(|partition| partition.call_count).sum::<usize>(), 2);
}

#[test]
fn full_response_tuple_replays_and_primary_legacy_key_keeps_unknown_optional_identity() {
    let mut response_only = call("call-response", "scope-1");
    let mut response_reference = response_only.key.clone();
    response_reference.call_id = None;
    let summary = join_control_plane_usage(
        &scope(),
        &[response_only.clone()],
        &[reference_with_key(response_reference, 8)],
    );
    assert_eq!(summary.associated_call_count, 1);
    assert_eq!(summary.unqualified_response_event_count, 0);

    let mut legacy_ref = response_only.key.clone();
    legacy_ref.provider = None;
    legacy_ref.account_scope = UsageAccountScope::Unknown;
    legacy_ref.response_id = None;
    legacy_ref.call_id = Some("call-response".to_string());
    response_only.key.call_id = Some("call-response".to_string());
    response_only.key.response_id = None;
    response_only.identity_quarantined = false;
    let summary = join_control_plane_usage(
        &scope(),
        &[response_only],
        &[reference_with_key(legacy_ref.clone(), 9)],
    );
    assert_eq!(summary.associated_call_count, 1);
    assert_eq!(legacy_ref.provider, None);
    assert_eq!(legacy_ref.account_scope, UsageAccountScope::Unknown);
    assert_eq!(summary.partitions.keys().next().unwrap().root_thread_id, Some("root-1".to_string()));
}

#[test]
fn actual_provider_completion_mapper_accepts_inserted_pk_and_duplicate_tuple_only() {
    let inserted = provider_event(
        ProviderLedgerWriteOutcome::Inserted,
        Some("committed-provider-pk"),
        UsageAccountScope::KnownScope("account-1".to_string()),
    );
    let inserted_ref = map_provider_completion_reference(&inserted, "selected-usage-source")
        .unwrap().unwrap();
    assert_eq!(inserted_ref.key.call_id.as_deref(), Some("committed-provider-pk"));
    assert_eq!(inserted_ref.key.response_id.as_deref(), Some("native-response"));
    assert_eq!(inserted_ref.key.thread_id, "native-thread");
    assert_eq!(inserted_ref.key.turn_id.as_deref(), Some("native-turn"));
    assert_eq!(inserted_ref.key.provider.as_deref(), Some("openai"));

    let duplicate = provider_event(
        ProviderLedgerWriteOutcome::Duplicate,
        None,
        UsageAccountScope::WriterUnscoped,
    );
    let duplicate_ref = map_provider_completion_reference(&duplicate, "selected-usage-source")
        .unwrap().unwrap();
    assert_eq!(duplicate_ref.key.call_id, None);
    assert_eq!(duplicate_ref.key.response_id.as_deref(), Some("native-response"));
    assert_eq!(duplicate_ref.key.account_scope, UsageAccountScope::WriterUnscoped);
    assert_eq!(duplicate_ref.key.source_namespace, "selected-usage-source");
}

#[test]
fn provider_completion_mapper_rejects_malformed_receipts_and_unknown_scope() {
    let missing_inserted_pk = provider_event(
        ProviderLedgerWriteOutcome::Inserted,
        None,
        UsageAccountScope::KnownScope("account-1".to_string()),
    );
    assert_eq!(
        map_provider_completion_reference(&missing_inserted_pk, "usage"),
        Err(ProviderReferenceMapError::InvalidReceipt),
    );

    let duplicate_with_provisional_pk = provider_event(
        ProviderLedgerWriteOutcome::Duplicate,
        Some("provisional-not-committed"),
        UsageAccountScope::KnownScope("account-1".to_string()),
    );
    assert_eq!(
        map_provider_completion_reference(&duplicate_with_provisional_pk, "usage"),
        Err(ProviderReferenceMapError::InvalidReceipt),
    );

    let failed_with_pk = provider_event(
        ProviderLedgerWriteOutcome::FailedUnknown,
        Some("not-a-committed-pk"),
        UsageAccountScope::Unknown,
    );
    assert_eq!(
        map_provider_completion_reference(&failed_with_pk, "usage"),
        Err(ProviderReferenceMapError::InvalidReceipt),
    );

    let unknown_scope = provider_event(
        ProviderLedgerWriteOutcome::Inserted,
        Some("committed-provider-pk"),
        UsageAccountScope::Unknown,
    );
    assert_eq!(map_provider_completion_reference(&unknown_scope, "usage").unwrap(), None);
}

#[test]
fn provider_completion_mapper_never_aliases_equal_host_operation_text() {
    let mut host_event = provider_event(
        ProviderLedgerWriteOutcome::Inserted,
        Some("same-string-as-provider-pk"),
        UsageAccountScope::KnownScope("account-1".to_string()),
    );
    host_event.input.source_plane = SourcePlane::ExternalHostEnvelope;
    host_event.input.producer_boundary = "external-host-envelope".to_string();
    assert_eq!(map_provider_completion_reference(&host_event, "usage").unwrap(), None);
}

#[test]
fn provider_completion_mapper_rejects_unbounded_values_and_failed_receipts_are_unavailable() {
    assert_eq!(
        map_provider_completion_reference(&provider_event(
            ProviderLedgerWriteOutcome::Inserted,
            Some("pk"),
            UsageAccountScope::KnownScope("account".to_string()),
        ), "x".repeat(257).as_str()),
        Err(ProviderReferenceMapError::InvalidSelectedUsageNamespace),
    );

    let mut overbound = provider_event(
        ProviderLedgerWriteOutcome::Inserted,
        Some("pk"),
        UsageAccountScope::KnownScope("account".to_string()),
    );
    overbound.input.provider_call.as_mut().unwrap().response_id = "r".repeat(257);
    assert_eq!(
        map_provider_completion_reference(&overbound, "usage"),
        Err(ProviderReferenceMapError::InvalidReceipt),
    );

    let mut overbound_scope = provider_event(
        ProviderLedgerWriteOutcome::Duplicate,
        None,
        UsageAccountScope::KnownScope("s".repeat(257)),
    );
    assert_eq!(
        map_provider_completion_reference(&overbound_scope, "usage"),
        Err(ProviderReferenceMapError::InvalidReceipt),
    );
    overbound_scope.input.provider_call.as_mut().unwrap().ledger_response_scope =
        UsageAccountScope::KnownScope("account".to_string());
    overbound_scope.input.provider_call.as_mut().unwrap().provider = "p".repeat(257);
    assert_eq!(
        map_provider_completion_reference(&overbound_scope, "usage"),
        Err(ProviderReferenceMapError::InvalidReceipt),
    );
    overbound_scope.input.provider_call.as_mut().unwrap().provider = "openai".to_string();
    overbound_scope.input.thread_id = Some("t".repeat(257));
    assert_eq!(
        map_provider_completion_reference(&overbound_scope, "usage"),
        Err(ProviderReferenceMapError::InvalidReceipt),
    );

    for outcome in [ProviderLedgerWriteOutcome::FailedUnknown, ProviderLedgerWriteOutcome::NotConfigured] {
        let failed = provider_event(outcome, None, UsageAccountScope::KnownScope("account".to_string()));
        assert_eq!(map_provider_completion_reference(&failed, "usage").unwrap(), None);
    }
}
