use std::error::Error;
use std::fmt;

use codex_protocol::models::ThreadInferenceIdentity;
use codex_protocol::models::ThreadInferenceIdentityAuthority;
use codex_protocol::models::ThreadInferenceIdentityValidationError;
use codex_protocol::openai_models::ReasoningEffort;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::de::Error as _;
use serde::de::MapAccess;
use serde::de::Visitor;

pub const THREAD_INFERENCE_IDENTITY_AUTHORITY_VERSION: u8 = 1;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredThreadInferenceIdentityEnvelopeV1 {
    version: u8,
    authority: StoredThreadInferenceIdentityAuthorityV1,
}

#[derive(Deserialize, Serialize)]
#[serde(
    deny_unknown_fields,
    tag = "status",
    content = "value",
    rename_all = "snake_case"
)]
enum StoredThreadInferenceIdentityAuthorityV1 {
    Valid(StoredThreadInferenceIdentityValueV1),
    Cleared(StoredThreadInferenceIdentityClearedV1),
}

#[derive(Serialize)]
struct StoredThreadInferenceIdentityValueV1 {
    model: String,
    model_provider_id: String,
    reasoning_effort: Option<ReasoningEffort>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredThreadInferenceIdentityClearedV1 {}

const STORED_THREAD_INFERENCE_IDENTITY_VALUE_FIELDS_V1: &[&str] =
    &["model", "model_provider_id", "reasoning_effort"];

struct StoredThreadInferenceIdentityValueVisitorV1;

impl<'de> Visitor<'de> for StoredThreadInferenceIdentityValueVisitorV1 {
    type Value = StoredThreadInferenceIdentityValueV1;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a strict version 1 thread inference identity value object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut model = None;
        let mut model_provider_id = None;
        let mut reasoning_effort = None;
        while let Some(field) = map.next_key::<String>()? {
            match field.as_str() {
                "model" => {
                    if model.is_some() {
                        return Err(A::Error::duplicate_field("model"));
                    }
                    model = Some(map.next_value()?);
                }
                "model_provider_id" => {
                    if model_provider_id.is_some() {
                        return Err(A::Error::duplicate_field("model_provider_id"));
                    }
                    model_provider_id = Some(map.next_value()?);
                }
                "reasoning_effort" => {
                    if reasoning_effort.is_some() {
                        return Err(A::Error::duplicate_field("reasoning_effort"));
                    }
                    reasoning_effort = Some(map.next_value::<Option<ReasoningEffort>>()?);
                }
                _ => {
                    return Err(A::Error::unknown_field(
                        &field,
                        STORED_THREAD_INFERENCE_IDENTITY_VALUE_FIELDS_V1,
                    ));
                }
            }
        }
        Ok(StoredThreadInferenceIdentityValueV1 {
            model: model.ok_or_else(|| A::Error::missing_field("model"))?,
            model_provider_id: model_provider_id
                .ok_or_else(|| A::Error::missing_field("model_provider_id"))?,
            reasoning_effort: reasoning_effort
                .ok_or_else(|| A::Error::missing_field("reasoning_effort"))?,
        })
    }
}

impl<'de> Deserialize<'de> for StoredThreadInferenceIdentityValueV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_struct(
            "StoredThreadInferenceIdentityValueV1",
            STORED_THREAD_INFERENCE_IDENTITY_VALUE_FIELDS_V1,
            StoredThreadInferenceIdentityValueVisitorV1,
        )
    }
}

/// Failure to encode typed thread inference identity authority for durable storage.
#[derive(Debug)]
pub enum ThreadInferenceIdentityAuthorityEncodeError {
    InvalidIdentity(ThreadInferenceIdentityValidationError),
    MalformedAuthority,
    Serialization(serde_json::Error),
}

impl fmt::Display for ThreadInferenceIdentityAuthorityEncodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentity(error) => {
                write!(formatter, "invalid inference identity: {error}")
            }
            Self::MalformedAuthority => {
                formatter.write_str("malformed inference identity authority cannot be written")
            }
            Self::Serialization(error) => {
                write!(
                    formatter,
                    "failed to serialize inference identity authority: {error}"
                )
            }
        }
    }
}

impl Error for ThreadInferenceIdentityAuthorityEncodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidIdentity(error) => Some(error),
            Self::Serialization(error) => Some(error),
            Self::MalformedAuthority => None,
        }
    }
}

/// Decodes strict version 1 authority without treating malformed data as legacy absence.
///
/// SQL `NULL` is the only representation of [`ThreadInferenceIdentityAuthority::LegacyMissing`].
/// Every stored object rejects unknown fields, and valid identity payloads must explicitly include
/// a nullable `reasoning_effort` field.
pub fn decode_thread_inference_identity_authority(
    raw: Option<&str>,
) -> ThreadInferenceIdentityAuthority {
    let Some(raw) = raw else {
        return ThreadInferenceIdentityAuthority::LegacyMissing;
    };
    let Ok(envelope) = serde_json::from_str::<StoredThreadInferenceIdentityEnvelopeV1>(raw) else {
        return ThreadInferenceIdentityAuthority::Malformed {
            raw: raw.to_string(),
        };
    };
    if envelope.version != THREAD_INFERENCE_IDENTITY_AUTHORITY_VERSION {
        return ThreadInferenceIdentityAuthority::Malformed {
            raw: raw.to_string(),
        };
    }
    match envelope.authority {
        StoredThreadInferenceIdentityAuthorityV1::Valid(value) => {
            match ThreadInferenceIdentity::new(
                value.model,
                value.model_provider_id,
                value.reasoning_effort,
            ) {
                Ok(identity) => ThreadInferenceIdentityAuthority::Valid(identity),
                Err(_) => ThreadInferenceIdentityAuthority::Malformed {
                    raw: raw.to_string(),
                },
            }
        }
        StoredThreadInferenceIdentityAuthorityV1::Cleared(_) => {
            ThreadInferenceIdentityAuthority::cleared()
        }
    }
}

/// Encodes validated authority using the strict version 1 storage representation.
pub fn encode_thread_inference_identity_authority(
    authority: &ThreadInferenceIdentityAuthority,
) -> Result<Option<String>, ThreadInferenceIdentityAuthorityEncodeError> {
    let authority = match authority {
        ThreadInferenceIdentityAuthority::LegacyMissing => return Ok(None),
        ThreadInferenceIdentityAuthority::Valid(identity) => {
            identity
                .validate()
                .map_err(ThreadInferenceIdentityAuthorityEncodeError::InvalidIdentity)?;
            StoredThreadInferenceIdentityAuthorityV1::Valid(StoredThreadInferenceIdentityValueV1 {
                model: identity.model().to_string(),
                model_provider_id: identity.model_provider_id().to_string(),
                reasoning_effort: identity.reasoning_effort().cloned(),
            })
        }
        ThreadInferenceIdentityAuthority::Cleared => {
            StoredThreadInferenceIdentityAuthorityV1::Cleared(
                StoredThreadInferenceIdentityClearedV1 {},
            )
        }
        ThreadInferenceIdentityAuthority::Malformed { .. } => {
            return Err(ThreadInferenceIdentityAuthorityEncodeError::MalformedAuthority);
        }
    };
    serde_json::to_string(&StoredThreadInferenceIdentityEnvelopeV1 {
        version: THREAD_INFERENCE_IDENTITY_AUTHORITY_VERSION,
        authority,
    })
    .map(Some)
    .map_err(ThreadInferenceIdentityAuthorityEncodeError::Serialization)
}
