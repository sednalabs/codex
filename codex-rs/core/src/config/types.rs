//! Compatibility re-exports for callers that still import `codex_core::config::types`.

pub use codex_config::types::*;

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

/// Downstream-only display style for the weekly limit UI pacing indicator.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum WeeklyLimitPacingStyle {
    #[default]
    Qualitative,
    Ratio,
}
