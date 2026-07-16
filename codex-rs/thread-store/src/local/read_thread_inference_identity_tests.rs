use codex_protocol::models::ThreadInferenceIdentity;
use codex_protocol::models::ThreadInferenceIdentityAuthority;
use tempfile::TempDir;
use uuid::Uuid;

use super::*;
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

    let legacy_uuid = Uuid::from_u128(902);
    let legacy_thread_id = thread_id(legacy_uuid);
    write_session_file(home.path(), "2025-01-03T21-30-00", legacy_uuid)
        .expect("legacy session file");
    assert_eq!(
        ThreadStore::read_thread_inference_identity_sidecar(
            &store,
            read_params(legacy_thread_id, /*include_archived*/ false),
        )
        .await
        .expect("eligible thread without projection should be legacy-missing"),
        ThreadInferenceIdentitySidecar::default()
    );

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
        Err(ThreadStoreError::InvalidRequest { .. })
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

    ThreadStore::delete_thread(
        &store,
        crate::DeleteThreadParams {
            thread_id: active_thread_id,
        },
    )
    .await
    .expect("active rollout should delete");
    assert!(
        runtime
            .get_thread_inference_identity_authority(active_thread_id)
            .await
            .expect("projection lookup should still succeed")
            .is_some(),
        "local deletion intentionally precedes projection cleanup"
    );
    assert!(matches!(
        ThreadStore::read_thread_inference_identity_sidecar(
            &store,
            read_params(active_thread_id, /*include_archived*/ false),
        )
        .await,
        Err(ThreadStoreError::InvalidRequest { .. })
    ));

    runtime.close().await;
    let closed_db_error = ThreadStore::read_thread_inference_identity_sidecar(
        &store,
        read_params(legacy_thread_id, /*include_archived*/ false),
    )
    .await
    .expect_err("authority query failure should not become legacy-missing");
    assert!(matches!(
        closed_db_error,
        ThreadStoreError::Internal { message }
            if message.contains("failed to read inference identity authority")
    ));
}

fn read_params(
    thread_id: codex_protocol::ThreadId,
    include_archived: bool,
) -> ReadThreadInferenceIdentitySidecarParams {
    ReadThreadInferenceIdentitySidecarParams {
        thread_id,
        include_archived,
    }
}

fn thread_id(uuid: Uuid) -> codex_protocol::ThreadId {
    codex_protocol::ThreadId::from_string(&uuid.to_string()).expect("valid thread id")
}

fn identity(model: &str) -> ThreadInferenceIdentity {
    ThreadInferenceIdentity::new(model, "provider", /*reasoning_effort*/ None)
        .expect("valid identity")
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
