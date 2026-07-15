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

#[cfg(test)]
mod tests {
    use codex_protocol::ThreadId;
    use codex_protocol::models::ThreadInferenceIdentity;
    use tempfile::TempDir;

    use super::*;
    use crate::ThreadInferenceIdentitySidecarPatch;
    use crate::local::test_support::test_config;

    #[tokio::test]
    async fn identity_update_without_sqlite_fails_closed() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let thread_id = ThreadId::default();
        let identity =
            ThreadInferenceIdentity::new("configured", "provider", /*reasoning_effort*/ None)
                .expect("identity should be valid");

        let error = update(
            &store,
            UpdateThreadInferenceIdentitySidecarParams {
                thread_id,
                patch: ThreadInferenceIdentitySidecarPatch {
                    configured: Some(Some(identity)),
                    latest_request: None,
                },
            },
        )
        .await
        .expect_err("identity authority must not acknowledge a SQLite-less write");
        assert!(matches!(error, ThreadStoreError::Internal { .. }));

        update(
            &store,
            UpdateThreadInferenceIdentitySidecarParams {
                thread_id,
                patch: ThreadInferenceIdentitySidecarPatch::default(),
            },
        )
        .await
        .expect("omitted authority should not require SQLite");
    }
}
