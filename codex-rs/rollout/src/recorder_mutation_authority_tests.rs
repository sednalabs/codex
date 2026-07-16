use std::fs;
use std::io;
use std::sync::Arc;
use std::sync::Barrier;

use tempfile::TempDir;
use tokio::task::JoinHandle;

use super::*;
use crate::RolloutMutationAuthority;
use crate::compression::compressed_rollout_path;
use crate::config::RolloutConfig;
use crate::mutation_authority::test_support;

const LEGACY_ROLLOUT: &[u8] = br#"{"timestamp":"2026-07-16T00:00:00Z","type":"session_meta","payload":{"session_id":"00000000-0000-0000-0000-000000000001","id":"00000000-0000-0000-0000-000000000001","timestamp":"2026-07-16T00:00:00Z","cwd":".","originator":"test","cli_version":"test","source":"cli"}}"#;

struct MutationGate {
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
}

impl MutationGate {
    fn install(authority: &RolloutMutationAuthority, expected_kind: RolloutMutationKind) -> Self {
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        test_support::set_after_acquire_hook(
            authority,
            Arc::new({
                let entered = Arc::clone(&entered);
                let release = Arc::clone(&release);
                move |kind| {
                    if kind == expected_kind {
                        entered.wait();
                        release.wait();
                    }
                }
            }),
        );
        Self { entered, release }
    }

    async fn wait_until_entered(&self) {
        let entered = Arc::clone(&self.entered);
        tokio::task::spawn_blocking(move || {
            entered.wait();
        })
        .await
        .expect("mutation gate wait should finish");
    }

    fn release(self) {
        self.release.wait();
    }
}

fn test_config(codex_home: &std::path::Path) -> RolloutConfig {
    RolloutConfig {
        codex_home: codex_home.to_path_buf(),
        sqlite_home: codex_home.to_path_buf(),
        cwd: codex_home.to_path_buf(),
        model_provider_id: "test-provider".to_string(),
        generate_memories: true,
    }
}

fn write_rollout(path: &std::path::Path) -> io::Result<Vec<u8>> {
    fs::write(path, LEGACY_ROLLOUT)?;
    Ok(LEGACY_ROLLOUT.to_vec())
}

fn compress_rollout(path: &std::path::Path) -> io::Result<std::path::PathBuf> {
    let compressed_path = compressed_rollout_path(path);
    let input = fs::File::open(path)?;
    let output = fs::File::create(&compressed_path)?;
    let mut encoder = zstd::stream::write::Encoder::new(output, 3)?;
    io::copy(&mut io::BufReader::new(input), &mut encoder)?;
    encoder.finish()?;
    fs::remove_file(path)?;
    Ok(compressed_path)
}

fn spawn_resume(
    config: RolloutConfig,
    path: std::path::PathBuf,
    authority: RolloutMutationAuthority,
) -> JoinHandle<io::Result<RolloutRecorder>> {
    tokio::spawn(async move {
        RolloutRecorder::resume_with_mutation_authority(&config, path, authority).await
    })
}

async fn cancel_and_begin_revocation<T>(
    task: JoinHandle<T>,
    authority: &RolloutMutationAuthority,
) -> JoinHandle<()> {
    task.abort();
    assert!(matches!(task.await, Err(error) if error.is_cancelled()));
    let revoke_task = tokio::spawn({
        let authority = authority.clone();
        async move { authority.revoke().await }
    });
    while !test_support::is_revoked(authority) {
        tokio::task::yield_now().await;
    }
    assert!(
        !revoke_task.is_finished(),
        "revocation must wait while detached mutation custody is held"
    );
    revoke_task
}

#[tokio::test]
async fn cancelled_compressed_resume_keeps_materialization_in_revocation_custody()
-> anyhow::Result<()> {
    let home = TempDir::new()?;
    let rollout_path = home.path().join("rollout.jsonl");
    let original = write_rollout(&rollout_path)?;
    let compressed_path = compress_rollout(&rollout_path)?;
    let authority = RolloutMutationAuthority::new();
    let gate = MutationGate::install(
        &authority,
        RolloutMutationKind::RepresentationMaterialization,
    );
    let constructor = spawn_resume(test_config(home.path()), compressed_path, authority.clone());

    gate.wait_until_entered().await;
    let revoke_task = cancel_and_begin_revocation(constructor, &authority).await;
    assert!(!rollout_path.exists());
    gate.release();
    revoke_task.await?;

    assert_eq!(fs::read(&rollout_path)?, original);
    assert!(!compressed_rollout_path(&rollout_path).exists());
    Ok(())
}

#[tokio::test]
async fn cancelled_plain_resume_keeps_open_and_tail_repair_in_revocation_custody()
-> anyhow::Result<()> {
    let home = TempDir::new()?;
    let rollout_path = home.path().join("rollout.jsonl");
    let mut expected = write_rollout(&rollout_path)?;
    expected.push(b'\n');
    let authority = RolloutMutationAuthority::new();
    let gate = MutationGate::install(&authority, RolloutMutationKind::AppendOpen);
    let constructor = spawn_resume(
        test_config(home.path()),
        rollout_path.clone(),
        authority.clone(),
    );

    gate.wait_until_entered().await;
    let revoke_task = cancel_and_begin_revocation(constructor, &authority).await;
    gate.release();
    revoke_task.await?;

    assert_eq!(fs::read(&rollout_path)?, expected);
    Ok(())
}

#[tokio::test]
async fn cancelled_recovery_open_releases_custody_before_revocation_returns() -> anyhow::Result<()>
{
    let home = TempDir::new()?;
    let rollout_path = home.path().join("rollout.jsonl");
    let mut expected = write_rollout(&rollout_path)?;
    expected.push(b'\n');
    let authority = RolloutMutationAuthority::new();
    let mutation_policy = RolloutMutationPolicy::Revocable(authority.clone());
    let gate = MutationGate::install(&authority, RolloutMutationKind::AppendOpen);
    let mut state = RolloutWriterState {
        writer: None,
        deferred_log_file_info: None,
        pending_items: Vec::new(),
        meta: None,
        cwd: home.path().to_path_buf(),
        rollout_path: rollout_path.clone(),
        ordinal_state: RolloutOrdinalState::Legacy,
        last_logged_error: None,
        mutation_policy: mutation_policy.clone(),
    };
    let recovery = tokio::spawn(async move { state.ensure_writer_open().await });

    gate.wait_until_entered().await;
    let revoke_task = cancel_and_begin_revocation(recovery, &authority).await;
    gate.release();
    revoke_task.await?;
    assert_eq!(fs::read(&rollout_path)?, expected);

    let missing_path = home.path().join("must-not-be-created.jsonl");
    let attempted_path = missing_path.clone();
    let denied = tokio::task::spawn_blocking(move || {
        open_log_file(attempted_path.as_path(), &mutation_policy)
    })
    .await?;
    assert_eq!(
        denied
            .expect_err("revoked recovery open should fail")
            .kind(),
        io::ErrorKind::Other
    );
    assert!(!missing_path.exists());
    Ok(())
}
