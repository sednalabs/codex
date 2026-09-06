use super::*;
use std::fs;
use std::path::PathBuf;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use tempfile::tempdir;

fn thread_id(value: u128) -> ThreadId {
    ThreadId::from_string(&format!("{value:032x}")).expect("valid thread id")
}

fn goals_db(dir: &Path) -> PathBuf {
    let path = dir.join("goals_1.sqlite");
    fs::write(&path, b"").expect("create goals database");
    fs::canonicalize(path).expect("canonical goals database")
}

#[test]
fn distinct_threads_have_independent_leases() {
    let directory = tempdir().expect("temporary directory");
    let database = goals_db(directory.path());
    let first = GoalExecutionLease::acquire(&database, thread_id(1)).expect("first lease");
    let second = GoalExecutionLease::acquire(&database, thread_id(2)).expect("second lease");
    assert!(matches!(
        GoalExecutionLease::acquire(&database, thread_id(1)),
        Err(GoalExecutionLeaseError::Busy)
    ));
    drop((first, second));
}

#[test]
fn cloned_lease_retains_lock_until_final_drop() {
    let directory = tempdir().expect("temporary directory");
    let database = goals_db(directory.path());
    let lease = GoalExecutionLease::acquire(&database, thread_id(3)).expect("lease");
    let clone = lease.clone();
    drop(lease);
    assert!(matches!(
        GoalExecutionLease::acquire(&database, thread_id(3)),
        Err(GoalExecutionLeaseError::Busy)
    ));
    drop(clone);
    GoalExecutionLease::acquire(&database, thread_id(3)).expect("lease after final drop");
}

#[test]
fn lock_file_is_stable_and_not_unlinked() {
    let directory = tempdir().expect("temporary directory");
    let database = goals_db(directory.path());
    let lease = GoalExecutionLease::acquire(&database, thread_id(4)).expect("lease");
    let lock = lock_path(&database, thread_id(4));
    assert!(lock.exists());
    drop(lease);
    assert!(lock.exists());
}

#[test]
fn cross_process_contention_uses_native_lock() {
    if std::env::var_os("CODEX_GOAL_LEASE_CHILD").is_some() {
        let database = PathBuf::from(std::env::var_os("CODEX_GOAL_LEASE_DB").expect("database"));
        let _lease = GoalExecutionLease::acquire(&database, thread_id(5)).expect("child lease");
        std::thread::sleep(std::time::Duration::from_millis(250));
        return;
    }

    let directory = tempdir().expect("temporary directory");
    let database = goals_db(directory.path());
    let mut child = Command::new(std::env::current_exe().expect("test executable"))
        .arg("--exact")
        .arg("runtime::goal_execution_lease::tests::cross_process_contention_uses_native_lock")
        .arg("--nocapture")
        .env("CODEX_GOAL_LEASE_CHILD", "1")
        .env("CODEX_GOAL_LEASE_DB", &database)
        .spawn()
        .expect("spawn child");
    std::thread::sleep(std::time::Duration::from_millis(75));
    assert!(matches!(
        GoalExecutionLease::acquire(&database, thread_id(5)),
        Err(GoalExecutionLeaseError::Busy)
    ));
    assert!(child.wait().expect("wait child").success());
}

#[test]
fn arc_clone_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Arc<GoalExecutionLease>>();
}
