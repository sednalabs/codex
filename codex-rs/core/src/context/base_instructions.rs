//! Initial base instructions, recorded in history for incremental-tools windows.

use super::ContextualUserFragment;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::input_text;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BaseInstructionsFragment(pub(crate) String);

impl BaseInstructionsFragment {
    pub(crate) const KIND: &str = "model.base_instructions";
    pub(crate) const CLIENT_KIND: &str = "client.base_instructions";

    pub(crate) fn with_client_ownership(self, client_owned: bool) -> impl ContextualUserFragment {
        ClassifiedBaseInstructionsFragment {
            fragment: self,
            client_owned,
        }
    }

    /// Reads only a dedicated, harness-classified base item, including an explicitly empty base.
    pub(crate) fn text_from_item(item: &ResponseItem) -> Option<&str> {
        let ResponseItem::Message {
            role,
            content,
            internal_chat_message_metadata_passthrough: Some(metadata),
            ..
        } = item
        else {
            return None;
        };
        match (
            role.as_str(),
            content.as_slice(),
            metadata.content_item_kinds.as_deref(),
        ) {
            ("developer", [ContentItem::InputText { text }], Some([kind]))
                if matches!(kind.as_str(), Self::KIND | Self::CLIENT_KIND) =>
            {
                Some(text)
            }
            _ => None,
        }
    }
}

struct ClassifiedBaseInstructionsFragment {
    fragment: BaseInstructionsFragment,
    client_owned: bool,
}

impl ContextualUserFragment for ClassifiedBaseInstructionsFragment {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind(
            if self.client_owned {
                BaseInstructionsFragment::CLIENT_KIND
            } else {
                BaseInstructionsFragment::KIND
            }
            .to_string(),
        )
    }

    fn role(&self) -> &'static str {
        self.fragment.role()
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn body(&self) -> ContentItem {
        self.fragment.body()
    }
}

impl ContextualUserFragment for BaseInstructionsFragment {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind(Self::KIND.to_string())
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
        input_text(self.0.clone())
    }
}
