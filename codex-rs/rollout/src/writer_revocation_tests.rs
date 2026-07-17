use std::future::Future;
use std::future::poll_fn;
use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::time::Duration;

use pretty_assertions::assert_eq;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use super::RetainedIoError;
use super::RevocationOutcome;
use super::RevocationStatus;
use super::RolloutWriterRevocation;
use super::revoked;
use super::terminal_failure as terminal;
use crate::command_admission::RolloutCommandAdmission;

const DEADLINE: Duration = Duration::from_secs(10);

#[tokio::test]
async fn cancellation_before_commit_keeps_attempt_and_writer_active() {
    let admission = RolloutCommandAdmission::new();
    let data = deadline(admission.acquire_data()).await.unwrap();
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<oneshot::Sender<io::Result<()>>>();
    let writer = tokio::spawn(async move {
        if let Some(ack) = cmd_rx.recv().await {
            let _ = ack.send(Ok(()));
        }
    });
    let revocation = RolloutWriterRevocation::new(admission.clone(), writer);
    let transferred = Arc::new(AtomicBool::new(false));
    let transferred_in_task = Arc::clone(&transferred);
    let (pending_tx, pending_rx) = oneshot::channel();
    let task_revocation = revocation.clone();
    let task = tokio::spawn(async move {
        let mut revoke = Box::pin(task_revocation.revoke(|_| {
            transferred_in_task.store(true, Ordering::Release);
        }));
        let mut pending_tx = Some(pending_tx);
        poll_fn(move |cx| {
            if let Poll::Ready(result) = revoke.as_mut().poll(cx) {
                return Poll::Ready(result);
            }
            if let Some(tx) = pending_tx.take() {
                let _ = tx.send(());
            }
            Poll::Pending
        })
        .await
    });
    deadline(pending_rx).await.unwrap();
    task.abort();
    assert!(deadline(task).await.unwrap_err().is_cancelled());
    data.commit(|| {});
    let outcome = deadline(revocation.revoke(|ack| cmd_tx.send(ack).unwrap())).await;
    assert!(!transferred.load(Ordering::Acquire));
    assert_eq!(outcome, revoked(1));
}

#[tokio::test]
async fn cancellation_after_commit_leaves_shared_success_for_participants() {
    let admission = RolloutCommandAdmission::new();
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    let (committed_tx, committed_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let writer = tokio::spawn(async move {
        let ack = cmd_rx.recv().await.unwrap();
        let _ = committed_tx.send(());
        let _ = release_rx.await;
        let _ = ack.send(Ok(()));
    });
    let revocation = RolloutWriterRevocation::new(admission, writer);
    let start = revocation.shared.start.lock().await;
    let first_revocation = revocation.clone();
    let first = tokio::spawn(async move {
        first_revocation
            .revoke(|ack| cmd_tx.send(ack).unwrap())
            .await
    });
    tokio::task::yield_now().await;
    let participant_revocation = revocation.clone();
    let participant = tokio::spawn(async move { participant_revocation.revoke(drop).await });
    tokio::task::yield_now().await;
    drop(start);
    deadline(committed_rx).await.unwrap();
    drop(deadline(revocation.shared.start.lock()).await);
    first.abort();
    assert!(deadline(first).await.unwrap_err().is_cancelled());
    let _ = release_tx.send(());
    assert_eq!(deadline(participant).await.unwrap(), revoked(1));
}

#[tokio::test]
async fn recoverable_failure_reopens_after_shared_outcome_and_advances_attempt() {
    let admission = RolloutCommandAdmission::new();
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    let writer = tokio::spawn(async move {
        let first = cmd_rx.recv().await.unwrap();
        let _ = first.send(Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "drain blocked",
        )));
        let second = cmd_rx.recv().await.unwrap();
        let _ = second.send(Ok(()));
    });
    let revocation = RolloutWriterRevocation::new(admission.clone(), writer);
    let failure = deadline(revocation.revoke(|ack| cmd_tx.send(ack).unwrap())).await;
    assert_eq!(
        failure,
        RevocationOutcome {
            attempt: 1,
            status: RevocationStatus::RetryableFailure(RetainedIoError {
                kind: io::ErrorKind::WriteZero,
                message: "drain blocked".to_string(),
            }),
        }
    );
    let data = deadline(admission.acquire_data()).await.unwrap();
    data.commit(|| {});
    assert_eq!(
        deadline(revocation.revoke(|ack| cmd_tx.send(ack).unwrap())).await,
        revoked(2)
    );
}

#[tokio::test]
async fn acknowledgement_loss_and_writer_disappearance_are_stable_terminal_failures() {
    let admission = RolloutCommandAdmission::new();
    let writer = tokio::spawn(std::future::pending::<()>());
    let revocation = RolloutWriterRevocation::new(admission.clone(), writer);
    let lost = deadline(revocation.revoke(drop)).await;
    assert_eq!(
        lost,
        terminal(1, "rollout writer dropped revocation acknowledgement")
    );
    assert!(admission.is_terminal());
    assert_eq!(deadline(revocation.revoke(drop)).await, lost);

    let admission = RolloutCommandAdmission::new();
    let (exit_tx, exit_rx) = oneshot::channel();
    let writer = tokio::spawn(async move {
        let _ = exit_rx.await;
    });
    let revocation = RolloutWriterRevocation::new(admission.clone(), writer);
    let (held_tx, mut held_rx) = mpsc::unbounded_channel();
    let vanished = tokio::spawn({
        let revocation = revocation.clone();
        async move { revocation.revoke(|ack| held_tx.send(ack).unwrap()).await }
    });
    let _held_ack = deadline(held_rx.recv()).await.unwrap();
    let _ = exit_tx.send(());
    assert_eq!(
        deadline(vanished).await.unwrap(),
        terminal(1, "rollout writer exited before revocation acknowledgement")
    );
    assert!(admission.is_terminal());
}

async fn deadline<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(DEADLINE, future)
        .await
        .expect("revocation test timed out")
}
