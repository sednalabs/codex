use std::borrow::Cow;
use std::fmt;
use std::io;
use std::str::FromStr;

use schemars::JsonSchema;
use schemars::Schema;
use schemars::SchemaGenerator;
use schemars::json_schema;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use serde::de::Error;
use serde_json::Value as JsonValue;
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
    /// Local operation that caused this inference attempt, when known.
    ///
    /// This identifies local provenance only. It does not assert provider
    /// execution, billing, or a successful response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub source: Option<InferenceCallSource>,
    pub status: InferenceCallStatus,
    pub transport: InferenceCallTransport,
    /// Provider selected by local configuration.
    pub configured_provider: String,
    /// Model selected by local configuration, when explicitly configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub configured_model: Option<String>,
    /// Model placed on this concrete provider request.
    pub requested_model: String,
    /// Effective provider identity at the execution boundary.
    #[serde(default = "unknown_identity")]
    pub effective_provider: String,
    /// Effective model identity at the execution boundary.
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
    /// Exact usage for this response. This is never an accumulated or estimated total.
    pub token_usage: Option<TokenUsage>,
    /// Bounded detail explaining a non-success outcome, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub outcome_detail: Option<String>,
    /// Required string fields shortened to fit the durable observation limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub truncated_fields: Option<Vec<InferenceCallField>>,
    /// Optional or lifecycle-inapplicable evidence removed from this observation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub omitted_fields: Option<Vec<InferenceCallField>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, TS, JsonSchema)]
#[schemars(with = "String")]
#[ts(type = "string")]
pub enum InferenceCallStatus {
    Started,
    Completed,
    Failed,
    Cancelled,
    UsageLimitReached,
    LocalDenied,
    TransportUncertain,
    /// A status introduced by a newer producer. The exact wire token is
    /// retained so it can be round-tripped without being mistaken for
    /// success.
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, TS, JsonSchema)]
#[schemars(with = "String")]
#[ts(type = "string")]
pub enum InferenceCallTransport {
    ResponsesHttp,
    ResponsesWebsocket,
    /// A transport introduced by a newer producer. The exact wire token is
    /// retained for lossless round-tripping.
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, TS, JsonSchema)]
#[schemars(with = "String")]
#[ts(type = "string")]
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
    /// A receipt field introduced by a newer producer. The exact wire token
    /// is retained so older readers do not silently rewrite it.
    Unknown(String),
}

/// Local provenance for one inference attempt.
///
/// Known source objects use a tagged object shape. Unknown objects retain the
/// complete raw object, including the discriminator and any future fields, so
/// decoding and re-encoding cannot silently discard provenance.
#[derive(Debug, Clone, PartialEq, Eq, TS)]
#[ts(
    type = r#"{ type: "direct" } | { type: "host_continuity_check" } | { type: "code_mode"; cell_id: string; runtime_tool_call_id: string } | { type: string; [key: string]: unknown }"#
)]
pub enum InferenceCallSource {
    Direct,
    HostContinuityCheck,
    CodeMode {
        cell_id: String,
        runtime_tool_call_id: String,
    },
    Unknown {
        #[ts(skip)]
        raw: serde_json::Map<String, JsonValue>,
    },
}

impl InferenceCallStatus {
    /// Returns the exact value used on the wire.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Started => "started",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::UsageLimitReached => "usage_limit_reached",
            Self::LocalDenied => "local_denied",
            Self::TransportUncertain => "transport_uncertain",
            Self::Unknown(value) => value,
        }
    }
}

impl fmt::Display for InferenceCallStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for InferenceCallStatus {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "started" => Ok(Self::Started),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "usage_limit_reached" => Ok(Self::UsageLimitReached),
            "local_denied" => Ok(Self::LocalDenied),
            "transport_uncertain" => Ok(Self::TransportUncertain),
            "" => Err("inference_call status must not be empty".to_string()),
            value => Ok(Self::Unknown(value.to_string())),
        }
    }
}

impl Serialize for InferenceCallStatus {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for InferenceCallStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(D::Error::custom)
    }
}

impl InferenceCallTransport {
    /// Returns the exact value used on the wire.
    pub fn as_str(&self) -> &str {
        match self {
            Self::ResponsesHttp => "responses_http",
            Self::ResponsesWebsocket => "responses_websocket",
            Self::Unknown(value) => value,
        }
    }
}

impl fmt::Display for InferenceCallTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for InferenceCallTransport {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "responses_http" => Ok(Self::ResponsesHttp),
            "responses_websocket" => Ok(Self::ResponsesWebsocket),
            "" => Err("inference_call transport must not be empty".to_string()),
            value => Ok(Self::Unknown(value.to_string())),
        }
    }
}

impl Serialize for InferenceCallTransport {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for InferenceCallTransport {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(D::Error::custom)
    }
}

impl InferenceCallField {
    /// Returns the exact value used on the wire.
    pub fn as_str(&self) -> &str {
        match self {
            Self::TurnId => "turn_id",
            Self::SpawnRequestId => "spawn_request_id",
            Self::ConfiguredProvider => "configured_provider",
            Self::ConfiguredModel => "configured_model",
            Self::RequestedModel => "requested_model",
            Self::EffectiveProvider => "effective_provider",
            Self::EffectiveModel => "effective_model",
            Self::RequestedServiceTier => "requested_service_tier",
            Self::RequestCompletedAtMs => "request_completed_at_ms",
            Self::ResponseId => "response_id",
            Self::UpstreamRequestId => "upstream_request_id",
            Self::ObservedProvider => "observed_provider",
            Self::ObservedModel => "observed_model",
            Self::ObservedModelSnapshot => "observed_model_snapshot",
            Self::ObservedServiceTier => "observed_service_tier",
            Self::TokenUsage => "token_usage",
            Self::OutcomeDetail => "outcome_detail",
            Self::Unknown(value) => value,
        }
    }
}

impl fmt::Display for InferenceCallField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for InferenceCallField {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "turn_id" => Ok(Self::TurnId),
            "spawn_request_id" => Ok(Self::SpawnRequestId),
            "configured_provider" => Ok(Self::ConfiguredProvider),
            "configured_model" => Ok(Self::ConfiguredModel),
            "requested_model" => Ok(Self::RequestedModel),
            "effective_provider" => Ok(Self::EffectiveProvider),
            "effective_model" => Ok(Self::EffectiveModel),
            "requested_service_tier" => Ok(Self::RequestedServiceTier),
            "request_completed_at_ms" => Ok(Self::RequestCompletedAtMs),
            "response_id" => Ok(Self::ResponseId),
            "upstream_request_id" => Ok(Self::UpstreamRequestId),
            "observed_provider" => Ok(Self::ObservedProvider),
            "observed_model" => Ok(Self::ObservedModel),
            "observed_model_snapshot" => Ok(Self::ObservedModelSnapshot),
            "observed_service_tier" => Ok(Self::ObservedServiceTier),
            "token_usage" => Ok(Self::TokenUsage),
            "outcome_detail" => Ok(Self::OutcomeDetail),
            "" => Err("inference_call field must not be empty".to_string()),
            value => Ok(Self::Unknown(value.to_string())),
        }
    }
}

impl Serialize for InferenceCallField {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for InferenceCallField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(D::Error::custom)
    }
}

impl Serialize for InferenceCallSource {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeMap;

        match self {
            Self::Direct => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("type", "direct")?;
                map.end()
            }
            Self::HostContinuityCheck => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("type", "host_continuity_check")?;
                map.end()
            }
            Self::CodeMode {
                cell_id,
                runtime_tool_call_id,
            } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("type", "code_mode")?;
                map.serialize_entry("cell_id", cell_id)?;
                map.serialize_entry("runtime_tool_call_id", runtime_tool_call_id)?;
                map.end()
            }
            Self::Unknown { raw } => raw.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for InferenceCallSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let JsonValue::Object(raw) = JsonValue::deserialize(deserializer)? else {
            return Err(D::Error::custom("inference_call source must be an object"));
        };
        let Some(type_value) = raw.get("type") else {
            return Err(D::Error::custom("inference_call source type is required"));
        };
        let Some(source_type) = type_value.as_str().map(str::to_owned) else {
            return Err(D::Error::custom(
                "inference_call source type must be a string",
            ));
        };
        if source_type.is_empty() {
            return Err(D::Error::custom(
                "inference_call source type must not be empty",
            ));
        }

        match source_type.as_str() {
            "direct" if raw.len() == 1 => Ok(Self::Direct),
            "host_continuity_check" if raw.len() == 1 => Ok(Self::HostContinuityCheck),
            "code_mode" => {
                let Some(cell_id) = raw.get("cell_id").and_then(JsonValue::as_str) else {
                    return Err(D::Error::custom(
                        "inference_call code_mode source cell_id must be a string",
                    ));
                };
                let Some(runtime_tool_call_id) =
                    raw.get("runtime_tool_call_id").and_then(JsonValue::as_str)
                else {
                    return Err(D::Error::custom(
                        "inference_call code_mode source runtime_tool_call_id must be a string",
                    ));
                };
                if raw.len() == 3 {
                    Ok(Self::CodeMode {
                        cell_id: cell_id.to_string(),
                        runtime_tool_call_id: runtime_tool_call_id.to_string(),
                    })
                } else {
                    Ok(Self::Unknown { raw })
                }
            }
            "direct" | "host_continuity_check" => Ok(Self::Unknown { raw }),
            _ => Ok(Self::Unknown { raw }),
        }
    }
}

impl JsonSchema for InferenceCallSource {
    fn schema_name() -> Cow<'static, str> {
        "InferenceCallSource".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "object",
            "anyOf": [
                {
                    "type": "object",
                    "properties": {"type": {"const": "direct"}},
                    "required": ["type"],
                    "additionalProperties": false,
                },
                {
                    "type": "object",
                    "properties": {"type": {"const": "host_continuity_check"}},
                    "required": ["type"],
                    "additionalProperties": false,
                },
                {
                    "type": "object",
                    "properties": {
                        "type": {"const": "code_mode"},
                        "cell_id": {"type": "string"},
                        "runtime_tool_call_id": {"type": "string"},
                    },
                    "required": ["type", "cell_id", "runtime_tool_call_id"],
                    "additionalProperties": false,
                },
                {
                    "type": "object",
                    "properties": {"type": {"type": "string", "minLength": 1}},
                    "required": ["type"],
                    "additionalProperties": true,
                },
            ],
        })
    }
}

impl InferenceCallSource {
    fn has_oversized_correlation_id(&self) -> bool {
        match self {
            Self::CodeMode {
                cell_id,
                runtime_tool_call_id,
            } => {
                cell_id.len() > INFERENCE_CALL_CORRELATION_ID_MAX_BYTES
                    || runtime_tool_call_id.len() > INFERENCE_CALL_CORRELATION_ID_MAX_BYTES
            }
            Self::Unknown { raw }
                if raw.get("type").and_then(JsonValue::as_str) == Some("code_mode") =>
            {
                raw.get("cell_id")
                    .and_then(JsonValue::as_str)
                    .is_some_and(|id| id.len() > INFERENCE_CALL_CORRELATION_ID_MAX_BYTES)
                    || raw
                        .get("runtime_tool_call_id")
                        .and_then(JsonValue::as_str)
                        .is_some_and(|id| id.len() > INFERENCE_CALL_CORRELATION_ID_MAX_BYTES)
            }
            Self::Direct | Self::HostContinuityCheck | Self::Unknown { .. } => false,
        }
    }
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
            || self
                .source
                .as_ref()
                .is_some_and(InferenceCallSource::has_oversized_correlation_id)
        {
            return None;
        }

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
                .string_field(&field)
                .is_some_and(|value| value.len() > INFERENCE_CALL_STRING_MAX_BYTES)
            {
                self.omit_field(&field);
            }
        }

        for field in [
            InferenceCallField::ConfiguredModel,
            InferenceCallField::RequestedServiceTier,
            InferenceCallField::OutcomeDetail,
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
            self.omit_field(&field);
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
        let fields: &[InferenceCallField] = match &self.status {
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
            | InferenceCallStatus::Unknown(_) => &[
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
            self.omit_field(field);
        }
    }

    fn string_field(&self, field: &InferenceCallField) -> Option<&str> {
        match field {
            InferenceCallField::RequestedServiceTier => self.requested_service_tier.as_deref(),
            InferenceCallField::ConfiguredModel => self.configured_model.as_deref(),
            InferenceCallField::ResponseId => self.response_id.as_deref(),
            InferenceCallField::UpstreamRequestId => self.upstream_request_id.as_deref(),
            InferenceCallField::ObservedProvider => self.observed_provider.as_deref(),
            InferenceCallField::ObservedModel => self.observed_model.as_deref(),
            InferenceCallField::ObservedModelSnapshot => self.observed_model_snapshot.as_deref(),
            InferenceCallField::ObservedServiceTier => self.observed_service_tier.as_deref(),
            InferenceCallField::OutcomeDetail => self.outcome_detail.as_deref(),
            InferenceCallField::TurnId
            | InferenceCallField::SpawnRequestId
            | InferenceCallField::ConfiguredProvider
            | InferenceCallField::RequestedModel
            | InferenceCallField::EffectiveProvider
            | InferenceCallField::EffectiveModel
            | InferenceCallField::RequestCompletedAtMs
            | InferenceCallField::TokenUsage
            | InferenceCallField::Unknown(_) => None,
        }
    }

    fn omit_field(&mut self, field: &InferenceCallField) {
        let present = match field {
            InferenceCallField::RequestedServiceTier => {
                self.requested_service_tier.take().is_some()
            }
            InferenceCallField::ConfiguredModel => self.configured_model.take().is_some(),
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
            InferenceCallField::TurnId
            | InferenceCallField::SpawnRequestId
            | InferenceCallField::ConfiguredProvider
            | InferenceCallField::RequestedModel
            | InferenceCallField::EffectiveProvider
            | InferenceCallField::EffectiveModel
            | InferenceCallField::Unknown(_) => false,
        };
        if present {
            record_field(&mut self.omitted_fields, field.clone());
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

fn unknown_identity() -> String {
    "<unknown>".to_string()
}

#[cfg(test)]
#[path = "inference_observation_tests.rs"]
mod tests;
