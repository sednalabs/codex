use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

const GOAL_ERROR_CONTINUATION_ENV: &str = "CODEX_EXPERIMENTAL_GOAL_ERROR_CONTINUATION";
const GOAL_ERROR_RETRY_IN_PLACE_ENV: &str = "CODEX_EXPERIMENTAL_GOAL_ERROR_RETRY_IN_PLACE";
const GOAL_MULTI_AGENT_STRESS_ENV: &str = "CODEX_EXPERIMENTAL_GOAL_MULTI_AGENT_STRESS";
const GOAL_DIAGNOSTIC_MAX_SECONDS_ENV: &str = "CODEX_EXPERIMENTAL_GOAL_DIAGNOSTIC_MAX_SECONDS";
const GOAL_DIAGNOSTIC_MAX_CONTINUATIONS_ENV: &str =
    "CODEX_EXPERIMENTAL_GOAL_DIAGNOSTIC_MAX_CONTINUATIONS";
const GOAL_DIAGNOSTIC_MAX_POST_USAGE_LIMIT_SPAWNS_ENV: &str =
    "CODEX_EXPERIMENTAL_GOAL_DIAGNOSTIC_MAX_POST_USAGE_LIMIT_SPAWNS";
const GOAL_DIAGNOSTIC_PROBE_TIMEOUT_SECONDS_ENV: &str =
    "CODEX_EXPERIMENTAL_GOAL_DIAGNOSTIC_PROBE_TIMEOUT_SECONDS";

const DEFAULT_GOAL_DIAGNOSTIC_MAX_SECONDS: u64 = 30 * 60;
const DEFAULT_GOAL_DIAGNOSTIC_MAX_CONTINUATIONS: u64 = 32;
const DEFAULT_GOAL_DIAGNOSTIC_MAX_POST_USAGE_LIMIT_SPAWNS: u64 = 16;
const DEFAULT_GOAL_DIAGNOSTIC_PROBE_TIMEOUT_SECONDS: u64 = 15;

pub const GOAL_MULTI_AGENT_STRESS_METRIC: &str = "codex.diagnostic.goal_multi_agent_stress";

static GOAL_MULTI_AGENT_PROBE_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static GOAL_DIAGNOSTIC_STARTED_AT: OnceLock<Instant> = OnceLock::new();
static GOAL_DIAGNOSTIC_USAGE_LIMIT_OBSERVED: AtomicBool = AtomicBool::new(false);
static GOAL_DIAGNOSTIC_CONTINUATIONS: AtomicU64 = AtomicU64::new(0);
static GOAL_DIAGNOSTIC_POST_USAGE_LIMIT_SPAWNS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GoalMultiAgentStressStage {
    ContinuationAttempt,
    ContinuationStarted,
    ContinuationRejected,
    ContinuationBudgetExhausted,
    PostUsageLimitSpawnBudgetExhausted,
    PostUsageLimitDispatchBuildFailed,
    PostUsageLimitDispatchAttempt,
    PostUsageLimitDispatchTimeoutCancelRequested,
    PostUsageLimitDispatchSettledAfterTimeout,
    PostUsageLimitDispatchCompleted,
    PostUsageLimitDispatchFailed,
    SpawnHandlerAttempt,
    SpawnControlAttempt,
    ChildInitialWorkSubmitted,
    SpawnPublished,
}

impl GoalMultiAgentStressStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ContinuationAttempt => "continuation_attempt",
            Self::ContinuationStarted => "continuation_started",
            Self::ContinuationRejected => "continuation_rejected",
            Self::ContinuationBudgetExhausted => "continuation_budget_exhausted",
            Self::PostUsageLimitSpawnBudgetExhausted => "post_usage_limit_spawn_budget_exhausted",
            Self::PostUsageLimitDispatchBuildFailed => "post_usage_limit_dispatch_build_failed",
            Self::PostUsageLimitDispatchAttempt => "post_usage_limit_dispatch_attempt",
            Self::PostUsageLimitDispatchTimeoutCancelRequested => {
                "post_usage_limit_dispatch_timeout_cancel_requested"
            }
            Self::PostUsageLimitDispatchSettledAfterTimeout => {
                "post_usage_limit_dispatch_settled_after_timeout"
            }
            Self::PostUsageLimitDispatchCompleted => "post_usage_limit_dispatch_completed",
            Self::PostUsageLimitDispatchFailed => "post_usage_limit_dispatch_failed",
            Self::SpawnHandlerAttempt => "spawn_handler_attempt",
            Self::SpawnControlAttempt => "spawn_control_attempt",
            Self::ChildInitialWorkSubmitted => "child_initial_work_submitted",
            Self::SpawnPublished => "spawn_published",
        }
    }
}

pub fn goal_error_continuation_enabled() -> bool {
    env_enabled(GOAL_ERROR_CONTINUATION_ENV)
}

pub fn goal_error_retry_in_place_enabled() -> bool {
    env_enabled(GOAL_ERROR_RETRY_IN_PLACE_ENV)
}

pub fn goal_multi_agent_stress_enabled() -> bool {
    env_enabled(GOAL_MULTI_AGENT_STRESS_ENV)
}

pub fn goal_diagnostic_mark_usage_limit_observed() {
    if !goal_diagnostic_mode_enabled() {
        return;
    }
    GOAL_DIAGNOSTIC_STARTED_AT.get_or_init(Instant::now);
    GOAL_DIAGNOSTIC_USAGE_LIMIT_OBSERVED.store(true, Ordering::Release);
}

pub fn goal_diagnostic_usage_limit_observed() -> bool {
    GOAL_DIAGNOSTIC_USAGE_LIMIT_OBSERVED.load(Ordering::Acquire)
}

pub fn goal_diagnostic_window_open() -> bool {
    let Some(started_at) = GOAL_DIAGNOSTIC_STARTED_AT.get() else {
        return true;
    };
    window_open_for(
        started_at.elapsed(),
        env_u64_or_default(
            GOAL_DIAGNOSTIC_MAX_SECONDS_ENV,
            DEFAULT_GOAL_DIAGNOSTIC_MAX_SECONDS,
        ),
    )
}

pub fn goal_error_continuation_active() -> bool {
    goal_error_continuation_enabled() && goal_diagnostic_window_open()
}

pub fn goal_error_retry_in_place_active() -> bool {
    goal_error_retry_in_place_enabled() && goal_diagnostic_window_open()
}

pub fn goal_multi_agent_stress_active() -> bool {
    goal_multi_agent_stress_enabled() && goal_diagnostic_window_open()
}

pub fn next_goal_multi_agent_probe_task_name(kind: &str) -> String {
    let epoch_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let sequence = GOAL_MULTI_AGENT_PROBE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("goal_{kind}_{epoch_millis}_{sequence}")
}

pub fn suppress_usage_limit_state_updates() -> bool {
    (goal_error_continuation_enabled() || goal_error_retry_in_place_enabled())
        && goal_diagnostic_window_open()
}

pub fn try_reserve_post_usage_limit_continuation() -> bool {
    if !goal_diagnostic_usage_limit_observed() {
        return true;
    }
    goal_diagnostic_window_open()
        && try_reserve(
            &GOAL_DIAGNOSTIC_CONTINUATIONS,
            env_u64_or_default(
                GOAL_DIAGNOSTIC_MAX_CONTINUATIONS_ENV,
                DEFAULT_GOAL_DIAGNOSTIC_MAX_CONTINUATIONS,
            ),
        )
}

pub fn try_reserve_post_usage_limit_spawn_probe() -> bool {
    if !goal_diagnostic_usage_limit_observed() {
        return true;
    }
    goal_diagnostic_window_open()
        && try_reserve(
            &GOAL_DIAGNOSTIC_POST_USAGE_LIMIT_SPAWNS,
            env_u64_or_default(
                GOAL_DIAGNOSTIC_MAX_POST_USAGE_LIMIT_SPAWNS_ENV,
                DEFAULT_GOAL_DIAGNOSTIC_MAX_POST_USAGE_LIMIT_SPAWNS,
            ),
        )
}

pub fn goal_multi_agent_probe_timeout() -> Duration {
    Duration::from_secs(env_u64_or_default(
        GOAL_DIAGNOSTIC_PROBE_TIMEOUT_SECONDS_ENV,
        DEFAULT_GOAL_DIAGNOSTIC_PROBE_TIMEOUT_SECONDS,
    ))
}

fn goal_diagnostic_mode_enabled() -> bool {
    goal_error_continuation_enabled()
        || goal_error_retry_in_place_enabled()
        || goal_multi_agent_stress_enabled()
}

fn try_reserve(counter: &AtomicU64, limit: u64) -> bool {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            (current < limit).then_some(current + 1)
        })
        .is_ok()
}

fn env_u64_or_default(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| parse_u64(value.as_str()))
        .unwrap_or(default)
}

fn parse_u64(value: &str) -> Option<u64> {
    value.trim().parse::<u64>().ok()
}

fn window_open_for(elapsed: Duration, max_seconds: u64) -> bool {
    elapsed < Duration::from_secs(max_seconds)
}

fn env_enabled(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .is_some_and(|value| is_truthy(value.as_str()))
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
    use super::parse_u64;
    use super::try_reserve;
    use super::window_open_for;
    use std::sync::atomic::AtomicU64;
    use std::time::Duration;

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
    fn goal_diagnostic_numeric_limits_accept_zero_and_trimmed_values() {
        assert_eq!(parse_u64("0"), Some(0));
        assert_eq!(parse_u64(" 15 "), Some(15));
        assert_eq!(parse_u64("-1"), None);
        assert_eq!(parse_u64("nope"), None);
    }

    #[test]
    fn goal_diagnostic_budget_reservation_stops_at_limit() {
        let counter = AtomicU64::new(0);
        assert!(try_reserve(&counter, 2));
        assert!(try_reserve(&counter, 2));
        assert!(!try_reserve(&counter, 2));
    }

    #[test]
    fn goal_diagnostic_window_deadline_is_bounded() {
        assert!(window_open_for(Duration::from_secs(14), 15));
        assert!(!window_open_for(Duration::from_secs(15), 15));
        assert!(!window_open_for(Duration::ZERO, 0));
    }
}
