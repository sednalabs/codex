use super::LocalThreadStore;
use super::read_thread;
use crate::ReadThreadInferenceIdentitySidecarParams;
use crate::ReadThreadParams;
use crate::ThreadInferenceIdentitySidecar;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

pub(super) async fn read_thread_inference_identity_sidecar(
    store: &LocalThreadStore,
    params: ReadThreadInferenceIdentitySidecarParams,
) -> ThreadStoreResult<ThreadInferenceIdentitySidecar> {
    let Some(state_db) = store.state_db().await else {
        return Err(ThreadStoreError::Unsupported {
            operation: "read_thread_inference_identity_sidecar",
        });
    };
    read_thread::read_thread(
        store,
        ReadThreadParams {
            thread_id: params.thread_id,
            include_archived: params.include_archived,
            include_history: false,
        },
    )
    .await?;
    state_db
        .get_thread_inference_identity_authority(params.thread_id)
        .await
        .map(|snapshot| snapshot.map(sidecar_from_snapshot).unwrap_or_default())
        .map_err(|err| ThreadStoreError::Internal {
            message: format!(
                "failed to read inference identity authority for thread {}: {err}",
                params.thread_id
            ),
        })
}

fn sidecar_from_snapshot(
    snapshot: codex_state::ThreadInferenceIdentityAuthoritySnapshot,
) -> ThreadInferenceIdentitySidecar {
    ThreadInferenceIdentitySidecar {
        configured: snapshot.configured,
        latest_request: snapshot.latest_request,
    }
}

#[cfg(test)]
#[path = "read_thread_inference_identity_tests.rs"]
mod tests;
