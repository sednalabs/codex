use std::collections::HashMap;

use codex_protocol::ThreadId;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ThreadInferenceIdentity;
use codex_protocol::models::ThreadInferenceIdentityAuthority;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;

use super::*;
use crate::ThreadPersistenceMetadata;

#[tokio::test]
async fn identity_patch_preserves_omitted_state_and_thread_isolation() {
    let store = InMemoryThreadStore::default();
    let first_thread_id = thread_id(1);
    let second_thread_id = thread_id(2);
    let missing_thread_id = thread_id(3);
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
async fn identity_sidecar_lifecycle_does_not_resurrect_deleted_state() {
    let store = InMemoryThreadStore::default();
    let thread_id = thread_id(4);
    store
        .create_thread(create_thread_params(thread_id))
        .await
        .expect("thread should be created");
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
    ThreadStore::resume_thread(
        &store,
        ResumeThreadParams {
            thread_id,
            rollout_path: None,
            history: None,
            include_archived: false,
            metadata: thread_metadata(),
        },
    )
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

    store
        .create_thread(create_thread_params(thread_id))
        .await
        .expect("thread should be recreated");
    assert_eq!(
        sidecars(&store).await,
        HashMap::from([(thread_id, ThreadInferenceIdentitySidecar::default())])
    );
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
    ThreadId::from_string(format!("00000000-0000-0000-0000-{suffix:012}"))
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

fn thread_metadata() -> ThreadPersistenceMetadata {
    ThreadPersistenceMetadata {
        cwd: None,
        model_provider: "test-provider".to_string(),
        memory_mode: ThreadMemoryMode::Enabled,
    }
}
