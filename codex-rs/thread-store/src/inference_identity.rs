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

/// Presence-aware inference identity update owned by the thread store.
///
/// An omitted field is a no-op, an explicit `null` clears the authority, and an identity sets
/// valid authority. Legacy-missing and malformed authority cannot be written through this API.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ThreadInferenceIdentitySidecarPatch {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::types::optional_option"
    )]
    pub configured: ClearableField<ThreadInferenceIdentity>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::types::optional_option"
    )]
    pub latest_request: ClearableField<ThreadInferenceIdentity>,
}

impl ThreadInferenceIdentitySidecarPatch {
    pub fn is_empty(&self) -> bool {
        self.configured.is_none() && self.latest_request.is_none()
    }

    pub(crate) fn apply_to(self, sidecar: &mut ThreadInferenceIdentitySidecar) {
        if let Some(configured) = self.configured {
            sidecar.configured = configured.map_or_else(
                ThreadInferenceIdentityAuthority::cleared,
                ThreadInferenceIdentityAuthority::Valid,
            );
        }
        if let Some(latest_request) = self.latest_request {
            sidecar.latest_request = latest_request.map_or_else(
                ThreadInferenceIdentityAuthority::cleared,
                ThreadInferenceIdentityAuthority::Valid,
            );
        }
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
