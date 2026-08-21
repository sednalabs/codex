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
const GOAL_ERROR_RETRY_IN_PLACE_ENV: &str = "CODEX_EXPERIMENTAL_GOAL_ERROR_RETRY_IN_PLACE";
''',
    '''const GOAL_ERROR_CONTINUATION_ENV: &str = "CODEX_EXPERIMENTAL_GOAL_ERROR_CONTINUATION";
const GOAL_ERROR_RETRY_IN_PLACE_ENV: &str = "CODEX_EXPERIMENTAL_GOAL_ERROR_RETRY_IN_PLACE";
const GOAL_MULTI_AGENT_STRESS_ENV: &str = "CODEX_EXPERIMENTAL_GOAL_MULTI_AGENT_STRESS";
''',
)

replace_one(
    "codex-rs/core/src/diagnostic_flags.rs",
    '''pub fn goal_error_retry_in_place_enabled() -> bool {
    env_enabled(GOAL_ERROR_RETRY_IN_PLACE_ENV)
}

pub fn suppress_usage_limit_state_updates() -> bool {
''',
    '''pub fn goal_error_retry_in_place_enabled() -> bool {
    env_enabled(GOAL_ERROR_RETRY_IN_PLACE_ENV)
}

pub fn goal_multi_agent_stress_enabled() -> bool {
    env_enabled(GOAL_MULTI_AGENT_STRESS_ENV)
}

pub fn suppress_usage_limit_state_updates() -> bool {
''',
)

replace_one(
    "codex-rs/ext/goal/src/steering.rs",
    '''use std::sync::LazyLock;
''',
    '''use std::sync::LazyLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
''',
)

replace_one(
    "codex-rs/ext/goal/src/steering.rs",
    '''static OBJECTIVE_UPDATED_PROMPT_TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
    parse_embedded_template(
        include_str!("../templates/goals/objective_updated.md"),
        "goals/objective_updated.md",
    )
});
''',
    '''static OBJECTIVE_UPDATED_PROMPT_TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
    parse_embedded_template(
        include_str!("../templates/goals/objective_updated.md"),
        "goals/objective_updated.md",
    )
});
static DIAGNOSTIC_GOAL_PROBE_SEQUENCE: AtomicU64 = AtomicU64::new(1);
''',
)

replace_one(
    "codex-rs/ext/goal/src/steering.rs",
    '''    CONTINUATION_PROMPT_TEMPLATE
        .render([
            ("objective", objective.as_str()),
            ("tokens_used", tokens_used.as_str()),
            ("token_budget", token_budget.as_str()),
            ("remaining_tokens", remaining_tokens.as_str()),
        ])
        .unwrap_or_else(|err| {
            panic!("embedded goals/continuation.md template failed to render: {err}")
        })
}
''',
    '''    let mut prompt = CONTINUATION_PROMPT_TEMPLATE
        .render([
            ("objective", objective.as_str()),
            ("tokens_used", tokens_used.as_str()),
            ("token_budget", token_budget.as_str()),
            ("remaining_tokens", remaining_tokens.as_str()),
        ])
        .unwrap_or_else(|err| {
            panic!("embedded goals/continuation.md template failed to render: {err}")
        });

    if codex_core::diagnostic_flags::goal_multi_agent_stress_enabled() {
        let task_name = diagnostic_goal_probe_task_name();
        prompt.push_str(&format!(
            "\n\nDiagnostic continuation probe:\n\
             - Before other substantive work in this automatic continuation, call the plain `spawn_agent` tool exactly once.\n\
             - Use task_name `{task_name}` and fork_turns `none`. Do not substitute a namespaced/legacy collaboration spawn tool.\n\
             - In the child message, briefly restate the active objective and ask the child to perform one concrete tool-backed inspection or work step that advances it, then report evidence back.\n\
             - The parent should continue its own useful work after issuing the spawn; do not wait for the child unless the normal task flow requires it.\n\
             - If the plain `spawn_agent` call fails or is unavailable, keep the goal active and continue useful work. A later automatic continuation will try a fresh probe.\n"
        ));
    }

    prompt
}

fn diagnostic_goal_probe_task_name() -> String {
    let epoch_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let sequence = DIAGNOSTIC_GOAL_PROBE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("goal_probe_{epoch_millis}_{sequence}")
}
''',
)

replace_one(
    "codex-rs/ext/goal/src/runtime.rs",
    '''        let item = continuation_steering_item(&protocol_goal_from_state(goal));

        if let Err(err) = thread.try_start_turn_if_idle(vec![item]).await {
            let reason = err.reason();
            tracing::debug!(
                ?reason,
                "skipping goal continuation because automatic idle work was rejected"
            );
        }
''',
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
                let reason = err.reason();
                tracing::debug!(
                    ?reason,
                    "skipping goal continuation because automatic idle work was rejected"
                );
            }
        }
''',
)

replace_one(
    "codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs",
    '''    let arguments = function_arguments(payload)?;
    let args: SpawnAgentArgs = parse_arguments(&arguments)?;
''',
    '''    if crate::diagnostic_flags::goal_multi_agent_stress_enabled() {
        turn.session_telemetry.counter(
            "codex.diagnostic.goal_multi_agent_stress",
            1,
            &[("stage", "spawn_handler_attempt")],
        );
        tracing::info!(
            turn_id = %turn.sub_id,
            call_id = %call_id,
            "multi-agent stress diagnostic entered V2 spawn handler"
        );
    }

    let arguments = function_arguments(payload)?;
    let args: SpawnAgentArgs = parse_arguments(&arguments)?;
''',
)

replace_one(
    "codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs",
    '''    let context = AgentCommunicationContext::new(AgentCommunicationKind::Spawn, session.thread_id);
    let spawned_agent = Box::pin(
''',
    '''    let context = AgentCommunicationContext::new(AgentCommunicationKind::Spawn, session.thread_id);
    if crate::diagnostic_flags::goal_multi_agent_stress_enabled() {
        turn.session_telemetry.counter(
            "codex.diagnostic.goal_multi_agent_stress",
            1,
            &[("stage", "spawn_control_attempt")],
        );
        tracing::info!(
            turn_id = %turn.sub_id,
            call_id = %call_id,
            task_name = %args.task_name,
            "multi-agent stress diagnostic calling V2 spawn control"
        );
    }
    let spawned_agent = Box::pin(
''',
)

replace_one(
    "codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs",
    '''    .await
    .map_err(collab_spawn_error)?;
    let new_thread_id = spawned_agent.thread_id;
''',
    '''    .await
    .map_err(collab_spawn_error)?;
    if crate::diagnostic_flags::goal_multi_agent_stress_enabled() {
        turn.session_telemetry.counter(
            "codex.diagnostic.goal_multi_agent_stress",
            1,
            &[("stage", "child_work_submitted")],
        );
    }
    let new_thread_id = spawned_agent.thread_id;
''',
)

replace_one(
    "codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs",
    '''    turn.session_telemetry.counter(
        "codex.multi_agent.spawn",
        /*inc*/ 1,
        &[("role", role_tag), ("version", "v2")],
    );
    let task_name = String::from(new_agent_path);
''',
    '''    turn.session_telemetry.counter(
        "codex.multi_agent.spawn",
        /*inc*/ 1,
        &[("role", role_tag), ("version", "v2")],
    );
    if crate::diagnostic_flags::goal_multi_agent_stress_enabled() {
        turn.session_telemetry.counter(
            "codex.diagnostic.goal_multi_agent_stress",
            1,
            &[("stage", "spawn_published")],
        );
    }
    let task_name = String::from(new_agent_path);
''',
)
