#![allow(warnings, clippy::all)]

use super::*;
use crate::config::RolloutConfig;
use codex_protocol::models::BaseInstructions;
use codex_protocol::protocol::AgentMessageEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use pretty_assertions::assert_eq;
use std::future::Future;
use std::time::Duration;
use tempfile::TempDir;

const TEST_DEADLINE: Duration = Duration::from_secs(10);

fn test_config(codex_home: &Path) -> RolloutConfig {
    RolloutConfig {
        codex_home: codex_home.to_path_buf(),
        sqlite_home: codex_home.to_path_buf(),
        cwd: codex_home.to_path_buf(),
        model_provider_id: "test-provider".to_string(),
        generate_memories: true,
    }
}

fn message(text: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::AgentMessage(AgentMessageEvent {
        message: text.to_string(),
        phase: None,
        memory_citation: None,
    }))
}

async fn within_deadline<T>(
    context: &str,
    future: impl Future<Output = std::io::Result<T>>,
) -> anyhow::Result<T> {
    tokio::time::timeout(TEST_DEADLINE, future)
        .await
        .map_err(|_| anyhow::anyhow!("timed out {context}"))?
        .map_err(Into::into)
}

fn assert_revoked(error: IoError) {
    assert_eq!(error.kind(), std::io::ErrorKind::Other);
    assert_eq!(error.to_string(), REVOKED_RECORDER_MESSAGE);
}

fn writer_state(
    home: &TempDir,
    pending_items: Vec<RolloutItem>,
    ordinal_state: RolloutOrdinalState,
) -> std::io::Result<(RolloutWriterState, RolloutMutationAuthority, PathBuf)> {
    let rollout_path = home.path().join("rollout.jsonl");
    let file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&rollout_path)?;
    let authority = RolloutMutationAuthority::new();
    Ok((
        RolloutWriterState {
            writer: Some(JsonlWriter {
                file: tokio::fs::File::from_std(file),
            }),
            deferred_log_file_info: None,
            pending_items,
            meta: None,
            cwd: home.path().to_path_buf(),
            rollout_path: rollout_path.clone(),
            ordinal_state,
            last_logged_error: None,
            mutation_authority: authority.clone(),
            lifecycle: RolloutWriterLifecycle::Active,
        },
        authority,
        rollout_path,
    ))
}

#[tokio::test]
async fn revoke_persists_exact_admitted_suffix_and_rejects_every_later_command()
-> anyhow::Result<()> {
    let home = TempDir::new()?;
    let recorder = RolloutRecorder::new(
        &test_config(home.path()),
        RolloutRecorderParams::new(
            ThreadId::new(),
            /*forked_from_id*/ None,
            /*parent_thread_id*/ None,
            SessionSource::Exec,
            /*thread_source*/ None,
            "test-originator".to_string(),
            BaseInstructions::default(),
            Vec::new(),
        ),
    )
    .await?;
    let rollout_path = recorder.rollout_path().to_path_buf();
    let admitted_suffix = vec![message("first-before-revoke"), message("second-before-revoke")];

    recorder.record_canonical_items(&admitted_suffix).await?;
    assert!(!rollout_path.exists(), "recording alone must remain deferred");
    within_deadline("revoking the recorder", recorder.revoke()).await?;

    let (persisted, _, parse_errors) = RolloutRecorder::load_rollout_items(&rollout_path).await?;
    assert_eq!(parse_errors, 0);
    assert_eq!(persisted[1..], admitted_suffix);
    let stable_bytes = std::fs::read(&rollout_path)?;
    assert!(
        recorder
            .writer_task
            .handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_none(),
        "revoke must join and retire the actor handle"
    );

    for attempt in 0..2 {
        assert_revoked(
            recorder
                .record_canonical_items(&[message(&format!("after-revoke-{attempt}"))])
                .await
                .expect_err("post-revocation items must be rejected"),
        );
        assert_revoked(recorder.persist().await.expect_err("revoked persist must fail"));
        assert_revoked(recorder.flush().await.expect_err("revoked flush must fail"));
        assert_revoked(recorder.shutdown().await.expect_err("revoked shutdown must fail"));
    }
    assert_eq!(std::fs::read(&rollout_path)?, stable_bytes);
    Ok(())
}

#[tokio::test]
async fn writer_state_revoke_is_terminal_lossless_and_releases_the_file() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let exact_suffix = vec![message("state-first"), message("state-second")];
    let (mut state, authority, rollout_path) =
        writer_state(&home, exact_suffix.clone(), RolloutOrdinalState::Legacy)?;

    within_deadline("revoking writer state", state.revoke()).await?;
    assert_eq!(state.lifecycle, RolloutWriterLifecycle::Revoked);
    assert!(state.writer.is_none(), "file must close before revoke returns");
    assert!(state.pending_items.is_empty());
    assert_eq!(
        read_rollout_items(&rollout_path)?,
        exact_suffix,
        "the complete pending suffix must become the exact persisted prefix"
    );
    assert!(matches!(
        authority.admit(),
        Err(MutationAdmissionError::AdmissionClosed)
    ));
    let stable_bytes = std::fs::read(&rollout_path)?;

    assert_revoked(
        state
            .add_items(vec![message("later")])
            .expect_err("terminal state must reject items"),
    );
    assert_revoked(state.persist().await.expect_err("terminal persist must fail"));
    assert_revoked(state.flush().await.expect_err("terminal flush must fail"));
    assert_revoked(
        state
            .shutdown()
            .await
            .expect_err("terminal shutdown must fail"),
    );
    assert_eq!(state.lifecycle, RolloutWriterLifecycle::Revoked);
    assert!(state.writer.is_none());
    assert!(state.pending_items.is_empty());
    assert_eq!(std::fs::read(&rollout_path)?, stable_bytes);
    Ok(())
}

#[tokio::test]
async fn failed_revoke_preserves_the_exact_suffix_and_keeps_authority_active()
-> anyhow::Result<()> {
    let home = TempDir::new()?;
    let exact_suffix = vec![message("must-remain-pending")];
    let (mut state, authority, rollout_path) = writer_state(
        &home,
        exact_suffix.clone(),
        RolloutOrdinalState::Paginated { next: None },
    )?;
    let stable_bytes = std::fs::read(&rollout_path)?;

    let error = tokio::time::timeout(TEST_DEADLINE, state.revoke())
        .await
        .map_err(|_| anyhow::anyhow!("timed out rejecting revocation"))?
        .expect_err("ordinal failure must prevent terminal revocation");
    assert!(error.to_string().contains("ordinal overflow"));
    assert_eq!(state.lifecycle, RolloutWriterLifecycle::Active);
    assert_eq!(state.pending_items, exact_suffix);
    assert!(state.writer.is_none(), "failed drain must enter recovery mode");
    assert_eq!(std::fs::read(&rollout_path)?, stable_bytes);
    drop(authority.admit().expect("failed revoke must leave admission open"));
    Ok(())
}

fn read_rollout_items(path: &Path) -> std::io::Result<Vec<RolloutItem>> {
    std::fs::read_to_string(path)?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<RolloutLine>(line)
                .map(|line| line.item)
                .map_err(IoError::other)
        })
        .collect()
}
