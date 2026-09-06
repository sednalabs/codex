//! Fork-owned instruction adjustments applied after provider catalog composition.
//!
//! This deliberately does not modify the bundled `models.json` source.  The
//! provider marker is supplied only by the OpenAI-compatible manager, so static
//! catalogs and other consumers remain authoritative and untouched.

use codex_protocol::openai_models::ModelInfo;

const TARGET_SLUGS: &[&str] = &[
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "codex-auto-review",
    "gpt-6-astra",
    "gpt-daybreak-blue-latest",
    "gpt-daybreak-red-latest",
];
const COMMENTARY_CADENCE_LITERAL: &str = "If the user's request requires calling tools, start with a message in the `commentary` channel. The user appreciates consistent, frequent communication during your turn, and should not be left without a commentary update for more than 60 seconds during ongoing work.";
const COMMENTARY_CADENCE_REPLACEMENT: &str = "If the user's request requires calling tools, start with a message in the `commentary` channel. Keep the user informed with concise updates when active work is progressing or a meaningful state changes; passive waits that remain interruptible by mailbox, user steer, or cancellation do not require periodic narration.";
const BLOCKING_WAIT_LITERAL: &str = "- Avoid performing blocking sleep or wait calls longer than 60 seconds, as they may prevent you from communicating with the user for their duration.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverlayOutcome {
    NotApplicable,
    Applied,
    TargetMatchedSentenceAbsent,
}

/// Apply the fork overlay to one fully composed OpenAI-compatible model.
pub(crate) fn apply_openai_compatible(model: &mut ModelInfo) -> OverlayOutcome {
    if !TARGET_SLUGS.contains(&model.slug.as_str()) {
        return OverlayOutcome::NotApplicable;
    }

    let mut applied = transform(&mut model.base_instructions);
    if let Some(messages) = model.model_messages.as_mut()
        && let Some(template) = messages.instructions_template.as_mut()
    {
        applied |= transform(template);
    }
    let outcome = if applied {
        OverlayOutcome::Applied
    } else {
        OverlayOutcome::TargetMatchedSentenceAbsent
    };
    tracing::debug!(model = %model.slug, outcome = ?outcome, "openai-compatible instruction overlay");
    outcome
}

fn transform(instructions: &mut String) -> bool {
    let cadence = instructions.contains(COMMENTARY_CADENCE_LITERAL);
    let blocking = instructions.contains(BLOCKING_WAIT_LITERAL);
    if cadence {
        *instructions =
            instructions.replace(COMMENTARY_CADENCE_LITERAL, COMMENTARY_CADENCE_REPLACEMENT);
    }
    if blocking {
        *instructions = instructions.replace(BLOCKING_WAIT_LITERAL, "");
    }
    cadence || blocking
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
            "before {COMMENTARY_CADENCE_LITERAL} middle {BLOCKING_WAIT_LITERAL} after; a deliberate short timeout of 5 seconds remains allowed"
        );
        let mut model = model("codex-auto-review", &source, Some(&source));
        assert_eq!(apply_openai_compatible(&mut model), OverlayOutcome::Applied);
        assert_eq!(
            model.base_instructions,
            format!(
                "before {COMMENTARY_CADENCE_REPLACEMENT} middle  after; a deliberate short timeout of 5 seconds remains allowed"
            )
        );
        assert_eq!(
            model.model_messages.as_ref().unwrap().instructions_template,
            Some(
                format!(
                    "before {COMMENTARY_CADENCE_REPLACEMENT} middle  after; a deliberate short timeout of 5 seconds remains allowed"
                )
            )
        );

        assert!(
            model
                .base_instructions
                .contains(COMMENTARY_CADENCE_REPLACEMENT)
        );
        assert!(model.base_instructions.contains("active work is progressing"));
        assert!(model.base_instructions.contains("passive waits that remain interruptible by mailbox, user steer, or cancellation"));
        assert!(model.base_instructions.contains("deliberate short timeout of 5 seconds remains allowed"));
        assert!(!model.base_instructions.contains(COMMENTARY_CADENCE_LITERAL));
        assert!(!model.base_instructions.contains(BLOCKING_WAIT_LITERAL));

        let transformed = model.clone();
        assert_eq!(
            apply_openai_compatible(&mut model),
            OverlayOutcome::TargetMatchedSentenceAbsent
        );
        assert_eq!(model, transformed);
    }

    #[test]
    fn all_target_slugs_transform_both_fields_and_astra_shape() {
        let source =
            format!("{COMMENTARY_CADENCE_LITERAL}\n{BLOCKING_WAIT_LITERAL}");
        for slug in TARGET_SLUGS {
            let mut model = model(slug, &source, Some(&source));
            assert_eq!(
                apply_openai_compatible(&mut model),
                OverlayOutcome::Applied,
                "{slug}"
            );
            assert!(!model.base_instructions.contains(COMMENTARY_CADENCE_LITERAL));
            assert!(!model.base_instructions.contains(BLOCKING_WAIT_LITERAL));
            let template = model.model_messages.unwrap().instructions_template.unwrap();
            assert!(!template.contains(COMMENTARY_CADENCE_LITERAL));
            assert!(!template.contains(BLOCKING_WAIT_LITERAL));
        }

        let mut astra = model("gpt-6-astra", "", Some(&source));
        assert_eq!(apply_openai_compatible(&mut astra), OverlayOutcome::Applied);
        assert_eq!(astra.base_instructions, "");
    }

    #[test]
    fn missing_marker_or_non_exact_slug_is_unchanged() {
        let source = format!(
            "before {COMMENTARY_CADENCE_LITERAL} middle {BLOCKING_WAIT_LITERAL} after"
        );
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
    fn non_target_preserves_provider_literals_byte_for_byte() {
        let source = format!(
            "prefix {COMMENTARY_CADENCE_LITERAL} middle {BLOCKING_WAIT_LITERAL} suffix"
        );
        let mut model = model("gpt-5.5", &source, Some(&source));
        let original = model.clone();
        assert_eq!(apply_openai_compatible(&mut model), OverlayOutcome::NotApplicable);
        assert_eq!(model, original);
    }

    #[test]
    fn exact_target_without_sentence_reports_drift() {
        let mut model = model("codex-auto-review", "already clean", Some("also clean"));
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
