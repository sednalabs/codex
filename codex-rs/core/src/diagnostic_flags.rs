//! Explicit, opt-in controls for continuity and provider-boundary research.
//!
//! These controls are intentionally environment based so an operator can select
//! one seam at a time without changing normal defaults. `*_RESEARCH_HARNESS`
//! enables the complete set for a controlled mock-provider run. None of the
//! controls changes credentials, endpoints, quota identity, or provider
//! responses.

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_otel::SessionTelemetry;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::UsageLimitReachedError;
use codex_protocol::protocol::RateLimitReachedType;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;

const RESEARCH_HARNESS_ENV: &str = "CODEX_EXPERIMENTAL_CONTINUITY_RESEARCH_HARNESS";
const PRESERVE_AFTER_USAGE_LIMIT_ENV: &str =
    "CODEX_EXPERIMENTAL_CONTINUITY_PRESERVE_AFTER_USAGE_LIMIT";
const SUPPRESS_USAGE_LIMIT_SNAPSHOT_ENV: &str =
    "CODEX_EXPERIMENTAL_CONTINUITY_SUPPRESS_USAGE_LIMIT_SNAPSHOT";
const RETRY_SAME_TURN_ENV: &str = "CODEX_EXPERIMENTAL_CONTINUITY_RETRY_SAME_TURN";
const UNBOUNDED_SEQUENTIAL_RETRY_ENV: &str =
    "CODEX_EXPERIMENTAL_CONTINUITY_UNBOUNDED_SEQUENTIAL_RETRY";
const V2_POST_USAGE_LIMIT_SPAWN_ENV: &str =
    "CODEX_EXPERIMENTAL_CONTINUITY_V2_POST_USAGE_LIMIT_SPAWN";
const OBSERVATION_ENV: &str = "CODEX_EXPERIMENTAL_CONTINUITY_OBSERVATION";

static GOAL_MULTI_AGENT_PROBE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Enable the optional continuation prompt probe used by the complete
/// research profile.
pub fn continuity_continuation_probe_enabled() -> bool {
    continuity_research_harness_enabled()
}

/// Preserve an active persisted goal for the normal idle continuation path
/// after the provider authoritatively reports a usage limit.
pub fn continuity_preserve_after_usage_limit_enabled() -> bool {
    selected(PRESERVE_AFTER_USAGE_LIMIT_ENV)
}

/// Keep the local rate-limit snapshot unchanged after a provider usage-limit
/// response. This is intentionally independent from goal preservation.
pub fn continuity_suppress_usage_limit_snapshot_enabled() -> bool {
    selected(SUPPRESS_USAGE_LIMIT_SNAPSHOT_ENV)
}

/// Retry a usage-limit response sequentially within the same turn. The normal
/// retry budget remains in force unless the separate unbounded control is set.
pub fn continuity_retry_same_turn_enabled() -> bool {
    selected(RETRY_SAME_TURN_ENV)
}

/// Remove the client attempt-count ceiling for the diagnostic same-turn retry.
/// Attempts still execute one at a time and wait for provider/fallback backoff.
pub fn continuity_unbounded_sequential_retry_enabled() -> bool {
    selected(UNBOUNDED_SEQUENTIAL_RETRY_ENV)
}

/// Dispatch one bounded V2 child probe after an authoritative usage-limit
/// response on an eligible parent turn.
pub fn continuity_v2_post_usage_limit_spawn_enabled() -> bool {
    selected(V2_POST_USAGE_LIMIT_SPAWN_ENV)
}

/// Enable stage and provider-outcome telemetry for the research harness.
pub fn continuity_observation_enabled() -> bool {
    selected(OBSERVATION_ENV) || continuity_research_harness_enabled()
}

/// True when the complete explicit research profile is selected.
pub fn continuity_research_harness_enabled() -> bool {
    env_enabled(RESEARCH_HARNESS_ENV)
}

/// Return true only for the explicit temporary rate-limit response. Quota and
/// workspace entitlement denials are deliberately excluded even though the
/// client-facing protocol maps them to the same usage-limit error.
pub fn is_temporary_usage_limit_error(details: &CodexErrorDetails) -> bool {
    let CodexErrorDetails::UsageLimitReached(error) = details else {
        return false;
    };
    is_temporary_usage_limit(error)
}

fn is_temporary_usage_limit(error: &UsageLimitReachedError) -> bool {
    match error.rate_limit_reached_type {
        Some(RateLimitReachedType::RateLimitReached) => true,
        Some(
            RateLimitReachedType::WorkspaceOwnerCreditsDepleted
            | RateLimitReachedType::WorkspaceMemberCreditsDepleted
            | RateLimitReachedType::WorkspaceOwnerUsageLimitReached
            | RateLimitReachedType::WorkspaceMemberUsageLimitReached,
        ) => false,
        // The upstream `usage_limit_reached` error is itself the temporary
        // class. Older responses may omit `resets_at`; the explicit
        // entitlement/rejection kinds above remain fail-closed.
        None => true,
    }
}

/// Identify the child created by the automatic diagnostic probe from durable
/// V2 session metadata. This keeps its restrictive tool admission in force
/// after a cold reload without widening restrictions for model-created agents.
/// The source variant is host-authored; task names and agent paths are not
/// security or provenance signals.
pub fn is_continuity_diagnostic_child(source: &SessionSource) -> bool {
    matches!(
        source,
        SessionSource::SubAgent(SubAgentSource::ContinuityDiagnostic { .. })
    )
}

pub fn continuity_observation_origin(source: &SessionSource) -> &'static str {
    if is_continuity_diagnostic_child(source) {
        "direct_probe"
    } else {
        "model_driven"
    }
}

/// Return the immutable host-generated diagnostic chain used to join parent
/// spawn, child publication, transport, and outcome observations.
pub fn continuity_observation_chain_id(source: &SessionSource) -> Option<String> {
    let SessionSource::SubAgent(SubAgentSource::ContinuityDiagnostic { chain_id, .. }) = source
    else {
        return None;
    };
    Some(chain_id.clone())
}

/// Build a causal request id with the immutable parent and child identities
/// carried by the host-authored diagnostic source. This is a join key for
/// trace logs; aggregate counters intentionally remain low-cardinality.
pub fn continuity_observation_request_correlation(
    source: &SessionSource,
    child_thread_id: &str,
    turn_id: &str,
    request_id: &str,
) -> Option<String> {
    let SessionSource::SubAgent(SubAgentSource::ContinuityDiagnostic {
        chain_id,
        parent_thread_id,
        parent_turn_id,
        spawn_call_id,
        parent_sampling_request_id,
        ..
    }) = source
    else {
        return None;
    };
    Some(format!(
        "continuity:{chain_id}:parent_thread:{parent_thread_id}:parent_turn:{parent_turn_id}:spawn:{spawn_call_id}:parent_sampling_request:{parent_sampling_request_id}:child_thread:{child_thread_id}:turn:{turn_id}:request:{request_id}"
    ))
}

pub fn continuity_observation_child_correlation(
    source: &SessionSource,
    child_thread_id: ThreadId,
    stage: &str,
) -> Option<String> {
    let SessionSource::SubAgent(SubAgentSource::ContinuityDiagnostic {
        chain_id,
        parent_thread_id,
        parent_turn_id,
        spawn_call_id,
        parent_sampling_request_id,
        ..
    }) = source
    else {
        return None;
    };
    Some(format!(
        "continuity:{chain_id}:parent_thread:{parent_thread_id}:parent_turn:{parent_turn_id}:spawn:{spawn_call_id}:parent_sampling_request:{parent_sampling_request_id}:child_thread:{child_thread_id}:{stage}"
    ))
}

/// Identity of the most recent client sampling request. The host stores this
/// before opening the provider stream so a subsequent diagnostic probe can
/// join itself to the exact request that received a usage-limit response.
#[derive(Clone, Debug)]
pub struct ContinuitySamplingRequestIdentity {
    pub request_id: String,
    pub correlation_id: String,
}

/// Record a bounded continuity stage without including provider or account
/// identity in the diagnostic event.
pub fn record_continuity_stage(
    telemetry: &SessionTelemetry,
    actor: &'static str,
    stage: &'static str,
) {
    record_continuity_stage_with_context(telemetry, actor, stage, "unspecified", None);
}

pub fn record_continuity_stage_with_context(
    telemetry: &SessionTelemetry,
    actor: &'static str,
    stage: &'static str,
    origin: &'static str,
    correlation_id: Option<&str>,
) {
    if continuity_observation_enabled() {
        tracing::debug!(
            continuity_actor = actor,
            continuity_stage = stage,
            continuity_origin = origin,
            continuity_correlation_id = correlation_id.unwrap_or("unknown"),
            "continuity observation stage recorded"
        );
        telemetry.counter(
            "codex.diagnostic.continuity_observation",
            /*inc*/ 1,
            &[("actor", actor), ("stage", stage), ("origin", origin)],
        );
    }
}

/// Record a stage that has an exact child identity. Unlike the aggregate
/// counters, this event is suitable for causal joins because the child id is
/// emitted at the point the manager creates the child runtime.
pub fn record_continuity_stage_with_child_context(
    telemetry: &SessionTelemetry,
    actor: &'static str,
    stage: &'static str,
    origin: &'static str,
    correlation_id: Option<&str>,
    child_thread_id: ThreadId,
) {
    if continuity_observation_enabled() {
        tracing::debug!(
            continuity_actor = actor,
            continuity_stage = stage,
            continuity_origin = origin,
            continuity_correlation_id = correlation_id.unwrap_or("unknown"),
            continuity_child_thread_id = %child_thread_id,
            "continuity observation stage recorded"
        );
        telemetry.counter(
            "codex.diagnostic.continuity_observation",
            /*inc*/ 1,
            &[("actor", actor), ("stage", stage), ("origin", origin)],
        );
    }
}

/// Record the provider result category observed by the client. The category is
/// deliberately coarse and never claims a rejected request was accepted.
pub fn record_continuity_provider_outcome(
    telemetry: &SessionTelemetry,
    actor: &'static str,
    outcome: &'static str,
) {
    record_continuity_provider_outcome_with_context(telemetry, actor, outcome, "unspecified", None);
}

pub fn record_continuity_provider_outcome_with_context(
    telemetry: &SessionTelemetry,
    actor: &'static str,
    outcome: &'static str,
    origin: &'static str,
    correlation_id: Option<&str>,
) {
    if continuity_observation_enabled() {
        tracing::debug!(
            continuity_actor = actor,
            continuity_outcome = outcome,
            continuity_origin = origin,
            continuity_correlation_id = correlation_id.unwrap_or("unknown"),
            "continuity observation provider outcome recorded"
        );
        telemetry.counter(
            "codex.diagnostic.continuity_observation",
            /*inc*/ 1,
            &[
                ("actor", actor),
                ("stage", "provider_outcome"),
                ("outcome", outcome),
                ("origin", origin),
            ],
        );
    }
}

pub fn next_continuity_probe_task_name(kind: &str) -> String {
    let epoch_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let sequence = GOAL_MULTI_AGENT_PROBE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("continuity_{kind}_{epoch_millis}_{sequence}")
}

pub fn next_continuity_correlation_id(kind: &str) -> String {
    let sequence = GOAL_MULTI_AGENT_PROBE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("continuity_{kind}_{sequence}")
}

pub fn suppress_usage_limit_state_updates() -> bool {
    continuity_suppress_usage_limit_snapshot_enabled()
}

fn env_enabled(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .is_some_and(|value| is_truthy(value.as_str()))
}

fn selected(name: &str) -> bool {
    env_enabled(name) || continuity_research_harness_enabled()
}

fn is_truthy(value: &str) -> bool {
    let value = value.trim();
    value == "1"
        || value.eq_ignore_ascii_case("true")
        || value.eq_ignore_ascii_case("yes")
        || value.eq_ignore_ascii_case("on")
}

#[cfg(test)]
mod tests {
    use super::continuity_observation_child_correlation;
    use super::continuity_observation_request_correlation;
    use super::is_continuity_diagnostic_child;
    use super::is_temporary_usage_limit_error;
    use super::is_truthy;
    use codex_protocol::ThreadId;
    use codex_protocol::error::CodexErrorDetails;
    use codex_protocol::error::UsageLimitReachedError;
    use codex_protocol::protocol::RateLimitReachedType;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::SubAgentSource;

    fn usage_limit(rate_limit_reached_type: Option<RateLimitReachedType>) -> CodexErrorDetails {
        CodexErrorDetails::UsageLimitReached(UsageLimitReachedError {
            plan_type: None,
            resets_at: None,
            rate_limits: None,
            promo_message: None,
            rate_limit_reached_type,
        })
    }

    #[test]
    fn parses_truthy_values() {
        for value in ["1", "true", "TRUE", "yes", "On", " on "] {
            assert!(is_truthy(value), "expected {value:?} to be truthy");
        }
    }

    #[test]
    fn rejects_other_values() {
        for value in ["", "0", "false", "off", "no", "anything"] {
            assert!(!is_truthy(value), "expected {value:?} to be falsey");
        }
    }

    #[test]
    fn only_explicit_temporary_rate_limits_are_continuable() {
        assert!(is_temporary_usage_limit_error(&usage_limit(Some(
            RateLimitReachedType::RateLimitReached,
        ))));
        // Older providers omit the subtype while still returning the explicit
        // usage-limit error; that remains a temporary limit rather than an
        // entitlement denial.
        assert!(is_temporary_usage_limit_error(&usage_limit(None)));
        for denial in [
            RateLimitReachedType::WorkspaceOwnerCreditsDepleted,
            RateLimitReachedType::WorkspaceMemberCreditsDepleted,
            RateLimitReachedType::WorkspaceOwnerUsageLimitReached,
            RateLimitReachedType::WorkspaceMemberUsageLimitReached,
        ] {
            assert!(!is_temporary_usage_limit_error(&usage_limit(Some(denial))));
        }
        assert!(!is_temporary_usage_limit_error(
            &CodexErrorDetails::QuotaExceeded
        ));
        assert!(!is_temporary_usage_limit_error(
            &CodexErrorDetails::UsageNotIncluded
        ));
    }

    #[test]
    fn diagnostic_child_detection_requires_host_authored_source() {
        let parent_thread_id = ThreadId::from_string("22222222-2222-4222-8222-222222222222")
            .expect("valid parent thread id");
        let source = |path, diagnostic| {
            if diagnostic {
                SessionSource::SubAgent(SubAgentSource::ContinuityDiagnostic {
                    parent_thread_id,
                    depth: 1,
                    agent_path: Some(path.parse().expect("valid agent path")),
                    agent_nickname: None,
                    agent_role: None,
                    chain_id: "chain-1".to_string(),
                    parent_turn_id: "turn-1".to_string(),
                    spawn_call_id: "call-1".to_string(),
                    parent_sampling_request_id: "request-1".to_string(),
                })
            } else {
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id,
                    depth: 1,
                    agent_path: Some(path.parse().expect("valid agent path")),
                    agent_nickname: None,
                    agent_role: None,
                })
            }
        };

        assert!(is_continuity_diagnostic_child(&source(
            "/root/model_worker",
            true
        )));
        assert!(!is_continuity_diagnostic_child(&source(
            "/root/continuity_probe",
            false
        )));
        assert!(!is_continuity_diagnostic_child(&SessionSource::SubAgent(
            SubAgentSource::Review
        )));

        let request_correlation = continuity_observation_request_correlation(
            &source("/root/diagnostic", true),
            "33333333-3333-4333-8333-333333333333",
            "child-turn",
            "child-request",
        )
        .expect("diagnostic request should have a causal correlation");
        assert!(request_correlation.contains("parent_sampling_request:request-1"));
        assert!(request_correlation.contains("child_thread:33333333-3333-4333-8333-333333333333"));
        assert!(
            continuity_observation_child_correlation(
                &source("/root/diagnostic", true),
                parent_thread_id,
                "child_created",
            )
            .expect("diagnostic child stage should have a causal correlation")
            .contains("parent_sampling_request:request-1")
        );
    }
}
