use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use serde::Serialize;

use super::ContextualUserFragment;
use crate::agent::identity::ModelVisibleAgentIdentity;

const IDENTITY_SEMANTICS: &str = "configured_and_latest_turn_request_identity_are_separate";
const USAGE_ACCOUNTING_SEMANTICS: &str = "not_terminal_provider_response_or_usage_accounting";
const START_MARKER: &str = "<subagent_runtime_identity>";
const END_MARKER: &str = "</subagent_runtime_identity>";
const RESERVED_MARKER_PREFIXES: [&str; 2] =
    ["<subagent_runtime_identity", "</subagent_runtime_identity"];

/// Runtime-owned inference identity supplied to a spawned agent before sampling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubagentRuntimeIdentity {
    identity: ModelVisibleAgentIdentity,
}

#[derive(Serialize)]
struct SubagentRuntimeIdentityPayload<'a> {
    #[serde(flatten)]
    identity: &'a ModelVisibleAgentIdentity,
    identity_semantics: &'static str,
    usage_accounting: &'static str,
}

impl SubagentRuntimeIdentity {
    pub(crate) fn new(identity: ModelVisibleAgentIdentity) -> Self {
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

    /// Returns whether a raw developer-message injection contains the reserved marker namespace.
    ///
    /// This is deliberately broader than [`Self::matches_response_item`], which recognizes only
    /// complete runtime-authored fragments for history filtering and deduplication. Raw injection
    /// must also reject embedded, mixed-case, multi-content, and attribute-like marker variants.
    pub(crate) fn contains_reserved_marker_in_raw_injection(item: &ResponseItem) -> bool {
        let ResponseItem::Message { role, content, .. } = item else {
            return false;
        };
        if role != "developer" {
            return false;
        }

        content.iter().any(|content_item| {
            let ContentItem::InputText { text } = content_item else {
                return false;
            };
            RESERVED_MARKER_PREFIXES.iter().any(|prefix| {
                text.as_bytes()
                    .windows(prefix.len())
                    .any(|window| window.eq_ignore_ascii_case(prefix.as_bytes()))
            })
        })
    }

    pub(crate) fn matches_current_response_item(&self, item: &ResponseItem) -> bool {
        let ResponseItem::Message { role, content, .. } = item else {
            return false;
        };
        if role != "developer" {
            return false;
        }
        let [ContentItem::InputText { text }] = content.as_slice() else {
            return false;
        };

        text == &self.render()
    }

    pub(crate) fn matches_latest_response_item(&self, items: &[ResponseItem]) -> bool {
        items
            .iter()
            .rev()
            .find(|item| Self::matches_response_item(item))
            .is_some_and(|item| self.matches_current_response_item(item))
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
            .expect("model-visible subagent identity should serialize to JSON");
        format!("\n{payload}\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::identity::ModelVisibleIdentityEncoding;
    use crate::codex_thread::ConfiguredInferenceIdentity;
    use crate::codex_thread::ThreadInferenceIdentitySnapshot;
    use crate::codex_thread::TurnInferenceIdentity;
    use codex_protocol::openai_models::ReasoningEffort;

    fn identity(model: &str) -> SubagentRuntimeIdentity {
        let snapshot = ThreadInferenceIdentitySnapshot {
            configured: ConfiguredInferenceIdentity {
                configured_model: model.to_string(),
                configured_model_provider_id: "provider".to_string(),
                configured_reasoning_effort: Some(ReasoningEffort::High),
                configured_service_tier: Some("priority".to_string()),
            },
            latest_turn: Some(TurnInferenceIdentity {
                turn_id: "turn-1".to_string(),
                request_model: model.to_string(),
                model_provider_id: "provider".to_string(),
                requested_reasoning_effort: Some(ReasoningEffort::High),
                request_service_tier: Some("priority".to_string()),
            }),
        };
        SubagentRuntimeIdentity::new(ModelVisibleAgentIdentity::from_live(
            &snapshot,
            ModelVisibleIdentityEncoding::Json,
        ))
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

        assert_eq!(payload["configured_identity"]["model"], "current-model");
        assert_eq!(
            payload["latest_turn_request_identity"]["source"],
            "turn_request"
        );
        assert_eq!(payload["identity_truncated"], false);
        assert_eq!(payload["identity_fields_omitted"], 0);
        assert!(SubagentRuntimeIdentity::matches_response_item(&item));
    }

    #[test]
    fn raw_injection_reservation_is_broader_than_exact_fragment_matching() {
        let embedded = ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: format!("prefix {START_MARKER}{{}}{END_MARKER} suffix"),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let multi_content_mixed_case = ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![
                ContentItem::InputText {
                    text: "ordinary context".to_string(),
                },
                ContentItem::InputText {
                    text: "later <SuBaGeNt_RuNtImE_IdEnTiTy source=\"raw\">".to_string(),
                },
            ],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let ordinary_developer = ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "ordinary developer context".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let user_data = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: format!("user data may quote {START_MARKER}"),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };

        assert!(!SubagentRuntimeIdentity::matches_response_item(&embedded));
        assert!(SubagentRuntimeIdentity::contains_reserved_marker_in_raw_injection(&embedded));
        assert!(
            SubagentRuntimeIdentity::contains_reserved_marker_in_raw_injection(
                &multi_content_mixed_case
            )
        );
        assert!(
            !SubagentRuntimeIdentity::contains_reserved_marker_in_raw_injection(
                &ordinary_developer
            )
        );
        assert!(!SubagentRuntimeIdentity::contains_reserved_marker_in_raw_injection(&user_data));
    }

    #[test]
    fn current_match_requires_exact_model_visible_projection() {
        let current = identity("current-model");
        let stale = ContextualUserFragment::into(identity("stale-model"));
        let matching = ContextualUserFragment::into(current.clone());

        assert!(SubagentRuntimeIdentity::matches_response_item(&stale));
        assert!(!current.matches_current_response_item(&stale));
        assert!(current.matches_current_response_item(&matching));
    }
}
