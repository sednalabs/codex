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

// Compatibility aliases retained for operators using the earlier experimental
// surface. New runs should use the neutral names above.
const GOAL_ERROR_CONTINUATION_ENV: &str = "CODEX_EXPERIMENTAL_GOAL_ERROR_CONTINUATION";
const GOAL_ERROR_RETRY_IN_PLACE_ENV: &str = "CODEX_EXPERIMENTAL_GOAL_ERROR_RETRY_IN_PLACE";
const GOAL_MULTI_AGENT_STRESS_ENV: &str = "CODEX_EXPERIMENTAL_GOAL_MULTI_AGENT_STRESS";

static GOAL_MULTI_AGENT_PROBE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[doc(hidden)]
pub fn goal_error_continuation_enabled() -> bool {
    continuity_preserve_after_usage_limit_enabled()
}

#[doc(hidden)]
pub fn goal_error_retry_in_place_enabled() -> bool {
    continuity_retry_same_turn_enabled()
}

#[doc(hidden)]
pub fn goal_multi_agent_stress_enabled() -> bool {
    env_enabled(GOAL_MULTI_AGENT_STRESS_ENV) || continuity_research_harness_enabled()
}

/// Enable the optional continuation prompt probe used by the complete
/// research profile (or by the preserved legacy stress alias).
pub fn continuity_continuation_probe_enabled() -> bool {
    env_enabled(GOAL_MULTI_AGENT_STRESS_ENV) || continuity_research_harness_enabled()
}

/// Preserve an active persisted goal for the normal idle continuation path
/// after the provider authoritatively reports a usage limit.
pub fn continuity_preserve_after_usage_limit_enabled() -> bool {
    selected(PRESERVE_AFTER_USAGE_LIMIT_ENV) || env_enabled(GOAL_ERROR_CONTINUATION_ENV)
}

/// Keep the local rate-limit snapshot unchanged after a provider usage-limit
/// response. This is intentionally independent from goal preservation.
pub fn continuity_suppress_usage_limit_snapshot_enabled() -> bool {
    selected(SUPPRESS_USAGE_LIMIT_SNAPSHOT_ENV)
}

/// Retry a usage-limit response sequentially within the same turn. The normal
/// retry budget remains in force unless the separate unbounded control is set.
pub fn continuity_retry_same_turn_enabled() -> bool {
    selected(RETRY_SAME_TURN_ENV) || env_enabled(GOAL_ERROR_RETRY_IN_PLACE_ENV)
}

/// Remove the client attempt-count ceiling for the diagnostic same-turn retry.
/// Attempts still execute one at a time and wait for provider/fallback backoff.
pub fn continuity_unbounded_sequential_retry_enabled() -> bool {
    selected(UNBOUNDED_SEQUENTIAL_RETRY_ENV)
}

/// Dispatch one bounded V2 child probe after an authoritative usage-limit
/// response on an eligible parent turn.
pub fn continuity_v2_post_usage_limit_spawn_enabled() -> bool {
    selected(V2_POST_USAGE_LIMIT_SPAWN_ENV) || env_enabled(GOAL_MULTI_AGENT_STRESS_ENV)
}

/// Enable stage and provider-outcome telemetry for the research harness.
pub fn continuity_observation_enabled() -> bool {
    selected(OBSERVATION_ENV)
        || env_enabled(GOAL_MULTI_AGENT_STRESS_ENV)
        || continuity_research_harness_enabled()
}

/// True when the complete explicit research profile is selected.
pub fn continuity_research_harness_enabled() -> bool {
    env_enabled(RESEARCH_HARNESS_ENV)
}

/// Record a bounded continuity stage without including provider or account
/// identity in the diagnostic event.
pub fn record_continuity_stage(
    telemetry: &SessionTelemetry,
    actor: &'static str,
    stage: &'static str,
) {
    if continuity_observation_enabled() {
        telemetry.counter(
            "codex.diagnostic.continuity_observation",
            1,
            &[("actor", actor), ("stage", stage)],
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
    if continuity_observation_enabled() {
        telemetry.counter(
            "codex.diagnostic.continuity_observation",
            1,
            &[
                ("actor", actor),
                ("stage", "provider_outcome"),
                ("outcome", outcome),
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

#[doc(hidden)]
pub fn next_goal_multi_agent_probe_task_name(kind: &str) -> String {
    next_continuity_probe_task_name(kind)
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
    use super::is_truthy;

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
}
