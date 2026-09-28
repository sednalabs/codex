//! Fork-owned instruction adjustments applied after provider catalog composition.
//!
//! The bundled catalog remains provider-owned.  This overlay is applied only
//! to exact OpenAI model descriptors after they have been selected from a
//! remote, cached, or configured static catalog.

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

/// Apply the exact fork overlay to one fully composed OpenAI model.
pub(crate) fn apply_openai_compatible(model: &mut ModelInfo) -> OverlayOutcome {
    if !TARGET_SLUGS.contains(&model.slug.as_str()) {
        return OverlayOutcome::NotApplicable;
    }

    let applied = model
        .model_messages
        .as_mut()
        .and_then(|messages| messages.instructions_template.as_mut())
        .is_some_and(transform);
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

    fn model(slug: &str, template: Option<&str>) -> ModelInfo {
        let mut model = crate::bundled_models_response()
            .expect("bundled models should parse")
            .models
            .into_iter()
            .next()
            .expect("bundled models should contain a model");
        model.slug = slug.to_string();
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
    fn exact_target_transforms_canonical_instruction_source() {
        let source = format!(
            "before {COMMENTARY_CADENCE_LITERAL} middle {BLOCKING_WAIT_LITERAL} after; a deliberate short timeout of 5 seconds remains allowed"
        );
        let mut model = model("codex-auto-review", Some(&source));
        assert_eq!(apply_openai_compatible(&mut model), OverlayOutcome::Applied);
        let instructions = model
            .model_messages
            .as_ref()
            .and_then(|messages| messages.instructions_template.as_deref())
            .expect("canonical instructions template");
        assert!(instructions.contains(COMMENTARY_CADENCE_REPLACEMENT));
        assert!(!instructions.contains(COMMENTARY_CADENCE_LITERAL));
        assert!(!instructions.contains(BLOCKING_WAIT_LITERAL));

        let transformed = model.clone();
        assert_eq!(
            apply_openai_compatible(&mut model),
            OverlayOutcome::TargetMatchedSentenceAbsent
        );
        assert_eq!(model, transformed);
    }

    #[test]
    fn all_exact_targets_transform_and_namespaced_variants_do_not() {
        let source = format!("{COMMENTARY_CADENCE_LITERAL}\n{BLOCKING_WAIT_LITERAL}");
        for slug in TARGET_SLUGS {
            let mut model = model(slug, Some(&source));
            assert_eq!(apply_openai_compatible(&mut model), OverlayOutcome::Applied);
        }
        for slug in [
            "custom",
            "codex-auto-review-v2",
            "openai-codex/codex-auto-review",
        ] {
            let mut model = model(slug, Some(&source));
            let original = model.clone();
            assert_eq!(
                apply_openai_compatible(&mut model),
                OverlayOutcome::NotApplicable
            );
            assert_eq!(model, original);
        }
    }

    #[test]
    fn exact_target_without_marker_reports_drift() {
        let mut model = model("codex-auto-review", Some("also clean"));
        assert_eq!(
            apply_openai_compatible(&mut model),
            OverlayOutcome::TargetMatchedSentenceAbsent
        );
    }
}
