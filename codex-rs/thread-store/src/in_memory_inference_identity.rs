use std::collections::HashMap;

use codex_protocol::ThreadId;
use codex_protocol::models::ThreadInferenceIdentityAuthority;

use super::InMemoryThreadStore;
use crate::ReadThreadInferenceIdentitySidecarParams;
use crate::ThreadInferenceIdentitySidecar;
use crate::ThreadInferenceIdentitySidecarPatch;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::UpdateThreadInferenceIdentitySidecarParams;

#[derive(Default)]
pub(super) struct InMemoryInferenceIdentityState {
    pub(super) sidecars: HashMap<ThreadId, ThreadInferenceIdentitySidecar>,
}

impl InMemoryInferenceIdentityState {
    pub(super) fn create_thread(&mut self, thread_id: ThreadId) {
        self.sidecars
            .insert(thread_id, ThreadInferenceIdentitySidecar::default());
    }

    pub(super) fn resume_thread(&mut self, thread_id: ThreadId) {
        self.sidecars.entry(thread_id).or_default();
    }

    pub(super) fn delete_thread(&mut self, thread_id: ThreadId) {
        self.sidecars.remove(&thread_id);
    }

    fn apply_patch(&mut self, thread_id: ThreadId, patch: ThreadInferenceIdentitySidecarPatch) {
        let ThreadInferenceIdentitySidecarPatch {
            configured,
            latest_request,
        } = patch;
        let sidecar = self.sidecars.entry(thread_id).or_default();
        if let Some(configured) = configured {
            sidecar.configured = configured.map_or_else(
                ThreadInferenceIdentityAuthority::cleared,
                ThreadInferenceIdentityAuthority::Valid,
            );
        }
        if let Some(latest_request) = latest_request {
            sidecar.latest_request = latest_request.map_or_else(
                ThreadInferenceIdentityAuthority::cleared,
                ThreadInferenceIdentityAuthority::Valid,
            );
        }
    }
}

impl InMemoryThreadStore {
    pub(super) async fn read_thread_inference_identity_sidecar(
        &self,
        params: ReadThreadInferenceIdentitySidecarParams,
    ) -> ThreadStoreResult<ThreadInferenceIdentitySidecar> {
        let state = self.state.lock().await;
        if !state.created_threads.contains_key(&params.thread_id) {
            return Err(ThreadStoreError::ThreadNotFound {
                thread_id: params.thread_id,
            });
        }
        Ok(state
            .inference_identity
            .sidecars
            .get(&params.thread_id)
            .cloned()
            .unwrap_or_default())
    }

    pub(super) async fn update_thread_inference_identity_sidecar(
        &self,
        params: UpdateThreadInferenceIdentitySidecarParams,
    ) -> ThreadStoreResult<()> {
        if params.patch.is_empty() {
            return Ok(());
        }
        let mut state = self.state.lock().await;
        if !state.created_threads.contains_key(&params.thread_id) {
            return Err(ThreadStoreError::ThreadNotFound {
                thread_id: params.thread_id,
            });
        }
        state
            .inference_identity
            .apply_patch(params.thread_id, params.patch);
        Ok(())
    }
}
