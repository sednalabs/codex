use super::*;
use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
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
        let lease = GoalExecutionLease::acquire(&database, thread_id(5)).expect("child lease");
        println!("READY");
        std::io::stdout().flush().expect("flush ready signal");
        let mut release = [0_u8; 1];
        std::io::stdin()
            .read_exact(&mut release)
            .expect("read release signal");
        drop(lease);
        println!("RELEASED");
        std::io::stdout().flush().expect("flush release signal");
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
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn child");
    let mut child_stdin = child.stdin.take().expect("child stdin");
    let child_stdout = child.stdout.take().expect("child stdout");
    let mut child_stdout = BufReader::new(child_stdout);
    let mut signal = String::new();
    child_stdout
        .read_line(&mut signal)
        .expect("read child ready signal");
    assert_eq!(signal.trim(), "READY");
    let busy = matches!(
        GoalExecutionLease::acquire(&database, thread_id(5)),
        Err(GoalExecutionLeaseError::Busy)
    );
    child_stdin.write_all(b"x").expect("send release signal");
    child_stdin.flush().expect("flush release signal");
    signal.clear();
    child_stdout
        .read_line(&mut signal)
        .expect("read child release signal");
    assert_eq!(signal.trim(), "RELEASED");
    assert!(child.wait().expect("wait child").success());
    assert!(busy, "parent should observe the child-held lease as busy");
    GoalExecutionLease::acquire(&database, thread_id(5)).expect("lease after child release");
}

#[test]
fn arc_clone_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Arc<GoalExecutionLease>>();
}
