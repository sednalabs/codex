use crate::legacy_core::config::Config;
use codex_app_server_client::dynamic_tool_host_command_from_env;

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
        let command = dynamic_tool_host_command_from_env()?;
        Some(DynamicToolCommandConfig { command })
    }
}
