use super::*;
use crate::config::Config;
use crate::config::test_config;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_state::Phase2InputSelection;
use codex_state::Stage1Output;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

fn stage1_output_with_source_updated_at(source_updated_at: i64) -> Stage1Output {
    Stage1Output {
        thread_id: ThreadId::new(),
        source_updated_at: chrono::DateTime::<Utc>::from_timestamp(source_updated_at, 0)
            .expect("valid source_updated_at timestamp"),
        raw_memory: "raw memory".to_string(),
        rollout_summary: "rollout summary".to_string(),
        rollout_slug: None,
        rollout_path: PathBuf::from("/tmp/rollout-summary.jsonl"),
        cwd: PathBuf::from("/tmp/workspace"),
        git_branch: None,
        generated_at: chrono::DateTime::<Utc>::from_timestamp(source_updated_at + 1, 0)
            .expect("valid generated_at timestamp"),
    }
}

fn selection_for_attested_outputs(selected: Vec<Stage1Output>) -> Phase2InputSelection {
    Phase2InputSelection {
        previous_selected: selected.clone(),
        retained_thread_ids: selected.iter().map(|output| output.thread_id).collect(),
        selected,
        removed: Vec::new(),
    }
}

fn config_for_memory_root(root: &Path) -> Arc<Config> {
    let mut config = test_config();
    config.codex_home = root
        .parent()
        .expect("memory root should have a codex home parent")
        .to_path_buf();
    Arc::new(config)
}

#[tokio::test]
async fn consolidation_artifacts_ready_rejects_rollout_summary_drift_even_when_outputs_are_fresh() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let codex_home = temp_dir.path().join("codex-home");
    let root = memory_root(&codex_home);
    let config = config_for_memory_root(&root);
    let selection = selection_for_attested_outputs(Vec::new());
    let memory_index_path = root.join("MEMORY.md");
    let memory_summary_path = root.join("memory_summary.md");
    let raw_memories_path = root.join("raw_memories.md");
    let rollout_summary_path = root.join("rollout_summaries/demo.md");

    tokio::fs::create_dir_all(
        rollout_summary_path
            .parent()
            .expect("rollout summary parent should exist"),
    )
    .await
    .expect("create rollout summaries dir");
    tokio::fs::write(&memory_index_path, "memory index\n")
        .await
        .expect("write memory index");
    tokio::fs::write(&memory_summary_path, "memory summary\n")
        .await
        .expect("write memory summary");
    tokio::fs::write(&raw_memories_path, "# Raw Memories\n\ntrusted raw memories\n")
        .await
        .expect("write raw memories");
    tokio::fs::write(&rollout_summary_path, "trusted rollout summary\n")
        .await
        .expect("write rollout summary");

    let expected_prepared_input_tree = test_prepared_input_artifact_tree_sha256(&root)
        .expect("fingerprint prepared immutable inputs");

    tokio::fs::write(&rollout_summary_path, "tampered rollout summary\n")
        .await
        .expect("tamper rollout summary");
    tokio::fs::write(&memory_index_path, "fresh memory index\n")
        .await
        .expect("refresh memory index");
    tokio::fs::write(&memory_summary_path, "fresh memory summary\n")
        .await
        .expect("refresh memory summary");

    assert!(
        !agent::consolidation_artifacts_ready_with_expected_supporting_tree(
            &root,
            &config,
            std::time::SystemTime::UNIX_EPOCH,
            Some(expected_prepared_input_tree.as_str()),
            false,
            &selection,
        )
        .await,
        "fresh outputs should still fail closed when rollout summaries drift before validation"
    );
}

#[tokio::test]
async fn consolidation_artifacts_ready_rejects_missing_attestation_after_db_requirement_initialized_even_when_support_marker_is_deleted() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let codex_home = temp_dir.path().join("codex-home");
    let root = memory_root(&codex_home);
    let config = config_for_memory_root(&root);
    let state_db = codex_state::StateRuntime::init(
        temp_dir.path().join("sqlite-home"),
        config.model_provider_id.clone(),
    )
    .await
    .expect("initialize state db");
    let memory_index_path = root.join("MEMORY.md");
    let memory_summary_path = root.join("memory_summary.md");

    tokio::fs::create_dir_all(&root)
        .await
        .expect("create memory root");
    tokio::fs::write(&memory_index_path, "memory index\n")
        .await
        .expect("write memory index");
    tokio::fs::write(&memory_summary_path, "memory summary\n")
        .await
        .expect("write memory summary");

    let selected_outputs = vec![stage1_output_with_source_updated_at(200)];
    let selection = selection_for_attested_outputs(selected_outputs);
    let expected_prepared_input_tree = test_prepared_input_artifact_tree_sha256(&root)
        .expect("prepared input tree hash");

    test_write_consolidation_artifact_attestation_with_state_db(
        Arc::clone(&config),
        &root,
        &selection,
        state_db.as_ref(),
    )
    .await
    .expect("write attestation and persist db requirement state");

    let attestation_path = test_consolidation_artifact_attestation_path(&root)
        .expect("attestation path");
    tokio::fs::remove_file(attestation_path)
        .await
        .expect("remove attestation");
    let support_path = test_consolidation_artifact_attestation_support_path(&root)
        .expect("attestation support path");
    tokio::fs::remove_file(support_path)
        .await
        .expect("remove attestation support marker");

    assert!(
        !test_consolidation_artifacts_ready_with_state_db_and_expected_prepared_input_tree(
            &root,
            &config,
            state_db.as_ref(),
            std::time::SystemTime::now() + Duration::from_secs(60),
            Some(expected_prepared_input_tree.as_str()),
            true,
            &selection,
        )
        .await,
        "once durable attestation state is initialized, deleting both sidecars must not reopen bootstrap reuse"
    );
}
