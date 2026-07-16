use std::collections::HashSet;
use std::future::Future;
use std::future::poll_fn;
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;

use codex_protocol::ThreadId;
use codex_protocol::models::BaseInstructions;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::time::timeout;
use uuid::Uuid;

use super::LocalThreadLifecycle;
use crate::ArchiveThreadParams;
use crate::CreateThreadParams;
use crate::DeleteThreadParams;
use crate::ResumeThreadParams;
use crate::ThreadPersistenceMetadata;
use crate::ThreadStore;
use crate::local::LocalThreadStore;
use crate::local::test_support::test_config;
use crate::local::test_support::write_archived_session_file;
use crate::local::test_support::write_session_file;

const TRANSITION_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::test]
async fn classifies_missing_unmaterialized_materialized_and_archived_live_threads() {
    let home = TempDir::new().expect("temp dir");
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
    let missing_id = thread_id(501);
    assert_eq!(
        classify(&store, missing_id).await,
        LocalThreadLifecycle::Missing
    );

    let active_id = thread_id(502);
    store
        .create_thread(create_thread_params(active_id, home.path()))
        .await
        .expect("create live thread");
    assert_eq!(
        classify(&store, active_id).await,
        LocalThreadLifecycle::UnmaterializedActive
    );
    store
        .persist_thread(active_id)
        .await
        .expect("materialize live thread");
    let active_path = store
        .live_rollout_path(active_id)
        .await
        .expect("active rollout path");
    assert_eq!(
        classify(&store, active_id).await,
        LocalThreadLifecycle::Active(active_path.clone())
    );
    store
        .shutdown_thread(active_id)
        .await
        .expect("shutdown active thread");
    assert_eq!(
        classify(&store, active_id).await,
        LocalThreadLifecycle::Active(active_path)
    );

    let archived_uuid = Uuid::from_u128(503);
    let archived_id = ThreadId::from_string(&archived_uuid.to_string()).expect("valid thread id");
    let archived_path =
        write_archived_session_file(home.path(), "2025-01-05T10-30-00", archived_uuid)
            .expect("archived rollout");
    store
        .resume_thread(ResumeThreadParams {
            thread_id: archived_id,
            rollout_path: Some(archived_path.clone()),
            history: None,
            include_archived: true,
            metadata: thread_metadata(home.path()),
        })
        .await
        .expect("resume archived live thread");
    assert_eq!(
        classify(&store, archived_id).await,
        LocalThreadLifecycle::Archived(archived_path.clone())
    );
    store
        .discard_thread(archived_id)
        .await
        .expect("discard archived live thread");
    assert_eq!(
        classify(&store, archived_id).await,
        LocalThreadLifecycle::Archived(archived_path)
    );
}

#[tokio::test]
async fn rejects_simultaneous_active_and_archived_materialization() {
    let home = TempDir::new().expect("temp dir");
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
    let uuid = Uuid::from_u128(509);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
    let active_path =
        write_session_file(home.path(), "2025-01-05T12-00-00", uuid).expect("active rollout");
    let archived_path = write_archived_session_file(home.path(), "2025-01-05T12-30-00", uuid)
        .expect("archived rollout");

    let guard = store
        .acquire_lifecycle_custody(thread_id)
        .await
        .expect("lifecycle custody");
    let err = guard
        .classify()
        .await
        .expect_err("ambiguous materialization should fail closed");
    assert_eq!(
        err.to_string(),
        format!(
            "thread-store conflict: thread {thread_id} has active `{}` and archived `{}` rollout paths",
            active_path.display(),
            archived_path.display()
        )
    );
}

#[tokio::test]
async fn archive_transition_completes_before_queued_classification() {
    let home = TempDir::new().expect("temp dir");
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
    let uuid = Uuid::from_u128(504);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
    let active_path = write_session_file(home.path(), "2025-01-05T11-00-00", uuid)
        .expect("active rollout");
    let archived_path = home
        .path()
        .join(codex_rollout::ARCHIVED_SESSIONS_SUBDIR)
        .join(active_path.file_name().expect("rollout file name"));
    let initial_guard = store
        .acquire_lifecycle_custody(thread_id)
        .await
        .expect("initial custody");

    let mut archive_task = store.archive_thread(ArchiveThreadParams { thread_id });
    poll_once_pending(&mut archive_task, "archive").await;
    let mut classify_task = Box::pin(async {
        let guard = store.acquire_lifecycle_custody(thread_id).await?;
        guard.classify().await
    });
    poll_once_pending(&mut classify_task, "classification").await;
    drop(initial_guard);

    timeout(TRANSITION_TIMEOUT, archive_task)
        .await
        .expect("archive should not stall")
        .expect("archive thread");
    let lifecycle = timeout(TRANSITION_TIMEOUT, classify_task)
        .await
        .expect("classification should not stall")
        .expect("classify archived thread");
    assert_eq!(lifecycle, LocalThreadLifecycle::Archived(archived_path));
}

#[tokio::test]
async fn unarchive_transition_completes_before_queued_classification() {
    let home = TempDir::new().expect("temp dir");
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
    let uuid = Uuid::from_u128(505);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
    let archived_path = write_archived_session_file(home.path(), "2025-01-05T11-30-00", uuid)
        .expect("archived rollout");
    let active_path = home
        .path()
        .join("sessions/2025/01/05")
        .join(archived_path.file_name().expect("rollout file name"));
    let initial_guard = store
        .acquire_lifecycle_custody(thread_id)
        .await
        .expect("initial custody");

    let mut unarchive_task = store.unarchive_thread(ArchiveThreadParams { thread_id });
    poll_once_pending(&mut unarchive_task, "unarchive").await;
    let mut classify_task = Box::pin(async {
        let guard = store.acquire_lifecycle_custody(thread_id).await?;
        guard.classify().await
    });
    poll_once_pending(&mut classify_task, "classification").await;
    drop(initial_guard);

    timeout(TRANSITION_TIMEOUT, unarchive_task)
        .await
        .expect("unarchive should not stall")
        .expect("unarchive thread");
    let lifecycle = timeout(TRANSITION_TIMEOUT, classify_task)
        .await
        .expect("classification should not stall")
        .expect("classify active thread");
    assert_eq!(lifecycle, LocalThreadLifecycle::Active(active_path));
}

#[tokio::test]
async fn delete_transition_completes_before_queued_classification() {
    let home = TempDir::new().expect("temp dir");
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
    let uuid = Uuid::from_u128(510);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
    let active_path = write_session_file(home.path(), "2025-01-05T13-00-00", uuid)
        .expect("active rollout");
    let initial_guard = store
        .acquire_lifecycle_custody(thread_id)
        .await
        .expect("initial custody");

    let mut delete_task = store.delete_thread(DeleteThreadParams { thread_id });
    poll_once_pending(&mut delete_task, "delete").await;
    let mut classify_task = Box::pin(async {
        let guard = store.acquire_lifecycle_custody(thread_id).await?;
        guard.classify().await
    });
    poll_once_pending(&mut classify_task, "classification").await;
    drop(initial_guard);

    timeout(TRANSITION_TIMEOUT, delete_task)
        .await
        .expect("delete should not stall")
        .expect("delete thread");
    let lifecycle = timeout(TRANSITION_TIMEOUT, classify_task)
        .await
        .expect("classification should not stall")
        .expect("classify deleted thread");
    assert_eq!(lifecycle, LocalThreadLifecycle::Missing);
    assert!(!active_path.exists());
}

#[tokio::test]
async fn discard_transition_completes_before_queued_classification() {
    let home = TempDir::new().expect("temp dir");
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
    let thread_id = thread_id(511);
    store
        .create_thread(create_thread_params(thread_id, home.path()))
        .await
        .expect("create unmaterialized live thread");
    let initial_guard = store
        .acquire_lifecycle_custody(thread_id)
        .await
        .expect("initial custody");

    let mut discard_task = store.discard_thread(thread_id);
    poll_once_pending(&mut discard_task, "discard").await;
    let mut classify_task = Box::pin(async {
        let guard = store.acquire_lifecycle_custody(thread_id).await?;
        guard.classify().await
    });
    poll_once_pending(&mut classify_task, "classification").await;
    drop(initial_guard);

    timeout(TRANSITION_TIMEOUT, discard_task)
        .await
        .expect("discard should not stall")
        .expect("discard thread");
    let lifecycle = timeout(TRANSITION_TIMEOUT, classify_task)
        .await
        .expect("classification should not stall")
        .expect("classify discarded thread");
    assert_eq!(lifecycle, LocalThreadLifecycle::Missing);
}

#[tokio::test]
async fn custody_is_per_thread_and_prunes_idle_entries() {
    let home = TempDir::new().expect("temp dir");
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
    let thread_a = thread_id(506);
    let thread_b = thread_id(507);
    let thread_c = thread_id(508);

    let guard_a = store
        .acquire_lifecycle_custody(thread_a)
        .await
        .expect("thread A custody");
    let guard_b = timeout(
        TRANSITION_TIMEOUT,
        store.acquire_lifecycle_custody(thread_b),
    )
    .await
    .expect("thread B custody should not wait on thread A")
    .expect("thread B custody");
    drop(guard_b);
    drop(guard_a);

    let guard_c = store
        .acquire_lifecycle_custody(thread_c)
        .await
        .expect("thread C custody");
    let retained_ids = store
        .lifecycle_custody
        .lock()
        .await
        .keys()
        .copied()
        .collect::<HashSet<_>>();
    assert_eq!(retained_ids, HashSet::from([thread_c]));
    drop(guard_c);
}

async fn classify(store: &LocalThreadStore, thread_id: ThreadId) -> LocalThreadLifecycle {
    let guard = store
        .acquire_lifecycle_custody(thread_id)
        .await
        .expect("lifecycle custody");
    guard.classify().await.expect("classify thread")
}

async fn poll_once_pending<F>(future: &mut Pin<Box<F>>, operation: &str)
where
    F: Future + ?Sized,
{
    poll_fn(|cx| match future.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("{operation} completed before lifecycle custody was released"),
    })
    .await;
}

fn thread_id(value: u128) -> ThreadId {
    ThreadId::from_string(&Uuid::from_u128(value).to_string()).expect("valid thread id")
}

fn create_thread_params(thread_id: ThreadId, cwd: &std::path::Path) -> CreateThreadParams {
    CreateThreadParams {
        session_id: thread_id.into(),
        thread_id,
        extra_config: None,
        forked_from_id: None,
        parent_thread_id: None,
        source: SessionSource::Exec,
        thread_source: None,
        originator: "test_originator".to_string(),
        base_instructions: BaseInstructions::default(),
        dynamic_tools: Vec::new(),
        selected_capability_roots: Vec::new(),
        multi_agent_version: None,
        history_mode: ThreadHistoryMode::Legacy,
        initial_window_id: Uuid::now_v7().to_string(),
        metadata: thread_metadata(cwd),
    }
}

fn thread_metadata(cwd: &std::path::Path) -> ThreadPersistenceMetadata {
    ThreadPersistenceMetadata {
        cwd: Some(cwd.to_path_buf()),
        model_provider: "test-provider".to_string(),
        memory_mode: ThreadMemoryMode::Enabled,
    }
}
