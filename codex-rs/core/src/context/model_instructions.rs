//! A single model-context fragment for independent model and instruction changes.

use super::BaseInstructionsFragment;
use super::ContextualUserFragment;
use codex_context_fragments::AnnotatedContent;
use codex_context_fragments::message_from_parts;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ContentItemMetadata;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::input_text;

const REPLACEMENT_NOTICE: &str =
    "The following instructions replace the previous base instructions:";
const REMOVAL_NOTICE: &str =
    "The previous base instructions no longer apply. No replacement base instructions are active.";

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ModelInstructions {
    pub(crate) model: Option<String>,
    /// None leaves instructions unchanged; Some replaces them, including an empty value.
    pub(crate) instructions: Option<String>,
}

impl ContextualUserFragment for ModelInstructions {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("model.instructions".to_string())
    }

    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<model_instructions>", "</model_instructions>")
    }

    fn body(&self) -> ContentItem {
        let mut body = String::from("\n");
        if let Some(model) = &self.model {
            body.push_str(&format!("The current model is {model}.\n"));
        }
        match &self.instructions {
            Some(instructions) if instructions.is_empty() => {
                body.push_str(REMOVAL_NOTICE);
                body.push('\n');
            }
            Some(instructions) => {
                body.push_str(&format!("{REPLACEMENT_NOTICE}\n\n{instructions}\n"));
            }
            None => {}
        }
        input_text(body)
    }
}

/// A model-instruction replacement whose body is client-owned base guidance.
/// It keeps the normal model-switch explanation in separate classified content so
/// request-scoped overlays can edit only the replaceable baseline body.
pub(crate) struct ClientOwnedModelInstructions(ModelInstructions);

impl ClientOwnedModelInstructions {
    pub(crate) fn from_model_instructions(instructions: ModelInstructions) -> Self {
        Self(instructions)
    }

    pub(crate) fn into_response_item(self) -> ResponseItem {
        let mut notice = String::from("\n");
        if let Some(model) = &self.0.model {
            notice.push_str(&format!("The current model is {model}.\n"));
        }
        let model_instruction_kind = ContentItemKind("model.instructions".to_string());
        let harness_metadata = ContentItemMetadata::harness();
        let mut parts = Vec::with_capacity(3);
        match &self.0.instructions {
            Some(instructions) if !instructions.is_empty() => {
                notice.push_str(&format!("{REPLACEMENT_NOTICE}\n\n"));
                parts.push(AnnotatedContent::text(
                    notice,
                    model_instruction_kind.clone(),
                    harness_metadata.clone(),
                ));
                parts.push(AnnotatedContent::text(
                    instructions.clone(),
                    ContentItemKind(BaseInstructionsFragment::CLIENT_KIND.to_string()),
                    harness_metadata.clone(),
                ));
                parts.push(AnnotatedContent::text(
                    "\n",
                    model_instruction_kind,
                    harness_metadata,
                ));
            }
            Some(_) => {
                notice.push_str(REMOVAL_NOTICE);
                notice.push('\n');
                parts.push(AnnotatedContent::text(
                    notice,
                    model_instruction_kind,
                    harness_metadata,
                ));
            }
            None => parts.push(AnnotatedContent::text(
                notice,
                model_instruction_kind,
                harness_metadata,
            )),
        }
        message_from_parts("developer", parts)
    }
}
