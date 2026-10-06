//! Exact-key, association-only joins between diagnostic calls and usage rows.
//!
//! A matching source namespace is a scope boundary, not identity evidence by
//! itself. Primary-key joins reject all supplied identity contradictions;
//! response replay requires the full provider/account/thread/response tuple.

use std::collections::{BTreeMap, BTreeSet};

use super::types::{EventIdentity, SourcePlane};

const MAX_USAGE_CALLS: usize = 2048;
const MAX_USAGE_ID_BYTES: usize = 256;
const CREDIT_SCALE: f64 = 1_000_000.0;

#[path = "usage_provider_reference.rs"]
mod provider_reference;
pub use provider_reference::{map_provider_completion_reference, ProviderReferenceMapError};

/// Exact identifiers retained separately from their source namespace.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct UsageJoinKey {
    pub source_namespace: String,
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub provider: Option<String>,
    pub account_scope: UsageAccountScope,
    pub call_id: Option<String>,
    pub response_id: Option<String>,
}

/// Explicitly separates known account scopes, the writer's persisted empty
/// scope, and unavailable scope evidence.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum UsageAccountScope {
    KnownScope(String),
    WriterUnscoped,
    Unknown,
}

/// Binds one usage read to one selected diagnostic capture prefix and interval.
/// The two source snapshot identities remain distinct; this is not an atomic
/// cross-store snapshot claim.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UsageJoinScope {
    pub usage_snapshot_id: String,
    pub usage_source_namespace: String,
    pub usage_from_started_at: String,
    pub usage_to_started_at: String,
    pub usage_snapshot_complete: bool,
    pub diagnostic_capture_instance_id: String,
    pub diagnostic_high_water: u64,
    pub diagnostic_snapshot_complete: bool,
}

/// Per-call metadata mapped from the state projection without filling unknowns.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageJoinCall {
    pub source_snapshot_id: String,
    /// Canonical UTC RFC3339 timestamp with exactly nine fractional digits.
    pub started_at: String,
    pub key: UsageJoinKey,
    pub identity_quarantined: bool,
    pub root_thread_id: Option<String>,
    pub configured_role: Option<String>,
    pub observed_model: Option<String>,
    pub input_tokens_uncached: Option<i64>,
    pub input_tokens_cached: Option<i64>,
    pub input_tokens_cache_write: Option<i64>,
    pub output_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub strict_pricing_status: Option<String>,
    pub strict_credit_source: Option<String>,
    pub strict_estimated_credits: Option<f64>,
    pub provider_reported_credits: Option<f64>,
    pub standard_scenario_status: Option<String>,
    pub standard_scenario_credits: Option<f64>,
}

/// One exact diagnostic-side identity. It may identify an event, but it never
/// claims that a wake caused a provider call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticCallReference {
    pub key: UsageJoinKey,
    pub source_plane: SourcePlane,
    pub event: EventIdentity,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CreditMicros(pub i128);

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct UsagePartitionKey {
    pub root_thread_id: Option<String>,
    pub configured_role: Option<String>,
    pub observed_model: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UsagePartition {
    pub call_count: usize,
    pub usage_present_call_count: usize,
    pub strict_covered_call_count: usize,
    pub strict_unpriced_or_missing_call_count: usize,
    pub provider_reported_credit_micros: CreditMicros,
    pub strict_rate_estimate_credit_micros: CreditMicros,
    pub standard_scenario_covered_call_count: usize,
    pub standard_scenario_unpriced_or_missing_call_count: usize,
    pub standard_scenario_credit_micros: CreditMicros,
    pub input_tokens_uncached_sum: Option<i128>,
    pub input_tokens_uncached_rows: usize,
    pub input_tokens_cached_sum: Option<i128>,
    pub input_tokens_cached_rows: usize,
    pub input_tokens_cache_write_sum: Option<i128>,
    pub input_tokens_cache_write_rows: usize,
    pub output_tokens_sum: Option<i128>,
    pub output_tokens_rows: usize,
    pub total_tokens_sum: Option<i128>,
    pub total_tokens_rows: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageCallAssociation {
    pub call_id: String,
    pub diagnostic_events: Vec<(SourcePlane, EventIdentity)>,
    pub unmatched: bool,
    pub conflicting_usage_identity: bool,
    pub identity_quarantined: bool,
    pub association_truncated: bool,
    pub conflicting_identity_event_count: usize,
    pub unqualified_response_event_count: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UsageJoinSummary {
    pub scope: UsageJoinScope,
    pub complete: bool,
    pub input_call_rows: usize,
    pub call_rows_truncated: bool,
    pub diagnostic_references_truncated: bool,
    pub invalid_usage_call_rows: usize,
    pub out_of_scope_diagnostic_references: usize,
    pub conflicting_identity_event_count: usize,
    pub unqualified_response_event_count: usize,
    pub distinct_call_count: usize,
    pub duplicate_call_rows: usize,
    pub conflicting_call_ids: usize,
    pub quarantined_call_count: usize,
    pub association_truncated_count: usize,
    pub associated_call_count: usize,
    pub unmatched_call_count: usize,
    pub partitions: BTreeMap<UsagePartitionKey, UsagePartition>,
    pub associations: Vec<UsageCallAssociation>,
}

/// Associates per-call usage with diagnostic references and conserves every
/// distinct call into one root/role/observed-model partition. Credit values are
/// rounded per row to six decimal places before sums, so aggregate conservation
/// is over the same deterministic per-call representation. Rows outside the
/// exact micro-credit range of a finite `f64` remain unknown, not fabricated.
pub fn join_control_plane_usage(
    scope: &UsageJoinScope,
    calls: &[UsageJoinCall],
    diagnostics: &[DiagnosticCallReference],
) -> UsageJoinSummary {
    let mut summary = UsageJoinSummary {
        scope: scope.clone(),
        input_call_rows: calls.len().min(MAX_USAGE_CALLS),
        call_rows_truncated: calls.len() > MAX_USAGE_CALLS,
        diagnostic_references_truncated: diagnostics.len() > MAX_USAGE_CALLS,
        ..UsageJoinSummary::default()
    };
    let mut grouped = BTreeMap::<String, Vec<&UsageJoinCall>>::new();
    for call in calls.iter().take(MAX_USAGE_CALLS) {
        if valid_join_key(&call.key)
            && call.source_snapshot_id == scope.usage_snapshot_id
            && call.key.source_namespace == scope.usage_source_namespace
            && valid_usage_window(scope)
            && canonical_utc_timestamp(&call.started_at)
            && call.started_at >= scope.usage_from_started_at
            && call.started_at < scope.usage_to_started_at
            && let Some(id) = call.key.call_id.as_deref()
        {
            let id = id.to_string();
            grouped.entry(id).or_default().push(call);
        } else {
            summary.invalid_usage_call_rows += 1;
        }
    }
    let mut references_by_identity = BTreeMap::<EventIdentity, BTreeSet<(SourcePlane, UsageJoinKey)>>::new();
    for reference in diagnostics.iter().take(MAX_USAGE_CALLS) {
        if valid_join_key(&reference.key)
            && reference.key.source_namespace == scope.usage_source_namespace
            && reference.event.capture_instance_id == scope.diagnostic_capture_instance_id
            && reference.event.sequence > 0
            && reference.event.sequence <= scope.diagnostic_high_water
        {
            references_by_identity.entry(reference.event.clone()).or_default()
                .insert((reference.source_plane, reference.key.clone()));
        } else {
            summary.out_of_scope_diagnostic_references += 1;
        }
    }
    let mut diagnostic_by_key = BTreeMap::<UsageJoinKey, BTreeSet<(SourcePlane, EventIdentity)>>::new();
    let mut conflicting_reference_ids = BTreeSet::new();
    let mut unqualified_reference_ids = BTreeSet::new();
    for (identity, variants) in references_by_identity {
        if variants.len() != 1 {
            conflicting_reference_ids.insert(identity);
            continue;
        }
        if let Some((source, key)) = variants.into_iter().next() {
            diagnostic_by_key.entry(key).or_default().insert((source, identity));
        }
    }
    for (call_id, mut rows) in grouped {
        rows.sort_by_key(|row| row.key.clone());
        let first = rows[0];
        let conflict = rows.iter().any(|row| !same_usage_identity(first, row));
        if rows.len() > 1 {
            summary.duplicate_call_rows += rows.len() - 1;
        }
        if conflict {
            summary.conflicting_call_ids += 1;
        }
        summary.distinct_call_count += 1;
        let (partition_key, call) = if conflict {
            (
                UsagePartitionKey { root_thread_id: None, configured_role: None, observed_model: None },
                None,
            )
        } else {
            (
                UsagePartitionKey {
                    root_thread_id: first.root_thread_id.clone(),
                    configured_role: first.configured_role.clone(),
                    observed_model: first.observed_model.clone(),
                },
                Some(first),
            )
        };
        let partition = summary.partitions.entry(partition_key).or_default();
        partition.call_count += 1;
        if let Some(call) = call {
            add_call(partition, call);
        } else {
            partition.strict_unpriced_or_missing_call_count += 1;
            if rows.iter().any(|row| row.standard_scenario_status.is_some()
                || row.standard_scenario_credits.is_some()) {
                partition.standard_scenario_unpriced_or_missing_call_count += 1;
            }
        }
        let mut all_events = BTreeSet::new();
        let mut conflicting_identity_events = BTreeSet::new();
        let mut unqualified_response_events = BTreeSet::new();
        let identity_quarantined = !conflict && first.identity_quarantined;
        if !conflict && !identity_quarantined {
            for (key, ids) in &diagnostic_by_key {
                match association_decision(&first.key, key) {
                    AssociationDecision::Match => all_events.extend(ids.iter().cloned()),
                    AssociationDecision::Conflict => conflicting_identity_events.extend(ids.iter().cloned()),
                    AssociationDecision::UnqualifiedResponse => {
                        unqualified_response_events.extend(ids.iter().cloned())
                    }
                    AssociationDecision::NoMatch => {}
                }
            }
        }
        let conflicting_identity_event_count = conflicting_identity_events.len();
        let unqualified_response_event_count = unqualified_response_events.len();
        conflicting_reference_ids.extend(
            conflicting_identity_events.into_iter().map(|(_, identity)| identity),
        );
        unqualified_reference_ids.extend(
            unqualified_response_events.into_iter().map(|(_, identity)| identity),
        );
        if identity_quarantined {
            summary.quarantined_call_count += 1;
        }
        let all_events = all_events.into_iter().collect::<Vec<_>>();
        let association_truncated = all_events.len() > 64;
        if association_truncated { summary.association_truncated_count += 1; }
        let events = all_events.into_iter().take(64).collect::<Vec<_>>();
        let unmatched = events.is_empty();
        if unmatched { summary.unmatched_call_count += 1; }
        else { summary.associated_call_count += 1; }
        summary.associations.push(UsageCallAssociation {
            call_id,
            diagnostic_events: events,
            unmatched,
            conflicting_usage_identity: conflict,
            identity_quarantined,
            association_truncated,
            conflicting_identity_event_count,
            unqualified_response_event_count,
        });
    }
    summary.complete = scope.usage_snapshot_complete
        && scope.diagnostic_snapshot_complete
        && valid_usage_window(scope)
        && !summary.call_rows_truncated
        && !summary.diagnostic_references_truncated
        && summary.invalid_usage_call_rows == 0
        && summary.out_of_scope_diagnostic_references == 0
        && summary.conflicting_call_ids == 0
        && summary.quarantined_call_count == 0
        && summary.association_truncated_count == 0
        && conflicting_reference_ids.is_empty()
        && unqualified_reference_ids.is_empty();
    summary.conflicting_identity_event_count = conflicting_reference_ids.len();
    summary.unqualified_response_event_count = unqualified_reference_ids.len();
    summary
}

fn valid_join_key(key: &UsageJoinKey) -> bool {
    let bounded = |value: &str| {
        !value.is_empty()
            && value.len() <= MAX_USAGE_ID_BYTES
            && !value.chars().any(char::is_control)
    };
    bounded(&key.source_namespace)
        && bounded(&key.thread_id)
        && key.turn_id.as_deref().is_none_or(bounded)
        && key.provider.as_deref().is_none_or(bounded)
        && match &key.account_scope {
            UsageAccountScope::KnownScope(value) => bounded(value),
            UsageAccountScope::WriterUnscoped | UsageAccountScope::Unknown => true,
        }
        && key.call_id.as_deref().is_none_or(bounded)
        && key.response_id.as_deref().is_none_or(bounded)
        && (key.call_id.as_deref().is_some_and(bounded)
            || key.response_id.as_deref().is_some_and(bounded))
}

fn valid_usage_window(scope: &UsageJoinScope) -> bool {
    let bounded = |value: &str| {
        !value.is_empty()
            && value.len() <= MAX_USAGE_ID_BYTES
            && !value.chars().any(char::is_control)
    };
    bounded(&scope.usage_snapshot_id)
        && bounded(&scope.usage_source_namespace)
        && bounded(&scope.usage_from_started_at)
        && bounded(&scope.usage_to_started_at)
        && bounded(&scope.diagnostic_capture_instance_id)
        && canonical_utc_timestamp(&scope.usage_from_started_at)
        && canonical_utc_timestamp(&scope.usage_to_started_at)
        && scope.usage_from_started_at < scope.usage_to_started_at
}

fn canonical_utc_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 30
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'.'
        && bytes[29] == b'Z'
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 29) || byte.is_ascii_digit()
        })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AssociationDecision {
    Match,
    Conflict,
    UnqualifiedResponse,
    NoMatch,
}

fn association_decision(usage: &UsageJoinKey, diagnostic: &UsageJoinKey) -> AssociationDecision {
    if usage.source_namespace != diagnostic.source_namespace {
        return AssociationDecision::NoMatch;
    }
    let exact_call = usage.call_id.is_some() && usage.call_id == diagnostic.call_id;
    let exact_response = usage.response_id.is_some() && usage.response_id == diagnostic.response_id;
    if exact_call {
        return if known_identity_conflict(usage, diagnostic) {
            AssociationDecision::Conflict
        } else {
            AssociationDecision::Match
        };
    }
    if !exact_response { return AssociationDecision::NoMatch; }
    if usage.thread_id != diagnostic.thread_id { return AssociationDecision::NoMatch; }
    if usage.provider.is_some() && diagnostic.provider.is_some()
        && usage.provider != diagnostic.provider {
        return AssociationDecision::NoMatch;
    }
    if usage.account_scope != UsageAccountScope::Unknown
        && diagnostic.account_scope != UsageAccountScope::Unknown
        && usage.account_scope != diagnostic.account_scope {
        return AssociationDecision::NoMatch;
    }
    if usage.provider.is_none() || diagnostic.provider.is_none()
        || usage.account_scope == UsageAccountScope::Unknown
        || diagnostic.account_scope == UsageAccountScope::Unknown {
        return AssociationDecision::UnqualifiedResponse;
    }
    if usage.turn_id.is_some() && diagnostic.turn_id.is_some() && usage.turn_id != diagnostic.turn_id {
        return AssociationDecision::Conflict;
    }
    // Only the fully qualified response tuple makes a different supplied PK
    // a contradiction. Equal response text in a foreign tuple is not a conflict.
    if diagnostic.call_id.is_some() && usage.call_id != diagnostic.call_id {
        return AssociationDecision::Conflict;
    }
    AssociationDecision::Match
}

fn known_identity_conflict(usage: &UsageJoinKey, diagnostic: &UsageJoinKey) -> bool {
    usage.thread_id != diagnostic.thread_id
        || (usage.turn_id.is_some() && diagnostic.turn_id.is_some() && usage.turn_id != diagnostic.turn_id)
        || (usage.call_id.is_some() && diagnostic.call_id.is_some() && usage.call_id != diagnostic.call_id)
        || (usage.response_id.is_some() && diagnostic.response_id.is_some() && usage.response_id != diagnostic.response_id)
        || (usage.provider.is_some() && diagnostic.provider.is_some() && usage.provider != diagnostic.provider)
        || match (&usage.account_scope, &diagnostic.account_scope) {
            (UsageAccountScope::KnownScope(left), UsageAccountScope::KnownScope(right)) => left != right,
            (UsageAccountScope::KnownScope(_), UsageAccountScope::WriterUnscoped)
            | (UsageAccountScope::WriterUnscoped, UsageAccountScope::KnownScope(_)) => true,
            _ => false,
        }
}

fn same_usage_identity(left: &UsageJoinCall, right: &UsageJoinCall) -> bool {
    left.key == right.key
        && left.source_snapshot_id == right.source_snapshot_id
        && left.started_at == right.started_at
        && left.identity_quarantined == right.identity_quarantined
        && left.root_thread_id == right.root_thread_id
        && left.configured_role == right.configured_role
        && left.observed_model == right.observed_model
        && left.input_tokens_uncached == right.input_tokens_uncached
        && left.input_tokens_cached == right.input_tokens_cached
        && left.input_tokens_cache_write == right.input_tokens_cache_write
        && left.output_tokens == right.output_tokens
        && left.total_tokens == right.total_tokens
        && left.strict_pricing_status == right.strict_pricing_status
        && left.strict_credit_source == right.strict_credit_source
        && left.strict_estimated_credits == right.strict_estimated_credits
        && left.provider_reported_credits == right.provider_reported_credits
        && left.standard_scenario_status == right.standard_scenario_status
        && left.standard_scenario_credits == right.standard_scenario_credits
}

fn add_call(partition: &mut UsagePartition, call: &UsageJoinCall) {
    let usage_present = call.input_tokens_uncached.is_some()
        || call.input_tokens_cached.is_some()
        || call.input_tokens_cache_write.is_some()
        || call.output_tokens.is_some()
        || call.total_tokens.is_some();
    if usage_present { partition.usage_present_call_count += 1; }
    let strict_status_covered = matches!(call.strict_pricing_status.as_deref(), Some("priced_estimate" | "provider_reported"));
    let provider_credits = (strict_status_covered
        && call.strict_credit_source.as_deref() == Some("provider_reported"))
        .then(|| to_credit_micros(call.provider_reported_credits))
        .flatten();
    let strict_estimate = (strict_status_covered
        && call.strict_pricing_status.as_deref() == Some("priced_estimate")
        && call.strict_credit_source.as_deref() == Some("rate_card_estimate"))
        .then(|| to_credit_micros(call.strict_estimated_credits))
        .flatten();
    let strict_covered = match call.strict_credit_source.as_deref() {
        Some("provider_reported") => provider_credits.is_some(),
        Some("rate_card_estimate") => strict_estimate.is_some(),
        _ => false,
    };
    if strict_covered { partition.strict_covered_call_count += 1; }
    else { partition.strict_unpriced_or_missing_call_count += 1; }
    if let Some(value) = provider_credits {
        partition.provider_reported_credit_micros.0 = partition.provider_reported_credit_micros.0.saturating_add(value.0);
    }
    if let Some(value) = strict_estimate {
        partition.strict_rate_estimate_credit_micros.0 = partition.strict_rate_estimate_credit_micros.0.saturating_add(value.0);
    }
    if let Some(status) = call.standard_scenario_status.as_deref() {
        if status == "priced_scenario_estimate"
            && let Some(value) = to_credit_micros(call.standard_scenario_credits)
        {
            partition.standard_scenario_covered_call_count += 1;
            partition.standard_scenario_credit_micros.0 = partition.standard_scenario_credit_micros.0.saturating_add(value.0);
        } else {
            partition.standard_scenario_unpriced_or_missing_call_count += 1;
        }
    }
    add_tokens(&mut partition.input_tokens_uncached_sum, &mut partition.input_tokens_uncached_rows, call.input_tokens_uncached);
    add_tokens(&mut partition.input_tokens_cached_sum, &mut partition.input_tokens_cached_rows, call.input_tokens_cached);
    add_tokens(&mut partition.input_tokens_cache_write_sum, &mut partition.input_tokens_cache_write_rows, call.input_tokens_cache_write);
    add_tokens(&mut partition.output_tokens_sum, &mut partition.output_tokens_rows, call.output_tokens);
    add_tokens(&mut partition.total_tokens_sum, &mut partition.total_tokens_rows, call.total_tokens);
}

fn add_tokens(total: &mut Option<i128>, rows: &mut usize, value: Option<i64>) {
    if let Some(value) = value {
        *total = Some(total.unwrap_or_default().saturating_add(i128::from(value)));
        *rows += 1;
    }
}

fn to_credit_micros(value: Option<f64>) -> Option<CreditMicros> {
    let scaled = value? * CREDIT_SCALE;
    let exact_integer_limit = (1_u64 << 53) as f64;
    (scaled.is_finite() && scaled >= 0.0 && scaled <= exact_integer_limit)
        .then(|| CreditMicros(scaled.round() as i128))
}

#[cfg(test)]
#[path = "usage_tests.rs"]
mod tests;
