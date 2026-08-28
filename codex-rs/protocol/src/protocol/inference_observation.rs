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
    /// Session source selected by the local caller, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub session_source: Option<String>,
    /// Parent thread for a spawned request, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub parent_thread_id: Option<ThreadId>,
    /// Protocol-neutral kind of request represented by this observation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub request_kind: Option<String>,
    /// Whether request/session cache state was reused for this attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub cache_state: Option<String>,
    /// Local admission result attached to this exact attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub admission: Option<String>,
    /// Durable authority carried by a goal-owner continuation, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub goal_owner_admission_ref: Option<GoalOwnerAdmissionRef>,
    pub status: InferenceCallStatus,
    pub transport: InferenceCallTransport,
    /// Explicit local provenance for the operation that caused this attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub source: Option<InferenceCallSource>,
    /// Provider selected by local configuration.
    pub configured_provider: String,
    /// Model setting selected by local configuration, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub configured_model: Option<String>,
    /// Model placed on this concrete provider request.
    pub requested_model: String,
    /// Provider that owns the request after local routing/admission resolution.
    /// This is never inferred from `configured_provider`.
    #[serde(default = "unknown_identity")]
    pub effective_provider: String,
    /// Model resolved for the concrete request after local routing/admission.
    #[serde(default = "unknown_identity")]
    pub effective_model: String,
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
    /// Bounded terminal detail such as denial or usage-limit evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub outcome_detail: Option<String>,
    /// Exact usage for this response. This is never an accumulated or estimated total.
    pub token_usage: Option<TokenUsage>,
    /// Required string fields shortened to fit the durable observation limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub truncated_fields: Option<Vec<InferenceCallField>>,
    /// Optional or lifecycle-inapplicable evidence removed from this observation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub omitted_fields: Option<Vec<InferenceCallField>>,
}

/// Bounded, replayable authority for one scheduled goal-owner successor.
///
/// This is deliberately protocol-neutral: the core admission broker validates
/// it against its state-store record before opening a physical request.
#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
pub struct GoalOwnerAdmissionRef {
    pub goal_id: String,
    pub generation: i64,
    pub cancellation_epoch: i64,
    pub decision_id: String,
    pub intended_request_kind: String,
    pub successor_turn_id: String,
    pub logical_successor_request_id: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum InferenceCallStatus {
    Started,
    Completed,
    Failed,
    Cancelled,
    UsageLimitReached,
    LocalDenied,
    TransportUncertain,
    /// A status introduced by a newer producer. Unknown statuses remain
    /// durable evidence and are never interpreted as success.
    #[serde(other)]
    Unknown,
}

/// Protocol-neutral provenance for the local operation that caused an attempt.
/// This is not provider execution evidence.
#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "type")]
#[ts(rename_all = "snake_case")]
pub enum InferenceCallSource {
    Direct,
    HostContinuityCheck,
    CodeMode {
        cell_id: String,
        runtime_tool_call_id: String,
    },
    /// A provenance source introduced by a newer producer.
    #[serde(other)]
    Unknown,
}

impl InferenceCallSource {
    pub fn code_mode(cell_id: String, runtime_tool_call_id: String) -> Self {
        Self::CodeMode {
            cell_id,
            runtime_tool_call_id,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum InferenceCallTransport {
    ResponsesHttp,
    ResponsesWebsocket,
    /// A transport introduced by a newer producer.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum InferenceCallField {
    TurnId,
    SpawnRequestId,
    ConfiguredProvider,
    ConfiguredModel,
    RequestedModel,
    EffectiveProvider,
    EffectiveModel,
    RequestedServiceTier,
    RequestCompletedAtMs,
    ResponseId,
    UpstreamRequestId,
    ObservedProvider,
    ObservedModel,
    ObservedModelSnapshot,
    ObservedServiceTier,
    TokenUsage,
    OutcomeDetail,
    /// A metadata field introduced by a newer producer.
    #[serde(other)]
    Unknown,
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
            || self.source.as_ref().is_some_and(|source| {
                matches!(
                    source,
                    InferenceCallSource::CodeMode {
                        cell_id,
                        runtime_tool_call_id,
                    } if cell_id.len() > INFERENCE_CALL_CORRELATION_ID_MAX_BYTES
                        || runtime_tool_call_id.len() > INFERENCE_CALL_CORRELATION_ID_MAX_BYTES
                )
            })
        {
            return None;
        }

        // Preserve prior normalization receipts when a durable event is
        // validated or replay-normalized a second time. This keeps
        // truncation/omission provenance lossless across reducer replay.
        let prior_truncated_fields = self.truncated_fields.take();
        let prior_omitted_fields = self.omitted_fields.take();
        self.truncated_fields = prior_truncated_fields;
        self.omitted_fields = prior_omitted_fields;
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
        truncate_required_string(
            &mut self.effective_provider,
            INFERENCE_CALL_STRING_MAX_BYTES,
            InferenceCallField::EffectiveProvider,
            &mut self.truncated_fields,
        );
        truncate_required_string(
            &mut self.effective_model,
            INFERENCE_CALL_STRING_MAX_BYTES,
            InferenceCallField::EffectiveModel,
            &mut self.truncated_fields,
        );

        for field in [
            InferenceCallField::ConfiguredModel,
            InferenceCallField::RequestedServiceTier,
            InferenceCallField::ResponseId,
            InferenceCallField::UpstreamRequestId,
            InferenceCallField::ObservedProvider,
            InferenceCallField::ObservedModel,
            InferenceCallField::ObservedModelSnapshot,
            InferenceCallField::ObservedServiceTier,
            InferenceCallField::OutcomeDetail,
        ] {
            if self
                .string_field(field)
                .is_some_and(|value| value.len() > INFERENCE_CALL_STRING_MAX_BYTES)
            {
                self.omit_field(field);
            }
        }

        for field in [
            InferenceCallField::OutcomeDetail,
            InferenceCallField::ConfiguredModel,
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
            truncate_required_string(
                &mut self.effective_provider,
                INFERENCE_CALL_REQUIRED_STRING_FALLBACK_MAX_BYTES,
                InferenceCallField::EffectiveProvider,
                &mut self.truncated_fields,
            );
            truncate_required_string(
                &mut self.effective_model,
                INFERENCE_CALL_REQUIRED_STRING_FALLBACK_MAX_BYTES,
                InferenceCallField::EffectiveModel,
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
                InferenceCallField::OutcomeDetail,
            ],
            InferenceCallStatus::Failed
            | InferenceCallStatus::Cancelled
            | InferenceCallStatus::UsageLimitReached
            | InferenceCallStatus::TransportUncertain
            | InferenceCallStatus::Unknown => &[
                InferenceCallField::ResponseId,
                InferenceCallField::ObservedProvider,
                InferenceCallField::ObservedModel,
                InferenceCallField::ObservedModelSnapshot,
                InferenceCallField::ObservedServiceTier,
                InferenceCallField::TokenUsage,
            ],
            InferenceCallStatus::LocalDenied => &[
                InferenceCallField::RequestCompletedAtMs,
                InferenceCallField::ResponseId,
                InferenceCallField::UpstreamRequestId,
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
            InferenceCallField::OutcomeDetail => self.outcome_detail.as_deref(),
            InferenceCallField::ConfiguredModel => self.configured_model.as_deref(),
            InferenceCallField::TurnId
            | InferenceCallField::SpawnRequestId
            | InferenceCallField::ConfiguredProvider
            | InferenceCallField::RequestedModel
            | InferenceCallField::EffectiveProvider
            | InferenceCallField::EffectiveModel
            | InferenceCallField::RequestCompletedAtMs
            | InferenceCallField::TokenUsage
            | InferenceCallField::Unknown => None,
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
            InferenceCallField::OutcomeDetail => self.outcome_detail.take().is_some(),
            InferenceCallField::ConfiguredModel => self.configured_model.take().is_some(),
            InferenceCallField::TurnId
            | InferenceCallField::SpawnRequestId
            | InferenceCallField::ConfiguredProvider
            | InferenceCallField::RequestedModel
            | InferenceCallField::EffectiveProvider
            | InferenceCallField::EffectiveModel
            | InferenceCallField::Unknown => false,
        };
        if present {
            record_field(&mut self.omitted_fields, field);
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

fn unknown_identity() -> String {
    "<unknown>".to_string()
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
    truncated_fields: &mut Option<Vec<InferenceCallField>>,
) {
    if value.len() <= max_bytes {
        return;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    record_field(truncated_fields, field);
}

fn record_field(fields: &mut Option<Vec<InferenceCallField>>, field: InferenceCallField) {
    let fields = fields.get_or_insert_default();
    if !fields.contains(&field) {
        fields.push(field);
    }
}

#[cfg(test)]
#[path = "inference_observation_tests.rs"]
mod tests;
