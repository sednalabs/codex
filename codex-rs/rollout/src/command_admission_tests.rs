use std::future::Future;
use std::future::poll_fn;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::task::Poll;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::RolloutCommandAdmission;
use super::RolloutCommandAdmissionError;
use super::RolloutTerminalAdmission;

const DIAGNOSTIC_DEADLINE: Duration = Duration::from_secs(10);

#[tokio::test]
async fn terminal_waits_for_data_commit_and_excludes_later_data() {
    let admission = RolloutCommandAdmission::new();
    let data = data_admission(&admission).await;
    let order = Arc::new(Mutex::new(Vec::new()));
    let order_for_terminal = Arc::clone(&order);
    let (terminal_pending_tx, terminal_pending_rx) = oneshot::channel();
    let terminal_admission = admission.clone();
    let terminal_task = tokio::spawn(async move {
        let terminal =
            acquire_terminal_after_pending(terminal_admission, terminal_pending_tx).await;
        terminal
            .commit(|| lock(&order_for_terminal).push("terminal"))
            .seal();
    });

    assert!(
        with_diagnostic_deadline("waiting for terminal admission", terminal_pending_rx)
            .await
            .is_ok()
    );
    data.commit(|| lock(&order).push("data"));
    assert!(
        with_diagnostic_deadline("joining terminal admission", terminal_task)
            .await
            .is_ok()
    );

    assert_eq!(lock(&order).as_slice(), ["data", "terminal"]);
    assert!(admission.is_terminal());
    assert!(matches!(
        admission.acquire_data().await,
        Err(RolloutCommandAdmissionError::Terminal)
    ));
}

#[tokio::test]
async fn reserved_channel_transfers_commit_without_an_intervening_await() {
    let admission = RolloutCommandAdmission::new();
    let (tx, mut rx) = mpsc::channel(/*buffer*/ 2);

    let data = data_admission(&admission).await;
    let data_permit = match with_diagnostic_deadline("reserving data capacity", tx.reserve()).await
    {
        Ok(permit) => permit,
        Err(err) => panic!("data channel reservation unexpectedly failed: {err}"),
    };
    data.commit(|| data_permit.send("data"));

    let terminal = terminal_admission(&admission).await;
    let terminal_permit =
        match with_diagnostic_deadline("reserving terminal capacity", tx.reserve()).await {
            Ok(permit) => permit,
            Err(err) => panic!("terminal channel reservation unexpectedly failed: {err}"),
        };
    terminal.commit(|| terminal_permit.send("terminal")).seal();

    assert_eq!(
        with_diagnostic_deadline("receiving data command", rx.recv()).await,
        Some("data")
    );
    assert_eq!(
        with_diagnostic_deadline("receiving terminal command", rx.recv()).await,
        Some("terminal")
    );
}

#[tokio::test]
async fn cancelled_waiter_does_not_retain_admission() {
    let admission = RolloutCommandAdmission::new();
    let data = data_admission(&admission).await;
    let (pending_tx, pending_rx) = oneshot::channel();
    let waiting_admission = admission.clone();
    let waiting_task =
        tokio::spawn(
            async move { acquire_terminal_after_pending(waiting_admission, pending_tx).await },
        );

    assert!(
        with_diagnostic_deadline("waiting for queued terminal admission", pending_rx)
            .await
            .is_ok()
    );
    waiting_task.abort();
    assert_task_cancelled("joining cancelled terminal admission", waiting_task).await;
    data.commit(|| {});

    terminal_admission(&admission).await.commit(|| {}).seal();
    assert!(admission.is_terminal());
}

#[tokio::test]
async fn cancellation_while_waiting_for_channel_capacity_releases_data_admission() {
    let admission = RolloutCommandAdmission::new();
    let (tx, _rx) = mpsc::channel(1);
    assert!(tx.send(()).await.is_ok());
    let (reserve_pending_tx, reserve_pending_rx) = oneshot::channel();
    let task_admission = admission.clone();
    let task = tokio::spawn(async move {
        let data = data_admission(&task_admission).await;
        let mut reserve = Box::pin(tx.reserve());
        let mut pending_tx = Some(reserve_pending_tx);
        let _permit = poll_fn(move |cx| {
            if let Poll::Ready(result) = reserve.as_mut().poll(cx) {
                return Poll::Ready(result);
            }
            if let Some(pending_tx) = pending_tx.take() {
                let _ = pending_tx.send(());
            }
            Poll::Pending
        })
        .await;
        data.commit(|| {});
    });

    assert!(
        with_diagnostic_deadline("waiting for pending data reservation", reserve_pending_rx)
            .await
            .is_ok()
    );
    task.abort();
    assert_task_cancelled("joining cancelled data reservation", task).await;

    terminal_admission(&admission).await.commit(|| {}).seal();
    assert!(admission.is_terminal());
}

#[tokio::test]
async fn cancelled_terminal_reservation_leaves_admission_active() {
    let admission = RolloutCommandAdmission::new();
    let (tx, _rx) = mpsc::channel(1);
    assert!(tx.send(()).await.is_ok());
    let (reserve_pending_tx, reserve_pending_rx) = oneshot::channel();
    let task_admission = admission.clone();
    let task = tokio::spawn(async move {
        let terminal = terminal_admission(&task_admission).await;
        let mut reserve = Box::pin(tx.reserve());
        let mut pending_tx = Some(reserve_pending_tx);
        let _permit = poll_fn(move |cx| {
            if let Poll::Ready(result) = reserve.as_mut().poll(cx) {
                return Poll::Ready(result);
            }
            if let Some(pending_tx) = pending_tx.take() {
                let _ = pending_tx.send(());
            }
            Poll::Pending
        })
        .await;
        terminal.commit(|| {}).seal();
    });

    assert!(
        with_diagnostic_deadline(
            "waiting for pending terminal reservation",
            reserve_pending_rx,
        )
        .await
        .is_ok()
    );
    task.abort();
    assert_task_cancelled("joining cancelled terminal reservation", task).await;

    assert!(!admission.is_terminal());
    data_admission(&admission).await.commit(|| {});
}

#[tokio::test]
async fn recoverable_terminal_failure_reopens_a_new_epoch() {
    let admission = RolloutCommandAdmission::new();
    let first_transition = terminal_admission(&admission).await.commit(|| {});
    assert!(admission.is_terminal());
    first_transition.reopen();
    assert!(!admission.is_terminal());

    data_admission(&admission).await.commit(|| {});
    terminal_admission(&admission).await.commit(|| {}).seal();
    assert!(admission.is_terminal());
}

#[tokio::test]
async fn dropped_terminal_transition_fails_closed() {
    let admission = RolloutCommandAdmission::new();
    let transition = terminal_admission(&admission).await.commit(|| {});

    drop(transition);

    assert!(admission.is_terminal());
    assert!(matches!(
        admission.acquire_data().await,
        Err(RolloutCommandAdmissionError::Terminal)
    ));
    assert!(matches!(
        admission.acquire_terminal().await,
        Err(RolloutCommandAdmissionError::Terminal)
    ));
}

async fn acquire_terminal_after_pending(
    admission: RolloutCommandAdmission,
    pending_tx: oneshot::Sender<()>,
) -> RolloutTerminalAdmission {
    let mut acquire = Box::pin(admission.acquire_terminal());
    let mut pending_tx = Some(pending_tx);
    let result = poll_fn(move |cx| {
        if let Poll::Ready(result) = acquire.as_mut().poll(cx) {
            return Poll::Ready(result);
        }
        if let Some(pending_tx) = pending_tx.take() {
            let _ = pending_tx.send(());
        }
        Poll::Pending
    })
    .await;
    match result {
        Ok(terminal) => terminal,
        Err(err) => panic!("terminal admission unexpectedly failed: {err:?}"),
    }
}

async fn data_admission(admission: &RolloutCommandAdmission) -> super::RolloutDataAdmission {
    match with_diagnostic_deadline("acquiring data admission", admission.acquire_data()).await {
        Ok(data) => data,
        Err(err) => panic!("data admission unexpectedly failed: {err:?}"),
    }
}

async fn terminal_admission(admission: &RolloutCommandAdmission) -> RolloutTerminalAdmission {
    match with_diagnostic_deadline("acquiring terminal admission", admission.acquire_terminal())
        .await
    {
        Ok(terminal) => terminal,
        Err(err) => panic!("terminal admission unexpectedly failed: {err:?}"),
    }
}

async fn with_diagnostic_deadline<T>(context: &'static str, future: impl Future<Output = T>) -> T {
    match tokio::time::timeout(DIAGNOSTIC_DEADLINE, future).await {
        Ok(output) => output,
        Err(_) => panic!("timed out while {context}"),
    }
}

async fn assert_task_cancelled<T>(context: &'static str, task: JoinHandle<T>) {
    match with_diagnostic_deadline(context, task).await {
        Ok(_) => panic!("task completed instead of being cancelled while {context}"),
        Err(err) => assert!(
            err.is_cancelled(),
            "task failed without cancellation: {err}"
        ),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
