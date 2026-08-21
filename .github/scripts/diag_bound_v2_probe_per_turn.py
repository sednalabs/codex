from pathlib import Path


def replace_one(path: str, old: str, new: str) -> None:
    file_path = Path(path)
    text = file_path.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, found {count}")
    file_path.write_text(text.replace(old, new, 1))


replace_one(
    "codex-rs/core/src/session/turn.rs",
    '''const GOAL_MULTI_AGENT_STRESS_CONTINUATION_MARKER: &str = "Diagnostic continuation probe:";
''',
    '''const GOAL_MULTI_AGENT_STRESS_CONTINUATION_MARKER: &str =
    "<goal_multi_agent_stress_continuation_probe>";

#[derive(Clone, Copy, Debug)]
struct GoalMultiAgentStressTurn;

#[derive(Clone, Copy, Debug)]
struct GoalMultiAgentStressPostUsageLimitProbeDispatched;
''',
)

replace_one(
    "codex-rs/core/src/session/turn.rs",
    '''    let max_retries = turn_context.provider.info().stream_max_retries();
    let multi_agent_stress_goal_turn = crate::diagnostic_flags::goal_multi_agent_stress_enabled()
        && turn_context.multi_agent_version == codex_protocol::protocol::MultiAgentVersion::V2
        && !matches!(
            &turn_context.session_source,
            codex_protocol::protocol::SessionSource::SubAgent(_)
        )
        && goal_multi_agent_stress_continuation_input(&input);
    let mut retries = 0;
    let mut usage_limit_retries = 0;
    let mut post_usage_limit_v2_probe_dispatched = false;
''',
    '''    let max_retries = turn_context.provider.info().stream_max_retries();
    let multi_agent_stress_enabled = crate::diagnostic_flags::goal_multi_agent_stress_enabled();
    if multi_agent_stress_enabled && goal_multi_agent_stress_continuation_input(&input) {
        turn_context.extension_data.insert(GoalMultiAgentStressTurn);
    }
    let multi_agent_stress_goal_turn = multi_agent_stress_enabled
        && turn_context.multi_agent_version == codex_protocol::protocol::MultiAgentVersion::V2
        && !matches!(
            &turn_context.session_source,
            codex_protocol::protocol::SessionSource::SubAgent(_)
        )
        && turn_context
            .extension_data
            .get::<GoalMultiAgentStressTurn>()
            .is_some();
    let mut retries = 0;
    let mut usage_limit_retries = 0;
''',
)

replace_one(
    "codex-rs/core/src/session/turn.rs",
    '''                    if multi_agent_stress_goal_turn && !post_usage_limit_v2_probe_dispatched {
                        post_usage_limit_v2_probe_dispatched = true;
                        run_goal_multi_agent_stress_post_usage_limit_probe(
''',
    '''                    if multi_agent_stress_goal_turn
                        && turn_context
                            .extension_data
                            .get::<GoalMultiAgentStressPostUsageLimitProbeDispatched>()
                            .is_none()
                    {
                        turn_context
                            .extension_data
                            .insert(GoalMultiAgentStressPostUsageLimitProbeDispatched);
                        run_goal_multi_agent_stress_post_usage_limit_probe(
''',
)

replace_one(
    "codex-rs/ext/goal/src/steering.rs",
    '''        prompt.push_str(&format!(
            "

Diagnostic continuation probe:
''',
    '''        prompt.push_str(&format!(
            "

<goal_multi_agent_stress_continuation_probe>
Diagnostic continuation probe:
''',
)
