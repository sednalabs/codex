use super::ContextualUserFragment;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ContentItemMetadata;
use codex_protocol::models::input_text;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeveloperInstructions {
    instructions: String,
    client_owned: bool,
}

impl DeveloperInstructions {
    pub(crate) fn new(instructions: impl Into<String>) -> Self {
        Self {
            instructions: instructions.into(),
            client_owned: false,
        }
    }

    pub(crate) fn client_owned(instructions: impl Into<String>) -> Self {
        Self {
            instructions: instructions.into(),
            client_owned: true,
        }
    }
}

impl ContextualUserFragment for DeveloperInstructions {
    fn content_metadata(&self) -> ContentItemMetadata {
        ContentItemMetadata::developer_instructions(/*from_additional_requirements*/ false)
    }

    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind(
            if self.client_owned {
                "client.developer_instructions"
            } else {
                "generic.developer_instructions"
            }
            .to_string(),
        )
    }

    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn body(&self) -> ContentItem {
        input_text(self.instructions.clone())
    }
}
