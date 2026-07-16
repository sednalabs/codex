use codex_protocol::ThreadId;
use codex_protocol::models::ThreadInferenceIdentity;
use codex_protocol::models::ThreadInferenceIdentityAuthority;
use serde::Deserialize;
use serde::Serialize;

use crate::ClearableField;

/// Optional typed sidecar for inference identity authority owned by a thread store.
///
/// Keeping this state separate from [`crate::StoredThread`] and [`crate::ThreadMetadataPatch`]
/// lets stores expose an optional, fail-closed capability without breaking external struct
/// literals or existing [`crate::ThreadStore`] implementations.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ThreadInferenceIdentitySidecar {
    #[serde(default)]
    pub configured: ThreadInferenceIdentityAuthority,
    #[serde(default)]
    pub latest_request: ThreadInferenceIdentityAuthority,
}

/// Parameters for reading the complete inference identity sidecar for a stored thread.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReadThreadInferenceIdentitySidecarParams {
    pub thread_id: ThreadId,
    pub include_archived: bool,
}

/// Presence-aware inference identity update: omission is a no-op, `null` clears, and an identity
/// sets valid authority. Legacy-missing and malformed authority cannot be written through it.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadInferenceIdentitySidecarPatch {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "strict_optional_identity"
    )]
    pub configured: ClearableField<ThreadInferenceIdentity>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "strict_optional_identity"
    )]
    pub latest_request: ClearableField<ThreadInferenceIdentity>,
}

mod strict_optional_identity {
    use std::fmt;

    use codex_protocol::models::ThreadInferenceIdentity;
    use codex_protocol::openai_models::ReasoningEffort;
    use serde::Deserialize;
    use serde::Deserializer;
    use serde::Serializer;
    use serde::de::Error as _;
    use serde::de::MapAccess;
    use serde::de::Visitor;

    use crate::ClearableField;

    const FIELDS: &[&str] = &["model", "model_provider_id", "reasoning_effort"];

    struct StrictThreadInferenceIdentity {
        model: String,
        model_provider_id: String,
        reasoning_effort: Option<ReasoningEffort>,
    }

    struct StrictThreadInferenceIdentityVisitor;

    impl<'de> Visitor<'de> for StrictThreadInferenceIdentityVisitor {
        type Value = StrictThreadInferenceIdentity;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a strict thread inference identity object")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut model = None;
            let mut model_provider_id = None;
            let mut reasoning_effort: Option<Option<ReasoningEffort>> = None;
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
                        reasoning_effort = Some(map.next_value()?);
                    }
                    _ => return Err(A::Error::unknown_field(field.as_str(), FIELDS)),
                }
            }
            Ok(StrictThreadInferenceIdentity {
                model: model.ok_or_else(|| A::Error::missing_field("model"))?,
                model_provider_id: model_provider_id
                    .ok_or_else(|| A::Error::missing_field("model_provider_id"))?,
                reasoning_effort: reasoning_effort
                    .ok_or_else(|| A::Error::missing_field("reasoning_effort"))?,
            })
        }
    }

    impl<'de> Deserialize<'de> for StrictThreadInferenceIdentity {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            deserializer.deserialize_struct(
                "ThreadInferenceIdentity",
                FIELDS,
                StrictThreadInferenceIdentityVisitor,
            )
        }
    }

    pub fn serialize<S>(
        value: &ClearableField<ThreadInferenceIdentity>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        crate::types::optional_option::serialize(value, serializer)
    }

    pub fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<ClearableField<ThreadInferenceIdentity>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let identity = Option::<StrictThreadInferenceIdentity>::deserialize(deserializer)?;
        let identity = identity
            .map(|identity| {
                ThreadInferenceIdentity::new(
                    identity.model,
                    identity.model_provider_id,
                    identity.reasoning_effort,
                )
                .map_err(D::Error::custom)
            })
            .transpose()?;
        Ok(Some(identity))
    }
}

impl ThreadInferenceIdentitySidecarPatch {
    pub fn is_empty(&self) -> bool {
        let Self {
            configured,
            latest_request,
        } = self;
        configured.is_none() && latest_request.is_none()
    }
}

/// Parameters for atomically updating inference identity authority.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UpdateThreadInferenceIdentitySidecarParams {
    pub thread_id: ThreadId,
    pub patch: ThreadInferenceIdentitySidecarPatch,
}

#[cfg(test)]
#[path = "inference_identity_tests.rs"]
mod tests;
