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

    /// Reads one harness-classified base component, including an explicitly empty base.
    pub(crate) fn text_from_item(item: &ResponseItem) -> Option<&str> {
        Self::content_index_and_text(item).map(|(_, text)| text)
    }

    /// Removes an exact duplicate base component from the request copy, preserving other
    /// classified content in the same developer message. Returns whether the message is empty.
    pub(crate) fn remove_matching_text_from_item(
        item: &mut ResponseItem,
        expected_text: &str,
    ) -> bool {
        let Some((content_index, text)) = Self::content_index_and_text(item) else {
            return false;
        };
        if text != expected_text {
            return false;
        }
        let ResponseItem::Message {
            id,
            content,
            internal_chat_message_metadata_passthrough: Some(message_metadata),
            ..
        } = item
        else {
            unreachable!("base instruction component belongs to a message")
        };
        if message_metadata
            .content_item_metadata
            .as_ref()
            .is_some_and(|metadata| metadata.len() != content.len())
        {
            return false;
        }

        content.remove(content_index);
        let kinds_are_empty = {
            let Some(kinds) = message_metadata.content_item_kinds.as_mut() else {
                unreachable!("base instruction component has classified content")
            };
            kinds.remove(content_index);
            kinds.is_empty()
        };
        if kinds_are_empty {
            message_metadata.content_item_kinds = None;
        }
        let metadata_is_empty = message_metadata
            .content_item_metadata
            .as_mut()
            .is_some_and(|metadata| {
                metadata.remove(content_index);
                metadata.is_empty()
            });
        if metadata_is_empty {
            message_metadata.content_item_metadata = None;
        }
        *id = None;
        content.is_empty()
    }

    fn content_index_and_text(item: &ResponseItem) -> Option<(usize, &str)> {
        let ResponseItem::Message {
            role,
            content,
            internal_chat_message_metadata_passthrough: Some(metadata),
            ..
        } = item
        else {
            return None;
        };
        if role != "developer" {
            return None;
        }
        let kinds = metadata.content_item_kinds.as_deref()?;
        if kinds.len() != content.len() {
            return None;
        }
        let mut selected = None;
        for (content_index, (kind, item)) in kinds.iter().zip(content).enumerate() {
            if !matches!(kind.as_str(), Self::KIND | Self::CLIENT_KIND) {
                continue;
            }
            let ContentItem::InputText { text } = item else {
                return None;
            };
            if selected.replace((content_index, text.as_str())).is_some() {
                return None;
            }
        }
        selected
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
