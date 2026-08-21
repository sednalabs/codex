from pathlib import Path


def replace_one(path: str, old: str, new: str) -> None:
    file_path = Path(path)
    text = file_path.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, found {count}")
    file_path.write_text(text.replace(old, new, 1))


replace_one(
    "codex-rs/core/src/diagnostic_flags.rs",
    '''const GOAL_ERROR_CONTINUATION_ENV: &str = "CODEX_EXPERIMENTAL_GOAL_ERROR_CONTINUATION";

pub fn goal_error_continuation_enabled() -> bool {
    std::env::var(GOAL_ERROR_CONTINUATION_ENV)
        .ok()
        .is_some_and(|value| is_truthy(value.as_str()))
}
''',
    '''const GOAL_ERROR_CONTINUATION_ENV: &str = "CODEX_EXPERIMENTAL_GOAL_ERROR_CONTINUATION";
const GOAL_ERROR_RETRY_IN_PLACE_ENV: &str = "CODEX_EXPERIMENTAL_GOAL_ERROR_RETRY_IN_PLACE";

pub fn goal_error_continuation_enabled() -> bool {
    env_enabled(GOAL_ERROR_CONTINUATION_ENV)
}

pub fn goal_error_retry_in_place_enabled() -> bool {
    env_enabled(GOAL_ERROR_RETRY_IN_PLACE_ENV)
}

pub fn suppress_usage_limit_state_updates() -> bool {
    goal_error_continuation_enabled() || goal_error_retry_in_place_enabled()
}

fn env_enabled(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .is_some_and(|value| is_truthy(value.as_str()))
}
''',
)

replace_one(
    "codex-rs/core/src/session/turn.rs",
    '''    let max_retries = turn_context.provider.info().stream_max_retries();
    let mut retries = 0;
    let mut capacity_retries = 0;
''',
    '''    let max_retries = turn_context.provider.info().stream_max_retries();
    let mut retries = 0;
    let mut usage_limit_retries = 0;
    let mut capacity_retries = 0;
''',
)

replace_one(
    "codex-rs/core/src/session/turn.rs",
    '''                CodexErrorDetails::UsageLimitReached(e) => {
                    if !crate::diagnostic_flags::goal_error_continuation_enabled() {
                        let rate_limits = e.rate_limits.clone();
                        if let Some(rate_limits) = rate_limits {
                            sess.update_rate_limits(&turn_context, *rate_limits).await;
                        }
                    } else {
                        warn!(
                            turn_id = %turn_context.sub_id,
                            "goal error continuation diagnostic mode skipped rate-limit snapshot update"
                        );
                    }
                    return Err(err);
                }
''',
    '''                CodexErrorDetails::UsageLimitReached(e) => {
                    if !crate::diagnostic_flags::suppress_usage_limit_state_updates() {
                        let rate_limits = e.rate_limits.clone();
                        if let Some(rate_limits) = rate_limits {
                            sess.update_rate_limits(&turn_context, *rate_limits).await;
                        }
                    } else {
                        warn!(
                            turn_id = %turn_context.sub_id,
                            "goal error diagnostic mode skipped rate-limit snapshot update"
                        );
                    }
                    err
                }
''',
)

replace_one(
    "codex-rs/core/src/session/turn.rs",
    '''        if original_input.is_none() {
            original_input = Some(prompt.input);
        }

        if matches!(err.details(), CodexErrorDetails::ServerOverloaded) {
''',
    '''        if original_input.is_none() {
            original_input = Some(prompt.input);
        }

        if matches!(err.details(), CodexErrorDetails::UsageLimitReached(_))
            && crate::diagnostic_flags::goal_error_retry_in_place_enabled()
        {
            if usage_limit_retries >= max_retries {
                return Err(err);
            }
            usage_limit_retries += 1;
            let retry_count = usage_limit_retries;
            let delay = err
                .retry_delay()
                .unwrap_or_else(|| crate::util::backoff(retry_count));
            warn!(
                turn_id = %turn_context.sub_id,
                retry_count,
                max_retries,
                ?delay,
                "retrying usage-limit diagnostic sampling request in place"
            );
            tokio::time::sleep(delay).await;
            turn_context.turn_timing_state.record_sampling_retry();
            continue;
        }

        if matches!(err.details(), CodexErrorDetails::ServerOverloaded) {
''',
)
