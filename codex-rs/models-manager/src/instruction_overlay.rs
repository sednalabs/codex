//! Fork-owned instruction adjustment for the default OpenAI-compatible catalog.

use codex_protocol::openai_models::ModelInfo;

const FIXED_WAIT_SENTENCE: &str = "Avoid performing blocking sleep or wait calls longer than 60 seconds, as they may prevent you from communicating with the user for their duration.";
const FIXED_COMMENTARY_CADENCE_SENTENCE: &str = "The user appreciates consistent, frequent communication during your turn, and should not be left without a commentary update for more than 60 seconds during ongoing work.";
const TARGETED_SENTENCES: [&str; 2] = [FIXED_WAIT_SENTENCE, FIXED_COMMENTARY_CADENCE_SENTENCE];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverlayOutcome {
    Applied,
    SentenceAbsent,
}

/// Remove only the two fixed 60-second cadence rules from a default-catalog model.
pub(crate) fn apply(model: &mut ModelInfo) -> OverlayOutcome {
    let Some(template) = model
        .model_messages
        .as_mut()
        .and_then(|messages| messages.instructions_template.as_mut())
    else {
        return OverlayOutcome::SentenceAbsent;
    };

    let mut removed_any = false;
    for sentence in TARGETED_SENTENCES {
        if template.contains(sentence) {
            *template = template.replace(sentence, "");
            removed_any = true;
        }
    }
    if removed_any {
        tracing::debug!(model = %model.slug, "removed fixed 60-second cadence instructions from default catalog model");
        OverlayOutcome::Applied
    } else {
        OverlayOutcome::SentenceAbsent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_only_the_fixed_60_second_rules_and_is_idempotent() {
        let mut model = crate::bundled_models_response()
            .expect("bundled models should parse")
            .models
            .into_iter()
            .next()
            .expect("bundled models should contain a model");
        let before = format!(
            "preserved guidance. {FIXED_WAIT_SENTENCE} Keep commentary useful and concise. {FIXED_COMMENTARY_CADENCE_SENTENCE} Native event-driven waits remain allowed."
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
            Some(
                "preserved guidance.  Keep commentary useful and concise.  Native event-driven waits remain allowed."
            )
        );
        let transformed = model.clone();
        assert_eq!(apply(&mut model), OverlayOutcome::SentenceAbsent);
        assert_eq!(model, transformed);
    }
}
