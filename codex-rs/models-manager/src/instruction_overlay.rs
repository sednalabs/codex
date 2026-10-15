//! Fork-owned instruction adjustment for the default OpenAI-compatible catalog.

use codex_protocol::openai_models::ModelInfo;

const BLOCKING_WAIT_SENTENCE: &str = "Avoid performing blocking sleep or wait calls longer than 60 seconds, as they may prevent you from communicating with the user for their duration.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverlayOutcome {
    Applied,
    SentenceAbsent,
}

/// Remove only the obsolete fixed wait-duration guidance from a default-catalog model.
pub(crate) fn apply(model: &mut ModelInfo) -> OverlayOutcome {
    let Some(template) = model
        .model_messages
        .as_mut()
        .and_then(|messages| messages.instructions_template.as_mut())
    else {
        return OverlayOutcome::SentenceAbsent;
    };

    if template.contains(BLOCKING_WAIT_SENTENCE) {
        *template = template.replace(BLOCKING_WAIT_SENTENCE, "");
        tracing::debug!(model = %model.slug, "removed fixed wait-duration instruction from default catalog model");
        OverlayOutcome::Applied
    } else {
        OverlayOutcome::SentenceAbsent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_only_the_fixed_wait_sentence_and_is_idempotent() {
        let mut model = crate::bundled_models_response()
            .expect("bundled models should parse")
            .models
            .into_iter()
            .next()
            .expect("bundled models should contain a model");
        let before = format!(
            "preserved guidance. {BLOCKING_WAIT_SENTENCE} Native event-driven waits remain allowed."
        );
        model
            .model_messages
            .as_mut()
            .expect("bundled model should contain model messages")
            .instructions_template = Some(before);

        assert_eq!(apply(&mut model), OverlayOutcome::Applied);
        assert_eq!(
            model
                .model_messages
                .as_ref()
                .and_then(|messages| messages.instructions_template.as_deref()),
            Some("preserved guidance.  Native event-driven waits remain allowed.")
        );
        let transformed = model.clone();
        assert_eq!(apply(&mut model), OverlayOutcome::SentenceAbsent);
        assert_eq!(model, transformed);
    }
}
