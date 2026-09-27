use codex_protocol::config_types::Personality;
use codex_protocol::openai_models::ModelsResponse;

#[derive(Debug, Clone, Default)]
pub struct ModelsManagerConfig {
    pub model_context_window: Option<i64>,
    pub model_auto_compact_token_limit: Option<i64>,
    pub tool_output_token_limit: Option<usize>,
    pub base_instructions: Option<String>,
    /// The value in `base_instructions` came from an already-resolved parent
    /// model, rather than from an operator override.  Recompose it from the
    /// selected child's catalog entry before applying model-specific overlays.
    pub base_instructions_are_inherited: bool,
    pub personality_enabled: bool,
    pub personality: Option<Personality>,
    pub model_catalog: Option<ModelsResponse>,
}
