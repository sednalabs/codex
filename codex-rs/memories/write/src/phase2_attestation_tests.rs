use super::phase2_attestation;
use crate::workspace_diff;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

#[tokio::test]
async fn output_tree_hash_excludes_git_and_generated_workspace_diff() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let root = temp_dir.path();

    tokio::fs::create_dir_all(root.join(".git")).await?;
    tokio::fs::write(root.join("MEMORY.md"), "memory index\n").await?;
    tokio::fs::write(root.join("memory_summary.md"), "v1\n\nsummary\n").await?;
    tokio::fs::write(root.join(workspace_diff::FILENAME), "diff one\n").await?;
    tokio::fs::write(root.join(".git/ignored"), "git state one\n").await?;

    let first = phase2_attestation::current_output_tree_sha256(root).await?;
    tokio::fs::write(root.join(workspace_diff::FILENAME), "diff two\n").await?;
    tokio::fs::write(root.join(".git/ignored"), "git state two\n").await?;
    let second = phase2_attestation::current_output_tree_sha256(root).await?;

    assert_eq!(
        first, second,
        "output-tree attestations should ignore git internals and the generated diff handoff"
    );
    Ok(())
}

#[tokio::test]
async fn completed_run_requires_memory_summary_schema_line() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let root = temp_dir.path();
    let root_key = phase2_attestation::memory_root_key(root).await?;
    tokio::fs::write(root.join("MEMORY.md"), "memory index\n").await?;
    tokio::fs::write(root.join("memory_summary.md"), "summary without schema\n").await?;

    let context = phase2_attestation::Phase2AttestationContext {
        memory_root_key: root_key,
        selection_sha256: "selection".to_string(),
        prepared_inputs_sha256: "prepared".to_string(),
        consolidator_sha256: "consolidator".to_string(),
        selected_count: 0,
    };
    let err = phase2_attestation::validate_completed_run(root, &context)
        .await
        .expect_err("schema-less memory summary should be rejected");

    assert!(
        err.to_string()
            .contains("memory_summary.md must start with schema line"),
        "unexpected validation error: {err:?}"
    );
    Ok(())
}

#[tokio::test]
async fn completed_run_rejects_symlinks_in_attested_tree() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let root = temp_dir.path();
    tokio::fs::write(root.join("MEMORY.md"), "memory index\n").await?;
    tokio::fs::write(root.join("memory_summary.md"), "v1\n\nsummary\n").await?;

    #[cfg(unix)]
    {
        let root_key = phase2_attestation::memory_root_key(root).await?;
        std::os::unix::fs::symlink("MEMORY.md", root.join("linked-memory.md"))?;
        let context = phase2_attestation::Phase2AttestationContext {
            memory_root_key: root_key,
            selection_sha256: "selection".to_string(),
            prepared_inputs_sha256: "prepared".to_string(),
            consolidator_sha256: "consolidator".to_string(),
            selected_count: 0,
        };
        let err = phase2_attestation::validate_completed_run(root, &context)
            .await
            .expect_err("symlinked output should be rejected");
        assert!(
            err.to_string().contains("refusing to attest symlink"),
            "unexpected validation error: {err:?}"
        );
    }

    Ok(())
}

#[tokio::test]
async fn prepared_input_tree_hash_includes_generated_workspace_diff() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let root = temp_dir.path().join("memories");
    tokio::fs::create_dir_all(&root).await?;
    tokio::fs::write(root.join("raw_memories.md"), "raw\n").await?;
    tokio::fs::write(root.join(workspace_diff::FILENAME), "diff one\n").await?;

    let first = phase2_attestation::prepared_inputs_tree_sha256_for_tests(&root).await?;
    tokio::fs::write(root.join(workspace_diff::FILENAME), "diff two\n").await?;
    let second = phase2_attestation::prepared_inputs_tree_sha256_for_tests(&root).await?;

    assert_ne!(
        first, second,
        "prepared-input attestations must include the agent-facing diff"
    );
    Ok(())
}
