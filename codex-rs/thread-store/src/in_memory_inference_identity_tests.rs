use std::collections::HashMap;

use codex_protocol::ThreadId;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ThreadInferenceIdentity;
use codex_protocol::models::ThreadInferenceIdentityAuthority;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use pretty_assertions::assert_eq;

use super::*;
use crate::ReadThreadByRolloutPathParams;
use crate::ReadThreadInferenceIdentitySidecarParams;
use crate::SortDirection;
use crate::ThreadInferenceIdentitySidecar;
use crate::ThreadInferenceIdentitySidecarPatch;
use crate::ThreadPersistenceMetadata;
use crate::ThreadSortKey;

#[tokio::test]
async fn identity_patch_preserves_omitted_state_and_thread_isolation() {
    let store = InMemoryThreadStore::default();
    let first_thread_id = thread_id(/*suffix*/ 1);
    let second_thread_id = thread_id(/*suffix*/ 2);
    let missing_thread_id = thread_id(/*suffix*/ 3);
    for thread_id in [first_thread_id, second_thread_id] {
        store
            .create_thread(create_thread_params(thread_id))
            .await
            .expect("thread should be created");
    }

    let malformed = ThreadInferenceIdentityAuthority::Malformed {
        raw: "{exact malformed configured}".to_string(),
    };
    store.state.lock().await.inference_identity.sidecars.insert(
        first_thread_id,
        ThreadInferenceIdentitySidecar {
            configured: malformed.clone(),
            latest_request: ThreadInferenceIdentityAuthority::LegacyMissing,
        },
    );
    let before_empty = sidecars(&store).await;
    ThreadStore::update_thread_inference_identity_sidecar(
        &store,
        UpdateThreadInferenceIdentitySidecarParams {
            thread_id: missing_thread_id,
            patch: ThreadInferenceIdentitySidecarPatch::default(),
        },
    )
    .await
    .expect("empty patch should be a no-op even for an unknown thread");
    assert_eq!(sidecars(&store).await, before_empty);

    let missing_error = ThreadStore::update_thread_inference_identity_sidecar(
        &store,
        UpdateThreadInferenceIdentitySidecarParams {
            thread_id: missing_thread_id,
            patch: ThreadInferenceIdentitySidecarPatch {
                configured: Some(None),
                latest_request: None,
            },
        },
    )
    .await
    .expect_err("nonempty patch should reject an unknown thread");
    assert!(matches!(
        missing_error,
        ThreadStoreError::ThreadNotFound { thread_id } if thread_id == missing_thread_id
    ));

    let latest_request = identity("request");
    ThreadStore::update_thread_inference_identity_sidecar(
        &store,
        UpdateThreadInferenceIdentitySidecarParams {
            thread_id: first_thread_id,
            patch: ThreadInferenceIdentitySidecarPatch {
                configured: None,
                latest_request: Some(Some(latest_request.clone())),
            },
        },
    )
    .await
    .expect("latest-request update should succeed");
    assert_eq!(
        sidecars(&store).await,
        HashMap::from([
            (
                first_thread_id,
                ThreadInferenceIdentitySidecar {
                    configured: malformed,
                    latest_request: ThreadInferenceIdentityAuthority::Valid(latest_request),
                },
            ),
            (second_thread_id, ThreadInferenceIdentitySidecar::default()),
        ])
    );

    let paired_request = identity("paired-request");
    ThreadStore::update_thread_inference_identity_sidecar(
        &store,
        UpdateThreadInferenceIdentitySidecarParams {
            thread_id: first_thread_id,
            patch: ThreadInferenceIdentitySidecarPatch {
                configured: Some(None),
                latest_request: Some(Some(paired_request.clone())),
            },
        },
    )
    .await
    .expect("paired update should succeed");
    assert_eq!(
        sidecars(&store).await,
        HashMap::from([
            (
                first_thread_id,
                ThreadInferenceIdentitySidecar {
                    configured: ThreadInferenceIdentityAuthority::cleared(),
                    latest_request: ThreadInferenceIdentityAuthority::Valid(paired_request),
                },
            ),
            (second_thread_id, ThreadInferenceIdentitySidecar::default()),
        ])
    );
}

#[tokio::test]
async fn identity_sidecar_lifecycle_resets_restores_and_does_not_resurrect() {
    let store = InMemoryThreadStore::default();
    let thread_id = thread_id(/*suffix*/ 4);
    let stale_sidecar = ThreadInferenceIdentitySidecar {
        configured: ThreadInferenceIdentityAuthority::Valid(identity("stale-configured")),
        latest_request: ThreadInferenceIdentityAuthority::cleared(),
    };
    store
        .state
        .lock()
        .await
        .inference_identity
        .sidecars
        .insert(thread_id, stale_sidecar.clone());
    assert_eq!(
        sidecars(&store).await,
        HashMap::from([(thread_id, stale_sidecar)])
    );

    ThreadStore::create_thread(&store, create_thread_params(thread_id))
        .await
        .expect("thread should be created");
    assert_eq!(
        sidecars(&store).await,
        HashMap::from([(thread_id, ThreadInferenceIdentitySidecar::default())])
    );

    assert_eq!(
        store
            .state
            .lock()
            .await
            .inference_identity
            .sidecars
            .remove(&thread_id),
        Some(ThreadInferenceIdentitySidecar::default())
    );
    ThreadStore::resume_thread(&store, resume_thread_params(thread_id))
        .await
        .expect("resume should restore missing sidecar state");
    assert_eq!(
        sidecars(&store).await,
        HashMap::from([(thread_id, ThreadInferenceIdentitySidecar::default())])
    );

    let configured = identity("configured");
    ThreadStore::update_thread_inference_identity_sidecar(
        &store,
        UpdateThreadInferenceIdentitySidecarParams {
            thread_id,
            patch: ThreadInferenceIdentitySidecarPatch {
                configured: Some(Some(configured.clone())),
                latest_request: None,
            },
        },
    )
    .await
    .expect("configured update should succeed");
    ThreadStore::resume_thread(&store, resume_thread_params(thread_id))
        .await
        .expect("resume should preserve the sidecar");
    assert_eq!(
        sidecars(&store).await,
        HashMap::from([(
            thread_id,
            ThreadInferenceIdentitySidecar {
                configured: ThreadInferenceIdentityAuthority::Valid(configured),
                latest_request: ThreadInferenceIdentityAuthority::LegacyMissing,
            },
        )])
    );

    ThreadStore::delete_thread(&store, DeleteThreadParams { thread_id })
        .await
        .expect("delete should succeed");
    assert_eq!(sidecars(&store).await, HashMap::new());

    let deleted_error = ThreadStore::update_thread_inference_identity_sidecar(
        &store,
        UpdateThreadInferenceIdentitySidecarParams {
            thread_id,
            patch: ThreadInferenceIdentitySidecarPatch {
                configured: None,
                latest_request: Some(None),
            },
        },
    )
    .await
    .expect_err("nonempty patch should reject a deleted thread");
    assert!(matches!(
        deleted_error,
        ThreadStoreError::ThreadNotFound { thread_id: missing_thread_id }
            if missing_thread_id == thread_id
    ));

    ThreadStore::create_thread(&store, create_thread_params(thread_id))
        .await
        .expect("thread should be recreated");
    assert_eq!(
        sidecars(&store).await,
        HashMap::from([(thread_id, ThreadInferenceIdentitySidecar::default())])
    );
}

#[tokio::test]
async fn identity_sidecar_read_composes_with_thread_read_shapes() {
    let store = InMemoryThreadStore::default();
    let thread_id = thread_id(/*suffix*/ 5);
    let second_thread_id = thread_id(/*suffix*/ 6);
    let rollout_path = std::path::PathBuf::from("/tmp/in-memory-identity-sidecar.jsonl");
    ThreadStore::create_thread(&store, create_thread_params(thread_id))
        .await
        .expect("thread should be created");
    ThreadStore::create_thread(&store, create_thread_params(second_thread_id))
        .await
        .expect("second thread should be created");
    ThreadStore::resume_thread(
        &store,
        ResumeThreadParams {
            thread_id,
            rollout_path: Some(rollout_path.clone()),
            history: None,
            include_archived: false,
            metadata: thread_metadata(),
        },
    )
    .await
    .expect("thread should resume by rollout path");
    let expected = ThreadInferenceIdentitySidecar {
        configured: ThreadInferenceIdentityAuthority::Valid(identity("configured")),
        latest_request: ThreadInferenceIdentityAuthority::cleared(),
    };
    let second_expected = ThreadInferenceIdentitySidecar {
        configured: ThreadInferenceIdentityAuthority::Malformed {
            raw: "{exact malformed second configured}".to_string(),
        },
        latest_request: ThreadInferenceIdentityAuthority::LegacyMissing,
    };
    let mut state = store.state.lock().await;
    state
        .inference_identity
        .sidecars
        .insert(thread_id, expected.clone());
    state
        .inference_identity
        .sidecars
        .insert(second_thread_id, second_expected.clone());
    drop(state);

    let direct = ThreadStore::read_thread(
        &store,
        ReadThreadParams {
            thread_id,
            include_archived: false,
            include_history: false,
        },
    )
    .await
    .expect("direct read should find thread");
    let by_path = ThreadStore::read_thread_by_rollout_path(
        &store,
        ReadThreadByRolloutPathParams {
            rollout_path,
            include_archived: false,
            include_history: false,
        },
    )
    .await
    .expect("path read should find thread");
    let listed = ThreadStore::list_threads(&store, list_threads_params())
        .await
        .expect("list should find thread")
        .items
        .into_iter()
        .find(|item| item.thread_id == thread_id)
        .expect("listed thread");

    for observed_thread_id in [direct.thread_id, by_path.thread_id, listed.thread_id] {
        assert_eq!(
            ThreadStore::read_thread_inference_identity_sidecar(
                &store,
                ReadThreadInferenceIdentitySidecarParams {
                    thread_id: observed_thread_id,
                    include_archived: false,
                },
            )
            .await
            .expect("sidecar read should compose with thread id"),
            expected
        );
    }

    for (observed_thread_id, expected_sidecar) in
        [(thread_id, expected), (second_thread_id, second_expected)]
    {
        assert_eq!(
            ThreadStore::read_thread_inference_identity_sidecar(
                &store,
                ReadThreadInferenceIdentitySidecarParams {
                    thread_id: observed_thread_id,
                    include_archived: false,
                },
            )
            .await
            .expect("sidecar read should preserve thread isolation"),
            expected_sidecar
        );
    }
}

#[tokio::test]
async fn identity_sidecar_read_rejects_missing_and_deleted_threads() {
    let store = InMemoryThreadStore::default();
    let thread_id = thread_id(/*suffix*/ 6);
    let missing_thread_id = thread_id(/*suffix*/ 7);
    assert!(matches!(
        ThreadStore::read_thread_inference_identity_sidecar(
            &store,
            ReadThreadInferenceIdentitySidecarParams {
                thread_id: missing_thread_id,
                include_archived: true,
            },
        )
        .await,
        Err(ThreadStoreError::ThreadNotFound { thread_id }) if thread_id == missing_thread_id
    ));

    ThreadStore::create_thread(&store, create_thread_params(thread_id))
        .await
        .expect("thread should be created");
    ThreadStore::delete_thread(&store, DeleteThreadParams { thread_id })
        .await
        .expect("thread should delete");
    assert!(matches!(
        ThreadStore::read_thread_inference_identity_sidecar(
            &store,
            ReadThreadInferenceIdentitySidecarParams {
                thread_id,
                include_archived: true,
            },
        )
        .await,
        Err(ThreadStoreError::ThreadNotFound { thread_id: deleted_thread_id })
            if deleted_thread_id == thread_id
    ));
}

async fn sidecars(
    store: &InMemoryThreadStore,
) -> HashMap<ThreadId, ThreadInferenceIdentitySidecar> {
    store.state.lock().await.inference_identity.sidecars.clone()
}

fn identity(model: &str) -> ThreadInferenceIdentity {
    ThreadInferenceIdentity::new(model, "provider", /*reasoning_effort*/ None)
        .expect("identity should be valid")
}

fn thread_id(suffix: u8) -> ThreadId {
    ThreadId::from_string(&format!("00000000-0000-0000-0000-{suffix:012}"))
        .expect("thread id should be valid")
}

fn create_thread_params(thread_id: ThreadId) -> CreateThreadParams {
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
        initial_window_id: uuid::Uuid::now_v7().to_string(),
        metadata: thread_metadata(),
    }
}

fn resume_thread_params(thread_id: ThreadId) -> ResumeThreadParams {
    ResumeThreadParams {
        thread_id,
        rollout_path: None,
        history: None,
        include_archived: false,
        metadata: thread_metadata(),
    }
}

fn thread_metadata() -> ThreadPersistenceMetadata {
    ThreadPersistenceMetadata {
        cwd: None,
        model_provider: "test-provider".to_string(),
        memory_mode: ThreadMemoryMode::Enabled,
    }
}

fn list_threads_params() -> ListThreadsParams {
    ListThreadsParams {
        page_size: 10,
        cursor: None,
        sort_key: ThreadSortKey::CreatedAt,
        sort_direction: SortDirection::Asc,
        allowed_sources: Vec::new(),
        model_providers: None,
        cwd_filters: None,
        archived: false,
        search_term: None,
        relation_filter: None,
        use_state_db_only: false,
    }
}
