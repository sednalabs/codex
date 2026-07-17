use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Poll;

use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_protocol::protocol::UserMessageEvent;
use pretty_assertions::assert_eq;
use tokio::sync::Barrier;
use tokio::sync::Mutex;

use super::*;
use crate::*;

#[derive(Default)]
struct ScriptedStore {
    durable: Mutex<Vec<RolloutItem>>,
    append_barriers: Option<(Arc<Barrier>, Arc<Barrier>)>,
    partial_once: AtomicBool,
    append_attempts: AtomicUsize,
    metadata_attempts: AtomicUsize,
}

macro_rules! delegate {
    ($name:ident, $params:ty, $output:ty) => {
        fn $name(&self, _params: $params) -> ThreadStoreFuture<'_, $output> {
            Box::pin(async {
                Err(ThreadStoreError::Unsupported {
                    operation: stringify!($name),
                })
            })
        }
    };
}

impl ThreadStore for ScriptedStore {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    delegate!(create_thread, CreateThreadParams, ());
    delegate!(resume_thread, ResumeThreadParams, ());
    delegate!(append_items, AppendThreadItemsParams, ());
    delegate!(persist_thread, ThreadId, ());
    delegate!(shutdown_thread, ThreadId, ());
    delegate!(discard_thread, ThreadId, ());
    delegate!(load_history, LoadThreadHistoryParams, StoredThreadHistory);
    delegate!(read_thread, ReadThreadParams, StoredThread);
    delegate!(
        read_thread_by_rollout_path,
        ReadThreadByRolloutPathParams,
        StoredThread
    );
    delegate!(list_threads, ListThreadsParams, ThreadPage);
    delegate!(archive_thread, ArchiveThreadParams, ());
    delegate!(unarchive_thread, ArchiveThreadParams, StoredThread);
    delegate!(delete_thread, DeleteThreadParams, ());

    fn flush_thread(&self, _thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn append_items_committed<'a>(
        &'a self,
        mut params: AppendThreadItemsParams,
        committed: &'a mut usize,
    ) -> ThreadStoreFuture<'a, ()> {
        Box::pin(async move {
            let attempt_index = self.append_attempts.fetch_add(1, Ordering::SeqCst);
            if attempt_index == 0
                && let Some((entered, release)) = self.append_barriers.as_ref()
            {
                entered.wait().await;
                release.wait().await;
            }
            if self.partial_once.swap(false, Ordering::SeqCst) && !params.items.is_empty() {
                self.durable.lock().await.push(params.items.remove(0));
                *committed = 1;
                return Err(ThreadStoreError::Internal {
                    message: "partial append".to_string(),
                });
            }
            *committed = params.items.len();
            self.durable.lock().await.extend(params.items);
            Ok(())
        })
    }

    fn update_thread_metadata(
        &self,
        _params: UpdateThreadMetadataParams,
    ) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(async move {
            self.metadata_attempts.fetch_add(1, Ordering::SeqCst);
            Err(ThreadStoreError::Internal {
                message: "metadata projection".to_string(),
            })
        })
    }
}

fn live_thread(store: Arc<ScriptedStore>) -> LiveThread {
    let thread_id = ThreadId::new();
    let metadata_sync = ThreadMetadataSync::for_resume(&ResumeThreadParams {
        thread_id,
        rollout_path: None,
        history: Some(Arc::new(Vec::new())),
        include_archived: false,
        metadata: ThreadPersistenceMetadata {
            cwd: None,
            model_provider: "test".to_string(),
            memory_mode: ThreadMemoryMode::Enabled,
        },
    });
    LiveThread {
        thread_id,
        history_mode: ThreadHistoryMode::Legacy,
        thread_store: store,
        append_gate: Arc::new(Semaphore::new(1)),
        metadata_sync: Arc::new(Mutex::new(metadata_sync)),
        persistence_telemetry: RolloutPersistenceTelemetry::new(thread_id),
    }
}

fn item(text: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
        message: text.to_string(),
        ..Default::default()
    }))
}

fn items_json(items: &[RolloutItem]) -> serde_json::Value {
    serde_json::to_value(items).expect("serialize rollout items")
}

#[tokio::test]
async fn concurrent_appends_preserve_admission_order() {
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let store = Arc::new(ScriptedStore {
        append_barriers: Some((Arc::clone(&entered), Arc::clone(&release))),
        ..Default::default()
    });
    let live = live_thread(Arc::clone(&store));
    let first = item("first");
    let second = item("second");
    let first_task = tokio::spawn({
        let live = live.clone();
        let first = first.clone();
        async move { live.append_items(&[first]).await }
    });
    entered.wait().await;
    let mut second_append = Box::pin(live.append_items(std::slice::from_ref(&second)));
    std::future::poll_fn(|cx| match second_append.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("append completed before its ordering barrier opened"),
    })
    .await;
    release.wait().await;
    first_task
        .await
        .expect("join first append")
        .expect_err("first metadata projection");
    second_append.await.expect_err("second metadata projection");
    assert_eq!(
        items_json(&store.durable.lock().await),
        items_json(&[first, second])
    );
}

#[tokio::test]
async fn partial_progress_retries_only_the_uncommitted_suffix() {
    let store = Arc::new(ScriptedStore::default());
    store.partial_once.store(true, Ordering::SeqCst);
    let live = live_thread(Arc::clone(&store));
    let items = vec![item("one"), item("two"), item("three")];
    live.append_items(items.as_slice())
        .await
        .expect_err("metadata projection");
    assert_eq!(items_json(&store.durable.lock().await), items_json(&items));
    assert_eq!(store.append_attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn metadata_failure_retries_projection_without_reappending_history() {
    let store = Arc::new(ScriptedStore::default());
    let live = live_thread(Arc::clone(&store));
    let history = vec![item("durable")];
    live.append_items(history.as_slice())
        .await
        .expect_err("metadata failure");
    live.flush().await.expect_err("retry metadata projection");
    assert_eq!(
        items_json(&store.durable.lock().await),
        items_json(&history)
    );
    assert_eq!(store.metadata_attempts.load(Ordering::SeqCst), 2);
}
