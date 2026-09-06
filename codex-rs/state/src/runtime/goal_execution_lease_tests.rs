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
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tempfile::tempdir;

fn thread_id(value: u128) -> ThreadId {
    ThreadId::from_string(&format!("{value:032x}")).expect("valid thread id")
}

fn goals_db(dir: &Path) -> PathBuf {
    let path = dir.join("goals_1.sqlite");
    fs::write(&path, b"").expect("create goals database");
    fs::canonicalize(path).expect("canonical goals database")
}

fn protocol_marker(prefix: &str, thread_id: ThreadId) -> String {
    format!("CODEX_GOAL_LEASE_{prefix}_{}", thread_id)
}

#[test]
fn distinct_threads_have_independent_leases() {
    let directory = tempdir().expect("temporary directory");
    let database = goals_db(directory.path());
    let first = GoalExecutionLease::acquire(&database, thread_id(/*value*/ 1))
        .expect("first lease");
    let second = GoalExecutionLease::acquire(&database, thread_id(/*value*/ 2))
        .expect("second lease");
    assert!(matches!(
        GoalExecutionLease::acquire(&database, thread_id(/*value*/ 1)),
        Err(GoalExecutionLeaseError::Busy)
    ));
    drop((first, second));
}

#[test]
fn cloned_lease_retains_lock_until_final_drop() {
    let directory = tempdir().expect("temporary directory");
    let database = goals_db(directory.path());
    let lease =
        GoalExecutionLease::acquire(&database, thread_id(/*value*/ 3)).expect("lease");
    let clone = lease.clone();
    drop(lease);
    assert!(matches!(
        GoalExecutionLease::acquire(&database, thread_id(/*value*/ 3)),
        Err(GoalExecutionLeaseError::Busy)
    ));
    drop(clone);
    GoalExecutionLease::acquire(&database, thread_id(/*value*/ 3))
        .expect("lease after final drop");
}

#[test]
fn lock_file_is_stable_and_not_unlinked() {
    let directory = tempdir().expect("temporary directory");
    let database = goals_db(directory.path());
    let lease =
        GoalExecutionLease::acquire(&database, thread_id(/*value*/ 4)).expect("lease");
    let lock = lock_path(&database, thread_id(/*value*/ 4)).expect("lock path");
    assert!(lock.exists());
    drop(lease);
    assert!(lock.exists());
}

#[test]
fn cross_process_contention_uses_native_lock() {
    if std::env::var_os("CODEX_GOAL_LEASE_CHILD").is_some() {
        let database = PathBuf::from(std::env::var_os("CODEX_GOAL_LEASE_DB").expect("database"));
        let lease = GoalExecutionLease::acquire(&database, thread_id(/*value*/ 5))
            .expect("child lease");
        println!("{}", protocol_marker("READY", thread_id(/*value*/ 5)));
        std::io::stdout().flush().expect("flush ready signal");
        let mut release = [0_u8; 1];
        std::io::stdin()
            .read_exact(&mut release)
            .expect("read release signal");
        drop(lease);
        println!("{}", protocol_marker("RELEASED", thread_id(/*value*/ 5)));
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
    let ready_marker = protocol_marker("READY", thread_id(/*value*/ 5));
    let released_marker = protocol_marker("RELEASED", thread_id(/*value*/ 5));
    let (signal_sender, signal_receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut child_stdout = BufReader::new(child_stdout);
        let mut signal = String::new();
        loop {
            signal.clear();
            match child_stdout.read_line(&mut signal) {
                Ok(0) => {
                    let _ = signal_sender.send(None);
                    return;
                }
                Ok(_) => {
                    let marker = signal.trim().to_string();
                    if signal_sender.send(Some(marker)).is_err() {
                        return;
                    }
                }
                Err(_) => {
                    let _ = signal_sender.send(None);
                    return;
                }
            }
        }
    });
    let ready = loop {
        match signal_receiver.recv_timeout(Duration::from_secs(10)) {
            Ok(Some(signal)) if signal.contains(&ready_marker) => break true,
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => break false,
        }
    };
    if !ready {
        let _ = child.kill();
        let _ = child.wait();
        panic!("child exited or timed out before READY");
    }
    let busy = matches!(
        GoalExecutionLease::acquire(&database, thread_id(/*value*/ 5)),
        Err(GoalExecutionLeaseError::Busy)
    );
    let release_sent = child_stdin
        .write_all(b"x")
        .and_then(|()| child_stdin.flush())
        .is_ok();
    if !release_sent {
        let _ = child.kill();
        let _ = child.wait();
        panic!("failed to send release signal");
    }
    let released = loop {
        match signal_receiver.recv_timeout(Duration::from_secs(10)) {
            Ok(Some(signal)) if signal.contains(&released_marker) => break true,
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => break false,
        }
    };
    if !released {
        let _ = child.kill();
        let _ = child.wait();
        panic!("child exited or timed out before RELEASED");
    }
    assert!(child.wait().expect("wait child").success());
    assert!(busy, "parent should observe the child-held lease as busy");
    GoalExecutionLease::acquire(&database, thread_id(/*value*/ 5))
        .expect("lease after child release");
}

#[test]
fn arc_clone_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Arc<GoalExecutionLease>>();
}
