use std::future::Future;
use std::future::poll_fn;
use std::path::PathBuf;
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
use crate::ThreadStoreError;
use crate::local::LocalThreadStore;
use crate::local::test_support::test_config;
use crate::local::test_support::write_archived_session_file;
use crate::local::test_support::write_session_file;

const TRANSITION_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::test]
async fn lifecycle_custody_regression_suite() {
    classifies_missing_unmaterialized_and_materialized_threads().await;
    archived_materialization_outweighs_stale_active_live_recorder().await;
    missing_materialized_live_rollout_fails_closed().await;
    delete_name_index_failure_preserves_evidence_and_retry_denies().await;
    shutdown_drains_before_its_custodied_removal_commit().await;
    rejects_simultaneous_active_and_archived_materialization().await;
    archive_transition_completes_before_queued_classification().await;
    unarchive_transition_completes_before_queued_classification().await;
    delete_transition_completes_before_queued_classification().await;
    discard_transition_completes_before_queued_classification().await;
    custody_is_per_thread_and_prunes_idle_entries().await;
}

async fn classifies_missing_unmaterialized_and_materialized_threads() {
    let (home, store) = test_store();
    let missing_id = thread_id(501);
    assert_lifecycle(&store, missing_id, LocalThreadLifecycle::Missing).await;

    let active_id = thread_id(502);
    store
        .create_thread(create_thread_params(active_id, home.path()))
        .await
        .expect("create live thread");
    assert_lifecycle(
        &store,
        active_id,
        LocalThreadLifecycle::UnmaterializedActive,
    )
    .await;
    store
        .persist_thread(active_id)
        .await
        .expect("materialize live thread");
    let active_path = store
        .live_rollout_path(active_id)
        .await
        .expect("active rollout path");
    assert_lifecycle(
        &store,
        active_id,
        LocalThreadLifecycle::Active(active_path.clone()),
    )
    .await;
    store
        .shutdown_thread(active_id)
        .await
        .expect("shutdown active thread");
    assert_lifecycle(&store, active_id, LocalThreadLifecycle::Active(active_path)).await;
}

async fn archived_materialization_outweighs_stale_active_live_recorder() {
    let (home, store) = test_store();
    let thread_id = thread_id(512);
    let active_path = create_materialized_thread(&store, thread_id, home.path()).await;
    close_live_writer_without_removal(&store, thread_id).await;

    store
        .archive_thread(ArchiveThreadParams { thread_id })
        .await
        .expect("archive materialized thread");
    let archived_path = home
        .path()
        .join(codex_rollout::ARCHIVED_SESSIONS_SUBDIR)
        .join(active_path.file_name().expect("rollout file name"));
    assert_eq!(
        store
            .live_rollout_path(thread_id)
            .await
            .expect("stale recorder path"),
        active_path
    );
    assert_lifecycle(
        &store,
        thread_id,
        LocalThreadLifecycle::Archived(archived_path),
    )
    .await;
    store
        .discard_thread(thread_id)
        .await
        .expect("discard stale recorder entry");
}

async fn missing_materialized_live_rollout_fails_closed() {
    let (home, store) = test_store();
    let thread_id = thread_id(513);
    let active_path = create_materialized_thread(&store, thread_id, home.path()).await;
    close_live_writer_without_removal(&store, thread_id).await;
    std::fs::remove_file(active_path).expect("remove materialized rollout");

    assert_lifecycle(&store, thread_id, LocalThreadLifecycle::Missing).await;
    store
        .discard_thread(thread_id)
        .await
        .expect("discard stale recorder entry");
}

async fn delete_name_index_failure_preserves_evidence_and_retry_denies() {
    let (home, store) = test_store();
    let uuid = Uuid::from_u128(514);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
    let active_path =
        write_session_file(home.path(), "2025-01-05T14-00-00", uuid).expect("active rollout");
    store
        .resume_thread(ResumeThreadParams {
            thread_id,
            rollout_path: Some(active_path.clone()),
            history: None,
            include_archived: false,
            metadata: thread_metadata(home.path()),
        })
        .await
        .expect("resume active thread");
    let session_index_path = home.path().join("session_index.jsonl");
    std::fs::create_dir(&session_index_path).expect("blocking session-index directory");

    let err = store
        .delete_thread(DeleteThreadParams { thread_id })
        .await
        .expect_err("name-index cleanup should fail before deletion");
    assert!(matches!(
        err,
        ThreadStoreError::Internal { message }
            if message.contains("failed to delete thread name index entries")
    ));
    assert!(active_path.exists());
    assert_lifecycle(
        &store,
        thread_id,
        LocalThreadLifecycle::Active(active_path.clone()),
    )
    .await;

    close_live_writer_without_removal(&store, thread_id).await;
    std::fs::remove_dir(session_index_path).expect("remove blocking directory");
    store
        .delete_thread(DeleteThreadParams { thread_id })
        .await
        .expect("retry delete");
    assert!(!active_path.exists());
    assert_lifecycle(&store, thread_id, LocalThreadLifecycle::Missing).await;
}

async fn shutdown_drains_before_its_custodied_removal_commit() {
    let (home, store) = test_store();
    let thread_id = thread_id(515);
    store
        .create_thread(create_thread_params(thread_id, home.path()))
        .await
        .expect("create unmaterialized live thread");
    let old_recorder = store
        .live_recorder(thread_id)
        .await
        .expect("old live recorder");
    let lifecycle_guard = store
        .acquire_lifecycle_custody(thread_id)
        .await
        .expect("hold lifecycle custody");
    let mut shutdown_task = store.shutdown_thread(thread_id);
    poll_once_pending(&mut shutdown_task, "shutdown removal commit").await;
    timeout(TRANSITION_TIMEOUT, old_recorder.recorder.persist())
        .await
        .expect("closed writer probe should not stall")
        .expect_err("writer should drain before waiting for removal custody");
    assert!(store.live_rollout_path(thread_id).await.is_ok());

    drop(lifecycle_guard);
    timeout(TRANSITION_TIMEOUT, shutdown_task)
        .await
        .expect("shutdown removal commit should not stall")
        .expect("shutdown thread");
    assert_lifecycle(&store, thread_id, LocalThreadLifecycle::Missing).await;

    store
        .create_thread(create_thread_params(thread_id, home.path()))
        .await
        .expect("create replacement recorder");
    store
        .remove_live_recorder_if_current(thread_id, &old_recorder.token)
        .await;
    assert!(store.live_rollout_path(thread_id).await.is_ok());
    store
        .discard_thread(thread_id)
        .await
        .expect("discard replacement recorder");
}

async fn rejects_simultaneous_active_and_archived_materialization() {
    let (home, store) = test_store();
    let uuid = Uuid::from_u128(509);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
    write_session_file(home.path(), "2025-01-05T12-00-00", uuid).expect("active rollout");
    write_archived_session_file(home.path(), "2025-01-05T12-30-00", uuid)
        .expect("archived rollout");

    let guard = store
        .acquire_lifecycle_custody(thread_id)
        .await
        .expect("lifecycle custody");
    assert!(matches!(
        guard.classify().await,
        Err(ThreadStoreError::Conflict { .. })
    ));
}

async fn archive_transition_completes_before_queued_classification() {
    let (home, store) = test_store();
    let uuid = Uuid::from_u128(504);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
    let active_path = write_session_file(home.path(), "2025-01-05T11-00-00", uuid)
        .expect("active rollout");
    let archived_path = home
        .path()
        .join(codex_rollout::ARCHIVED_SESSIONS_SUBDIR)
        .join(active_path.file_name().expect("rollout file name"));
    assert_custodied_transition(
        &store,
        thread_id,
        store.archive_thread(ArchiveThreadParams { thread_id }),
        "archive",
        LocalThreadLifecycle::Archived(archived_path),
    )
    .await;
}

async fn unarchive_transition_completes_before_queued_classification() {
    let (home, store) = test_store();
    let uuid = Uuid::from_u128(505);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
    let archived_path = write_archived_session_file(home.path(), "2025-01-05T11-30-00", uuid)
        .expect("archived rollout");
    let active_path = home
        .path()
        .join("sessions/2025/01/05")
        .join(archived_path.file_name().expect("rollout file name"));
    assert_custodied_transition(
        &store,
        thread_id,
        store.unarchive_thread(ArchiveThreadParams { thread_id }),
        "unarchive",
        LocalThreadLifecycle::Active(active_path),
    )
    .await;
}

async fn delete_transition_completes_before_queued_classification() {
    let (home, store) = test_store();
    let uuid = Uuid::from_u128(510);
    let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
    let active_path = write_session_file(home.path(), "2025-01-05T13-00-00", uuid)
        .expect("active rollout");
    assert_custodied_transition(
        &store,
        thread_id,
        store.delete_thread(DeleteThreadParams { thread_id }),
        "delete",
        LocalThreadLifecycle::Missing,
    )
    .await;
    assert!(!active_path.exists());
}

async fn discard_transition_completes_before_queued_classification() {
    let (home, store) = test_store();
    let thread_id = thread_id(511);
    store
        .create_thread(create_thread_params(thread_id, home.path()))
        .await
        .expect("create unmaterialized live thread");
    assert_custodied_transition(
        &store,
        thread_id,
        store.discard_thread(thread_id),
        "discard",
        LocalThreadLifecycle::Missing,
    )
    .await;
}

async fn custody_is_per_thread_and_prunes_idle_entries() {
    let (_home, store) = test_store();
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
    let mut cancelled_waiter = Box::pin(store.acquire_lifecycle_custody(thread_a));
    poll_once_pending(&mut cancelled_waiter, "same-thread custody waiter").await;
    drop(cancelled_waiter);
    drop(guard_a);

    let _guard_c = store
        .acquire_lifecycle_custody(thread_c)
        .await
        .expect("thread C custody");
    let custody = store.lifecycle_custody.lock().await;
    assert_eq!(custody.len(), 1);
    assert!(custody.contains_key(&thread_c));
}

fn test_store() -> (TempDir, LocalThreadStore) {
    let home = TempDir::new().expect("temp dir");
    let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
    (home, store)
}

async fn classify(store: &LocalThreadStore, thread_id: ThreadId) -> LocalThreadLifecycle {
    let guard = store
        .acquire_lifecycle_custody(thread_id)
        .await
        .expect("lifecycle custody");
    guard.classify().await.expect("classify thread")
}

async fn assert_lifecycle(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    expected: LocalThreadLifecycle,
) {
    assert_eq!(classify(store, thread_id).await, expected);
}

async fn create_materialized_thread(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    cwd: &std::path::Path,
) -> PathBuf {
    store
        .create_thread(create_thread_params(thread_id, cwd))
        .await
        .expect("create live thread");
    store
        .persist_thread(thread_id)
        .await
        .expect("materialize live thread");
    store
        .live_rollout_path(thread_id)
        .await
        .expect("active rollout path")
}

async fn close_live_writer_without_removal(store: &LocalThreadStore, thread_id: ThreadId) {
    store
        .live_recorder(thread_id)
        .await
        .expect("live recorder")
        .recorder
        .shutdown()
        .await
        .expect("close writer without removing recorder entry");
}

async fn assert_custodied_transition<F, T>(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    transition: F,
    operation: &str,
    expected: LocalThreadLifecycle,
) where
    F: Future<Output = crate::ThreadStoreResult<T>>,
{
    let initial_guard = store
        .acquire_lifecycle_custody(thread_id)
        .await
        .expect("initial custody");
    let mut transition = Box::pin(transition);
    poll_once_pending(&mut transition, operation).await;
    let mut classification = Box::pin(async {
        let guard = store.acquire_lifecycle_custody(thread_id).await?;
        guard.classify().await
    });
    poll_once_pending(&mut classification, "classification").await;
    drop(initial_guard);
    poll_once_pending(&mut classification, "classification barged ahead of transition").await;

    timeout(TRANSITION_TIMEOUT, transition)
        .await
        .expect("transition should not stall")
        .expect("transition failed");
    let lifecycle = timeout(TRANSITION_TIMEOUT, classification)
        .await
        .expect("classification should not stall")
        .expect("classification failed");
    assert_eq!(lifecycle, expected);
}

async fn poll_once_pending<F>(future: &mut Pin<Box<F>>, operation: &str)
where
    F: Future + ?Sized,
{
    poll_fn(|cx| match future.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("{operation} unexpectedly completed"),
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
