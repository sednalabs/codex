use std::fs;
use std::sync::Arc;

use codex_protocol::ThreadId;
use tempfile::TempDir;

use super::COORDINATION_LOCK_FILE;
use super::WRITER_LOCK_DIR;
use super::WriterLockCoordinator;
use pretty_assertions::assert_eq;
use std::io::ErrorKind;

#[test]
fn writer_locks_reject_competing_owners_and_release_their_files() {
    let home = TempDir::new().expect("temp dir");
    let primary = Arc::new(WriterLockCoordinator::new(home.path()));
    let secondary = Arc::new(WriterLockCoordinator::new(home.path()));
    let thread_id = ThreadId::default();
    let other_thread_id = ThreadId::default();

    let owner = primary.acquire(thread_id).expect("acquire writer lock");
    let lock_path = home
        .path()
        .join(WRITER_LOCK_DIR)
        .join(format!("{thread_id}.lock"));
    assert!(lock_path.exists());

    let err = match secondary.acquire(thread_id) {
        Ok(_) => panic!("competing owner should fail"),
        Err(err) => err,
    };
    assert_eq!(err.kind(), ErrorKind::WouldBlock);
    let other_owner = secondary
        .acquire(other_thread_id)
        .expect("other thread should acquire its own lock");

    drop(owner);
    assert!(!lock_path.exists());
    let next_owner = secondary
        .acquire(thread_id)
        .expect("released thread should accept another owner");
    drop(next_owner);
    drop(other_owner);

    let entries = fs::read_dir(home.path().join(WRITER_LOCK_DIR))
        .expect("read lock directory")
        .map(|entry| entry.expect("lock directory entry").file_name())
        .collect::<Vec<_>>();
    assert_eq!(entries, vec![COORDINATION_LOCK_FILE]);
}

#[test]
fn first_acquisition_removes_stale_locks_without_removing_active_locks() {
    let home = TempDir::new().expect("temp dir");
    let primary = Arc::new(WriterLockCoordinator::new(home.path()));
    let active_thread_id = ThreadId::default();
    let active_owner = primary
        .acquire(active_thread_id)
        .expect("acquire active writer lock");

    let stale_thread_id = ThreadId::default();
    let stale_path = home
        .path()
        .join(WRITER_LOCK_DIR)
        .join(format!("{stale_thread_id}.lock"));
    fs::File::create(&stale_path).expect("create stale writer lock");

    let secondary = Arc::new(WriterLockCoordinator::new(home.path()));
    let secondary_owner = secondary
        .acquire(ThreadId::default())
        .expect("acquire writer lock after cleanup");

    assert!(!stale_path.exists());
    let err = match secondary.acquire(active_thread_id) {
        Ok(_) => panic!("active writer should survive cleanup"),
        Err(err) => err,
    };
    assert_eq!(err.kind(), ErrorKind::WouldBlock);

    drop(secondary_owner);
    drop(active_owner);
}

#[test]
fn publication_skips_live_writers_and_keeps_coordination_locked() {
    let home = TempDir::new().expect("temp dir");
    let publisher = Arc::new(WriterLockCoordinator::new(home.path()));
    let writer = Arc::new(WriterLockCoordinator::new(home.path()));
    let thread_id = ThreadId::default();
    let owner = writer.acquire(thread_id).expect("writer owns thread");
    assert!(
        publisher
            .try_acquire_for_publication(thread_id)
            .unwrap()
            .is_none()
    );
    drop(owner);

    let publication = publisher
        .try_acquire_for_publication(thread_id)
        .unwrap()
        .unwrap();
    let coordination = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(writer.directory.join(COORDINATION_LOCK_FILE))
        .unwrap();
    assert!(matches!(
        coordination.try_lock(),
        Err(fs::TryLockError::WouldBlock)
    ));
    drop(publication);
    coordination
        .try_lock()
        .expect("publication releases coordination");
    drop(coordination);
    // An existing but unlocked file is also idle; file existence is not ownership.
    fs::File::create(writer.directory.join(format!("{thread_id}.lock"))).unwrap();
    drop(
        publisher
            .try_acquire_for_publication(thread_id)
            .unwrap()
            .expect("stale lock is idle"),
    );
    assert!(writer.acquire(thread_id).is_ok());
}

#[cfg(unix)]
#[test]
fn physical_fence_is_shared_by_distinct_homes_and_directory_and_file_aliases() -> std::io::Result<()>
{
    use super::PhysicalRolloutGuard;
    use std::os::unix::fs::symlink;
    let history = TempDir::new()?;
    let first_home = TempDir::new()?;
    let second_home = TempDir::new()?;
    let path = history.path().join("rollout.jsonl");
    fs::write(&path, b"{}\n")?;
    symlink(history.path(), first_home.path().join("sessions"))?;
    symlink(history.path(), second_home.path().join("sessions"))?;
    let first_alias = first_home.path().join("sessions/rollout.jsonl");
    let second_alias = second_home.path().join("sessions/rollout.jsonl");
    let file_alias = second_home.path().join("alias.jsonl");
    symlink(&path, &file_alias)?;
    let thread_id = ThreadId::new();
    // Logical profile ownership remains independent, but physical ownership does not.
    let _first_logical =
        Arc::new(WriterLockCoordinator::new(first_home.path())).acquire(thread_id)?;
    let _second_logical =
        Arc::new(WriterLockCoordinator::new(second_home.path())).acquire(thread_id)?;
    let owner = PhysicalRolloutGuard::acquire(&first_alias, false)?;
    for alias in [
        &second_alias,
        &file_alias,
        &path.with_extension("jsonl.zst"),
    ] {
        let err = match PhysicalRolloutGuard::acquire(alias, false) {
            Ok(_) => panic!("aliases must contend on the same physical lock"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), ErrorKind::WouldBlock);
    }
    assert_eq!(owner.path(), fs::canonicalize(&path)?.as_path());
    drop(owner);
    drop(PhysicalRolloutGuard::acquire(&second_alias, false)?);
    assert!(path.exists());
    Ok(())
}
