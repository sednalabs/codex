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
    use codex_protocol::models::ThreadInferenceIdentity;
    use codex_protocol::openai_models::ReasoningEffort;
    use serde::Deserialize;
    use serde::Deserializer;
    use serde::Serializer;
    use serde::de::Error as _;

    use crate::ClearableField;

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StrictThreadInferenceIdentity {
        model: String,
        model_provider_id: String,
        reasoning_effort: Option<ReasoningEffort>,
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
        self.configured.is_none() && self.latest_request.is_none()
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
