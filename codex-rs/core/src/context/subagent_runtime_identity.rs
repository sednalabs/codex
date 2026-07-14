use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use serde::Serialize;

use super::ContextualUserFragment;
use crate::agent::control::EffectiveAgentIdentity;

const IDENTITY_SEMANTICS: &str = "runtime_configured_request_identity";
const USAGE_ACCOUNTING_SEMANTICS: &str = "not_terminal_provider_response_or_usage_accounting";

/// Runtime-owned inference identity supplied to a spawned agent before sampling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubagentRuntimeIdentity {
    identity: EffectiveAgentIdentity,
}

#[derive(Serialize)]
struct SubagentRuntimeIdentityPayload<'a> {
    #[serde(flatten)]
    identity: &'a EffectiveAgentIdentity,
    identity_semantics: &'static str,
    usage_accounting: &'static str,
}

impl SubagentRuntimeIdentity {
    pub(crate) fn new(identity: EffectiveAgentIdentity) -> Self {
        Self { identity }
    }

    pub(crate) fn matches_response_item(item: &ResponseItem) -> bool {
        let ResponseItem::Message { role, content, .. } = item else {
            return false;
        };
        if role != "developer" {
            return false;
        }
        let [ContentItem::InputText { text }] = content.as_slice() else {
            return false;
        };

        Self::matches_text(text)
    }
}

impl ContextualUserFragment for SubagentRuntimeIdentity {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (
            "<subagent_runtime_identity>",
            "</subagent_runtime_identity>",
        )
    }

    fn body(&self) -> String {
        let payload = SubagentRuntimeIdentityPayload {
            identity: &self.identity,
            identity_semantics: IDENTITY_SEMANTICS,
            usage_accounting: USAGE_ACCOUNTING_SEMANTICS,
        };
        let payload = serde_json::to_string(&payload)
            .expect("effective subagent identity should serialize to JSON");
        format!("\n{payload}\n")
    }
}
