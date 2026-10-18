use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

/// Configured multi-agent instructions emitted as a standalone developer message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MultiAgentUsageHint {
    text: String,
    client_owned: bool,
}

impl MultiAgentUsageHint {
    pub(crate) fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            client_owned: false,
        }
    }

    pub(crate) fn with_client_ownership(mut self, client_owned: bool) -> Self {
        self.client_owned = client_owned;
        self
    }
}

impl ContextualUserFragment for MultiAgentUsageHint {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind(
            if self.client_owned {
                "client.multi_agent.usage_hint"
            } else {
                "multi_agent.usage_hint"
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

    fn body(&self) -> String {
        self.text.clone()
    }
}
