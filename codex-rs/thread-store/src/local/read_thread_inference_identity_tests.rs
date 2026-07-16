use codex_protocol::ThreadId;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ThreadInferenceIdentity;
use codex_protocol::models::ThreadInferenceIdentityAuthority;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::sync::oneshot;
use uuid::Uuid;

use super::*;
use crate::CreateThreadParams;
use crate::DeleteThreadParams;
use crate::ThreadPersistenceMetadata;
use crate::ThreadStore;
use crate::local::test_support::test_config;
use crate::local::test_support::write_archived_session_file;
use crate::local::test_support::write_session_file;

#[tokio::test]
async fn sidecar_read_distinguishes_capability_eligibility_and_authority() {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    let active_uuid = Uuid::from_u128(901);
    let active_thread_id = thread_id(active_uuid);
    let unsupported = LocalThreadStore::new(config.clone(), /*state_db*/ None);
    assert!(matches!(
        ThreadStore::read_thread_inference_identity_sidecar(
            &unsupported,
            read_params(active_thread_id, /*include_archived*/ false),
        )
        .await,
        Err(ThreadStoreError::Unsupported {
            operation: "read_thread_inference_identity_sidecar"
        })
    ));

    let runtime = codex_state::StateRuntime::init(
        config.sqlite_home.clone(),
        config.default_model_provider_id.clone(),
    )
    .await
    .expect("state db should initialize");
    let store = LocalThreadStore::new(config.clone(), Some(runtime.clone()));
    let active_path = write_session_file(home.path(), "2025-01-03T21-00-00", active_uuid)
        .expect("active session file");
    reconcile(
        &runtime,
        &config,
        active_path.as_path(),
        /*archived*/ false,
    )
    .await;
    let configured = identity("configured");
    assert!(
        runtime
            .update_thread_inference_identity_authority(
                active_thread_id,
                codex_state::ThreadInferenceIdentityAuthorityUpdate {
                    configured: codex_state::ThreadInferenceIdentityAuthorityFieldUpdate::Set(
                        configured.clone(),
                    ),
                    latest_request: codex_state::ThreadInferenceIdentityAuthorityFieldUpdate::Clear,
                },
            )
            .await
            .expect("authority update should succeed")
    );
    assert_eq!(
        ThreadStore::read_thread_inference_identity_sidecar(
            &store,
            read_params(active_thread_id, /*include_archived*/ false),
        )
        .await
        .expect("active sidecar should be readable"),
        ThreadInferenceIdentitySidecar {
            configured: ThreadInferenceIdentityAuthority::Valid(configured),
            latest_request: ThreadInferenceIdentityAuthority::cleared(),
        }
    );

    let live_thread_id = thread_id(Uuid::from_u128(902));
    ThreadStore::create_thread(&store, create_params(live_thread_id, home.path()))
        .await
        .expect("live recorder should open");
    assert_eq!(
        ThreadStore::read_thread_inference_identity_sidecar(
            &store,
            read_params(live_thread_id, /*include_archived*/ false),
        )
        .await
        .expect("unmaterialized live thread should be logically eligible"),
        ThreadInferenceIdentitySidecar::default()
    );
    ThreadStore::discard_thread(&store, live_thread_id)
        .await
        .expect("live recorder should discard");

    let archived_uuid = Uuid::from_u128(903);
    let archived_thread_id = thread_id(archived_uuid);
    let archived_path =
        write_archived_session_file(home.path(), "2025-01-03T22-00-00", archived_uuid)
            .expect("archived session file");
    reconcile(
        &runtime,
        &config,
        archived_path.as_path(),
        /*archived*/ true,
    )
    .await;
    assert!(matches!(
        ThreadStore::read_thread_inference_identity_sidecar(
            &store,
            read_params(archived_thread_id, /*include_archived*/ false),
        )
        .await,
        Err(ThreadStoreError::InvalidRequest { message }) if message.contains("is archived")
    ));
    assert_eq!(
        ThreadStore::read_thread_inference_identity_sidecar(
            &store,
            read_params(archived_thread_id, /*include_archived*/ true),
        )
        .await
        .expect("archived sidecar should be eligible when requested"),
        ThreadInferenceIdentitySidecar::default()
    );
}

#[tokio::test]
async fn sidecar_read_rejects_stale_projection_after_rollout_deletion() {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    let runtime = codex_state::StateRuntime::init(
        config.sqlite_home.clone(),
        config.default_model_provider_id.clone(),
    )
    .await
    .expect("state db should initialize");
    let store = LocalThreadStore::new(config.clone(), Some(runtime.clone()));
    let uuid = Uuid::from_u128(904);
    let thread_id = thread_id(uuid);
    let rollout_path =
        write_session_file(home.path(), "2025-01-03T23-00-00", uuid).expect("active session file");
    reconcile(
        &runtime,
        &config,
        rollout_path.as_path(),
        /*archived*/ false,
    )
    .await;
    std::fs::remove_file(&rollout_path).expect("stage rollout deletion");
    assert!(
        runtime
            .get_thread_inference_identity_authority(thread_id)
            .await
            .expect("projection query should succeed")
            .is_some(),
        "staged deletion intentionally leaves the projection row behind"
    );

    assert!(matches!(
        ThreadStore::read_thread_inference_identity_sidecar(
            &store,
            read_params(thread_id, /*include_archived*/ false),
        )
        .await,
        Err(ThreadStoreError::ThreadNotFound { thread_id: missing }) if missing == thread_id
    ));
}

#[tokio::test]
async fn sidecar_read_queued_after_delete_observes_logical_deletion() {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    let runtime = codex_state::StateRuntime::init(
        config.sqlite_home.clone(),
        config.default_model_provider_id.clone(),
    )
    .await
    .expect("state db should initialize");
    let store = LocalThreadStore::new(config.clone(), Some(runtime.clone()));
    let uuid = Uuid::from_u128(905);
    let thread_id = thread_id(uuid);
    let rollout_path =
        write_session_file(home.path(), "2025-01-04T00-00-00", uuid).expect("active session file");
    reconcile(
        &runtime,
        &config,
        rollout_path.as_path(),
        /*archived*/ false,
    )
    .await;

    let held_custody = store
        .acquire_lifecycle_custody(thread_id)
        .await
        .expect("test should hold lifecycle custody");
    let delete_store = store.clone();
    let (delete_queued_tx, delete_queued_rx) = oneshot::channel();
    let delete_task = tokio::spawn(async move {
        delete_queued_tx.send(()).expect("signal delete queued");
        ThreadStore::delete_thread(&delete_store, DeleteThreadParams { thread_id }).await
    });
    delete_queued_rx.await.expect("delete task should start");

    let read_store = store.clone();
    let (read_queued_tx, read_queued_rx) = oneshot::channel();
    let read_task = tokio::spawn(async move {
        read_queued_tx.send(()).expect("signal read queued");
        ThreadStore::read_thread_inference_identity_sidecar(
            &read_store,
            read_params(thread_id, /*include_archived*/ false),
        )
        .await
    });
    read_queued_rx.await.expect("read task should start");
    drop(held_custody);

    delete_task
        .await
        .expect("delete task should join")
        .expect("delete should complete");
    let read_error = read_task
        .await
        .expect("read task should join")
        .expect_err("read queued behind delete must fail closed");
    assert!(matches!(
        read_error,
        ThreadStoreError::ThreadNotFound { thread_id: missing } if missing == thread_id
    ));
    assert!(
        runtime
            .get_thread_inference_identity_authority(thread_id)
            .await
            .expect("projection query should succeed")
            .is_some(),
        "delete/read custody must reject the stale projection before app-layer cleanup"
    );
}

#[tokio::test]
async fn sidecar_read_maps_state_query_failure_to_internal() {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    let runtime = codex_state::StateRuntime::init(
        config.sqlite_home.clone(),
        config.default_model_provider_id.clone(),
    )
    .await
    .expect("state db should initialize");
    let store = LocalThreadStore::new(config.clone(), Some(runtime.clone()));
    let uuid = Uuid::from_u128(906);
    let thread_id = thread_id(uuid);
    write_session_file(home.path(), "2025-01-04T01-00-00", uuid).expect("active session file");
    runtime.close().await;

    let error = ThreadStore::read_thread_inference_identity_sidecar(
        &store,
        read_params(thread_id, /*include_archived*/ false),
    )
    .await
    .expect_err("authority query failure should not become legacy-missing");
    assert!(matches!(
        error,
        ThreadStoreError::Internal { message }
            if message.contains("failed to read inference identity authority")
    ));
}

fn read_params(
    thread_id: ThreadId,
    include_archived: bool,
) -> ReadThreadInferenceIdentitySidecarParams {
    ReadThreadInferenceIdentitySidecarParams {
        thread_id,
        include_archived,
    }
}

fn thread_id(uuid: Uuid) -> ThreadId {
    ThreadId::from_string(&uuid.to_string()).expect("valid thread id")
}

fn identity(model: &str) -> ThreadInferenceIdentity {
    ThreadInferenceIdentity::new(model, "provider", /*reasoning_effort*/ None)
        .expect("valid identity")
}

fn create_params(thread_id: ThreadId, cwd: &std::path::Path) -> CreateThreadParams {
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
        metadata: ThreadPersistenceMetadata {
            cwd: Some(cwd.to_path_buf()),
            model_provider: "test-provider".to_string(),
            memory_mode: ThreadMemoryMode::Enabled,
        },
    }
}

async fn reconcile(
    runtime: &codex_state::StateRuntime,
    config: &crate::local::LocalThreadStoreConfig,
    path: &std::path::Path,
    archived: bool,
) {
    codex_rollout::state_db::reconcile_rollout(
        Some(runtime),
        path,
        config.default_model_provider_id.as_str(),
        /*builder*/ None,
        &[],
        /*archived_only*/ Some(archived),
        /*new_thread_memory_mode*/ None,
    )
    .await;
}
