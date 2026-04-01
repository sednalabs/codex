use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingMessageSender;
use codex_app_server_protocol::FsChangedNotification;
use codex_app_server_protocol::FsUnwatchParams;
use codex_app_server_protocol::FsUnwatchResponse;
use codex_app_server_protocol::FsWatchParams;
use codex_app_server_protocol::FsWatchResponse;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ServerNotification;
use codex_core::file_watcher::FileWatcher;
use codex_core::file_watcher::FileWatcherEvent;
use codex_core::file_watcher::FileWatcherSubscriber;
use codex_core::file_watcher::Receiver;
use codex_core::file_watcher::WatchPath;
use codex_core::file_watcher::WatchRegistration;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::HashMap;
use std::collections::HashSet;
use std::hash::Hash;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
#[cfg(test)]
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tracing::warn;
use uuid::Uuid;

const FS_CHANGED_NOTIFICATION_DEBOUNCE: Duration = Duration::from_millis(200);

struct DebouncedReceiver {
    rx: Receiver,
    interval: Duration,
    changed_paths: HashSet<PathBuf>,
    next_allowance: Option<Instant>,
}

impl DebouncedReceiver {
    fn new(rx: Receiver, interval: Duration) -> Self {
        Self {
            rx,
            interval,
            changed_paths: HashSet::new(),
            next_allowance: None,
        }
    }

    async fn recv(&mut self) -> Option<FileWatcherEvent> {
        while self.changed_paths.is_empty() {
            self.changed_paths.extend(self.rx.recv().await?.paths);
        }
        let next_allowance = *self
            .next_allowance
            .get_or_insert_with(|| Instant::now() + self.interval);
        self.next_allowance = None;

        loop {
            tokio::select! {
                event = self.rx.recv() => match event {
                    Some(event) => self.changed_paths.extend(event.paths),
                    None => break,
                },
                _ = tokio::time::sleep_until(next_allowance) => break,
            }
        }

        if self.changed_paths.is_empty() {
            return None;
        }
        Some(FileWatcherEvent {
            paths: self.changed_paths.drain().collect(),
        })
    }
}

#[derive(Clone)]
pub(crate) struct FsWatchManager {
    outgoing: Arc<OutgoingMessageSender>,
    file_watcher: Arc<FileWatcher>,
    state: Arc<AsyncMutex<FsWatchState>>,
}

#[derive(Default)]
struct FsWatchState {
    entries: HashMap<WatchKey, WatchEntry>,
}

struct WatchEntry {
    terminate_tx: oneshot::Sender<oneshot::Sender<()>>,
    _subscriber: FileWatcherSubscriber,
    _registration: WatchRegistration,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct WatchKey {
    connection_id: ConnectionId,
    watch_id: String,
}

impl FsWatchManager {
    pub(crate) fn new(outgoing: Arc<OutgoingMessageSender>) -> Self {
        let file_watcher = match FileWatcher::new() {
            Ok(file_watcher) => Arc::new(file_watcher),
            Err(err) => {
                warn!("filesystem watch manager falling back to noop core watcher: {err}");
                Arc::new(FileWatcher::noop())
            }
        };
        Self::new_with_file_watcher(outgoing, file_watcher)
    }

    fn new_with_file_watcher(
        outgoing: Arc<OutgoingMessageSender>,
        file_watcher: Arc<FileWatcher>,
    ) -> Self {
        Self {
            outgoing,
            file_watcher,
            state: Arc::new(AsyncMutex::new(FsWatchState::default())),
        }
    }

    pub(crate) async fn watch(
        &self,
        connection_id: ConnectionId,
        params: FsWatchParams,
    ) -> Result<FsWatchResponse, JSONRPCErrorError> {
        let watch_id = Uuid::now_v7().to_string();
        let outgoing = self.outgoing.clone();
        let (subscriber, rx) = self.file_watcher.add_subscriber();
        let watch_root = params.path.clone();
        let registration = subscriber.register_paths(watch_paths_for_target(&params.path));
        let (terminate_tx, terminate_rx) = oneshot::channel();

        self.state.lock().await.entries.insert(
            WatchKey {
                connection_id,
                watch_id: watch_id.clone(),
            },
            WatchEntry {
                terminate_tx,
                _subscriber: subscriber,
                _registration: registration,
            },
        );

        let task_watch_id = watch_id.clone();
        tokio::spawn(async move {
            let mut rx = DebouncedReceiver::new(rx, FS_CHANGED_NOTIFICATION_DEBOUNCE);
            tokio::pin!(terminate_rx);
            loop {
                let event = tokio::select! {
                    biased;
                    _ = &mut terminate_rx => break,
                    event = rx.recv() => match event {
                        Some(event) => event,
                        None => break,
                    },
                };
                let mut changed_paths = event
                    .paths
                    .into_iter()
                    .filter_map(|path| {
                        match AbsolutePathBuf::resolve_path_against_base(&path, &watch_root) {
                            Ok(path) => changed_path_for_watch_target(&watch_root, path),
                            Err(err) => {
                                warn!(
                                    "failed to normalize watch event path ({}) for {}: {err}",
                                    path.display(),
                                    watch_root.display()
                                );
                                None
                            }
                        }
                    })
                    .collect::<Vec<_>>();
                changed_paths.sort_by(|left, right| left.as_path().cmp(right.as_path()));
                changed_paths.dedup();
                if !changed_paths.is_empty() {
                    tokio::select! {
                        biased;
                        _ = &mut terminate_rx => break,
                        _ = outgoing.send_server_notification_to_connection_and_wait(
                            connection_id,
                            ServerNotification::FsChanged(FsChangedNotification {
                                watch_id: task_watch_id.clone(),
                                changed_paths,
                            }),
                        ) => {}
                    }
                }
            }
        });

        Ok(FsWatchResponse {
            watch_id,
            path: params.path,
        })
    }

    pub(crate) async fn unwatch(
        &self,
        connection_id: ConnectionId,
        params: FsUnwatchParams,
    ) -> Result<FsUnwatchResponse, JSONRPCErrorError> {
        let watch_key = WatchKey {
            connection_id,
            watch_id: params.watch_id,
        };
        let entry = self.state.lock().await.entries.remove(&watch_key);
        if let Some(entry) = entry {
            // Wait for the oneshot to be destroyed by the task to ensure that no notifications
            // are send after the unwatch response.
            let (done_tx, done_rx) = oneshot::channel();
            let _ = entry.terminate_tx.send(done_tx);
            let _ = done_rx.await;
        }
        Ok(FsUnwatchResponse {})
    }

    pub(crate) async fn connection_closed(&self, connection_id: ConnectionId) {
        let mut state = self.state.lock().await;
        state
            .entries
            .extract_if(|key, _| key.connection_id == connection_id)
            .count();
    }
}

fn nearest_existing_watch_ancestor(path: &std::path::Path) -> Option<PathBuf> {
    path.ancestors()
        .skip(1)
        .find(|ancestor| ancestor.exists())
        .map(std::path::Path::to_path_buf)
}

fn watch_paths_for_target(path: &AbsolutePathBuf) -> Vec<WatchPath> {
    let watch_path = path.to_path_buf();
    let mut watched_paths = vec![WatchPath {
        path: watch_path.clone(),
        recursive: false,
    }];
    if !watch_path.exists()
        && let Some(existing_ancestor) = nearest_existing_watch_ancestor(&watch_path)
    {
        watched_paths.push(WatchPath {
            // If the target path does not yet exist, we must still watch a recursive parent
            // so that watch-before-create directory flows observe descendant creation.
            // Avoid recursively watching the filesystem root for typoed or invalid paths.
            recursive: existing_ancestor.parent().is_some(),
            path: existing_ancestor,
        });
    }
    watched_paths
}

fn changed_path_for_watch_target(
    watch_target: &AbsolutePathBuf,
    event_path: AbsolutePathBuf,
) -> Option<AbsolutePathBuf> {
    let watch_target = watch_target.as_path();
    let event_path_ref = event_path.as_path();
    if event_path_ref == watch_target {
        return Some(event_path);
    }
    if watch_target.starts_with(event_path_ref) {
        return AbsolutePathBuf::try_from(watch_target.to_path_buf()).ok();
    }
    if event_path_ref.starts_with(watch_target) {
        return Some(event_path);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outgoing_message::OutgoingEnvelope;
    use crate::outgoing_message::OutgoingMessage;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;
    use tokio::time::timeout;
    use uuid::Version;

    fn absolute_path(path: PathBuf) -> AbsolutePathBuf {
        assert!(
            path.is_absolute(),
            "path must be absolute: {}",
            path.display()
        );
        AbsolutePathBuf::try_from(path).expect("path should be absolute")
    }

    fn manager_with_noop_watcher() -> FsWatchManager {
        const OUTGOING_BUFFER: usize = 1;
        let (tx, _rx) = mpsc::channel(OUTGOING_BUFFER);
        FsWatchManager::new_with_file_watcher(
            Arc::new(OutgoingMessageSender::new(tx)),
            Arc::new(FileWatcher::noop()),
        )
    }

    #[tokio::test]
    async fn watch_returns_a_v7_id_and_tracks_the_owner_scoped_entry() {
        let temp_dir = TempDir::new().expect("temp dir");
        let head_path = temp_dir.path().join("HEAD");
        std::fs::write(&head_path, "ref: refs/heads/main\n").expect("write HEAD");

        let manager = manager_with_noop_watcher();
        let path = absolute_path(head_path);
        let response = manager
            .watch(ConnectionId(1), FsWatchParams { path: path.clone() })
            .await
            .expect("watch should succeed");

        assert_eq!(response.path, path);
        let watch_id = Uuid::parse_str(&response.watch_id).expect("watch id should be a UUID");
        assert_eq!(watch_id.get_version(), Some(Version::SortRand));

        let state = manager.state.lock().await;
        assert_eq!(
            state.entries.keys().cloned().collect::<HashSet<_>>(),
            HashSet::from([WatchKey {
                connection_id: ConnectionId(1),
                watch_id: response.watch_id,
            }])
        );
    }

    #[tokio::test]
    async fn unwatch_is_scoped_to_the_connection_that_created_the_watch() {
        let temp_dir = TempDir::new().expect("temp dir");
        let head_path = temp_dir.path().join("HEAD");
        std::fs::write(&head_path, "ref: refs/heads/main\n").expect("write HEAD");

        let manager = manager_with_noop_watcher();
        let response = manager
            .watch(
                ConnectionId(1),
                FsWatchParams {
                    path: absolute_path(head_path),
                },
            )
            .await
            .expect("watch should succeed");
        let watch_key = WatchKey {
            connection_id: ConnectionId(1),
            watch_id: response.watch_id.clone(),
        };

        manager
            .unwatch(
                ConnectionId(2),
                FsUnwatchParams {
                    watch_id: response.watch_id.clone(),
                },
            )
            .await
            .expect("foreign unwatch should be a no-op");
        assert!(manager.state.lock().await.entries.contains_key(&watch_key));

        manager
            .unwatch(
                ConnectionId(1),
                FsUnwatchParams {
                    watch_id: response.watch_id,
                },
            )
            .await
            .expect("owner unwatch should succeed");
        assert!(!manager.state.lock().await.entries.contains_key(&watch_key));
    }

    #[tokio::test]
    async fn connection_closed_removes_only_that_connections_watches() {
        let temp_dir = TempDir::new().expect("temp dir");
        let head_path = temp_dir.path().join("HEAD");
        let fetch_head_path = temp_dir.path().join("FETCH_HEAD");
        let packed_refs_path = temp_dir.path().join("packed-refs");
        std::fs::write(&head_path, "ref: refs/heads/main\n").expect("write HEAD");
        std::fs::write(&fetch_head_path, "old-fetch\n").expect("write FETCH_HEAD");
        std::fs::write(&packed_refs_path, "refs\n").expect("write packed-refs");

        let manager = manager_with_noop_watcher();
        let response_1 = manager
            .watch(
                ConnectionId(1),
                FsWatchParams {
                    path: absolute_path(head_path),
                },
            )
            .await
            .expect("first watch should succeed");
        let response_2 = manager
            .watch(
                ConnectionId(1),
                FsWatchParams {
                    path: absolute_path(fetch_head_path),
                },
            )
            .await
            .expect("second watch should succeed");
        let response_3 = manager
            .watch(
                ConnectionId(2),
                FsWatchParams {
                    path: absolute_path(packed_refs_path),
                },
            )
            .await
            .expect("third watch should succeed");

        manager.connection_closed(ConnectionId(1)).await;

        assert_eq!(
            manager
                .state
                .lock()
                .await
                .entries
                .keys()
                .cloned()
                .collect::<HashSet<_>>(),
            HashSet::from([WatchKey {
                connection_id: ConnectionId(2),
                watch_id: response_3.watch_id,
            }])
        );
        assert_ne!(response_1.watch_id, response_2.watch_id);
    }

    async fn collect_next_fs_changed(
        outgoing_rx: &mut mpsc::Receiver<OutgoingEnvelope>,
    ) -> FsChangedNotification {
        loop {
            let envelope = timeout(Duration::from_secs(5), outgoing_rx.recv())
                .await
                .expect("notification should arrive before test timeout")
                .expect("outgoing channel should remain open while notifications are expected");
            match envelope {
                OutgoingEnvelope::ToConnection {
                    message:
                        OutgoingMessage::AppServerNotification(ServerNotification::FsChanged(
                            notification,
                        )),
                    write_complete_tx,
                    ..
                } => {
                    if let Some(write_complete_tx) = write_complete_tx {
                        let _ = write_complete_tx.send(());
                    }
                    return notification;
                }
                OutgoingEnvelope::ToConnection {
                    write_complete_tx, ..
                } => {
                    if let Some(write_complete_tx) = write_complete_tx {
                        let _ = write_complete_tx.send(());
                    }
                }
                OutgoingEnvelope::Broadcast { .. } => {}
            }
        }
    }

    #[tokio::test]
    async fn debounce_window_is_reset_between_batches() {
        let temp_dir = TempDir::new().expect("temp dir");
        let watch_root = absolute_path(temp_dir.path().to_path_buf());
        let file_b = temp_dir.path().join("file-b.txt");
        let file_c = temp_dir.path().join("file-c.txt");

        let file_watcher = Arc::new(FileWatcher::noop());
        let (tx, mut rx) = mpsc::channel(16);
        let manager = FsWatchManager::new_with_file_watcher(
            Arc::new(OutgoingMessageSender::new(tx)),
            file_watcher.clone(),
        );
        let file_b = absolute_path(file_b);
        let file_c = absolute_path(file_c);

        let response = manager
            .watch(ConnectionId(1), FsWatchParams { path: watch_root })
            .await
            .expect("watch should succeed");

        file_watcher
            .send_paths_for_test(vec![file_b.to_path_buf()])
            .await;
        let first_notification = collect_next_fs_changed(&mut rx).await;
        assert_eq!(first_notification.watch_id, response.watch_id);
        assert!(first_notification.changed_paths.contains(&file_b));

        tokio::time::sleep(FS_CHANGED_NOTIFICATION_DEBOUNCE * 2).await;
        file_watcher
            .send_paths_for_test(vec![file_b.to_path_buf()])
            .await;
        let second_file_watcher = file_watcher.clone();
        let second_path = file_c.to_path_buf();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            second_file_watcher
                .send_paths_for_test(vec![second_path])
                .await;
        });

        let second_batch_start = Instant::now();
        let second_notification = collect_next_fs_changed(&mut rx).await;
        let second_batch_elapsed = second_batch_start.elapsed();
        assert!(
            second_batch_elapsed >= FS_CHANGED_NOTIFICATION_DEBOUNCE - Duration::from_millis(75),
            "expected a fresh debounce delay before the second batch is emitted"
        );
        assert_eq!(second_notification.watch_id, response.watch_id);
        let second_batch_paths = second_notification
            .changed_paths
            .into_iter()
            .collect::<HashSet<_>>();
        assert!(second_batch_paths.contains(&file_b));
        assert!(second_batch_paths.contains(&file_c));

        assert!(
            timeout(Duration::from_millis(100), rx.recv())
                .await
                .is_err(),
            "a subsequent batch should not arrive without another debounced change"
        );
    }

    #[tokio::test]
    async fn debounce_flushes_pending_events_before_close() {
        let temp_dir = TempDir::new().expect("temp dir");
        let watched_file = absolute_path(temp_dir.path().join("file.txt"));
        let file_watcher = Arc::new(FileWatcher::noop());
        let (subscriber, raw_rx) = file_watcher.add_subscriber();
        let _subscription = subscriber.register_paths(watch_paths_for_target(&watched_file));
        let mut rx = DebouncedReceiver::new(raw_rx, Duration::from_millis(20));

        file_watcher
            .send_paths_for_test(vec![watched_file.to_path_buf()])
            .await;
        drop(subscriber);

        let first_batch = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("debounced batch should flush before timeout")
            .expect("receiver should emit buffered paths before close");
        assert_eq!(first_batch.paths, vec![watched_file.to_path_buf()]);
        let second_batch = timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("debounced receiver should finish after close");
        assert!(
            second_batch.is_none(),
            "receiver should report close after flushing buffered paths"
        );
    }

    #[test]
    fn missing_file_watch_registers_the_direct_parent_recursively() {
        let temp_dir = TempDir::new().expect("temp dir");
        let missing_path = absolute_path(temp_dir.path().join("FETCH_HEAD"));
        let parent = missing_path
            .parent()
            .expect("missing file should have a parent");
        assert_eq!(
            watch_paths_for_target(&missing_path),
            vec![
                WatchPath {
                    path: missing_path.to_path_buf(),
                    recursive: false,
                },
                WatchPath {
                    path: parent.to_path_buf(),
                    recursive: true,
                },
            ]
        );
    }

    #[test]
    fn deeply_missing_file_watch_registers_the_nearest_existing_ancestor() {
        let temp_dir = TempDir::new().expect("temp dir");
        let missing_path = absolute_path(temp_dir.path().join("refs/remotes/origin/HEAD"));

        assert_eq!(
            watch_paths_for_target(&missing_path),
            vec![
                WatchPath {
                    path: missing_path.to_path_buf(),
                    recursive: false,
                },
                WatchPath {
                    path: temp_dir.path().to_path_buf(),
                    recursive: true,
                },
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn deeply_missing_rooted_target_does_not_watch_root_recursively() {
        let missing_path = absolute_path(PathBuf::from("/does/not/exist/file"));

        assert_eq!(
            watch_paths_for_target(&missing_path),
            vec![
                WatchPath {
                    path: missing_path.to_path_buf(),
                    recursive: false,
                },
                WatchPath {
                    path: PathBuf::from("/"),
                    recursive: false,
                },
            ]
        );
    }

    #[tokio::test]
    async fn deeply_missing_file_watch_notifies_when_nested_target_is_created() {
        let temp_dir = TempDir::new().expect("temp dir");
        let missing_path = absolute_path(temp_dir.path().join("refs/remotes/origin/HEAD"));

        let file_watcher = Arc::new(FileWatcher::noop());
        let (tx, mut rx) = mpsc::channel(16);
        let manager = FsWatchManager::new_with_file_watcher(
            Arc::new(OutgoingMessageSender::new(tx)),
            file_watcher.clone(),
        );

        let response = manager
            .watch(
                ConnectionId(1),
                FsWatchParams {
                    path: missing_path.clone(),
                },
            )
            .await
            .expect("watch should succeed");

        std::fs::create_dir_all(
            missing_path
                .parent()
                .expect("deeply missing target should have a parent"),
        )
        .expect("create nested parent directories");
        std::fs::write(&missing_path, "ref: refs/remotes/origin/main\n")
            .expect("create deeply missing file");

        file_watcher
            .send_paths_for_test(vec![missing_path.to_path_buf()])
            .await;

        let notification = collect_next_fs_changed(&mut rx).await;
        assert_eq!(notification.watch_id, response.watch_id);
        assert_eq!(notification.changed_paths, vec![missing_path]);
    }

    #[tokio::test]
    async fn missing_directory_watch_notifies_for_nested_children_after_creation() {
        let temp_dir = TempDir::new().expect("temp dir");
        let missing_dir = absolute_path(temp_dir.path().join("target"));
        let nested_file = absolute_path(temp_dir.path().join("target/subfile"));

        let file_watcher = Arc::new(FileWatcher::noop());
        let (tx, mut rx) = mpsc::channel(16);
        let manager = FsWatchManager::new_with_file_watcher(
            Arc::new(OutgoingMessageSender::new(tx)),
            file_watcher.clone(),
        );

        let response = manager
            .watch(
                ConnectionId(1),
                FsWatchParams {
                    path: missing_dir.clone(),
                },
            )
            .await
            .expect("watch should succeed");

        std::fs::create_dir_all(&missing_dir).expect("create watched directory");
        std::fs::write(&nested_file, "hello\n").expect("create nested file");

        file_watcher
            .send_paths_for_test(vec![nested_file.to_path_buf()])
            .await;

        let notification = collect_next_fs_changed(&mut rx).await;
        assert_eq!(notification.watch_id, response.watch_id);
        assert_eq!(notification.changed_paths, vec![nested_file]);
    }

    #[tokio::test]
    async fn missing_file_watch_ignores_sibling_parent_events() {
        let temp_dir = TempDir::new().expect("temp dir");
        let missing_path = absolute_path(temp_dir.path().join("FETCH_HEAD"));
        let parent_path = absolute_path(temp_dir.path().to_path_buf());
        let sibling_path = absolute_path(temp_dir.path().join("ORIG_HEAD"));

        let file_watcher = Arc::new(FileWatcher::noop());
        let (tx, mut rx) = mpsc::channel(16);
        let manager = FsWatchManager::new_with_file_watcher(
            Arc::new(OutgoingMessageSender::new(tx)),
            file_watcher.clone(),
        );

        let response = manager
            .watch(
                ConnectionId(1),
                FsWatchParams {
                    path: missing_path.clone(),
                },
            )
            .await
            .expect("watch should succeed");

        file_watcher
            .send_paths_for_test(vec![sibling_path.to_path_buf()])
            .await;
        assert!(
            timeout(FS_CHANGED_NOTIFICATION_DEBOUNCE * 2, rx.recv())
                .await
                .is_err(),
            "sibling changes should not be forwarded for a missing-file watch"
        );

        file_watcher
            .send_paths_for_test(vec![parent_path.to_path_buf()])
            .await;
        let notification = collect_next_fs_changed(&mut rx).await;
        assert_eq!(notification.watch_id, response.watch_id);
        assert_eq!(notification.changed_paths, vec![missing_path]);
    }

    #[test]
    fn missing_file_watch_maps_parent_directory_events_back_to_the_target_file() {
        let temp_dir = TempDir::new().expect("temp dir");
        let missing_path = absolute_path(temp_dir.path().join("FETCH_HEAD"));
        let parent = absolute_path(temp_dir.path().to_path_buf());
        let sibling = absolute_path(temp_dir.path().join("ORIG_HEAD"));

        assert_eq!(
            changed_path_for_watch_target(&missing_path, parent),
            Some(missing_path.clone())
        );
        assert_eq!(changed_path_for_watch_target(&missing_path, sibling), None);
    }

    #[tokio::test]
    async fn unwatch_completes_when_notification_write_completion_is_withheld() {
        let temp_dir = TempDir::new().expect("temp dir");
        let watched_path = absolute_path(temp_dir.path().join("watched"));
        std::fs::write(&watched_path, "hello\n").expect("write watched file");

        let file_watcher = Arc::new(FileWatcher::noop());
        let (tx, mut rx) = mpsc::channel(16);
        let manager = FsWatchManager::new_with_file_watcher(
            Arc::new(OutgoingMessageSender::new(tx)),
            file_watcher.clone(),
        );

        let response = manager
            .watch(
                ConnectionId(1),
                FsWatchParams {
                    path: watched_path.clone(),
                },
            )
            .await
            .expect("watch should succeed");

        file_watcher
            .send_paths_for_test(vec![watched_path.to_path_buf()])
            .await;

        let withheld_write_complete_tx = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("notification should arrive before test timeout")
            .expect("outgoing channel should remain open for expected notification");
        let OutgoingEnvelope::ToConnection {
            message:
                OutgoingMessage::AppServerNotification(ServerNotification::FsChanged(notification)),
            write_complete_tx,
            ..
        } = withheld_write_complete_tx
        else {
            panic!("expected fs-changed notification envelope");
        };
        assert_eq!(notification.watch_id, response.watch_id);
        assert_eq!(notification.changed_paths, vec![watched_path]);
        let _withheld_write_complete_tx = write_complete_tx
            .expect("expected write completion sender to withhold notification completion");

        let unwatch_result = timeout(
            Duration::from_secs(1),
            manager.unwatch(
                ConnectionId(1),
                FsUnwatchParams {
                    watch_id: response.watch_id,
                },
            ),
        )
        .await;

        assert!(
            unwatch_result.is_ok(),
            "unwatch should complete while write completion is withheld"
        );
        assert!(unwatch_result.unwrap().is_ok());

        drop(_withheld_write_complete_tx);
    }
}
