use std::fs;
use std::future::Future;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::mpsc;
use std::task::Poll;
use std::time::Duration;

use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::sync::oneshot;

use super::*;
use crate::compression::compressed_rollout_path;

const OPEN_DIAGNOSTIC_DEADLINE: Duration = Duration::from_secs(10);
const LEGACY_ROLLOUT: &[u8] = br#"{"timestamp":"2026-07-16T00:00:00Z","type":"session_meta","payload":{"session_id":"00000000-0000-0000-0000-000000000001","id":"00000000-0000-0000-0000-000000000001","timestamp":"2026-07-16T00:00:00Z","cwd":".","originator":"test","cli_version":"test","source":"cli"}}"#;

type DirectorySnapshot = Vec<(PathBuf, Option<Vec<u8>>)>;

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
    entered: Option<oneshot::Receiver<()>>,
    release: Option<mpsc::Sender<()>>,
}

impl BoundaryControl {
    async fn wait_until_entered(&mut self) -> anyhow::Result<()> {
        let entered = self
            .entered
            .take()
            .ok_or_else(|| anyhow::anyhow!("rollout open boundary awaited twice"))?;
        with_open_deadline(entered).await??;
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

fn boundary_pair() -> (BoundaryControl, BlockingBoundary) {
    let (entered_tx, entered) = oneshot::channel();
    let (release, release_rx) = mpsc::channel();
    (
        BoundaryControl {
            entered: Some(entered),
            release: Some(release),
        },
        BlockingBoundary {
            entered: entered_tx,
            release: release_rx,
        },
    )
}

fn start_controlled_recovery_open(
    path: PathBuf,
    authority: RolloutMutationAuthority,
) -> (
    tokio::task::JoinHandle<io::Result<File>>,
    BoundaryControl,
    BoundaryControl,
) {
    let (before_control, before_boundary) = boundary_pair();
    let (after_control, after_boundary) = boundary_pair();
    let task = tokio::spawn(async move {
        tokio::task::spawn_blocking(move || {
            open_log_file_with_authority(
                path.as_path(),
                authority,
                move || before_boundary.enter(),
                move || after_boundary.enter(),
            )
        })
        .await
        .map_err(IoError::other)?
    });
    (task, before_control, after_control)
}

async fn with_open_deadline<T>(future: impl Future<Output = T>) -> anyhow::Result<T> {
    tokio::time::timeout(OPEN_DIAGNOSTIC_DEADLINE, future)
        .await
        .map_err(|_| anyhow::anyhow!("timed out during rollout open coordination"))
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

async fn cancel_outer_task<T>(task: tokio::task::JoinHandle<T>) -> anyhow::Result<()> {
    task.abort();
    match with_open_deadline(task).await? {
        Err(error) if error.is_cancelled() => Ok(()),
        Err(error) => anyhow::bail!("outer rollout open failed instead of cancelling: {error}"),
        Ok(_) => anyhow::bail!("outer rollout open completed before cancellation"),
    }
}

fn snapshot_directory(root: &Path) -> anyhow::Result<DirectorySnapshot> {
    fn collect(
        root: &Path,
        directory: &Path,
        entries: &mut DirectorySnapshot,
    ) -> anyhow::Result<()> {
        let mut children = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
        children.sort_by_key(fs::DirEntry::path);
        for child in children {
            let path = child.path();
            let relative = path.strip_prefix(root)?.to_path_buf();
            if child.file_type()?.is_dir() {
                entries.push((relative, None));
                collect(root, path.as_path(), entries)?;
            } else if child.file_type()?.is_file() {
                entries.push((relative, Some(fs::read(path)?)));
            } else {
                anyhow::bail!("unexpected rollout snapshot entry: {}", path.display());
            }
        }
        Ok(())
    }

    let mut entries = Vec::new();
    collect(root, root, &mut entries)?;
    Ok(entries)
}

fn compress_rollout(path: &Path) -> io::Result<PathBuf> {
    let compressed_path = compressed_rollout_path(path);
    zstd::stream::copy_encode(
        fs::File::open(path)?,
        fs::File::create(&compressed_path)?,
        3,
    )?;
    fs::remove_file(path)?;
    Ok(compressed_path)
}

#[tokio::test]
async fn plain_resume_tail_repair_custody_survives_caller_cancellation() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let rollout_path = home.path().join("rollout.jsonl");
    fs::write(&rollout_path, LEGACY_ROLLOUT)?;
    let initial = snapshot_directory(home.path())?;
    let authority = RolloutMutationAuthority::new();
    let (mut before_mutation, before_boundary) = boundary_pair();
    let (mut after_mutation, after_boundary) = boundary_pair();
    let path_for_open = rollout_path.clone();
    let authority_for_open = authority.clone();
    let task = tokio::spawn(async move {
        open_rollout_for_append_with_authority_and_hooks(
            path_for_open.as_path(),
            authority_for_open,
            move || before_boundary.enter(),
            move || after_boundary.enter(),
        )
        .await
    });

    before_mutation.wait_until_entered().await?;
    cancel_outer_task(task).await?;
    let mut revocation = Box::pin(authority.revoke());
    assert!(is_pending(revocation.as_mut()).await);
    assert_eq!(snapshot_directory(home.path())?, initial);

    before_mutation.release();
    after_mutation.wait_until_entered().await?;
    let mut repaired = LEGACY_ROLLOUT.to_vec();
    repaired.push(b'\n');
    let repaired_snapshot = vec![(PathBuf::from("rollout.jsonl"), Some(repaired.clone()))];
    assert_eq!(snapshot_directory(home.path())?, repaired_snapshot);
    assert!(is_pending(revocation.as_mut()).await);

    after_mutation.release();
    with_open_deadline(revocation).await?;
    assert_eq!(snapshot_directory(home.path())?, repaired_snapshot);

    let unsafe_path = home.path().join("unsafe.jsonl");
    fs::write(&unsafe_path, LEGACY_ROLLOUT)?;
    let missing_path = home.path().join("must-not-exist/rollout.jsonl");
    let before_denials = snapshot_directory(home.path())?;
    open_rollout_for_append_with_authority(unsafe_path.as_path(), authority.clone())
        .await
        .expect_err("revoked authority must deny later tail repair");
    open_log_file_with_authority(missing_path.as_path(), authority, || {}, || {})
        .expect_err("revoked authority must deny later path creation");
    assert_eq!(snapshot_directory(home.path())?, before_denials);
    Ok(())
}

#[tokio::test]
async fn recovery_materialization_tail_repair_and_creation_are_guarded() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let rollout_path = home.path().join("rollout.jsonl");
    fs::write(&rollout_path, LEGACY_ROLLOUT)?;
    let compressed_path = compress_rollout(&rollout_path)?;
    let initial = snapshot_directory(home.path())?;
    let authority = RolloutMutationAuthority::new();
    let (task, mut before_mutation, mut after_mutation) =
        start_controlled_recovery_open(compressed_path, authority.clone());

    before_mutation.wait_until_entered().await?;
    cancel_outer_task(task).await?;
    let mut revocation = Box::pin(authority.revoke());
    assert!(is_pending(revocation.as_mut()).await);
    assert_eq!(snapshot_directory(home.path())?, initial);

    before_mutation.release();
    after_mutation.wait_until_entered().await?;
    let mut expected = LEGACY_ROLLOUT.to_vec();
    expected.push(b'\n');
    let recovered_snapshot = vec![(PathBuf::from("rollout.jsonl"), Some(expected.clone()))];
    assert_eq!(snapshot_directory(home.path())?, recovered_snapshot);
    assert!(is_pending(revocation.as_mut()).await);

    after_mutation.release();
    with_open_deadline(revocation).await?;
    assert_eq!(snapshot_directory(home.path())?, recovered_snapshot);

    let creation_authority = RolloutMutationAuthority::new();
    let created_path = home.path().join("sessions/2026/07/rollout.jsonl");
    drop(open_log_file_with_authority(
        created_path.as_path(),
        creation_authority.clone(),
        || {},
        || {},
    )?);
    let created_snapshot = vec![
        (PathBuf::from("rollout.jsonl"), Some(expected)),
        (PathBuf::from("sessions"), None),
        (PathBuf::from("sessions/2026"), None),
        (PathBuf::from("sessions/2026/07"), None),
        (
            PathBuf::from("sessions/2026/07/rollout.jsonl"),
            Some(Vec::new()),
        ),
    ];
    assert_eq!(snapshot_directory(home.path())?, created_snapshot);
    creation_authority.revoke().await;
    let later_path = home.path().join("sessions/2026/08/rollout.jsonl");
    open_log_file_with_authority(later_path.as_path(), creation_authority, || {}, || {})
        .expect_err("revoked authority must deny later recovery creation");
    assert_eq!(snapshot_directory(home.path())?, created_snapshot);
    Ok(())
}

#[tokio::test]
async fn admitted_recovery_error_releases_custody_without_filesystem_drift() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    let blocking_parent = home.path().join("not-a-directory");
    fs::write(&blocking_parent, b"block directory creation")?;
    let initial = snapshot_directory(home.path())?;
    let authority = RolloutMutationAuthority::new();

    open_log_file_with_authority(
        blocking_parent.join("rollout.jsonl").as_path(),
        authority.clone(),
        || {},
        || {},
    )
    .expect_err("post-admission recovery mutation should fail");
    with_open_deadline(authority.revoke()).await?;
    assert_eq!(snapshot_directory(home.path())?, initial);
    Ok(())
}
