//! Compatibility normalization for spawn-request provenance.

use codex_protocol::openai_models::ReasoningEffort;

/// Normalizes the V1 omitted-override sentinel before it reaches v2 request provenance.
///
/// Legacy rollout records encoded an absent model/effort override as an empty model paired with
/// the default `Medium` effort. The pair is meaningful only on legacy spawn-start boundaries;
/// callers must not use this to normalize a confirmed terminal effective identity.
pub(crate) fn normalize_legacy_spawn_requested_identity(
    model: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
) -> (Option<String>, Option<ReasoningEffort>) {
    if model.as_deref() == Some("") && reasoning_effort == Some(ReasoningEffort::Medium) {
        (None, None)
    } else {
        (model, reasoning_effort)
    }
}
