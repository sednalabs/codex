//! Fork-owned instruction adjustments applied after provider catalog composition.
//!
//! This deliberately does not modify the bundled `models.json` source.  The
//! provider marker is supplied only by the OpenAI-compatible manager, so static
//! catalogs and other consumers remain authoritative and untouched.

use codex_protocol::openai_models::ModelInfo;

const CODEX_AUTO_REVIEW_SLUG: &str = "codex-auto-review";
const OPENAI_COMPATIBLE_PROVIDER: &str = "openai-compatible";
const BLOCKING_WAIT_SENTENCE: &str =
    "Avoid performing blocking sleep or wait calls longer than 60 seconds, as they may prevent you from communicating with the user for their duration.";
#[cfg(test)]
pub(crate) const TEST_BLOCKING_WAIT_SENTENCE: &str = BLOCKING_WAIT_SENTENCE;

/// Apply the Sedna fork overlay to one fully composed provider model.
///
/// All guards are fail-closed: a missing or unexpected provider marker, a
/// non-exact slug, or an absent instruction field leaves the descriptor alone.
pub(crate) fn apply(model: &mut ModelInfo, provider_marker: Option<&str>) {
    if provider_marker != Some(OPENAI_COMPATIBLE_PROVIDER)
        || model.slug != CODEX_AUTO_REVIEW_SLUG
    {
        return;
    }

    // The overlay contract requires the provider's template. Without it there
    // is no safe way to prove that the effective prompt is the intended one.
    if model
        .model_messages
        .as_ref()
        .and_then(|messages| messages.instructions_template.as_ref())
        .is_none()
    {
        return;
    }

    model.base_instructions = remove_sentence(&model.base_instructions);
    if let Some(messages) = model.model_messages.as_mut()
        && let Some(template) = messages.instructions_template.as_mut()
    {
        *template = remove_sentence(template);
    }
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
        let source = format!("before {BLOCKING_WAIT_SENTENCE} after");
        let mut model = model(CODEX_AUTO_REVIEW_SLUG, &source, Some(&source));
        apply(&mut model, Some(OPENAI_COMPATIBLE_PROVIDER));
        assert_eq!(model.base_instructions, "before  after");
        assert_eq!(
            model.model_messages.unwrap().instructions_template,
            Some("before  after".to_string())
        );
    }

    #[test]
    fn missing_marker_or_non_exact_slug_is_unchanged() {
        let source = format!("before {BLOCKING_WAIT_SENTENCE} after");
        for (slug, marker) in [
            (CODEX_AUTO_REVIEW_SLUG, None),
            ("codex-auto-review-v2", Some(OPENAI_COMPATIBLE_PROVIDER)),
        ] {
            let mut model = model(slug, &source, Some(&source));
            let original = model.clone();
            apply(&mut model, marker);
            assert_eq!(model, original);
        }
    }

    #[test]
    fn missing_template_is_unchanged() {
        let source = format!("before {BLOCKING_WAIT_SENTENCE} after");
        let mut model = model(CODEX_AUTO_REVIEW_SLUG, &source, None);
        let original = model.clone();
        apply(&mut model, Some(OPENAI_COMPATIBLE_PROVIDER));
        assert_eq!(model, original);
    }
}
