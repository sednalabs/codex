use std::fs;
use std::fs::FileTimes;
use std::future::Future;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::mpsc;
use std::task::Poll;
use std::time::Duration;
use std::time::SystemTime;

use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::UserMessageEvent;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::sync::oneshot;
use uuid::Uuid;

use super::*;
use crate::RolloutConfig;
use crate::RolloutRecorder;
use crate::RolloutRecorderParams;
use crate::append_rollout_item_to_path;
use crate::mutation_authority::MutationAdmissionError;
use crate::mutation_authority::RolloutMutationAuthority;
use crate::search_rollout_matches;

const MATERIALIZATION_DIAGNOSTIC_DEADLINE: Duration = Duration::from_secs(10);

#[derive(Debug, PartialEq, Eq)]
struct RolloutRepresentationSnapshot {
    entries: Vec<(PathBuf, Vec<u8>)>,
}

struct BlockingBoundary {
    entered: oneshot::Sender<()>,
    release: mpsc::Receiver<()>,
}

impl BlockingBoundary {
    fn enter(self) {
        let _ = self.entered.send(());
        let _ = self.release.recv();
    }
}

struct BoundaryControl {
    name: &'static str,
    entered: Option<oneshot::Receiver<()>>,
    release: Option<mpsc::Sender<()>>,
}

impl BoundaryControl {
    async fn wait_until_entered(&mut self) -> anyhow::Result<()> {
        let entered = self
            .entered
            .take()
            .ok_or_else(|| anyhow::anyhow!("{} boundary awaited once", self.name))?;
        with_materialization_deadline(self.name, entered).await??;
        Ok(())
    }

    fn release(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}

impl Drop for BoundaryControl {
    fn drop(&mut self) {
        self.release();
    }
}

struct MaterializationControl {
    before_mutation: BoundaryControl,
    after_mutation_before_release: BoundaryControl,
}

fn boundary_pair(name: &'static str) -> (BoundaryControl, BlockingBoundary) {
    let (entered_tx, entered) = oneshot::channel();
    let (release, release_rx) = mpsc::channel();
    (
        BoundaryControl {
            name,
            entered: Some(entered),
            release: Some(release),
        },
        BlockingBoundary {
            entered: entered_tx,
            release: release_rx,
        },
    )
}

fn start_controlled_materialization(
    path: PathBuf,
    authority: RolloutMutationAuthority,
) -> (
    tokio::task::JoinHandle<std::io::Result<PathBuf>>,
    MaterializationControl,
) {
    let (before_control, before_boundary) = boundary_pair("waiting before rollout mutation");
    let (after_control, after_boundary) =
        boundary_pair("waiting after rollout mutation before custody release");
    let materialization = tokio::spawn(async move {
        materialize_rollout_for_append_with_authority(
            path.as_path(),
            authority,
            move || before_boundary.enter(),
            move || after_boundary.enter(),
        )
        .await
    });
    (
        materialization,
        MaterializationControl {
            before_mutation: before_control,
            after_mutation_before_release: after_control,
        },
    )
}

async fn with_materialization_deadline<T>(
    context: &'static str,
    future: impl Future<Output = T>,
) -> anyhow::Result<T> {
    match tokio::time::timeout(MATERIALIZATION_DIAGNOSTIC_DEADLINE, future).await {
        Ok(output) => Ok(output),
        Err(_) => anyhow::bail!("timed out while {context}"),
    }
}

async fn is_pending<F>(mut future: Pin<&mut F>) -> bool
where
    F: Future<Output = ()> + ?Sized,
{
    std::future::poll_fn(move |context| {
        Poll::Ready(matches!(future.as_mut().poll(context), Poll::Pending))
    })
    .await
}

async fn cancel_outer_materialization(
    materialization: tokio::task::JoinHandle<std::io::Result<PathBuf>>,
) -> anyhow::Result<()> {
    materialization.abort();
    match with_materialization_deadline("cancelling the outer materialization", materialization)
        .await?
    {
        Err(error) if error.is_cancelled() => Ok(()),
        Err(error) => anyhow::bail!("outer materialization failed instead of cancelling: {error}"),
        Ok(result) => {
            anyhow::bail!("outer materialization completed before cancellation: {result:?}")
        }
    }
}

fn rollout_representation_snapshot(
    directory: &Path,
) -> anyhow::Result<RolloutRepresentationSnapshot> {
    let mut entries = fs::read_dir(directory)?
        .map(|entry| -> anyhow::Result<(PathBuf, Vec<u8>)> {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                anyhow::bail!(
                    "unexpected non-file in rollout representation: {}",
                    entry.path().display()
                );
            }
            Ok((PathBuf::from(entry.file_name()), fs::read(entry.path())?))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(RolloutRepresentationSnapshot { entries })
}

async fn assert_cancelled_materialization_holds_custody(
    rollout_path: &Path,
    authority: RolloutMutationAuthority,
    expected_snapshot: RolloutRepresentationSnapshot,
) -> anyhow::Result<()> {
    let directory = rollout_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("rollout path should have a parent"))?;
    let (materialization, mut control) =
        start_controlled_materialization(rollout_path.to_path_buf(), authority.clone());
    control.before_mutation.wait_until_entered().await?;

    let mut revoke = Box::pin(authority.revoke());
    assert!(is_pending(revoke.as_mut()).await);
    assert!(matches!(
        authority.admit(),
        Err(MutationAdmissionError::AdmissionClosed)
    ));
    cancel_outer_materialization(materialization).await?;
    assert!(is_pending(revoke.as_mut()).await);

    control.before_mutation.release();
    control
        .after_mutation_before_release
        .wait_until_entered()
        .await?;
    assert_eq!(
        rollout_representation_snapshot(directory)?,
        expected_snapshot
    );
    assert!(is_pending(revoke.as_mut()).await);

    control.after_mutation_before_release.release();
    with_materialization_deadline("waiting for materialization custody release", revoke).await?;
    assert_eq!(
        rollout_representation_snapshot(directory)?,
        expected_snapshot
    );
    assert!(matches!(
        authority.admit(),
        Err(MutationAdmissionError::AdmissionClosed)
    ));
    Ok(())
}

#[tokio::test]
async fn compressed_materialization_custody_survives_caller_cancellation_through_success()
-> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(16);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "custodied materialization")?;
    let expected_plain_bytes = fs::read(&rollout_path)?;
    compress_now(&rollout_path)?;
    let expected_snapshot = RolloutRepresentationSnapshot {
        entries: vec![(
            PathBuf::from(
                rollout_path
                    .file_name()
                    .ok_or_else(|| anyhow::anyhow!("rollout path should have a file name"))?,
            ),
            expected_plain_bytes,
        )],
    };

    assert_cancelled_materialization_holds_custody(
        &rollout_path,
        RolloutMutationAuthority::new(),
        expected_snapshot,
    )
    .await
}

#[tokio::test]
async fn compressed_materialization_custody_survives_caller_cancellation_through_corrupt_zstd()
-> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(17);
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    let parent = rollout_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("rollout path should have a parent"))?;
    fs::create_dir_all(parent)?;
    fs::write(
        compressed_rollout_path(&rollout_path),
        [0x28, 0xb5, 0x2f, 0xfd],
    )?;
    let expected_snapshot = rollout_representation_snapshot(parent)?;

    assert_cancelled_materialization_holds_custody(
        &rollout_path,
        RolloutMutationAuthority::new(),
        expected_snapshot,
    )
    .await
}

#[tokio::test]
async fn load_rollout_items_reads_compressed_rollout() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(1);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "hello compressed")?;
    compress_now(&rollout_path)?;

    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;

    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 2);
    assert!(!rollout_path.exists());
    assert!(compressed_rollout_path(&rollout_path).exists());
    Ok(())
}

#[test]
fn rollout_file_from_path_normalizes_compressed_file_names() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(7);
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    let compressed_path = compressed_rollout_path(&rollout_path);

    assert_eq!(
        RolloutFile::from_path(compressed_path.clone()),
        Some(RolloutFile {
            path: compressed_path,
            plain_file_name: format!("rollout-2025-01-03T12-00-00-{uuid}.jsonl"),
        })
    );
    Ok(())
}

#[test]
fn rollout_file_from_path_hides_compressed_sibling_when_plain_exists() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(8);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "plain wins")?;

    assert_eq!(
        RolloutFile::from_path(compressed_rollout_path(&rollout_path)),
        None
    );
    Ok(())
}

#[tokio::test]
async fn append_rollout_item_materializes_compressed_rollout() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(2);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "hello before append")?;
    compress_now(&rollout_path)?;

    append_rollout_item_to_path(
        &rollout_path,
        &RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
            message: "hello after append".to_string(),
            ..Default::default()
        })),
    )
    .await?;

    assert!(rollout_path.exists());
    assert!(!compressed_rollout_path(&rollout_path).exists());
    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;
    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 3);
    Ok(())
}

#[tokio::test]
async fn search_rollout_matches_uses_logical_path_for_compressed_rollout() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(15);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "targeted search term")?;
    compress_now(&rollout_path)?;

    let matches = search_rollout_matches(
        std::path::Path::new("missing-rg-for-test"),
        home.path(),
        /*archived*/ false,
        "search term",
    )
    .await?;

    assert_eq!(
        matches.get(rollout_path.as_path()),
        Some(&Some("targeted search term".to_string()))
    );
    Ok(())
}

#[tokio::test]
async fn worker_compresses_old_active_and_archived_rollouts() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let active_uuid = Uuid::from_u128(3);
    let active_id = ThreadId::from_string(&active_uuid.to_string())?;
    let active_path = rollout_path(home.path(), "2025-01-03T12-00-00", active_uuid);
    write_rollout(&active_path, active_id, "old active")?;
    set_old_mtime(&active_path)?;

    let archived_uuid = Uuid::from_u128(4);
    let archived_id = ThreadId::from_string(&archived_uuid.to_string())?;
    let archived_path = archived_rollout_path(home.path(), "2025-01-04T12-00-00", archived_uuid);
    write_rollout(&archived_path, archived_id, "old archived")?;
    set_old_mtime(&archived_path)?;

    let fresh_uuid = Uuid::from_u128(5);
    let fresh_id = ThreadId::from_string(&fresh_uuid.to_string())?;
    let fresh_path = rollout_path(home.path(), "2025-01-05T12-00-00", fresh_uuid);
    write_rollout(&fresh_path, fresh_id, "fresh active")?;

    let stale_temp = active_path.with_file_name("rollout-stale.jsonl.zst.tmp");
    fs::write(&stale_temp, "stale temp")?;
    set_old_mtime(&stale_temp)?;

    let fresh_temp = active_path.with_file_name("rollout-fresh.jsonl.zst.tmp");
    fs::write(&fresh_temp, "fresh temp")?;

    worker::run(home.path().to_path_buf()).await?;

    assert!(!active_path.exists());
    assert!(compressed_rollout_path(&active_path).exists());
    assert!(!archived_path.exists());
    assert!(compressed_rollout_path(&archived_path).exists());
    assert!(fresh_path.exists());
    assert!(!compressed_rollout_path(&fresh_path).exists());
    assert!(!stale_temp.exists());
    assert!(fresh_temp.exists());
    assert!(
        home.path()
            .join(".tmp")
            .join("rollout-compression.lock")
            .exists()
    );
    Ok(())
}

#[tokio::test]
async fn resume_materializes_compressed_rollout_path() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let config = RolloutConfig {
        codex_home: home.path().to_path_buf(),
        sqlite_home: home.path().to_path_buf(),
        cwd: home.path().to_path_buf(),
        model_provider_id: "test-provider".to_string(),
        generate_memories: true,
    };
    let uuid = Uuid::from_u128(3);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "hello before resume")?;
    compress_now(&rollout_path)?;
    let compressed_path = compressed_rollout_path(&rollout_path);

    let InitialHistory::Resumed(history) =
        RolloutRecorder::get_rollout_history(compressed_path.as_path()).await?
    else {
        panic!("expected compressed rollout to load as resumed history");
    };
    assert_eq!(history.rollout_path, Some(rollout_path.clone()));

    let recorder = RolloutRecorder::new(
        &config,
        RolloutRecorderParams::resume(compressed_path.clone()),
    )
    .await?;

    assert_eq!(recorder.rollout_path(), rollout_path.as_path());
    assert!(rollout_path.exists());
    assert!(!compressed_path.exists());
    recorder
        .record_canonical_items(&[RolloutItem::EventMsg(EventMsg::UserMessage(
            UserMessageEvent {
                message: "hello after resume".to_string(),
                ..Default::default()
            },
        ))])
        .await?;
    recorder.flush().await?;
    recorder.shutdown().await?;

    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;
    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 3);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn compression_preserves_rollout_permissions() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(6);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = archived_rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "restricted transcript")?;
    fs::set_permissions(&rollout_path, fs::Permissions::from_mode(0o600))?;
    set_old_mtime(&rollout_path)?;

    worker::run(home.path().to_path_buf()).await?;

    let compressed_path = compressed_rollout_path(&rollout_path);
    assert!(!rollout_path.exists());
    assert_eq!(
        fs::metadata(&compressed_path)?.permissions().mode() & 0o777,
        0o600
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn append_materialization_preserves_compressed_rollout_permissions() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(6);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "restricted transcript")?;
    compress_now(&rollout_path)?;
    let compressed_path = compressed_rollout_path(&rollout_path);
    fs::set_permissions(&compressed_path, fs::Permissions::from_mode(0o600))?;

    append_rollout_item_to_path(
        &rollout_path,
        &RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
            message: "materialize restricted transcript".to_string(),
            ..Default::default()
        })),
    )
    .await?;

    assert!(rollout_path.exists());
    assert!(!compressed_path.exists());
    assert_eq!(
        fs::metadata(&rollout_path)?.permissions().mode() & 0o777,
        0o600
    );
    Ok(())
}

#[test]
fn persist_temp_file_noclobber_installs_completed_temp() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let temp_path = home.path().join("rollout.jsonl.tmp");
    let destination = home.path().join("rollout.jsonl");
    fs::write(&temp_path, "completed rollout")?;

    persist_temp_file_noclobber(&temp_path, &destination)?;

    assert!(!temp_path.exists());
    assert_eq!(fs::read_to_string(destination)?, "completed rollout");
    Ok(())
}

#[test]
fn persist_temp_file_noclobber_does_not_replace_existing_destination() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let temp_path = home.path().join("rollout.jsonl.tmp");
    let destination = home.path().join("rollout.jsonl");
    fs::write(&temp_path, "candidate rollout")?;
    fs::write(&destination, "existing rollout")?;

    persist_temp_file_noclobber(&temp_path, &destination)?;

    assert!(!temp_path.exists());
    assert_eq!(fs::read_to_string(destination)?, "existing rollout");
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn compression_preserves_read_only_rollout_permissions() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(7);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = archived_rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "read-only transcript")?;
    set_old_mtime(&rollout_path)?;
    fs::set_permissions(&rollout_path, fs::Permissions::from_mode(0o400))?;
    let source_modified = fs::metadata(&rollout_path)?.modified()?;

    worker::run(home.path().to_path_buf()).await?;

    let compressed_path = compressed_rollout_path(&rollout_path);
    let compressed_metadata = fs::metadata(&compressed_path)?;
    assert!(!rollout_path.exists());
    assert_eq!(compressed_metadata.permissions().mode() & 0o777, 0o400);
    assert_eq!(compressed_metadata.modified()?, source_modified);
    Ok(())
}

#[tokio::test]
async fn worker_skips_existing_compressed_archived_rollouts() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(10);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = archived_rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "already compressed")?;
    compress_now(&rollout_path)?;
    let compressed_path = compressed_rollout_path(&rollout_path);
    set_old_mtime(&compressed_path)?;

    worker::run(home.path().to_path_buf()).await?;

    assert!(!rollout_path.exists());
    assert!(compressed_path.exists());
    let (items, loaded_thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(&rollout_path).await?;
    assert_eq!(loaded_thread_id, Some(thread_id));
    assert_eq!(parse_errors, 0);
    assert_eq!(items.len(), 2);
    Ok(())
}

#[tokio::test]
async fn worker_skips_when_fresh_run_marker_exists() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(11);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = archived_rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "throttled worker")?;
    set_old_mtime(&rollout_path)?;
    let marker_dir = home.path().join(".tmp");
    fs::create_dir_all(marker_dir.as_path())?;
    fs::write(marker_dir.join("rollout-compression.lock"), "recent run")?;

    worker::run(home.path().to_path_buf()).await?;

    assert!(rollout_path.exists());
    assert!(!compressed_rollout_path(&rollout_path).exists());
    Ok(())
}

#[test]
fn run_marker_is_removed_unless_persisted() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let marker_path = home.path().join(".tmp").join("rollout-compression.lock");

    {
        let marker = worker::CompressionRunMarker::try_claim(home.path())?;
        assert!(marker.is_some());
    }
    assert!(!marker_path.exists());

    let marker = worker::CompressionRunMarker::try_claim(home.path())?;
    let Some(marker) = marker else {
        panic!("expected run marker claim");
    };
    marker.persist();
    assert!(marker_path.exists());
    assert!(worker::CompressionRunMarker::try_claim(home.path())?.is_none());
    Ok(())
}

#[tokio::test]
async fn find_thread_path_by_id_handles_compressed_rollout_filenames() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(8);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid);
    write_rollout(&rollout_path, thread_id, "compressed filename lookup")?;
    compress_now(&rollout_path)?;
    let compressed_path = compressed_rollout_path(&rollout_path);

    assert_eq!(
        crate::find_thread_path_by_id_str(
            home.path(),
            &uuid.to_string(),
            /*state_db_ctx*/ None
        )
        .await?,
        Some(compressed_path)
    );
    assert_eq!(
        crate::find_thread_path_by_id_str(home.path(), "not-a-uuid", /*state_db_ctx*/ None).await?,
        None
    );
    Ok(())
}

#[tokio::test]
async fn find_thread_path_by_id_ignores_compression_temp_matches() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let uuid = Uuid::from_u128(9);
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let temp_path = rollout_path(home.path(), "2025-01-03T12-00-00", uuid).with_file_name(format!(
        "rollout-2025-01-03T12-00-00-{uuid}.jsonl.zst.compress.1.0.tmp"
    ));
    write_rollout(&temp_path, thread_id, "temporary file should not resolve")?;

    assert_eq!(
        crate::find_thread_path_by_id_str(
            home.path(),
            &uuid.to_string(),
            /*state_db_ctx*/ None
        )
        .await?,
        None
    );
    Ok(())
}

fn rollout_path(home: &std::path::Path, ts: &str, uuid: Uuid) -> std::path::PathBuf {
    home.join("sessions/2025/01/03")
        .join(format!("rollout-{ts}-{uuid}.jsonl"))
}

fn archived_rollout_path(home: &std::path::Path, ts: &str, uuid: Uuid) -> std::path::PathBuf {
    home.join("archived_sessions")
        .join(format!("rollout-{ts}-{uuid}.jsonl"))
}

fn write_rollout(path: &std::path::Path, thread_id: ThreadId, message: &str) -> anyhow::Result<()> {
    let parent = path.parent().expect("rollout path should have parent");
    fs::create_dir_all(parent)?;
    let session_meta_line = SessionMetaLine {
        meta: SessionMeta {
            session_id: thread_id.into(),
            id: thread_id,
            forked_from_id: None,
            parent_thread_id: None,
            timestamp: "2025-01-03T12:00:00Z".to_string(),
            cwd: parent.to_path_buf(),
            originator: "test".to_string(),
            cli_version: "test".to_string(),
            source: SessionSource::Cli,
            thread_source: None,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
            model_provider: None,
            base_instructions: None,
            dynamic_tools: None,
            selected_capability_roots: Vec::new(),
            memory_mode: None,
            history_mode: Default::default(),
            multi_agent_version: None,
            context_window: None,
        },
        git: None,
    };
    let lines = [
        RolloutLine {
            timestamp: "2025-01-03T12:00:00Z".to_string(),
            ordinal: None,
            item: RolloutItem::SessionMeta(session_meta_line),
        },
        RolloutLine {
            timestamp: "2025-01-03T12:00:01Z".to_string(),
            ordinal: None,
            item: RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                message: message.to_string(),
                ..Default::default()
            })),
        },
    ];
    let jsonl = lines
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()?
        .join("\n");
    fs::write(path, format!("{jsonl}\n"))?;
    Ok(())
}

fn compress_now(path: &std::path::Path) -> anyhow::Result<()> {
    let compressed_path = compressed_rollout_path(path);
    let input = fs::File::open(path)?;
    let output = fs::File::create(compressed_path)?;
    let mut encoder = zstd::stream::write::Encoder::new(output, 3)?;
    let mut input = std::io::BufReader::new(input);
    std::io::copy(&mut input, &mut encoder)?;
    encoder.finish()?;
    fs::remove_file(path)?;
    Ok(())
}

fn set_old_mtime(path: &std::path::Path) -> anyhow::Result<()> {
    let old = SystemTime::now()
        .checked_sub(Duration::from_secs(8 * 24 * 60 * 60))
        .expect("old timestamp should be representable");
    let times = FileTimes::new().set_modified(old);
    fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .set_times(times)?;
    Ok(())
}
