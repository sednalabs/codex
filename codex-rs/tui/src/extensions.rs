use crate::exec_command::split_command_string;
use crate::legacy_core::config::Config;

const CODEX_DYNAMIC_TOOL_COMMAND_ENV_VAR: &str = "CODEX_DYNAMIC_TOOL_COMMAND";

/// Internal TUI extension seam.
///
/// This intentionally mirrors the app-server pattern: keep the upstream-facing
/// surface small and generic, and let Sedna wire in downstream behavior without
/// growing hot orchestration files.
pub(crate) trait TuiHooks: Send + Sync + 'static {
    fn dynamic_tool_command(&self, _config: &Config) -> Option<DynamicToolCommandConfig> {
        None
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DynamicToolCommandConfig {
    pub(crate) command: Vec<String>,
}

pub(crate) fn tui_hooks() -> &'static dyn TuiHooks {
    &SEDNA_TUI_HOOKS
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn noop_tui_hooks() -> &'static dyn TuiHooks {
    &NOOP_TUI_HOOKS
}

#[cfg_attr(not(test), allow(dead_code))]
struct NoopTuiHooks;
#[cfg_attr(not(test), allow(dead_code))]
static NOOP_TUI_HOOKS: NoopTuiHooks = NoopTuiHooks;

impl TuiHooks for NoopTuiHooks {}

struct SednaTuiHooks;
static SEDNA_TUI_HOOKS: SednaTuiHooks = SednaTuiHooks;

impl TuiHooks for SednaTuiHooks {
    fn dynamic_tool_command(&self, _config: &Config) -> Option<DynamicToolCommandConfig> {
        let command = std::env::var(CODEX_DYNAMIC_TOOL_COMMAND_ENV_VAR).ok()?;
        let command = split_command_string(command.trim());
        if command.is_empty() || command.first().is_some_and(String::is_empty) {
            return None;
        }
        Some(DynamicToolCommandConfig { command })
    }
}
