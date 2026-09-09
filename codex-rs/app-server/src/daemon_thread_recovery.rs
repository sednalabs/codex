//! Persists the candidate set for a planned managed-daemon replacement.

use std::collections::BTreeSet;
use std::io;
use std::path::PathBuf;

use codex_app_server_transport::daemon_recovery;

pub(crate) async fn snapshot(path: PathBuf, loaded: Vec<String>) -> io::Result<()> {
    // A forced exit must not wait for file I/O in Tokio's blocking pool.
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("daemon-snapshot".into())
        .spawn(move || {
            let result = daemon_recovery::write_candidates(
                &path,
                &loaded.into_iter().collect::<BTreeSet<_>>(),
            );
            if result.is_err()
                && let Err(err) = std::fs::remove_file(&path)
                && err.kind() != io::ErrorKind::NotFound
            {
                tracing::warn!("failed to clear stale daemon recovery file: {err}");
            }
            let _ = result_tx.send(result);
        })?;
    result_rx.await.map_err(io::Error::other)?
}
