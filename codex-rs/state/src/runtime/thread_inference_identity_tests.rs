use std::sync::Arc;

use codex_protocol::ThreadId;
use codex_protocol::models::ThreadInferenceIdentity;
use tokio::sync::Barrier;
use tokio::time::Duration;
use tokio::time::timeout;

use super::StateRuntime;
use crate::ThreadInferenceIdentityAuthorityFieldUpdate;
use crate::ThreadInferenceIdentityAuthorityUpdate;
use crate::runtime::test_support::test_thread_metadata;
use crate::runtime::test_support::unique_temp_dir;

type RawAuthorityRow = (Option<String>, Option<String>);

#[tokio::test]
async fn presence_patch_preserves_exact_raw_rows_and_missing_semantics() {
    let thread_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000701").expect("valid thread id");
    let runtime = runtime_with_thread(thread_id).await;

    let configured = set_update("configured");
    let latest_request = set_update("latest-request");
    let configured_raw = encoded(&configured);
    let latest_request_raw = encoded(&latest_request);
    let coordinated = runtime
        .update_thread_inference_identity_authority(
            thread_id,
            ThreadInferenceIdentityAuthorityUpdate {
                configured,
                latest_request,
            },
        )
        .await
        .expect("coordinated update should succeed");
    assert_eq!(
        (coordinated, raw_row(&runtime, thread_id).await),
        (
            true,
            Some((
                Some(configured_raw.clone()),
                Some(latest_request_raw.clone()),
            )),
        )
    );

    let malformed_latest = "{exact malformed latest-request bytes}";
    seed_raw_row(
        &runtime,
        thread_id,
        "{replace configured bytes}",
        malformed_latest,
    )
    .await;
    let configured_only = set_update("configured-only");
    let configured_only_raw = encoded(&configured_only);
    let configured_updated = runtime
        .update_thread_inference_identity_authority(
            thread_id,
            ThreadInferenceIdentityAuthorityUpdate {
                configured: configured_only,
                latest_request: ThreadInferenceIdentityAuthorityFieldUpdate::Omit,
            },
        )
        .await
        .expect("configured-only update should succeed");
    assert_eq!(
        (configured_updated, raw_row(&runtime, thread_id).await),
        (
            true,
            Some((
                Some(configured_only_raw),
                Some(malformed_latest.to_string()),
            )),
        )
    );

    let malformed_configured = "{exact malformed configured bytes}";
    seed_raw_row(
        &runtime,
        thread_id,
        malformed_configured,
        "{replace latest-request bytes}",
    )
    .await;
    let latest_only = set_update("latest-only");
    let latest_only_raw = encoded(&latest_only);
    let latest_updated = runtime
        .update_thread_inference_identity_authority(
            thread_id,
            ThreadInferenceIdentityAuthorityUpdate {
                configured: ThreadInferenceIdentityAuthorityFieldUpdate::Omit,
                latest_request: latest_only,
            },
        )
        .await
        .expect("latest-request-only update should succeed");
    assert_eq!(
        (latest_updated, raw_row(&runtime, thread_id).await),
        (
            true,
            Some((
                Some(malformed_configured.to_string()),
                Some(latest_only_raw),
            )),
        )
    );

    let cleared_raw = encoded(&ThreadInferenceIdentityAuthorityFieldUpdate::Clear);
    let after_clear = set_update("after-clear");
    let after_clear_raw = encoded(&after_clear);
    let clear_set_updated = runtime
        .update_thread_inference_identity_authority(
            thread_id,
            ThreadInferenceIdentityAuthorityUpdate {
                configured: ThreadInferenceIdentityAuthorityFieldUpdate::Clear,
                latest_request: after_clear,
            },
        )
        .await
        .expect("coordinated clear and set should succeed");
    assert_eq!(
        (clear_set_updated, raw_row(&runtime, thread_id).await),
        (true, Some((Some(cleared_raw), Some(after_clear_raw))),)
    );

    let omitted_raw = (
        "{omit-preserved configured bytes}",
        "{omit-preserved latest-request bytes}",
    );
    seed_raw_row(&runtime, thread_id, omitted_raw.0, omitted_raw.1).await;
    let omitted = runtime
        .update_thread_inference_identity_authority(
            thread_id,
            ThreadInferenceIdentityAuthorityUpdate::default(),
        )
        .await
        .expect("all-omitted update should succeed without writing");
    assert_eq!(
        (omitted, raw_row(&runtime, thread_id).await),
        (
            false,
            Some((
                Some(omitted_raw.0.to_string()),
                Some(omitted_raw.1.to_string()),
            )),
        )
    );

    let missing_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000799").expect("valid thread id");
    let missing_configured = runtime
        .update_thread_inference_identity_authority(
            missing_id,
            ThreadInferenceIdentityAuthorityUpdate {
                configured: set_update("missing-configured"),
                latest_request: ThreadInferenceIdentityAuthorityFieldUpdate::Omit,
            },
        )
        .await
        .expect("missing configured-only update should return a boolean");
    let missing_latest = runtime
        .update_thread_inference_identity_authority(
            missing_id,
            ThreadInferenceIdentityAuthorityUpdate {
                configured: ThreadInferenceIdentityAuthorityFieldUpdate::Omit,
                latest_request: set_update("missing-latest"),
            },
        )
        .await
        .expect("missing latest-request-only update should return a boolean");
    let missing_coordinated = runtime
        .update_thread_inference_identity_authority(
            missing_id,
            ThreadInferenceIdentityAuthorityUpdate {
                configured: ThreadInferenceIdentityAuthorityFieldUpdate::Clear,
                latest_request: set_update("missing-coordinated"),
            },
        )
        .await
        .expect("missing coordinated update should return a boolean");
    assert_eq!(
        (
            missing_configured,
            missing_latest,
            missing_coordinated,
            raw_row(&runtime, missing_id).await,
        ),
        (false, false, false, None)
    );
}

#[tokio::test]
async fn independently_spawned_single_field_updates_converge_exactly() {
    let thread_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000702").expect("valid thread id");
    let runtime = runtime_with_thread(thread_id).await;
    install_update_audit(&runtime).await;
    let barrier = Arc::new(Barrier::new(/*n*/ 3));
    let configured = set_update("spawned-configured");
    let latest_request = set_update("spawned-latest-request");
    let configured_raw = encoded(&configured);
    let latest_request_raw = encoded(&latest_request);

    let configured_task = tokio::spawn({
        let runtime = Arc::clone(&runtime);
        let barrier = Arc::clone(&barrier);
        async move {
            barrier.wait().await;
            runtime
                .update_thread_inference_identity_authority(
                    thread_id,
                    ThreadInferenceIdentityAuthorityUpdate {
                        configured,
                        latest_request: ThreadInferenceIdentityAuthorityFieldUpdate::Omit,
                    },
                )
                .await
        }
    });
    let latest_request_task = tokio::spawn({
        let runtime = Arc::clone(&runtime);
        let barrier = Arc::clone(&barrier);
        async move {
            barrier.wait().await;
            runtime
                .update_thread_inference_identity_authority(
                    thread_id,
                    ThreadInferenceIdentityAuthorityUpdate {
                        configured: ThreadInferenceIdentityAuthorityFieldUpdate::Omit,
                        latest_request,
                    },
                )
                .await
        }
    });

    let task_results = timeout(Duration::from_secs(/*secs*/ 5), async {
        barrier.wait().await;
        let configured_result = configured_task
            .await
            .expect("configured task should complete")
            .expect("configured update should succeed");
        let latest_request_result = latest_request_task
            .await
            .expect("latest-request task should complete")
            .expect("latest-request update should succeed");
        (configured_result, latest_request_result)
    })
    .await
    .expect("spawned updates should finish before the timeout");

    assert_eq!(
        (task_results, raw_row(&runtime, thread_id).await),
        (
            (true, true),
            Some((Some(configured_raw), Some(latest_request_raw))),
        )
    );
    let mut audit_events = audit_events(&runtime).await;
    audit_events.sort();
    assert_eq!(
        audit_events,
        vec!["configured".to_string(), "latest_request".to_string()]
    );
}

async fn runtime_with_thread(thread_id: ThreadId) -> Arc<StateRuntime> {
    let codex_home = unique_temp_dir();
    let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
        .await
        .expect("state db should initialize");
    runtime
        .upsert_thread(&test_thread_metadata(
            &codex_home,
            thread_id,
            codex_home.clone(),
        ))
        .await
        .expect("thread should exist");
    runtime
}

fn set_update(model: &str) -> ThreadInferenceIdentityAuthorityFieldUpdate {
    ThreadInferenceIdentityAuthorityFieldUpdate::Set(
        ThreadInferenceIdentity::new(model, "provider", /*reasoning_effort*/ None)
            .expect("identity should be valid"),
    )
}

fn encoded(update: &ThreadInferenceIdentityAuthorityFieldUpdate) -> String {
    update
        .encode()
        .expect("authority should encode")
        .expect("writable authority should have bytes")
}

async fn seed_raw_row(
    runtime: &StateRuntime,
    thread_id: ThreadId,
    configured: &str,
    latest_request: &str,
) {
    sqlx::query(
        "UPDATE threads SET configured_inference_identity_authority = ?, latest_request_inference_identity_authority = ? WHERE id = ?",
    )
    .bind(configured)
    .bind(latest_request)
    .bind(thread_id.to_string())
    .execute(runtime.pool.as_ref())
    .await
    .expect("raw authority fixture should be stored");
}

async fn raw_row(runtime: &StateRuntime, thread_id: ThreadId) -> Option<RawAuthorityRow> {
    sqlx::query_as::<_, RawAuthorityRow>(
        "SELECT configured_inference_identity_authority, latest_request_inference_identity_authority FROM threads WHERE id = ?",
    )
    .bind(thread_id.to_string())
    .fetch_optional(runtime.pool.as_ref())
    .await
    .expect("raw authority row should be readable")
}

async fn install_update_audit(runtime: &StateRuntime) {
    for statement in [
        "CREATE TABLE inference_identity_update_audit (event TEXT NOT NULL)",
        "CREATE TRIGGER audit_configured_inference_identity_update AFTER UPDATE OF configured_inference_identity_authority ON threads BEGIN INSERT INTO inference_identity_update_audit (event) VALUES ('configured'); END",
        "CREATE TRIGGER audit_latest_request_inference_identity_update AFTER UPDATE OF latest_request_inference_identity_authority ON threads BEGIN INSERT INTO inference_identity_update_audit (event) VALUES ('latest_request'); END",
    ] {
        sqlx::query(statement)
            .execute(runtime.pool.as_ref())
            .await
            .expect("update audit fixture should install");
    }
}

async fn audit_events(runtime: &StateRuntime) -> Vec<String> {
    sqlx::query_scalar("SELECT event FROM inference_identity_update_audit")
        .fetch_all(runtime.pool.as_ref())
        .await
        .expect("update audit events should be readable")
}
