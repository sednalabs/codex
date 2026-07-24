use std::sync::Arc;

use super::ConfiguredIdentityProvenance;
use super::StateRuntime;
use crate::runtime::test_support::test_thread_metadata;
use crate::runtime::test_support::unique_temp_dir;
use codex_protocol::ThreadId;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn configured_identity_provenance_transitions_monotonically() {
    let codex_home = unique_temp_dir();
    let runtime = StateRuntime::init(
        crate::SqliteConfig::new_for_testing(codex_home.as_path().abs()),
        "test-provider".to_string(),
    )
        .await
        .expect("state db should initialize");
    let first_thread_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000801").expect("valid thread id");
    let second_thread_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000802").expect("valid thread id");
    let missing_thread_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000804").expect("valid thread id");
    for thread_id in [first_thread_id, second_thread_id] {
        runtime
            .upsert_thread(&test_thread_metadata(
                &codex_home,
                thread_id,
                codex_home.clone(),
            ))
            .await
            .expect("thread should persist");
    }

    let first_initial = runtime
        .read_configured_identity_provenance(first_thread_id)
        .await
        .expect("first provenance should load");
    let missing_read = runtime
        .read_configured_identity_provenance(missing_thread_id)
        .await
        .expect("missing provenance should be reported");
    let missing_mark = runtime
        .mark_configured_identity_present(missing_thread_id)
        .await
        .expect("missing mutation should be reported");
    runtime
        .upsert_thread(&test_thread_metadata(
            &codex_home,
            missing_thread_id,
            codex_home.clone(),
        ))
        .await
        .expect("missing thread should later persist");
    let inserted_after_missing_mark = runtime
        .read_configured_identity_provenance(missing_thread_id)
        .await
        .expect("later insertion provenance should load");
    let marked_absent = runtime
        .mark_configured_identity_known_absent(first_thread_id)
        .await
        .expect("unknown should advance to known absent");
    let duplicate_absent = runtime
        .mark_configured_identity_known_absent(first_thread_id)
        .await
        .expect("known absent should remain known absent");
    let promoted_to_present = runtime
        .mark_configured_identity_present(first_thread_id)
        .await
        .expect("known absent should advance to present");
    let downgrade_preserved_present = runtime
        .mark_configured_identity_known_absent(first_thread_id)
        .await
        .expect("present-to-known-absent transition should be evaluated");
    let duplicate_present = runtime
        .mark_configured_identity_present(first_thread_id)
        .await
        .expect("duplicate present transition should be evaluated");
    let unknown_promoted_to_present = runtime
        .mark_configured_identity_present(second_thread_id)
        .await
        .expect("unknown should advance directly to present");

    assert_eq!(
        (
            first_initial,
            missing_read,
            missing_mark,
            inserted_after_missing_mark,
            marked_absent,
            duplicate_absent,
            promoted_to_present,
            downgrade_preserved_present,
            duplicate_present,
            unknown_promoted_to_present,
        ),
        (
            Some(ConfiguredIdentityProvenance::Unknown),
            None,
            None,
            Some(ConfiguredIdentityProvenance::Unknown),
            Some(ConfiguredIdentityProvenance::KnownAbsent),
            Some(ConfiguredIdentityProvenance::KnownAbsent),
            Some(ConfiguredIdentityProvenance::Present),
            Some(ConfiguredIdentityProvenance::Present),
            Some(ConfiguredIdentityProvenance::Present),
            Some(ConfiguredIdentityProvenance::Present),
        )
    );
}

#[tokio::test]
async fn configured_identity_provenance_competing_writers_preserve_present() {
    let codex_home = unique_temp_dir();
    let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
    let runtime_a = StateRuntime::init(sqlite.clone(), "test-provider".to_string())
        .await
        .expect("first state runtime should initialize");
    let runtime_b = StateRuntime::init(sqlite, "test-provider".to_string())
        .await
        .expect("second state runtime should initialize");
    let thread_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000805").expect("valid thread id");
    runtime_a
        .upsert_thread(&test_thread_metadata(
            &codex_home,
            thread_id,
            codex_home.clone(),
        ))
        .await
        .expect("thread should persist");
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let (absent_result, present_result) = tokio::join!(
        async {
            barrier.wait().await;
            runtime_a
                .mark_configured_identity_known_absent(thread_id)
                .await
        },
        async {
            barrier.wait().await;
            runtime_b.mark_configured_identity_present(thread_id).await
        }
    );
    let absent_result = absent_result.expect("known-absent writer should complete");
    let present_result = present_result.expect("present writer should complete");
    let final_state = runtime_a
        .read_configured_identity_provenance(thread_id)
        .await
        .expect("final provenance should load");

    assert_eq!(
        (
            matches!(
                absent_result,
                Some(
                    ConfiguredIdentityProvenance::KnownAbsent
                        | ConfiguredIdentityProvenance::Present
                )
            ),
            present_result,
            final_state,
        ),
        (
            true,
            Some(ConfiguredIdentityProvenance::Present),
            Some(ConfiguredIdentityProvenance::Present),
        )
    );
}

#[test]
fn configured_identity_provenance_rejects_invalid_values() {
    assert_eq!(
        ConfiguredIdentityProvenance::try_from(3)
            .expect_err("invalid provenance should fail")
            .to_string(),
        "invalid configured identity provenance value: 3"
    );
}

#[tokio::test]
async fn generic_thread_metadata_upsert_preserves_configured_identity_provenance() {
    let codex_home = unique_temp_dir();
    let runtime = StateRuntime::init(
        crate::SqliteConfig::new_for_testing(codex_home.as_path().abs()),
        "test-provider".to_string(),
    )
        .await
        .expect("state db should initialize");
    let thread_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000803").expect("valid thread id");
    let mut metadata = test_thread_metadata(&codex_home, thread_id, codex_home.clone());
    runtime
        .upsert_thread(&metadata)
        .await
        .expect("thread should persist");
    runtime
        .mark_configured_identity_present(thread_id)
        .await
        .expect("provenance should advance to present")
        .expect("thread should exist");

    metadata.title = "unrelated metadata update".to_string();
    runtime
        .upsert_thread(&metadata)
        .await
        .expect("generic metadata upsert should persist");
    let provenance = runtime
        .read_configured_identity_provenance(thread_id)
        .await
        .expect("provenance should load");
    let persisted = runtime
        .get_thread(thread_id)
        .await
        .expect("thread should load")
        .expect("thread should exist");

    assert_eq!(
        (provenance, persisted.title.as_str()),
        (
            Some(ConfiguredIdentityProvenance::Present),
            "unrelated metadata update",
        )
    );
}
