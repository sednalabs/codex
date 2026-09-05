//! Fork-owned instruction adjustments applied after provider catalog composition.
//!
//! This deliberately does not modify the bundled `models.json` source.  The
//! provider marker is supplied only by the OpenAI-compatible manager, so static
//! catalogs and other consumers remain authoritative and untouched.

use codex_protocol::openai_models::ModelInfo;

const CODEX_AUTO_REVIEW_SLUG: &str = "codex-auto-review";
const BLOCKING_WAIT_SENTENCE: &str = "Avoid performing blocking sleep or wait calls longer than 60 seconds, as they may prevent you from communicating with the user for their duration.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverlayOutcome {
    NotApplicable,
    Applied,
    TargetMatchedSentenceAbsent,
}

/// Apply the fork overlay to one fully composed OpenAI-compatible model.
pub(crate) fn apply_openai_compatible(model: &mut ModelInfo) -> OverlayOutcome {
    if model.slug != CODEX_AUTO_REVIEW_SLUG {
        return OverlayOutcome::NotApplicable;
    }

    let mut applied = model.base_instructions.contains(BLOCKING_WAIT_SENTENCE);
    model.base_instructions = remove_sentence(&model.base_instructions);
    if let Some(messages) = model.model_messages.as_mut()
        && let Some(template) = messages.instructions_template.as_mut()
    {
        applied |= template.contains(BLOCKING_WAIT_SENTENCE);
        *template = remove_sentence(template);
    }
    let outcome = if applied {
        OverlayOutcome::Applied
    } else {
        OverlayOutcome::TargetMatchedSentenceAbsent
    };
    tracing::debug!(model = %model.slug, outcome = ?outcome, "openai-compatible instruction overlay");
    outcome
}

fn remove_sentence(instructions: &str) -> String {
    instructions.replace(BLOCKING_WAIT_SENTENCE, "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::openai_models::ModelMessages;

    fn model(slug: &str, base: &str, template: Option<&str>) -> ModelInfo {
        let mut model = crate::bundled_models_response()
            .expect("bundled models should parse")
            .models
            .into_iter()
            .next()
            .expect("bundled models should contain a model");
        model.slug = slug.to_string();
        model.base_instructions = base.to_string();
        model.model_messages = template.map(|template| ModelMessages {
            instructions_template: Some(template.to_string()),
            instructions_variables: None,
            approvals: None,
            auto_review: None,
            permissions: None,
        });
        model
    }

    #[test]
    fn exact_openai_model_is_transformed_in_both_instruction_sources() {
        let source = format!(
            "before {BLOCKING_WAIT_SENTENCE} after; a deliberate short timeout of 5 seconds remains allowed"
        );
        let mut model = model(CODEX_AUTO_REVIEW_SLUG, &source, Some(&source));
        assert_eq!(apply_openai_compatible(&mut model), OverlayOutcome::Applied);
        assert_eq!(
            model.base_instructions,
            "before  after; a deliberate short timeout of 5 seconds remains allowed"
        );
        assert_eq!(
            model.model_messages.as_ref().unwrap().instructions_template,
            Some(
                "before  after; a deliberate short timeout of 5 seconds remains allowed"
                    .to_string()
            )
        );

        let transformed = model.clone();
        assert_eq!(
            apply_openai_compatible(&mut model),
            OverlayOutcome::TargetMatchedSentenceAbsent
        );
        assert_eq!(model, transformed);
    }

    #[test]
    fn missing_marker_or_non_exact_slug_is_unchanged() {
        let source = format!("before {BLOCKING_WAIT_SENTENCE} after");
        for (slug,) in [("custom",), ("codex-auto-review-v2",)] {
            let mut model = model(slug, &source, Some(&source));
            let original = model.clone();
            assert_eq!(
                apply_openai_compatible(&mut model),
                OverlayOutcome::NotApplicable
            );
            assert_eq!(model, original);
        }
    }

    #[test]
    fn exact_target_without_sentence_reports_drift() {
        let mut model = model(CODEX_AUTO_REVIEW_SLUG, "already clean", Some("also clean"));
        assert_eq!(
            apply_openai_compatible(&mut model),
            OverlayOutcome::TargetMatchedSentenceAbsent
        );
        assert_eq!(
            apply_openai_compatible(&mut model),
            OverlayOutcome::TargetMatchedSentenceAbsent
        );
    }
}
