from pathlib import Path


def replace_one(path: str, old: str, new: str) -> None:
    file_path = Path(path)
    text = file_path.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, found {count}")
    file_path.write_text(text.replace(old, new, 1))


Path("codex-rs/core/src/diagnostic_flags.rs").write_text('''use std::sync::OnceLock;
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
const GOAL_DIAGNOSTIC_MAX_SECONDS_ENV: &str =
    "CODEX_EXPERIMENTAL_GOAL_DIAGNOSTIC_MAX_SECONDS";
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
            Self::PostUsageLimitSpawnBudgetExhausted => {
                "post_usage_limit_spawn_budget_exhausted"
            }
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
''')

replace_one(
    "codex-rs/core/src/session/turn.rs",
    '''async fn run_goal_multi_agent_stress_post_usage_limit_probe(
    tool_runtime: ToolCallRuntime,
    turn_context: Arc<TurnContext>,
    cancellation_token: CancellationToken,
) {
    let task_name = crate::diagnostic_flags::next_goal_multi_agent_probe_task_name("post_429");
    let call_id = format!("diag_{task_name}");
    let tool_name = if turn_context.provider.capabilities().namespace_tools {
        turn_context
            .config
            .multi_agent_v2
            .tool_namespace
            .as_deref()
            .map(|namespace| ToolName::namespaced(namespace, "spawn_agent"))
            .unwrap_or_else(|| ToolName::plain("spawn_agent"))
    } else {
        ToolName::plain("spawn_agent")
    };
    let arguments = serde_json::json!({
        "message": "Run one bounded diagnostic child step: use an available read-only tool to inspect the current environment or worktree, then report one concise evidence-backed fact to the parent.",
        "task_name": task_name,
        "fork_turns": "none"
    })
    .to_string();
    let call = crate::tools::router::ToolCall {
        tool_name: tool_name.clone(),
        call_id: call_id.clone(),
        payload: crate::tools::context::ToolPayload::Function { arguments },
    };

    turn_context.session_telemetry.counter(
        "codex.diagnostic.goal_multi_agent_stress",
        1,
        &[("stage", "post_usage_limit_dispatch_attempt")],
    );
    tracing::info!(
        turn_id = %turn_context.sub_id,
        %call_id,
        tool = %tool_name,
        "multi-agent stress diagnostic dispatching bounded post-usage-limit V2 spawn"
    );

    match tool_runtime
        .handle_tool_call_with_source(
            call,
            crate::tools::router::ToolCallSource::Direct,
            cancellation_token,
        )
        .await
    {
        Ok(_) => {
            turn_context.session_telemetry.counter(
                "codex.diagnostic.goal_multi_agent_stress",
                1,
                &[("stage", "post_usage_limit_dispatch_completed")],
            );
        }
        Err(error) => {
            turn_context.session_telemetry.counter(
                "codex.diagnostic.goal_multi_agent_stress",
                1,
                &[("stage", "post_usage_limit_dispatch_failed")],
            );
            warn!(
                turn_id = %turn_context.sub_id,
                %call_id,
                %error,
                "multi-agent stress diagnostic post-usage-limit V2 spawn failed"
            );
        }
    }
}
''',
    '''fn record_goal_multi_agent_stress_stage(
    turn_context: &TurnContext,
    stage: crate::diagnostic_flags::GoalMultiAgentStressStage,
) {
    turn_context.session_telemetry.counter(
        crate::diagnostic_flags::GOAL_MULTI_AGENT_STRESS_METRIC,
        1,
        &[("stage", stage.as_str())],
    );
}

async fn run_goal_multi_agent_stress_post_usage_limit_probe(
    tool_runtime: ToolCallRuntime,
    turn_context: Arc<TurnContext>,
    cancellation_token: CancellationToken,
) {
    use crate::diagnostic_flags::GoalMultiAgentStressStage;

    if !crate::diagnostic_flags::try_reserve_post_usage_limit_spawn_probe() {
        record_goal_multi_agent_stress_stage(
            turn_context.as_ref(),
            GoalMultiAgentStressStage::PostUsageLimitSpawnBudgetExhausted,
        );
        warn!(
            turn_id = %turn_context.sub_id,
            "multi-agent stress diagnostic post-usage-limit spawn budget exhausted or diagnostic window closed"
        );
        return;
    }

    let task_name = crate::diagnostic_flags::next_goal_multi_agent_probe_task_name("post_429");
    let call_id = format!("diag_{task_name}");
    let tool_name = if turn_context.provider.capabilities().namespace_tools {
        turn_context
            .config
            .multi_agent_v2
            .tool_namespace
            .as_deref()
            .map(|namespace| ToolName::namespaced(namespace, "spawn_agent"))
            .unwrap_or_else(|| ToolName::plain("spawn_agent"))
    } else {
        ToolName::plain("spawn_agent")
    };
    let arguments = match crate::tools::handlers::multi_agents_v2::diagnostic_spawn_arguments(
        "Run one bounded diagnostic child step: use an available read-only tool to inspect the current environment or worktree, then report one concise evidence-backed fact to the parent."
            .to_string(),
        task_name,
    ) {
        Ok(arguments) => arguments,
        Err(error) => {
            record_goal_multi_agent_stress_stage(
                turn_context.as_ref(),
                GoalMultiAgentStressStage::PostUsageLimitDispatchBuildFailed,
            );
            warn!(
                turn_id = %turn_context.sub_id,
                %call_id,
                %error,
                "multi-agent stress diagnostic failed to build V2 spawn arguments"
            );
            return;
        }
    };
    let call = crate::tools::router::ToolCall {
        tool_name: tool_name.clone(),
        call_id: call_id.clone(),
        payload: crate::tools::context::ToolPayload::Function { arguments },
    };

    record_goal_multi_agent_stress_stage(
        turn_context.as_ref(),
        GoalMultiAgentStressStage::PostUsageLimitDispatchAttempt,
    );
    tracing::info!(
        turn_id = %turn_context.sub_id,
        %call_id,
        tool = %tool_name,
        "multi-agent stress diagnostic dispatching bounded post-usage-limit V2 spawn"
    );

    let probe_cancellation_token = cancellation_token.child_token();
    let probe = tool_runtime.handle_tool_call_with_source(
        call,
        crate::tools::router::ToolCallSource::Direct,
        probe_cancellation_token.clone(),
    );
    tokio::pin!(probe);
    let (result, timed_out) = tokio::select! {
        result = &mut probe => (result, false),
        () = tokio::time::sleep(crate::diagnostic_flags::goal_multi_agent_probe_timeout()) => {
            record_goal_multi_agent_stress_stage(
                turn_context.as_ref(),
                GoalMultiAgentStressStage::PostUsageLimitDispatchTimeoutCancelRequested,
            );
            warn!(
                turn_id = %turn_context.sub_id,
                %call_id,
                "multi-agent stress diagnostic probe deadline reached; requesting cooperative tool cancellation"
            );
            probe_cancellation_token.cancel();
            (probe.await, true)
        }
    };

    match result {
        Ok(_) if timed_out => {
            record_goal_multi_agent_stress_stage(
                turn_context.as_ref(),
                GoalMultiAgentStressStage::PostUsageLimitDispatchSettledAfterTimeout,
            );
        }
        Ok(_) => {
            record_goal_multi_agent_stress_stage(
                turn_context.as_ref(),
                GoalMultiAgentStressStage::PostUsageLimitDispatchCompleted,
            );
        }
        Err(error) => {
            record_goal_multi_agent_stress_stage(
                turn_context.as_ref(),
                GoalMultiAgentStressStage::PostUsageLimitDispatchFailed,
            );
            warn!(
                turn_id = %turn_context.sub_id,
                %call_id,
                timed_out,
                %error,
                "multi-agent stress diagnostic post-usage-limit V2 spawn failed"
            );
        }
    }
}
''',
)

replace_one(
    "codex-rs/core/src/session/turn.rs",
    '''    let max_retries = turn_context.provider.info().stream_max_retries();
    let multi_agent_stress_enabled = crate::diagnostic_flags::goal_multi_agent_stress_enabled();
''',
    '''    let max_retries = turn_context.provider.info().stream_max_retries();
    let multi_agent_stress_enabled = crate::diagnostic_flags::goal_multi_agent_stress_active();
''',
)

replace_one(
    "codex-rs/core/src/session/turn.rs",
    '''                CodexErrorDetails::UsageLimitReached(e) => {
                    if !crate::diagnostic_flags::suppress_usage_limit_state_updates() {
''',
    '''                CodexErrorDetails::UsageLimitReached(e) => {
                    crate::diagnostic_flags::goal_diagnostic_mark_usage_limit_observed();
                    if !crate::diagnostic_flags::suppress_usage_limit_state_updates() {
''',
)

replace_one(
    "codex-rs/core/src/session/turn.rs",
    '''        if matches!(err.details(), CodexErrorDetails::UsageLimitReached(_))
            && crate::diagnostic_flags::goal_error_retry_in_place_enabled()
''',
    '''        if matches!(err.details(), CodexErrorDetails::UsageLimitReached(_))
            && crate::diagnostic_flags::goal_error_retry_in_place_active()
''',
)

replace_one(
    "codex-rs/core/src/tools/handlers/multi_agents_v2.rs",
    '''pub(crate) use spawn::Handler as SpawnAgentHandler;
''',
    '''pub(crate) use spawn::Handler as SpawnAgentHandler;
pub(crate) use spawn::diagnostic_spawn_arguments;
''',
)

replace_one(
    "codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs",
    '''    if crate::diagnostic_flags::goal_multi_agent_stress_enabled() {
        turn.session_telemetry.counter(
            "codex.diagnostic.goal_multi_agent_stress",
            1,
            &[("stage", "spawn_handler_attempt")],
        );
''',
    '''    if crate::diagnostic_flags::goal_multi_agent_stress_enabled() {
        turn.session_telemetry.counter(
            crate::diagnostic_flags::GOAL_MULTI_AGENT_STRESS_METRIC,
            1,
            &[(
                "stage",
                crate::diagnostic_flags::GoalMultiAgentStressStage::SpawnHandlerAttempt.as_str(),
            )],
        );
''',
)

replace_one(
    "codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs",
    '''    if crate::diagnostic_flags::goal_multi_agent_stress_enabled() {
        turn.session_telemetry.counter(
            "codex.diagnostic.goal_multi_agent_stress",
            1,
            &[("stage", "spawn_control_attempt")],
        );
''',
    '''    if crate::diagnostic_flags::goal_multi_agent_stress_enabled() {
        turn.session_telemetry.counter(
            crate::diagnostic_flags::GOAL_MULTI_AGENT_STRESS_METRIC,
            1,
            &[(
                "stage",
                crate::diagnostic_flags::GoalMultiAgentStressStage::SpawnControlAttempt.as_str(),
            )],
        );
''',
)

replace_one(
    "codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs",
    '''    if crate::diagnostic_flags::goal_multi_agent_stress_enabled() {
        turn.session_telemetry.counter(
            "codex.diagnostic.goal_multi_agent_stress",
            1,
            &[("stage", "spawn_published")],
        );
''',
    '''    if crate::diagnostic_flags::goal_multi_agent_stress_enabled() {
        turn.session_telemetry.counter(
            crate::diagnostic_flags::GOAL_MULTI_AGENT_STRESS_METRIC,
            1,
            &[(
                "stage",
                crate::diagnostic_flags::GoalMultiAgentStressStage::SpawnPublished.as_str(),
            )],
        );
''',
)

replace_one(
    "codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs",
    '''#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnAgentArgs {
    message: String,
    task_name: String,
    agent_type: Option<String>,
    model: Option<String>,
    expected_model: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
    expected_reasoning_effort: Option<ReasoningEffort>,
    service_tier: Option<String>,
    fork_turns: Option<String>,
    fork_context: Option<bool>,
}

impl SpawnAgentArgs {
''',
    '''#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SpawnAgentArgs {
    message: String,
    task_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<ReasoningEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_reasoning_effort: Option<ReasoningEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fork_turns: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fork_context: Option<bool>,
}

pub(crate) fn diagnostic_spawn_arguments(
    message: String,
    task_name: String,
) -> Result<String, serde_json::Error> {
    serde_json::to_string(&SpawnAgentArgs {
        message,
        task_name,
        agent_type: None,
        model: None,
        expected_model: None,
        reasoning_effort: None,
        expected_reasoning_effort: None,
        service_tier: None,
        fork_turns: Some("none".to_string()),
        fork_context: None,
    })
}

impl SpawnAgentArgs {
''',
)

spawn_path = Path("codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs")
spawn_text = spawn_path.read_text()
if "goal_diagnostic_spawn_arguments_round_trip_through_handler_schema" in spawn_text:
    raise SystemExit("spawn.rs: diagnostic schema test already exists")
spawn_path.write_text(
    spawn_text
    + '''

#[cfg(test)]
mod diagnostic_tests {
    use super::SpawnAgentArgs;
    use super::diagnostic_spawn_arguments;

    #[test]
    fn goal_diagnostic_spawn_arguments_round_trip_through_handler_schema() {
        let arguments = diagnostic_spawn_arguments(
            "inspect one thing".to_string(),
            "goal_probe_test".to_string(),
        )
        .expect("diagnostic spawn arguments should serialize");
        let parsed: SpawnAgentArgs =
            serde_json::from_str(&arguments).expect("diagnostic arguments should parse");

        assert_eq!(parsed.message, "inspect one thing");
        assert_eq!(parsed.task_name, "goal_probe_test");
        assert!(parsed.fork_mode().expect("fork mode should parse").is_none());
        assert!(parsed.agent_type.is_none());
        assert!(parsed.model.is_none());
        assert!(parsed.expected_model.is_none());
        assert!(parsed.reasoning_effort.is_none());
        assert!(parsed.expected_reasoning_effort.is_none());
        assert!(parsed.service_tier.is_none());
        assert!(parsed.fork_context.is_none());
    }
}
'''
)

replace_one(
    "codex-rs/core/src/agent/control/spawn.rs",
    '''            new_thread.thread.session_telemetry().counter(
                "codex.diagnostic.goal_multi_agent_stress",
                1,
                &[("stage", "child_initial_work_submitted")],
            );
''',
    '''            new_thread.thread.session_telemetry().counter(
                crate::diagnostic_flags::GOAL_MULTI_AGENT_STRESS_METRIC,
                1,
                &[(
                    "stage",
                    crate::diagnostic_flags::GoalMultiAgentStressStage::ChildInitialWorkSubmitted
                        .as_str(),
                )],
            );
''',
)

replace_one(
    "codex-rs/ext/goal/src/extension.rs",
    '''            if matches!(input.error, CodexErrorInfo::UsageLimitExceeded)
                && codex_core::diagnostic_flags::goal_error_continuation_enabled()
            {
''',
    '''            let usage_limit_error = matches!(input.error, CodexErrorInfo::UsageLimitExceeded);
            if usage_limit_error {
                codex_core::diagnostic_flags::goal_diagnostic_mark_usage_limit_observed();
            }
            if usage_limit_error && codex_core::diagnostic_flags::goal_error_continuation_active() {
''',
)

replace_one(
    "codex-rs/ext/goal/src/runtime.rs",
    '''        let item = continuation_steering_item(&protocol_goal_from_state(goal));
        let multi_agent_stress = codex_core::diagnostic_flags::goal_multi_agent_stress_enabled();
        if multi_agent_stress {
            thread.session_telemetry().counter(
                "codex.diagnostic.goal_multi_agent_stress",
                1,
                &[("stage", "continuation_attempt")],
            );
        }

        match thread.try_start_turn_if_idle(vec![item]).await {
            Ok(()) => {
                if multi_agent_stress {
                    thread.session_telemetry().counter(
                        "codex.diagnostic.goal_multi_agent_stress",
                        1,
                        &[("stage", "continuation_started")],
                    );
                }
            }
            Err(err) => {
                if multi_agent_stress {
                    thread.session_telemetry().counter(
                        "codex.diagnostic.goal_multi_agent_stress",
                        1,
                        &[("stage", "continuation_rejected")],
                    );
                }
''',
    '''        let post_usage_limit_diagnostic =
            codex_core::diagnostic_flags::goal_error_continuation_enabled()
                && codex_core::diagnostic_flags::goal_diagnostic_usage_limit_observed();
        if post_usage_limit_diagnostic
            && !codex_core::diagnostic_flags::try_reserve_post_usage_limit_continuation()
        {
            if codex_core::diagnostic_flags::goal_multi_agent_stress_enabled() {
                thread.session_telemetry().counter(
                    codex_core::diagnostic_flags::GOAL_MULTI_AGENT_STRESS_METRIC,
                    1,
                    &[(
                        "stage",
                        codex_core::diagnostic_flags::GoalMultiAgentStressStage::ContinuationBudgetExhausted
                            .as_str(),
                    )],
                );
            }
            tracing::warn!(
                "goal error diagnostic continuation budget exhausted or diagnostic window closed; leaving active goal idle"
            );
            return Ok(());
        }

        let item = continuation_steering_item(&protocol_goal_from_state(goal));
        let multi_agent_stress = codex_core::diagnostic_flags::goal_multi_agent_stress_active();
        if multi_agent_stress {
            thread.session_telemetry().counter(
                codex_core::diagnostic_flags::GOAL_MULTI_AGENT_STRESS_METRIC,
                1,
                &[(
                    "stage",
                    codex_core::diagnostic_flags::GoalMultiAgentStressStage::ContinuationAttempt
                        .as_str(),
                )],
            );
        }

        match thread.try_start_turn_if_idle(vec![item]).await {
            Ok(()) => {
                if multi_agent_stress {
                    thread.session_telemetry().counter(
                        codex_core::diagnostic_flags::GOAL_MULTI_AGENT_STRESS_METRIC,
                        1,
                        &[(
                            "stage",
                            codex_core::diagnostic_flags::GoalMultiAgentStressStage::ContinuationStarted
                                .as_str(),
                        )],
                    );
                }
            }
            Err(err) => {
                if multi_agent_stress {
                    thread.session_telemetry().counter(
                        codex_core::diagnostic_flags::GOAL_MULTI_AGENT_STRESS_METRIC,
                        1,
                        &[(
                            "stage",
                            codex_core::diagnostic_flags::GoalMultiAgentStressStage::ContinuationRejected
                                .as_str(),
                        )],
                    );
                }
''',
)

replace_one(
    "codex-rs/ext/goal/src/steering.rs",
    '''    if codex_core::diagnostic_flags::goal_multi_agent_stress_enabled() {
''',
    '''    if codex_core::diagnostic_flags::goal_multi_agent_stress_active() {
''',
)

turn_tests_path = Path("codex-rs/core/src/session/turn_tests.rs")
turn_tests = turn_tests_path.read_text()
if "goal_diagnostic_continuation_marker_is_detected" in turn_tests:
    raise SystemExit("turn_tests.rs: diagnostic marker tests already exist")
turn_tests_path.write_text(
    turn_tests
    + '''

fn goal_diagnostic_input_text_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn goal_diagnostic_continuation_marker_is_detected() {
    let input = vec![goal_diagnostic_input_text_message(&format!(
        "before {GOAL_MULTI_AGENT_STRESS_CONTINUATION_MARKER} after"
    ))];
    assert!(goal_multi_agent_stress_continuation_input(&input));
}

#[test]
fn goal_diagnostic_continuation_marker_does_not_match_unmarked_input() {
    let input = vec![goal_diagnostic_input_text_message("ordinary continuation")];
    assert!(!goal_multi_agent_stress_continuation_input(&input));
}
'''
)
