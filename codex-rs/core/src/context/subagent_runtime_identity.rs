use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use serde::Serialize;

use super::ContextualUserFragment;
use crate::ThreadConfigSnapshot;
use crate::agent::control::EffectiveAgentIdentity;

const IDENTITY_SEMANTICS: &str = "latest_runtime_configured_request_identity_is_authoritative";
const USAGE_ACCOUNTING_SEMANTICS: &str = "not_terminal_provider_response_or_usage_accounting";
const START_MARKER: &str = "<subagent_runtime_identity>";
const END_MARKER: &str = "</subagent_runtime_identity>";

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

    pub(crate) fn from_thread_config_snapshot(snapshot: &ThreadConfigSnapshot) -> Option<Self> {
        matches!(
            &snapshot.session_source,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
        )
        .then(|| {
            Self::new(EffectiveAgentIdentity::from_thread_config_snapshot(
                snapshot,
            ))
        })
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

    fn payload(&self) -> SubagentRuntimeIdentityPayload<'_> {
        SubagentRuntimeIdentityPayload {
            identity: &self.identity,
            identity_semantics: IDENTITY_SEMANTICS,
            usage_accounting: USAGE_ACCOUNTING_SEMANTICS,
        }
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
        (START_MARKER, END_MARKER)
    }

    fn body(&self) -> String {
        let payload = serde_json::to_string(&self.payload())
            .expect("effective subagent identity should serialize to JSON");
        format!("\n{payload}\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::openai_models::ReasoningEffort;

    fn identity(model: &str) -> SubagentRuntimeIdentity {
        SubagentRuntimeIdentity::new(EffectiveAgentIdentity {
            effective_model: Some(model.to_string()),
            effective_model_provider_id: Some("provider".to_string()),
            effective_reasoning_effort: Some(ReasoningEffort::High),
            effective_service_tier: Some("priority".to_string()),
            identity_source: "thread_config_snapshot".to_string(),
        })
    }

    #[test]
    fn rendered_identity_is_a_runtime_owned_developer_fragment() {
        let current = identity("current-model");
        let rendered = current.render();
        let item = ContextualUserFragment::into(current);

        let payload: serde_json::Value = serde_json::from_str(
            rendered
                .trim()
                .strip_prefix(START_MARKER)
                .and_then(|text| text.strip_suffix(END_MARKER))
                .expect("balanced markers")
                .trim(),
        )
        .expect("identity payload should be JSON");

        assert_eq!(payload["effective_model"], "current-model");
        assert_eq!(payload["identity_source"], "thread_config_snapshot");
        assert!(SubagentRuntimeIdentity::matches_response_item(&item));
    }
}
