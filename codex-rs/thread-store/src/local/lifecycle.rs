use std::path::PathBuf;
use std::sync::Arc;

use codex_protocol::ThreadId;
use codex_rollout::find_archived_thread_path_by_id_str;
use codex_rollout::find_thread_path_by_id_str;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;

use super::LocalThreadStore;
use super::helpers::rollout_path_is_archived;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum LocalThreadLifecycle {
    UnmaterializedActive,
    Active(PathBuf),
    Archived(PathBuf),
    Missing,
}

pub(super) struct LocalThreadLifecycleGuard<'a> {
    store: &'a LocalThreadStore,
    thread_id: ThreadId,
    _permit: OwnedSemaphorePermit,
}

impl LocalThreadStore {
    pub(super) async fn acquire_lifecycle_custody(
        &self,
        thread_id: ThreadId,
    ) -> ThreadStoreResult<LocalThreadLifecycleGuard<'_>> {
        let semaphore = {
            let mut registry = self.lifecycle_custody.lock().await;
            // The registry owns only weak references, so idle thread ids are reclaimed on the next
            // acquisition. Leave this scope before waiting on the per-thread permit.
            registry.retain(|_, semaphore| semaphore.strong_count() > 0);
            if let Some(semaphore) = registry.get(&thread_id).and_then(|weak| weak.upgrade()) {
                semaphore
            } else {
                let semaphore = Arc::new(Semaphore::new(1));
                registry.insert(thread_id, Arc::downgrade(&semaphore));
                semaphore
            }
        };
        let permit = semaphore.acquire_owned().await.map_err(|err| {
            ThreadStoreError::Internal {
                message: format!("failed to acquire lifecycle custody for {thread_id}: {err}"),
            }
        })?;
        Ok(LocalThreadLifecycleGuard {
            store: self,
            thread_id,
            _permit: permit,
        })
    }
}

impl LocalThreadLifecycleGuard<'_> {
    pub(super) async fn classify(&self) -> ThreadStoreResult<LocalThreadLifecycle> {
        let thread_id = self.thread_id;
        let thread_id_str = thread_id.to_string();
        let state_db = self.store.state_db().await;
        let active_path = find_thread_path_by_id_str(
            self.store.config.codex_home.as_path(),
            thread_id_str.as_str(),
            state_db.as_deref(),
        )
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to classify active thread {thread_id}: {err}"),
        })?;
        let archived_path = find_archived_thread_path_by_id_str(
            self.store.config.codex_home.as_path(),
            thread_id_str.as_str(),
            state_db.as_deref(),
        )
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to classify archived thread {thread_id}: {err}"),
        })?;

        // Filesystem-backed state is stronger lifecycle evidence than a recorder entry. In
        // particular, app-layer shutdown can time out before archive moves a live rollout.
        match (active_path, archived_path) {
            (Some(active_path), Some(archived_path)) => {
                return Err(ThreadStoreError::Conflict {
                    message: format!(
                        "thread {thread_id} has active `{}` and archived `{}` rollout paths",
                        active_path.display(),
                        archived_path.display()
                    ),
                });
            }
            (Some(active_path), None) => return Ok(LocalThreadLifecycle::Active(active_path)),
            (None, Some(archived_path)) => {
                return Ok(LocalThreadLifecycle::Archived(archived_path));
            }
            (None, None) => {}
        }

        let live_recorder = self
            .store
            .live_recorders
            .lock()
            .await
            .get(&thread_id)
            .map(|entry| {
                (
                    entry.recorder.rollout_path().to_path_buf(),
                    entry.materialized,
                )
            });
        let Some((live_rollout_path, was_materialized)) = live_recorder else {
            return Ok(LocalThreadLifecycle::Missing);
        };
        let materialized_path =
            codex_rollout::existing_rollout_path(live_rollout_path.as_path()).await;
        if rollout_path_is_archived(
            self.store.config.codex_home.as_path(),
            live_rollout_path.as_path(),
        ) {
            return Ok(materialized_path.map_or(
                LocalThreadLifecycle::Missing,
                LocalThreadLifecycle::Archived,
            ));
        }
        if let Some(materialized_path) = materialized_path {
            return Ok(LocalThreadLifecycle::Active(materialized_path));
        }
        if was_materialized {
            return Ok(LocalThreadLifecycle::Missing);
        }
        Ok(LocalThreadLifecycle::UnmaterializedActive)
    }
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
