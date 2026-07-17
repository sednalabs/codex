use super::ConfiguredIdentityProvenance;
use super::StateRuntime;
use crate::runtime::test_support::test_thread_metadata;
use crate::runtime::test_support::unique_temp_dir;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn configured_identity_provenance_transitions_monotonically() {
    let codex_home = unique_temp_dir();
    let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
        .await
        .expect("state db should initialize");
    let first_thread_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000801").expect("valid thread id");
    let second_thread_id =
        ThreadId::from_string("00000000-0000-0000-0000-000000000802").expect("valid thread id");
    runtime
        .upsert_thread(&test_thread_metadata(
            &codex_home,
            first_thread_id,
            codex_home.clone(),
        ))
        .await
        .expect("first thread should persist");
    runtime
        .upsert_thread(&test_thread_metadata(
            &codex_home,
            second_thread_id,
            codex_home.clone(),
        ))
        .await
        .expect("second thread should persist");

    let first_initial = runtime
        .read_configured_identity_provenance(first_thread_id)
        .await
        .expect("first provenance should load");
    let second_initial = runtime
        .read_configured_identity_provenance(second_thread_id)
        .await
        .expect("second provenance should load");
    let marked_absent = runtime
        .mark_configured_identity_known_absent(first_thread_id)
        .await
        .expect("unknown should advance to known absent");
    let after_absent = runtime
        .read_configured_identity_provenance(first_thread_id)
        .await
        .expect("known-absent provenance should load");
    let promoted_to_present = runtime
        .mark_configured_identity_present(first_thread_id)
        .await
        .expect("known absent should advance to present");
    let downgrade_rejected = runtime
        .mark_configured_identity_known_absent(first_thread_id)
        .await
        .expect("present-to-known-absent transition should be evaluated");
    let duplicate_present_rejected = runtime
        .mark_configured_identity_present(first_thread_id)
        .await
        .expect("duplicate present transition should be evaluated");
    let first_final = runtime
        .read_configured_identity_provenance(first_thread_id)
        .await
        .expect("final first provenance should load");
    let unknown_promoted_to_present = runtime
        .mark_configured_identity_present(second_thread_id)
        .await
        .expect("unknown should advance directly to present");
    let second_final = runtime
        .read_configured_identity_provenance(second_thread_id)
        .await
        .expect("final second provenance should load");

    assert_eq!(
        (
            first_initial,
            second_initial,
            marked_absent,
            after_absent,
            promoted_to_present,
            downgrade_rejected,
            duplicate_present_rejected,
            first_final,
            unknown_promoted_to_present,
            second_final,
        ),
        (
            Some(ConfiguredIdentityProvenance::Unknown),
            Some(ConfiguredIdentityProvenance::Unknown),
            true,
            Some(ConfiguredIdentityProvenance::KnownAbsent),
            true,
            false,
            false,
            Some(ConfiguredIdentityProvenance::Present),
            true,
            Some(ConfiguredIdentityProvenance::Present),
        )
    );
}

#[tokio::test]
async fn generic_thread_metadata_upsert_preserves_configured_identity_provenance() {
    let codex_home = unique_temp_dir();
    let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
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
        .expect("provenance should advance to present");

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
