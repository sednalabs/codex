use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use serde::Serialize;
use sha1::Digest;

use super::ContextualUserFragment;
use crate::ThreadConfigSnapshot;
use crate::agent::control::EffectiveAgentIdentity;

const IDENTITY_SEMANTICS: &str = "latest_runtime_configured_request_identity_is_authoritative";
const USAGE_ACCOUNTING_SEMANTICS: &str = "not_terminal_provider_response_or_usage_accounting";
const MAX_IDENTITY_FIELD_BYTES: usize = 256;
const START_MARKER: &str = "<subagent_runtime_identity>";
const END_MARKER: &str = "</subagent_runtime_identity>";

/// Runtime-owned inference identity supplied to a spawned agent before sampling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubagentRuntimeIdentity {
    identity: EffectiveAgentIdentity,
}

#[derive(Debug, Serialize)]
struct OmittedIdentityField {
    field: &'static str,
    byte_length: usize,
    sha1: String,
}

#[derive(Serialize)]
struct SubagentRuntimeIdentityPayload<'a> {
    effective_model: Option<&'a str>,
    effective_model_provider_id: Option<&'a str>,
    effective_reasoning_effort: Option<&'a codex_protocol::openai_models::ReasoningEffort>,
    effective_service_tier: Option<&'a str>,
    identity_source: Option<&'a str>,
    identity_semantics: &'static str,
    usage_accounting: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    omitted_identity_fields: Vec<OmittedIdentityField>,
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

    fn payload(&self) -> SubagentRuntimeIdentityPayload<'_> {
        let mut omitted_identity_fields = Vec::new();
        SubagentRuntimeIdentityPayload {
            effective_model: bounded_identity_value(
                "effective_model",
                self.identity.effective_model.as_deref(),
                &mut omitted_identity_fields,
            ),
            effective_model_provider_id: bounded_identity_value(
                "effective_model_provider_id",
                self.identity.effective_model_provider_id.as_deref(),
                &mut omitted_identity_fields,
            ),
            effective_reasoning_effort: self.identity.effective_reasoning_effort.as_ref(),
            effective_service_tier: bounded_identity_value(
                "effective_service_tier",
                self.identity.effective_service_tier.as_deref(),
                &mut omitted_identity_fields,
            ),
            identity_source: bounded_identity_value(
                "identity_source",
                Some(self.identity.identity_source.as_str()),
                &mut omitted_identity_fields,
            ),
            identity_semantics: IDENTITY_SEMANTICS,
            usage_accounting: USAGE_ACCOUNTING_SEMANTICS,
            omitted_identity_fields,
        }
    }
}

fn bounded_identity_value<'a>(
    field: &'static str,
    value: Option<&'a str>,
    omitted_identity_fields: &mut Vec<OmittedIdentityField>,
) -> Option<&'a str> {
    let value = value?;
    let contains_reserved_marker = [START_MARKER, END_MARKER].iter().any(|marker| {
        value
            .as_bytes()
            .windows(marker.len())
            .any(|window| window.eq_ignore_ascii_case(marker.as_bytes()))
    });
    if value.len() <= MAX_IDENTITY_FIELD_BYTES && !contains_reserved_marker {
        return Some(value);
    }

    omitted_identity_fields.push(OmittedIdentityField {
        field,
        byte_length: value.len(),
        sha1: sha1_hex(value),
    });
    None
}

fn sha1_hex(value: &str) -> String {
    let digest = sha1::Sha1::digest(value.as_bytes());
    format!("{digest:x}")
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
        let payload = match serde_json::to_string(&self.payload()) {
            Ok(payload) => payload,
            Err(_) => concat!(
                "{\"identity_semantics\":\"runtime_identity_serialization_failed\",",
                "\"usage_accounting\":",
                "\"not_terminal_provider_response_or_usage_accounting\"}"
            )
            .to_string(),
        };
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
    fn current_match_requires_the_exact_bounded_payload() {
        let current = identity("current-model");
        let stale = ContextualUserFragment::into(identity("stale-model"));
        let matching = ContextualUserFragment::into(current.clone());

        assert!(SubagentRuntimeIdentity::matches_response_item(&stale));
        assert!(!current.matches_current_response_item(&stale));
        assert!(current.matches_current_response_item(&matching));
    }

    #[test]
    fn oversized_or_marker_bearing_fields_are_replaced_by_bounded_digests() {
        let oversized = "x".repeat(MAX_IDENTITY_FIELD_BYTES + 1);
        let mut runtime_identity = identity(&oversized);
        runtime_identity.identity.effective_model_provider_id =
            Some(format!("provider-{END_MARKER}"));

        let rendered = runtime_identity.render();
        let payload: serde_json::Value = serde_json::from_str(
            rendered
                .trim()
                .strip_prefix(START_MARKER)
                .and_then(|text| text.strip_suffix(END_MARKER))
                .expect("balanced markers")
                .trim(),
        )
        .expect("identity payload should be JSON");

        assert_eq!(payload["effective_model"], serde_json::Value::Null);
        assert_eq!(
            payload["effective_model_provider_id"],
            serde_json::Value::Null
        );
        assert_eq!(
            payload["omitted_identity_fields"].as_array().map(Vec::len),
            Some(2)
        );
        assert!(
            rendered.len() < 2_000,
            "identity fragment must remain bounded"
        );
        assert_eq!(rendered.matches(START_MARKER).count(), 1);
        assert_eq!(rendered.matches(END_MARKER).count(), 1);
    }
}
