use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;

use super::ContextualUserFragment;
use crate::ThreadConfigSnapshot;

const IDENTITY_SEMANTICS: &str = "runtime_configured_request_identity";
const USAGE_ACCOUNTING_SEMANTICS: &str = "not_terminal_provider_response_or_usage_accounting";
const MAX_IDENTITY_FIELD_BYTES: usize = 256;
const MAX_IDENTITY_FRAGMENT_BYTES: usize = 1_024;

/// Runtime-owned request identity supplied to a spawned agent before sampling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubagentRuntimeIdentity {
    effective_model: String,
    effective_model_provider_id: String,
    effective_reasoning_effort: Option<ReasoningEffort>,
    effective_service_tier: Option<String>,
}

impl SubagentRuntimeIdentity {
    pub(crate) fn from_snapshot(snapshot: &ThreadConfigSnapshot) -> Self {
        Self {
            effective_model: snapshot.model.clone(),
            effective_model_provider_id: snapshot.model_provider_id.clone(),
            effective_reasoning_effort: snapshot.reasoning_effort.clone(),
            effective_service_tier: snapshot.service_tier.clone(),
        }
    }

    pub(crate) fn matches_response_item(
        item: &ResponseItem,
        snapshot: &ThreadConfigSnapshot,
    ) -> bool {
        let ResponseItem::Message { role, content, .. } = item else {
            return false;
        };
        if role != "developer" {
            return false;
        }
        let [ContentItem::InputText { text }] = content.as_slice() else {
            return false;
        };
        Self::from_snapshot(snapshot).matches_rendered_text(text)
    }

    pub(crate) fn has_marked_response_item(item: &ResponseItem) -> bool {
        let ResponseItem::Message { role, content, .. } = item else {
            return false;
        };
        let [ContentItem::InputText { text }] = content.as_slice() else {
            return false;
        };
        role == "developer" && Self::has_reserved_markers(text)
    }

    pub(crate) fn is_bounded(&self) -> bool {
        self.effective_model.len() <= MAX_IDENTITY_FIELD_BYTES
            && self.effective_model_provider_id.len() <= MAX_IDENTITY_FIELD_BYTES
            && self
                .effective_service_tier
                .as_deref()
                .is_none_or(|tier| tier.len() <= MAX_IDENTITY_FIELD_BYTES)
            && self.render().len() <= MAX_IDENTITY_FRAGMENT_BYTES
    }

    fn matches_rendered_text(&self, text: &str) -> bool {
        self.is_bounded() && text == self.render()
    }

    /// Marker matching is intentionally more permissive than identity matching. Marker
    /// variants may have been serialized with surrounding whitespace or different case, but
    /// only the exact canonical fragment can match the configured identity above.
    fn has_reserved_markers(text: &str) -> bool {
        let normalized = text
            .chars()
            .filter(|character| !character.is_whitespace())
            .map(|character| character.to_ascii_lowercase())
            .collect::<String>();
        let (start, end) = Self::type_markers();
        normalized.starts_with(&start.to_ascii_lowercase())
            && normalized.ends_with(&end.to_ascii_lowercase())
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
        let payload = serde_json::json!({
            "effective_model": self.effective_model,
            "effective_model_provider_id": self.effective_model_provider_id,
            "effective_reasoning_effort": self.effective_reasoning_effort,
            "effective_service_tier": self.effective_service_tier,
            "identity_source": "thread_config_snapshot",
            "identity_semantics": IDENTITY_SEMANTICS,
            "usage_accounting": USAGE_ACCOUNTING_SEMANTICS,
        });
        format!("\n{payload}\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authoritative_identity_is_developer_context_and_user_spoof_does_not_match() {
        let identity = SubagentRuntimeIdentity {
            effective_model: "gpt-test".to_string(),
            effective_model_provider_id: "provider".to_string(),
            effective_reasoning_effort: None,
            effective_service_tier: Some("priority".to_string()),
        };
        let item = ContextualUserFragment::into(identity.clone());
        let rendered = match &item {
            ResponseItem::Message { content, .. } => match &content[..] {
                [ContentItem::InputText { text }] => text,
                _ => panic!("identity must contain one text item"),
            },
            _ => panic!("identity must be a message"),
        };
        assert!(identity.matches_rendered_text(rendered));
        assert!(
            serde_json::to_string(&item)
                .expect("serialize identity")
                .contains("runtime_configured_request_identity")
        );

        let spoof = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "<subagent_runtime_identity>{}</subagent_runtime_identity>".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        assert!(!SubagentRuntimeIdentity::has_marked_response_item(&spoof));
    }

    #[test]
    fn reserved_markers_are_case_and_whitespace_insensitive_but_identity_is_exact() {
        let variant = ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "  < SUBAGENT_RUNTIME_IDENTITY >\n{}\n</ SUBAGENT_RUNTIME_IDENTITY >  "
                    .to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        assert!(SubagentRuntimeIdentity::has_marked_response_item(&variant));

        let identity = SubagentRuntimeIdentity {
            effective_model: "gpt-test".to_string(),
            effective_model_provider_id: "provider".to_string(),
            effective_reasoning_effort: None,
            effective_service_tier: Some("priority".to_string()),
        };
        assert!(
            !identity
                .matches_rendered_text("<SUBAGENT_RUNTIME_IDENTITY>{}</SUBAGENT_RUNTIME_IDENTITY>")
        );
    }

    #[test]
    fn identity_rendering_exposes_the_resolved_request_shape() {
        let identity = SubagentRuntimeIdentity {
            effective_model: "gpt-test".to_string(),
            effective_model_provider_id: "provider".to_string(),
            effective_reasoning_effort: Some(ReasoningEffort::Low),
            effective_service_tier: Some("priority".to_string()),
        };
        let rendered = identity.render();
        let json: serde_json::Value = serde_json::from_str(
            rendered
                .trim()
                .trim_start_matches("<subagent_runtime_identity>")
                .trim_end_matches("</subagent_runtime_identity>")
                .trim(),
        )
        .expect("identity body should be JSON");
        assert_eq!(json["effective_model"], "gpt-test");
        assert_eq!(json["effective_model_provider_id"], "provider");
        assert_eq!(json["effective_reasoning_effort"], "low");
        assert_eq!(json["effective_service_tier"], "priority");
    }

    #[test]
    fn identity_bounds_cover_each_field_and_the_complete_fragment() {
        let overlong_field = SubagentRuntimeIdentity {
            effective_model: "m".repeat(MAX_IDENTITY_FIELD_BYTES + 1),
            effective_model_provider_id: "provider".to_string(),
            effective_reasoning_effort: None,
            effective_service_tier: None,
        };
        assert!(!overlong_field.is_bounded());

        let overlong_fragment = SubagentRuntimeIdentity {
            effective_model: "m".repeat(MAX_IDENTITY_FIELD_BYTES),
            effective_model_provider_id: "p".repeat(MAX_IDENTITY_FIELD_BYTES),
            effective_reasoning_effort: Some(ReasoningEffort::High),
            effective_service_tier: Some("t".repeat(MAX_IDENTITY_FIELD_BYTES)),
        };
        assert!(!overlong_fragment.is_bounded());
    }
}
