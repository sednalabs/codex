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

const DEADLINE: Duration = Duration::from_secs(10);

fn config(home: &Path) -> RolloutConfig {
    RolloutConfig {
        codex_home: home.to_path_buf(),
        sqlite_home: home.to_path_buf(),
        cwd: home.to_path_buf(),
        model_provider_id: "test-provider".to_string(),
        generate_memories: true,
    }
}

async fn recorder(home: &TempDir) -> std::io::Result<RolloutRecorder> {
    RolloutRecorder::new(
        &config(home.path()),
        RolloutRecorderParams::new(
            ThreadId::new(),
            None,
            None,
            SessionSource::Exec,
            None,
            "test-originator".to_string(),
            BaseInstructions::default(),
            Vec::new(),
        ),
    )
    .await
}

fn item(text: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::AgentMessage(AgentMessageEvent {
        message: text.to_string(),
        phase: None,
        memory_citation: None,
    }))
}

async fn bounded<T>(future: impl Future<Output = std::io::Result<T>>) -> anyhow::Result<T> {
    tokio::time::timeout(DEADLINE, future)
        .await
        .map_err(|_| anyhow::anyhow!("rollout operation exceeded diagnostic deadline"))?
        .map_err(Into::into)
}

async fn wait_for_lifecycle(
    recorder: &RolloutRecorder,
    expected: RolloutWriterLifecycle,
) -> anyhow::Result<()> {
    let mut lifecycle = recorder.writer_task.lifecycle.subscribe();
    tokio::time::timeout(DEADLINE, async {
        while *lifecycle.borrow_and_update() != expected {
            lifecycle.changed().await.expect("writer owns lifecycle");
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("writer did not reach {expected:?}"))
}

fn assert_terminal(error: IoError) {
    assert_eq!(error.kind(), std::io::ErrorKind::Other);
    assert_eq!(error.to_string(), REVOKED_RECORDER_MESSAGE);
}

fn serialized(items: &[RolloutItem]) -> serde_json::Value {
    serde_json::to_value(items).expect("rollout items serialize")
}

#[tokio::test]
async fn cancelled_shutdown_is_joined_by_revoke_and_orders_item_admission() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let recorder = recorder(&home).await?;
    let path = recorder.rollout_path().to_path_buf();
    let prefix = vec![item("accepted-before-revoke")];
    recorder.record_canonical_items(&prefix).await?;
    let custody = recorder.mutation_authority.admit().expect("hold custody");

    let initiator = recorder.clone();
    let revocation = tokio::spawn(async move { initiator.shutdown().await });
    wait_for_lifecycle(&recorder, RolloutWriterLifecycle::Revoking).await?;
    assert_eq!(
        recorder
            .record_canonical_items(&[item("rejected-after-revoke")])
            .await
            .expect_err("revoking recorder must reject later items")
            .to_string(),
        REVOKING_RECORDER_MESSAGE
    );
    revocation.abort();
    let aborted = revocation.await.expect_err("initiator must be cancelled");
    assert!(aborted.is_cancelled());
    drop(custody);
    bounded(recorder.revoke()).await?;

    let lifecycle = recorder.writer_task.lifecycle();
    assert_eq!(lifecycle, RolloutWriterLifecycle::Revoked);
    assert_eq!(
        *recorder.writer_task.join_state.borrow(),
        RolloutWriterJoinState::Succeeded
    );
    let (persisted, _, parse_errors) = RolloutRecorder::load_rollout_items(&path).await?;
    assert_eq!(parse_errors, 0);
    assert_eq!(serialized(&persisted[1..]), serialized(&prefix));
    let stable_bytes = std::fs::read(&path)?;
    for attempt in 0..2 {
        assert_terminal(
            recorder
                .record_canonical_items(&[item(&format!("terminal-{attempt}"))])
                .await
                .expect_err("terminal items must fail"),
        );
        for result in [
            recorder.persist().await,
            recorder.flush().await,
            recorder.shutdown().await,
        ] {
            assert_terminal(result.expect_err("terminal command must fail"));
        }
    }
    assert_eq!(std::fs::read(&path)?, stable_bytes);
    std::fs::rename(&path, path.with_extension("retired"))?;
    Ok(())
}

#[tokio::test]
async fn failed_revoke_reopens_admission_and_retry_persists_the_exact_suffix() -> anyhow::Result<()>
{
    let home = TempDir::new()?;
    let recorder = recorder(&home).await?;
    let path = recorder.rollout_path().to_path_buf();
    let expected = vec![item("before-failed-revoke"), item("after-failed-revoke")];
    recorder.record_canonical_items(&expected[..1]).await?;
    let blocker = home.path().join(SESSIONS_SUBDIR);
    std::fs::File::create(&blocker)?;

    bounded(recorder.revoke())
        .await
        .expect_err("blocked session directory must fail revocation");
    let lifecycle = recorder.writer_task.lifecycle();
    assert_eq!(lifecycle, RolloutWriterLifecycle::Active);
    recorder.record_canonical_items(&expected[1..]).await?;
    std::fs::remove_file(blocker)?;
    bounded(recorder.revoke()).await?;

    let (persisted, _, parse_errors) = RolloutRecorder::load_rollout_items(&path).await?;
    assert_eq!(parse_errors, 0);
    assert_eq!(serialized(&persisted[1..]), serialized(&expected));
    let lifecycle = recorder.writer_task.lifecycle();
    assert_eq!(lifecycle, RolloutWriterLifecycle::Revoked);
    Ok(())
}

#[tokio::test]
async fn empty_deferred_revoke_is_terminal_without_materializing_history() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let recorder = recorder(&home).await?;
    let path = recorder.rollout_path().to_path_buf();

    bounded(recorder.revoke()).await?;
    assert!(!path.exists());
    assert!(!home.path().join(SESSIONS_SUBDIR).exists());
    assert_eq!(
        *recorder.writer_task.join_state.borrow(),
        RolloutWriterJoinState::Succeeded
    );
    assert_terminal(
        recorder
            .record_canonical_items(&[item("never-admitted")])
            .await
            .expect_err("empty revoked recorder must stay terminal"),
    );
    Ok(())
}
