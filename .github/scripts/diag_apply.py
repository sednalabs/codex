from pathlib import Path


def replace_one(path: str, old: str, new: str) -> None:
    file_path = Path(path)
    text = file_path.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, found {count}")
    file_path.write_text(text.replace(old, new, 1))


def replace_all(path: str, old: str, new: str) -> None:
    file_path = Path(path)
    text = file_path.read_text()
    count = text.count(old)
    if count < 1:
        raise SystemExit(f"{path}: expected at least one match, found {count}")
    print(f"{path}: replacing {count} matching block(s)")
    file_path.write_text(text.replace(old, new))


replace_one(
    "codex-rs/core/src/lib.rs",
    "pub mod config;\npub mod connectors;",
    "pub mod config;\npub mod diagnostic_flags;\npub mod connectors;",
)

replace_all(
    "codex-rs/core/src/session/turn.rs",
    "                CodexErrorDetails::UsageLimitReached(e) => {\n"
    "                    let rate_limits = e.rate_limits.clone();\n"
    "                    if let Some(rate_limits) = rate_limits {\n"
    "                        sess.update_rate_limits(&turn_context, *rate_limits).await;\n"
    "                    }\n"
    "                    return Err(err);\n"
    "                }\n",
    "                CodexErrorDetails::UsageLimitReached(e) => {\n"
    "                    if !crate::diagnostic_flags::goal_error_continuation_enabled() {\n"
    "                        let rate_limits = e.rate_limits.clone();\n"
    "                        if let Some(rate_limits) = rate_limits {\n"
    "                            sess.update_rate_limits(&turn_context, *rate_limits).await;\n"
    "                        }\n"
    "                    } else {\n"
    "                        warn!(\n"
    "                            turn_id = %turn_context.sub_id,\n"
    "                            \"goal error continuation diagnostic mode skipped rate-limit snapshot update\"\n"
    "                        );\n"
    "                    }\n"
    "                    return Err(err);\n"
    "                }\n",
)

replace_one(
    "codex-rs/ext/goal/src/extension.rs",
    "            let Some(runtime) = goal_runtime_handle(input.thread_store) else {\n"
    "                return;\n"
    "            };\n\n"
    "            let reason = match input.error {\n",
    "            let Some(runtime) = goal_runtime_handle(input.thread_store) else {\n"
    "                return;\n"
    "            };\n\n"
    "            if matches!(input.error, CodexErrorInfo::UsageLimitExceeded)\n"
    "                && codex_core::diagnostic_flags::goal_error_continuation_enabled()\n"
    "            {\n"
    "                if let Err(err) = runtime\n"
    "                    .preserve_active_goal_after_turn_error(input.turn_id)\n"
    "                    .await\n"
    "                {\n"
    "                    tracing::warn!(\n"
    "                        error = ?input.error,\n"
    "                        \"failed to preserve active goal after turn error in diagnostic mode: {err}\"\n"
    "                    );\n"
    "                }\n"
    "                return;\n"
    "            }\n\n"
    "            let reason = match input.error {\n",
)

replace_one(
    "codex-rs/ext/goal/src/runtime.rs",
    "    pub async fn usage_limit_active_goal_for_turn(&self, turn_id: &str) -> Result<(), String> {\n"
    "        self.stop_active_goal_for_turn(turn_id, ActiveGoalStopReason::UsageLimit)\n"
    "            .await\n"
    "    }\n\n"
    "    /// Accounts the ending turn and stops its active goal after a terminal error.\n",
    "    pub async fn usage_limit_active_goal_for_turn(&self, turn_id: &str) -> Result<(), String> {\n"
    "        self.stop_active_goal_for_turn(turn_id, ActiveGoalStopReason::UsageLimit)\n"
    "            .await\n"
    "    }\n\n"
    "    pub(crate) async fn preserve_active_goal_after_turn_error(\n"
    "        &self,\n"
    "        turn_id: &str,\n"
    "    ) -> Result<(), String> {\n"
    "        if !self.is_enabled() {\n"
    "            return Ok(());\n"
    "        }\n\n"
    "        let _goal_state_permit = self.goal_state_permit().await?;\n"
    "        let accounting = self.accounting_state();\n"
    "        if !accounting.turn_is_current_active_goal(turn_id) {\n"
    "            accounting.finish_turn(turn_id);\n"
    "            return Ok(());\n"
    "        }\n\n"
    "        let goal = self\n"
    "            .inner\n"
    "            .state_dbs\n"
    "            .thread_goals()\n"
    "            .get_thread_goal(self.thread_id())\n"
    "            .await\n"
    "            .map_err(|err| err.to_string())?;\n\n"
    "        accounting.finish_turn(turn_id);\n"
    "        match goal {\n"
    "            Some(goal) if goal.status == codex_state::ThreadGoalStatus::Active => {\n"
    "                accounting.mark_idle_goal_active(goal.goal_id);\n"
    "            }\n"
    "            Some(_) | None => accounting.clear_active_goal(),\n"
    "        }\n"
    "        Ok(())\n"
    "    }\n\n"
    "    /// Accounts the ending turn and stops its active goal after a terminal error.\n",
)

Path("codex-rs/core/src/diagnostic_flags.rs").write_text(
    "const GOAL_ERROR_CONTINUATION_ENV: &str =\n"
    "    \"CODEX_EXPERIMENTAL_GOAL_ERROR_CONTINUATION\";\n\n"
    "pub fn goal_error_continuation_enabled() -> bool {\n"
    "    std::env::var(GOAL_ERROR_CONTINUATION_ENV)\n"
    "        .ok()\n"
    "        .is_some_and(|value| is_truthy(value.as_str()))\n"
    "}\n\n"
    "fn is_truthy(value: &str) -> bool {\n"
    "    let value = value.trim();\n"
    "    value == \"1\"\n"
    "        || value.eq_ignore_ascii_case(\"true\")\n"
    "        || value.eq_ignore_ascii_case(\"yes\")\n"
    "        || value.eq_ignore_ascii_case(\"on\")\n"
    "}\n\n"
    "#[cfg(test)]\n"
    "mod tests {\n"
    "    use super::is_truthy;\n\n"
    "    #[test]\n"
    "    fn parses_truthy_values() {\n"
    "        for value in [\"1\", \"true\", \"TRUE\", \"yes\", \"On\", \" on \"] {\n"
    "            assert!(is_truthy(value), \"expected {value:?} to be truthy\");\n"
    "        }\n"
    "    }\n\n"
    "    #[test]\n"
    "    fn rejects_other_values() {\n"
    "        for value in [\"\", \"0\", \"false\", \"off\", \"no\", \"anything\"] {\n"
    "            assert!(!is_truthy(value), \"expected {value:?} to be falsey\");\n"
    "        }\n"
    "    }\n"
    "}\n"
)
