use codex_protocol::models::ThreadInferenceIdentity;
use codex_protocol::models::ThreadInferenceIdentityAuthority;
use codex_state::encode_thread_inference_identity_authority;

use super::LocalThreadStore;
use crate::ClearableField;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::UpdateThreadInferenceIdentitySidecarParams;

pub(super) async fn update(
    store: &LocalThreadStore,
    params: UpdateThreadInferenceIdentitySidecarParams,
) -> ThreadStoreResult<()> {
    if params.patch.is_empty() {
        return Ok(());
    }
    let configured = encode(params.patch.configured)?;
    let latest_request = encode(params.patch.latest_request)?;
    let state_db = store
        .state_db()
        .await
        .ok_or_else(|| ThreadStoreError::Internal {
            message: format!(
                "sqlite state db unavailable for inference identity update: {}",
                params.thread_id
            ),
        })?;
    let updated = state_db
        .update_thread_inference_identity_authority(
            params.thread_id,
            configured.as_deref(),
            latest_request.as_deref(),
        )
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!(
                "failed to update inference identity for {}: {err}",
                params.thread_id
            ),
        })?;
    if !updated {
        return Err(ThreadStoreError::ThreadNotFound {
            thread_id: params.thread_id,
        });
    }
    Ok(())
}

fn encode(update: ClearableField<ThreadInferenceIdentity>) -> ThreadStoreResult<Option<String>> {
    let Some(identity) = update else {
        return Ok(None);
    };
    let authority = identity.map_or_else(
        ThreadInferenceIdentityAuthority::cleared,
        ThreadInferenceIdentityAuthority::Valid,
    );
    encode_thread_inference_identity_authority(&authority)
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to encode inference identity authority: {err}"),
        })?
        .ok_or_else(|| ThreadStoreError::Internal {
            message: "inference identity update encoded as legacy missing".to_string(),
        })
        .map(Some)
}
