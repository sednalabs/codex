use crate::codex_thread::ThreadInferenceIdentitySnapshot;
use codex_protocol::openai_models::ReasoningEffort;
use codex_thread_store::StoredThread;
use serde::Serialize;

pub(crate) const MODEL_VISIBLE_IDENTITY_FIELD_BYTES: usize = 96;
pub(crate) const MODEL_VISIBLE_IDENTITY_TOTAL_BYTES: usize = 640;
pub(crate) const MODEL_VISIBLE_IDENTITY_SCAN_BYTES: usize = 1_024;
const TRUNCATION_MARKER: &str = "…";
const XML_FRAGMENT_FIXED_OVERHEAD_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModelVisibleIdentityEncoding {
    Json,
    Xml,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConfiguredIdentitySource {
    LiveThreadConfig,
    StoredThreadMetadata,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TurnRequestIdentitySource {
    TurnRequest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ModelVisibleConfiguredIdentity {
    pub(crate) model: Option<String>,
    pub(crate) model_provider_id: Option<String>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    pub(crate) service_tier: Option<String>,
    pub(crate) source: ConfiguredIdentitySource,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ModelVisibleTurnRequestIdentity {
    pub(crate) turn_id: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) model_provider_id: Option<String>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    pub(crate) service_tier: Option<String>,
    pub(crate) source: TurnRequestIdentitySource,
}

/// Bounded identity data that is safe to include in model-visible output.
///
/// This projection deliberately keeps configured settings and latest-turn request facts in
/// separate nested receipts with separate provenance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ModelVisibleAgentIdentity {
    pub(crate) configured_identity: Option<ModelVisibleConfiguredIdentity>,
    pub(crate) latest_turn_request_identity: Option<ModelVisibleTurnRequestIdentity>,
    pub(crate) identity_truncated: bool,
    pub(crate) identity_fields_omitted: usize,
}

impl ModelVisibleAgentIdentity {
    pub(crate) fn from_live(
        snapshot: &ThreadInferenceIdentitySnapshot,
        encoding: ModelVisibleIdentityEncoding,
    ) -> Self {
        let mut truncated = false;
        let configured = &snapshot.configured;
        let configured_identity = ModelVisibleConfiguredIdentity {
            model: bounded_optional(
                Some(configured.configured_model.as_str()),
                encoding,
                &mut truncated,
            ),
            model_provider_id: bounded_optional(
                non_empty(configured.configured_model_provider_id.as_str()),
                encoding,
                &mut truncated,
            ),
            reasoning_effort: configured.configured_reasoning_effort.clone(),
            service_tier: bounded_optional(
                configured.configured_service_tier.as_deref(),
                encoding,
                &mut truncated,
            ),
            source: ConfiguredIdentitySource::LiveThreadConfig,
        };
        let latest_turn_request_identity =
            snapshot
                .latest_turn
                .as_ref()
                .map(|turn| ModelVisibleTurnRequestIdentity {
                    turn_id: bounded_optional(
                        Some(turn.turn_id.as_str()),
                        encoding,
                        &mut truncated,
                    ),
                    model: bounded_optional(
                        Some(turn.request_model.as_str()),
                        encoding,
                        &mut truncated,
                    ),
                    model_provider_id: bounded_optional(
                        non_empty(turn.model_provider_id.as_str()),
                        encoding,
                        &mut truncated,
                    ),
                    reasoning_effort: turn.requested_reasoning_effort.clone(),
                    service_tier: bounded_optional(
                        turn.request_service_tier.as_deref(),
                        encoding,
                        &mut truncated,
                    ),
                    source: TurnRequestIdentitySource::TurnRequest,
                });
        Self {
            configured_identity: Some(configured_identity),
            latest_turn_request_identity,
            identity_truncated: truncated,
            identity_fields_omitted: 0,
        }
        .enforce_total_bound(encoding)
    }

    pub(crate) fn from_stored(
        stored: &StoredThread,
        encoding: ModelVisibleIdentityEncoding,
    ) -> Self {
        let mut truncated = false;
        let configured_identity = ModelVisibleConfiguredIdentity {
            model: bounded_optional(stored.model.as_deref(), encoding, &mut truncated),
            model_provider_id: bounded_optional(
                non_empty(stored.model_provider.as_str()),
                encoding,
                &mut truncated,
            ),
            reasoning_effort: stored.reasoning_effort.clone(),
            // StoredThread does not yet persist configured service tier.
            service_tier: None,
            source: ConfiguredIdentitySource::StoredThreadMetadata,
        };
        Self {
            configured_identity: Some(configured_identity),
            latest_turn_request_identity: None,
            identity_truncated: truncated,
            identity_fields_omitted: 0,
        }
        .enforce_total_bound(encoding)
    }

    fn enforce_total_bound(mut self, encoding: ModelVisibleIdentityEncoding) -> Self {
        loop {
            match self.try_encoded_len(encoding) {
                Some(encoded_len) if encoded_len <= MODEL_VISIBLE_IDENTITY_TOTAL_BYTES => break,
                Some(_) => {
                    if self.omit_lowest_priority_field() {
                        self.identity_truncated = true;
                        self.identity_fields_omitted += 1;
                        continue;
                    }
                    self.fail_closed();
                    break;
                }
                None => {
                    self.fail_closed();
                    break;
                }
            }
        }
        self
    }

    fn fail_closed(&mut self) {
        while self.omit_lowest_priority_field() {
            self.identity_fields_omitted += 1;
        }
        self.identity_fields_omitted +=
            usize::from(self.latest_turn_request_identity.take().is_some());
        self.identity_fields_omitted += usize::from(self.configured_identity.take().is_some());
        self.identity_truncated = true;
    }

    fn omit_lowest_priority_field(&mut self) -> bool {
        if let Some(turn) = self.latest_turn_request_identity.as_mut() {
            if turn.service_tier.take().is_some()
                || turn.reasoning_effort.take().is_some()
                || turn.model_provider_id.take().is_some()
                || turn.model.take().is_some()
                || turn.turn_id.take().is_some()
            {
                return true;
            }
        }
        if let Some(configured) = self.configured_identity.as_mut()
            && (configured.service_tier.take().is_some()
                || configured.reasoning_effort.take().is_some()
                || configured.model_provider_id.take().is_some()
                || configured.model.take().is_some())
        {
            return true;
        }
        false
    }

    pub(crate) fn encoded_len(&self, encoding: ModelVisibleIdentityEncoding) -> usize {
        self.try_encoded_len(encoding).unwrap_or(usize::MAX)
    }

    fn try_encoded_len(&self, encoding: ModelVisibleIdentityEncoding) -> Option<usize> {
        match encoding {
            ModelVisibleIdentityEncoding::Json => {
                serde_json::to_vec(self).ok().map(|encoded| encoded.len())
            }
            ModelVisibleIdentityEncoding::Xml => Some(
                XML_FRAGMENT_FIXED_OVERHEAD_BYTES
                    + self
                        .string_fields()
                        .into_iter()
                        .flatten()
                        .map(xml_encoded_len)
                        .sum::<usize>(),
            ),
        }
    }

    fn string_fields(&self) -> Vec<Option<&str>> {
        let mut fields = Vec::with_capacity(8);
        if let Some(configured) = self.configured_identity.as_ref() {
            fields.extend([
                configured.model.as_deref(),
                configured.model_provider_id.as_deref(),
                configured.service_tier.as_deref(),
            ]);
        }
        if let Some(turn) = self.latest_turn_request_identity.as_ref() {
            fields.extend([
                turn.turn_id.as_deref(),
                turn.model.as_deref(),
                turn.model_provider_id.as_deref(),
                turn.service_tier.as_deref(),
            ]);
        }
        fields
    }
}

fn non_empty(value: &str) -> Option<&str> {
    (!value.trim().is_empty()).then_some(value)
}

fn bounded_optional(
    value: Option<&str>,
    encoding: ModelVisibleIdentityEncoding,
    truncated: &mut bool,
) -> Option<String> {
    value.map(|value| {
        let (value, was_truncated) = bounded_string(value, encoding);
        *truncated |= was_truncated;
        value
    })
}

fn bounded_string(value: &str, encoding: ModelVisibleIdentityEncoding) -> (String, bool) {
    let scan_end = char_boundary_at_or_before(value, MODEL_VISIBLE_IDENTITY_SCAN_BYTES);
    let scanned = &value[..scan_end];
    let input_truncated = scan_end < value.len();
    if !input_truncated
        && encoded_string_len(scanned, encoding) <= MODEL_VISIBLE_IDENTITY_FIELD_BYTES
    {
        return (scanned.to_string(), false);
    }

    let mut end = 0;
    for (index, character) in scanned.char_indices() {
        let candidate_end = index + character.len_utf8();
        let candidate = format!("{}{}", &scanned[..candidate_end], TRUNCATION_MARKER);
        if encoded_string_len(&candidate, encoding) > MODEL_VISIBLE_IDENTITY_FIELD_BYTES {
            break;
        }
        end = candidate_end;
    }
    (format!("{}{}", &scanned[..end], TRUNCATION_MARKER), true)
}

fn char_boundary_at_or_before(value: &str, byte_limit: usize) -> usize {
    if value.len() <= byte_limit {
        return value.len();
    }
    let mut end = byte_limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn encoded_string_len(value: &str, encoding: ModelVisibleIdentityEncoding) -> usize {
    match encoding {
        ModelVisibleIdentityEncoding::Json => {
            serde_json::to_vec(value).map_or(usize::MAX, |value| value.len().saturating_sub(2))
        }
        ModelVisibleIdentityEncoding::Xml => xml_encoded_len(value),
    }
}

fn xml_encoded_len(value: &str) -> usize {
    value
        .chars()
        .map(|character| match character {
            '&' => "&amp;".len(),
            '<' => "&lt;".len(),
            '>' => "&gt;".len(),
            '"' => "&quot;".len(),
            '\'' => "&apos;".len(),
            other => other.len_utf8(),
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_thread::ConfiguredInferenceIdentity;
    use crate::codex_thread::TurnInferenceIdentity;

    #[test]
    fn projection_caps_json_and_xml_after_escaping() {
        let hostile = "<&\"'\\\n".repeat(MODEL_VISIBLE_IDENTITY_SCAN_BYTES);
        let snapshot = ThreadInferenceIdentitySnapshot {
            configured: ConfiguredInferenceIdentity {
                configured_model: hostile.clone(),
                configured_model_provider_id: hostile.clone(),
                configured_reasoning_effort: Some(ReasoningEffort::High),
                configured_service_tier: Some(hostile.clone()),
            },
            latest_turn: Some(TurnInferenceIdentity {
                turn_id: hostile.clone(),
                request_model: hostile.clone(),
                model_provider_id: hostile.clone(),
                requested_reasoning_effort: Some(ReasoningEffort::Medium),
                request_service_tier: Some(hostile),
            }),
        };

        for encoding in [
            ModelVisibleIdentityEncoding::Json,
            ModelVisibleIdentityEncoding::Xml,
        ] {
            let projection = ModelVisibleAgentIdentity::from_live(&snapshot, encoding);
            assert!(projection.identity_truncated);
            assert!(projection.identity_fields_omitted > 0);
            assert!(projection.encoded_len(encoding) <= MODEL_VISIBLE_IDENTITY_TOTAL_BYTES);
            for value in projection.string_fields().into_iter().flatten() {
                assert!(encoded_string_len(value, encoding) <= MODEL_VISIBLE_IDENTITY_FIELD_BYTES);
                assert!(value.is_char_boundary(value.len()));
            }
        }
    }

    #[test]
    fn configured_and_turn_request_provenance_stay_separate() {
        let snapshot = ThreadInferenceIdentitySnapshot {
            configured: ConfiguredInferenceIdentity {
                configured_model: "configured-model".to_string(),
                configured_model_provider_id: "configured-provider".to_string(),
                configured_reasoning_effort: Some(ReasoningEffort::Low),
                configured_service_tier: Some("configured-tier".to_string()),
            },
            latest_turn: Some(TurnInferenceIdentity {
                turn_id: "turn-1".to_string(),
                request_model: "request-model".to_string(),
                model_provider_id: "request-provider".to_string(),
                requested_reasoning_effort: Some(ReasoningEffort::High),
                request_service_tier: None,
            }),
        };

        let projection =
            ModelVisibleAgentIdentity::from_live(&snapshot, ModelVisibleIdentityEncoding::Json);
        assert_eq!(
            projection,
            ModelVisibleAgentIdentity {
                configured_identity: Some(ModelVisibleConfiguredIdentity {
                    model: Some("configured-model".to_string()),
                    model_provider_id: Some("configured-provider".to_string()),
                    reasoning_effort: Some(ReasoningEffort::Low),
                    service_tier: Some("configured-tier".to_string()),
                    source: ConfiguredIdentitySource::LiveThreadConfig,
                }),
                latest_turn_request_identity: Some(ModelVisibleTurnRequestIdentity {
                    turn_id: Some("turn-1".to_string()),
                    model: Some("request-model".to_string()),
                    model_provider_id: Some("request-provider".to_string()),
                    reasoning_effort: Some(ReasoningEffort::High),
                    service_tier: None,
                    source: TurnRequestIdentitySource::TurnRequest,
                }),
                identity_truncated: false,
                identity_fields_omitted: 0,
            }
        );
    }
}
