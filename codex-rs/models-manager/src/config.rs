use codex_protocol::config_types::Personality;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::protocol::BaseInstructionsProvenance;

#[derive(Debug, Clone, Default)]
pub struct ModelsManagerConfig {
    pub model_context_window: Option<i64>,
    pub model_auto_compact_token_limit: Option<i64>,
    pub tool_output_token_limit: Option<usize>,
    pub base_instructions: Option<String>,
    /// Source authority for `base_instructions`. Model-owned text is
    /// recomposed for the selected child model before provider overlays.
    pub base_instructions_provenance: BaseInstructionsProvenance,
    pub personality_enabled: bool,
    pub personality: Option<Personality>,
    pub model_catalog: Option<ModelsResponse>,
}
