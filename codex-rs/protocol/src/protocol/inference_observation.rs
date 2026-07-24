use std::io;

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

use super::TokenUsage;
use crate::ThreadId;

/// Maximum byte length of the local identifier that joins one attempt's observations.
pub const INFERENCE_CALL_ID_MAX_BYTES: usize = 64;
/// Maximum byte length of exact turn and spawn correlation identifiers.
pub const INFERENCE_CALL_CORRELATION_ID_MAX_BYTES: usize = 256;
/// Maximum byte length of an individual configured, requested, or observed string.
pub const INFERENCE_CALL_STRING_MAX_BYTES: usize = 512;
/// Maximum serialized size of the tagged `inference_call` event.
pub const INFERENCE_CALL_EVENT_MAX_BYTES: usize = 4096;

const INFERENCE_CALL_REQUIRED_STRING_FALLBACK_MAX_BYTES: usize = 128;

/// One immutable, payload-free observation of a client-side inference attempt.
///
/// `inference_call_id` joins the started observation to at most one terminal
/// observation. `thread_id`, `turn_id`, and `spawn_request_id` provide local
/// correlation only; they do not assert that a provider executed or billed a
/// physical request.
#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
pub struct InferenceCallEvent {
    pub inference_call_id: String,
    pub thread_id: ThreadId,
    pub turn_id: String,
    /// Spawn tool-call identifier for a spawned thread, when known.
    pub spawn_request_id: Option<String>,
    pub status: InferenceCallStatus,
    pub transport: InferenceCallTransport,
    /// Provider selected by local configuration.
    pub configured_provider: String,
    /// Model placed on this concrete provider request.
    pub requested_model: String,
    /// Service tier placed on this concrete provider request, when present.
    pub requested_service_tier: Option<String>,
    pub request_started_at_ms: i64,
    pub request_completed_at_ms: Option<i64>,
    /// Responses API `response.id`, when a completed response supplied it.
    pub response_id: Option<String>,
    /// Provider transport request identifier, when observed.
    pub upstream_request_id: Option<String>,
    /// Provider identity reported at the response boundary, when supplied.
    pub observed_provider: Option<String>,
    /// Execution model reported at the response boundary, when supplied.
    pub observed_model: Option<String>,
    /// Model snapshot reported at the response boundary, when supplied.
    pub observed_model_snapshot: Option<String>,
    /// Service tier reported at the response boundary, when supplied.
    pub observed_service_tier: Option<String>,
    /// Exact usage for this response. This is never an accumulated or estimated total.
    pub token_usage: Option<TokenUsage>,
    /// Required string fields shortened to fit the durable observation limits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[ts(optional)]
    pub truncated_fields: Vec<InferenceCallField>,
    /// Optional or lifecycle-inapplicable evidence removed from this observation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[ts(optional)]
    pub omitted_fields: Vec<InferenceCallField>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum InferenceCallStatus {
    Started,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum InferenceCallTransport {
    ResponsesHttp,
    ResponsesWebsocket,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum InferenceCallField {
    TurnId,
    SpawnRequestId,
    ConfiguredProvider,
    RequestedModel,
    RequestedServiceTier,
    RequestCompletedAtMs,
    ResponseId,
    UpstreamRequestId,
    ObservedProvider,
    ObservedModel,
    ObservedModelSnapshot,
    ObservedServiceTier,
    TokenUsage,
}

impl InferenceCallEvent {
    /// Applies lifecycle, per-field, and aggregate bounds.
    ///
    /// Exact correlation identifiers are never truncated. `None` means one of
    /// those identifiers exceeded its contract or the bounded event could not
    /// be represented within [`INFERENCE_CALL_EVENT_MAX_BYTES`].
    pub fn into_durable(mut self) -> Option<Self> {
        if self.inference_call_id.len() > INFERENCE_CALL_ID_MAX_BYTES
            || self.turn_id.len() > INFERENCE_CALL_CORRELATION_ID_MAX_BYTES
            || self
                .spawn_request_id
                .as_ref()
                .is_some_and(|id| id.len() > INFERENCE_CALL_CORRELATION_ID_MAX_BYTES)
        {
            return None;
        }

        self.truncated_fields.clear();
        self.omitted_fields.clear();
        self.remove_lifecycle_inapplicable_evidence();

        truncate_required_string(
            &mut self.configured_provider,
            INFERENCE_CALL_STRING_MAX_BYTES,
            InferenceCallField::ConfiguredProvider,
            &mut self.truncated_fields,
        );
        truncate_required_string(
            &mut self.requested_model,
            INFERENCE_CALL_STRING_MAX_BYTES,
            InferenceCallField::RequestedModel,
            &mut self.truncated_fields,
        );

        for field in [
            InferenceCallField::RequestedServiceTier,
            InferenceCallField::ResponseId,
            InferenceCallField::UpstreamRequestId,
            InferenceCallField::ObservedProvider,
            InferenceCallField::ObservedModel,
            InferenceCallField::ObservedModelSnapshot,
            InferenceCallField::ObservedServiceTier,
        ] {
            if self
                .string_field(field)
                .is_some_and(|value| value.len() > INFERENCE_CALL_STRING_MAX_BYTES)
            {
                self.omit_field(field);
            }
        }

        for field in [
            InferenceCallField::RequestedServiceTier,
            InferenceCallField::ObservedModelSnapshot,
            InferenceCallField::ObservedServiceTier,
            InferenceCallField::ObservedModel,
            InferenceCallField::ObservedProvider,
            InferenceCallField::UpstreamRequestId,
            InferenceCallField::ResponseId,
        ] {
            if self.serialized_len()? <= INFERENCE_CALL_EVENT_MAX_BYTES {
                return Some(self);
            }
            self.omit_field(field);
        }

        if self.serialized_len()? > INFERENCE_CALL_EVENT_MAX_BYTES {
            truncate_required_string(
                &mut self.configured_provider,
                INFERENCE_CALL_REQUIRED_STRING_FALLBACK_MAX_BYTES,
                InferenceCallField::ConfiguredProvider,
                &mut self.truncated_fields,
            );
            truncate_required_string(
                &mut self.requested_model,
                INFERENCE_CALL_REQUIRED_STRING_FALLBACK_MAX_BYTES,
                InferenceCallField::RequestedModel,
                &mut self.truncated_fields,
            );
        }

        (self.serialized_len()? <= INFERENCE_CALL_EVENT_MAX_BYTES).then_some(self)
    }

    fn remove_lifecycle_inapplicable_evidence(&mut self) {
        let fields: &[InferenceCallField] = match self.status {
            InferenceCallStatus::Started => &[
                InferenceCallField::RequestCompletedAtMs,
                InferenceCallField::ResponseId,
                InferenceCallField::UpstreamRequestId,
                InferenceCallField::ObservedProvider,
                InferenceCallField::ObservedModel,
                InferenceCallField::ObservedModelSnapshot,
                InferenceCallField::ObservedServiceTier,
                InferenceCallField::TokenUsage,
            ],
            InferenceCallStatus::Failed | InferenceCallStatus::Cancelled => &[
                InferenceCallField::ResponseId,
                InferenceCallField::ObservedProvider,
                InferenceCallField::ObservedModel,
                InferenceCallField::ObservedModelSnapshot,
                InferenceCallField::ObservedServiceTier,
                InferenceCallField::TokenUsage,
            ],
            InferenceCallStatus::Completed => &[],
        };
        for field in fields {
            self.omit_field(*field);
        }
    }

    fn string_field(&self, field: InferenceCallField) -> Option<&str> {
        match field {
            InferenceCallField::RequestedServiceTier => self.requested_service_tier.as_deref(),
            InferenceCallField::ResponseId => self.response_id.as_deref(),
            InferenceCallField::UpstreamRequestId => self.upstream_request_id.as_deref(),
            InferenceCallField::ObservedProvider => self.observed_provider.as_deref(),
            InferenceCallField::ObservedModel => self.observed_model.as_deref(),
            InferenceCallField::ObservedModelSnapshot => self.observed_model_snapshot.as_deref(),
            InferenceCallField::ObservedServiceTier => self.observed_service_tier.as_deref(),
            InferenceCallField::TurnId
            | InferenceCallField::SpawnRequestId
            | InferenceCallField::ConfiguredProvider
            | InferenceCallField::RequestedModel
            | InferenceCallField::RequestCompletedAtMs
            | InferenceCallField::TokenUsage => None,
        }
    }

    fn omit_field(&mut self, field: InferenceCallField) {
        let present = match field {
            InferenceCallField::RequestedServiceTier => {
                self.requested_service_tier.take().is_some()
            }
            InferenceCallField::RequestCompletedAtMs => {
                self.request_completed_at_ms.take().is_some()
            }
            InferenceCallField::ResponseId => self.response_id.take().is_some(),
            InferenceCallField::UpstreamRequestId => self.upstream_request_id.take().is_some(),
            InferenceCallField::ObservedProvider => self.observed_provider.take().is_some(),
            InferenceCallField::ObservedModel => self.observed_model.take().is_some(),
            InferenceCallField::ObservedModelSnapshot => {
                self.observed_model_snapshot.take().is_some()
            }
            InferenceCallField::ObservedServiceTier => self.observed_service_tier.take().is_some(),
            InferenceCallField::TokenUsage => self.token_usage.take().is_some(),
            InferenceCallField::TurnId
            | InferenceCallField::SpawnRequestId
            | InferenceCallField::ConfiguredProvider
            | InferenceCallField::RequestedModel => false,
        };
        if present && !self.omitted_fields.contains(&field) {
            self.omitted_fields.push(field);
        }
    }

    fn serialized_len(&self) -> Option<usize> {
        #[derive(Serialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum EventRef<'a> {
            InferenceCall(&'a InferenceCallEvent),
        }

        let mut writer = LengthWriter::default();
        serde_json::to_writer(&mut writer, &EventRef::InferenceCall(self))
            .ok()
            .map(|()| writer.len)
    }
}

#[derive(Default)]
struct LengthWriter {
    len: usize,
}

impl io::Write for LengthWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.len += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn truncate_required_string(
    value: &mut String,
    max_bytes: usize,
    field: InferenceCallField,
    truncated_fields: &mut Vec<InferenceCallField>,
) {
    if value.len() <= max_bytes {
        return;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    if !truncated_fields.contains(&field) {
        truncated_fields.push(field);
    }
}

#[cfg(test)]
#[path = "inference_observation_tests.rs"]
mod tests;
