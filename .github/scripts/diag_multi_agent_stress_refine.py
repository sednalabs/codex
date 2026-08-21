from pathlib import Path


def replace_one(path: str, old: str, new: str) -> None:
    file_path = Path(path)
    text = file_path.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, found {count}")
    file_path.write_text(text.replace(old, new, 1))


replace_one(
    "codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs",
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
    '''    .await
    .map_err(collab_spawn_error)?;
    let new_thread_id = spawned_agent.thread_id;
''',
)

replace_one(
    "codex-rs/core/src/agent/control/spawn.rs",
    '''        if let Err(error) = initial_input_result {
            if let Err(cleanup_error) = self
                .reconcile_unpublished_spawn(
                    &state,
                    new_thread.thread_id,
                    &new_thread.thread,
                    &mut reservation,
                    &mut residency_slot,
                )
                .await
            {
                tracing::error!(
                    child_thread_id = %new_thread.thread_id,
                    spawn_error = %error,
                    %cleanup_error,
                    "failed to reconcile unpublished child after initial delivery error"
                );
            }
            return Err(error);
        }

        #[cfg(test)]
''',
    '''        if let Err(error) = initial_input_result {
            if let Err(cleanup_error) = self
                .reconcile_unpublished_spawn(
                    &state,
                    new_thread.thread_id,
                    &new_thread.thread,
                    &mut reservation,
                    &mut residency_slot,
                )
                .await
            {
                tracing::error!(
                    child_thread_id = %new_thread.thread_id,
                    spawn_error = %error,
                    %cleanup_error,
                    "failed to reconcile unpublished child after initial delivery error"
                );
            }
            return Err(error);
        }

        if multi_agent_version == MultiAgentVersion::V2
            && crate::diagnostic_flags::goal_multi_agent_stress_enabled()
        {
            new_thread.thread.session_telemetry().counter(
                "codex.diagnostic.goal_multi_agent_stress",
                1,
                &[("stage", "child_initial_work_submitted")],
            );
            tracing::info!(
                child_thread_id = %new_thread.thread_id,
                "multi-agent stress diagnostic submitted V2 child initial work"
            );
        }

        #[cfg(test)]
''',
)
