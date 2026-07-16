use std::path::PathBuf;

use codex_protocol::ThreadId;
use codex_rollout::find_archived_thread_path_by_id_str;
use codex_rollout::find_thread_path_by_id_str;

use super::LocalThreadStore;
use super::read_thread;
use crate::ReadThreadInferenceIdentitySidecarParams;
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
    let _lifecycle_custody = store.acquire_lifecycle_custody(params.thread_id).await?;
    ensure_logically_eligible(store, &state_db, &params).await?;

    state_db
        .get_thread_inference_identity_authority(params.thread_id)
        .await
        .map(|snapshot| {
            snapshot
                .map(|snapshot| ThreadInferenceIdentitySidecar {
                    configured: snapshot.configured,
                    latest_request: snapshot.latest_request,
                })
                .unwrap_or_default()
        })
        .map_err(|err| ThreadStoreError::Internal {
            message: format!(
                "failed to read inference identity authority for thread {}: {err}",
                params.thread_id
            ),
        })
}

async fn ensure_logically_eligible(
    store: &LocalThreadStore,
    state_db: &codex_state::StateRuntime,
    params: &ReadThreadInferenceIdentitySidecarParams,
) -> ThreadStoreResult<()> {
    let thread_id = params.thread_id;
    if store.live_recorders.lock().await.contains_key(&thread_id) {
        return Ok(());
    }

    if let Some(path) = find_rollout(store, state_db, thread_id, /*archived*/ false).await? {
        return verify_rollout_owner(store, path, thread_id).await;
    }

    if let Some(path) = find_rollout(store, state_db, thread_id, /*archived*/ true).await? {
        verify_rollout_owner(store, path, thread_id).await?;
        if params.include_archived {
            return Ok(());
        }
        return Err(ThreadStoreError::InvalidRequest {
            message: format!("thread {thread_id} is archived"),
        });
    }

    Err(ThreadStoreError::ThreadNotFound { thread_id })
}

async fn find_rollout(
    store: &LocalThreadStore,
    state_db: &codex_state::StateRuntime,
    thread_id: ThreadId,
    archived: bool,
) -> ThreadStoreResult<Option<PathBuf>> {
    let result = if archived {
        find_archived_thread_path_by_id_str(
            store.config.codex_home.as_path(),
            &thread_id.to_string(),
            Some(state_db),
        )
        .await
    } else {
        find_thread_path_by_id_str(
            store.config.codex_home.as_path(),
            &thread_id.to_string(),
            Some(state_db),
        )
        .await
    };
    result.map_err(|err| ThreadStoreError::Internal {
        message: format!("failed to locate rollout ownership for thread {thread_id}: {err}"),
    })
}

async fn verify_rollout_owner(
    store: &LocalThreadStore,
    rollout_path: PathBuf,
    thread_id: ThreadId,
) -> ThreadStoreResult<()> {
    let thread = read_thread::read_thread_by_rollout_path(
        store,
        rollout_path,
        /*include_archived*/ true,
        /*include_history*/ false,
    )
    .await
    .map_err(|err| ThreadStoreError::Internal {
        message: format!("failed to verify rollout ownership for thread {thread_id}: {err}"),
    })?;
    if thread.thread_id != thread_id {
        return Err(ThreadStoreError::Internal {
            message: format!(
                "rollout ownership mismatch for thread {thread_id}: found {}",
                thread.thread_id
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
#[path = "read_thread_inference_identity_tests.rs"]
mod tests;
