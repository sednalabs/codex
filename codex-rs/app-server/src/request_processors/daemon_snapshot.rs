//! Persists loaded persistent root threads before managed daemon shutdown.
//! Failures are logged so other threads can still be saved.

use super::ThreadRequestProcessor;
use codex_thread_store::PersistContext;
use tracing::warn;

impl ThreadRequestProcessor {
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "snapshot selection must be serialized against pending unloads"
    )]
    pub(crate) async fn persist_daemon_threads(&self) {
        for thread_id in self.thread_manager.list_thread_ids().await {
            let pending_unloads = self.pending_thread_unloads.lock().await;
            if pending_unloads.contains(&thread_id) {
                continue;
            }
            let Ok(thread) = self.thread_manager.get_thread(thread_id).await else {
                continue;
            };
            let config = thread.config_snapshot().await;
            if !config.ephemeral
                && config.parent_thread_id.is_none()
                && !config.session_source.is_non_root_agent()
                && let Err(err) = self
                    .thread_store
                    .persist_thread(thread_id, PersistContext::Standard)
                    .await
            {
                warn!(%thread_id, %err, "failed to persist thread during daemon shutdown");
            }
        }
    }
}
