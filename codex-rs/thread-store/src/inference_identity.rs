use codex_protocol::models::ThreadInferenceIdentityAuthority;
use serde::Deserialize;
use serde::Serialize;

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

#[cfg(test)]
#[path = "inference_identity_tests.rs"]
mod tests;
