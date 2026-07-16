use std::future::Future;
use std::future::poll_fn;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::mpsc;
use std::task::Poll;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context;
use pretty_assertions::assert_eq;
use tokio::sync::oneshot;

use super::AuthorityState;
use super::MutationAdmissionError;
use super::MutationCountUnderflow;
use super::RolloutMutationAuthority;
use super::RolloutMutationCustody;

const DIAGNOSTIC_DEADLINE: Duration = Duration::from_secs(10);

struct CompletionSignal(Option<mpsc::Sender<()>>);

impl Drop for CompletionSignal {
    fn drop(&mut self) {
        if let Some(completed) = self.0.take() {
            let _ = completed.send(());
        }
    }
}

struct WorkerCleanup {
    completed: mpsc::Receiver<()>,
    worker: JoinHandle<()>,
}

impl WorkerCleanup {
    fn wait_for_completion_and_join(self) -> anyhow::Result<()> {
        self.completed
            .recv_timeout(DIAGNOSTIC_DEADLINE)
            .context("parked rollout mutation did not signal bounded completion")?;
        if self.worker.join().is_err() {
            anyhow::bail!("parked rollout mutation panicked");
        }
        Ok(())
    }
}

struct ParkedMutation {
    entered: Option<oneshot::Receiver<()>>,
    release: Option<mpsc::Sender<()>>,
    cleanup: Option<WorkerCleanup>,
}

impl ParkedMutation {
    fn spawn(custody: RolloutMutationCustody) -> Self {
        let (entered_tx, entered) = oneshot::channel();
        let (release, release_rx) = mpsc::channel();
        let (completed_tx, completed) = mpsc::channel();
        let worker = thread::spawn(move || {
            let _completion = CompletionSignal(Some(completed_tx));
            let _ = entered_tx.send(());
            let _ = release_rx.recv();
            drop(custody);
        });
        Self {
            entered: Some(entered),
            release: Some(release),
            cleanup: Some(WorkerCleanup { completed, worker }),
        }
    }

    async fn wait_until_parked(&mut self) -> anyhow::Result<()> {
        let entered = self
            .entered
            .take()
            .context("parked mutation awaited once")?;
        with_diagnostic_deadline("waiting for parked rollout mutation", entered).await??;
        Ok(())
    }

    async fn release(mut self) -> anyhow::Result<()> {
        self.signal_release();
        let cleanup = self.cleanup.take().context("parked mutation cleaned once")?;
        await_worker_cleanup("joining completed rollout mutation", cleanup).await
    }

    fn signal_release(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}

impl Drop for ParkedMutation {
    fn drop(&mut self) {
        self.signal_release();
        if let Some(cleanup) = self.cleanup.take() {
            let _ = thread::Builder::new()
                .name("rollout-mutation-test-reaper".to_string())
                .spawn(move || {
                    let _ = cleanup.wait_for_completion_and_join();
                });
        }
    }
}

async fn with_diagnostic_deadline<T>(
    context: &'static str,
    future: impl Future<Output = T>,
) -> anyhow::Result<T> {
    match tokio::time::timeout(DIAGNOSTIC_DEADLINE, future).await {
        Ok(output) => Ok(output),
        Err(_) => anyhow::bail!("timed out while {context}"),
    }
}

async fn await_worker_cleanup(
    context: &'static str,
    cleanup: WorkerCleanup,
) -> anyhow::Result<()> {
    let cleanup_task =
        tokio::task::spawn_blocking(move || cleanup.wait_for_completion_and_join());
    let cleanup_result = with_diagnostic_deadline(context, cleanup_task).await??;
    cleanup_result
}

async fn is_pending<F>(mut future: Pin<&mut F>) -> bool
where
    F: Future<Output = ()> + ?Sized,
{
    poll_fn(move |context| Poll::Ready(matches!(future.as_mut().poll(context), Poll::Pending)))
        .await
}

fn assert_authority_snapshot(
    authority: &RolloutMutationAuthority,
    admission_closed: bool,
    in_flight: usize,
) {
    let state = *authority.inner.lock_state();
    let published = *authority.inner.in_flight.borrow();
    assert_eq!(
        (state, published),
        (
            AuthorityState {
                admission_closed,
                in_flight,
            },
            in_flight,
        )
    );
}

fn admit(
    authority: &RolloutMutationAuthority,
    context: &'static str,
) -> anyhow::Result<RolloutMutationCustody> {
    authority.admit().map_err(|_| anyhow::anyhow!("{context}"))
}

#[test]
fn counter_boundaries_leave_state_and_wake_signal_unchanged() {
    let saturated = RolloutMutationAuthority::new();
    {
        let mut state = saturated.inner.lock_state();
        state.in_flight = usize::MAX;
        assert_eq!(saturated.inner.in_flight.send_replace(usize::MAX), 0);
    }
    assert!(matches!(saturated.admit(), Err(MutationAdmissionError::CounterOverflow)));
    assert_authority_snapshot(
        &saturated,
        /*admission_closed*/ false,
        /*in_flight*/ usize::MAX,
    );

    let empty = RolloutMutationAuthority::new();
    assert_eq!(empty.inner.release_custody(), Err(MutationCountUnderflow));
    assert_authority_snapshot(&empty, /*admission_closed*/ false, /*in_flight*/ 0);
}

#[tokio::test]
async fn revoke_waits_for_every_custody_and_wakes_every_waiter() -> anyhow::Result<()> {
    let authority = RolloutMutationAuthority::new();
    let custody = admit(&authority, "rollout mutation should be admitted")?;
    let mut mutation = ParkedMutation::spawn(custody);
    mutation.wait_until_parked().await?;

    let (first_sleeping_tx, first_sleeping) = oneshot::channel();
    let (first_woke_tx, first_woke) = oneshot::channel();
    let first_authority = authority.clone();
    let first_revoker = tokio::spawn(async move {
        let mut revoke = Box::pin(first_authority.revoke());
        let sleeping = is_pending(revoke.as_mut()).await;
        let _ = first_sleeping_tx.send(sleeping);
        if sleeping {
            revoke.await;
            let _ = first_woke_tx.send(());
        }
    });
    let (second_sleeping_tx, second_sleeping) = oneshot::channel();
    let (second_woke_tx, second_woke) = oneshot::channel();
    let second_authority = authority.clone();
    let second_revoker = tokio::spawn(async move {
        let mut revoke = Box::pin(second_authority.revoke());
        let sleeping = is_pending(revoke.as_mut()).await;
        let _ = second_sleeping_tx.send(sleeping);
        if sleeping {
            revoke.await;
            let _ = second_woke_tx.send(());
        }
    });

    assert!(
        with_diagnostic_deadline("waiting for the first sleeping revoker", first_sleeping)
            .await??
    );
    assert!(
        with_diagnostic_deadline("waiting for the second sleeping revoker", second_sleeping)
            .await??
    );
    mutation.release().await?;
    with_diagnostic_deadline("waiting for the first revoker wake", first_woke).await??;
    with_diagnostic_deadline("waiting for the second revoker wake", second_woke).await??;
    with_diagnostic_deadline("waiting for the first revoker", first_revoker).await??;
    with_diagnostic_deadline("waiting for the second revoker", second_revoker).await??;
    Ok(())
}

#[tokio::test]
async fn close_counts_every_admission_and_retains_the_final_release() -> anyhow::Result<()> {
    let authority = RolloutMutationAuthority::new();
    let first_custody = admit(&authority, "first rollout mutation should be admitted")?;
    let second_custody = admit(&authority, "interleaved rollout mutation should be admitted")?;

    let mut revoke = Box::pin(authority.revoke());
    assert!(is_pending(revoke.as_mut()).await);
    assert!(matches!(authority.admit(), Err(MutationAdmissionError::AdmissionClosed)));
    drop(first_custody);
    assert!(is_pending(revoke.as_mut()).await);

    drop(second_custody);
    with_diagnostic_deadline("observing the retained final release", revoke).await?;
    assert_authority_snapshot(&authority, /*admission_closed*/ true, /*in_flight*/ 0);
    with_diagnostic_deadline("repeating one-way revocation", authority.revoke()).await?;
    assert!(matches!(authority.admit(), Err(MutationAdmissionError::AdmissionClosed)));
    Ok(())
}

#[tokio::test]
async fn poisoned_mutex_recovers_when_custody_releases_during_unwind() -> anyhow::Result<()> {
    let authority = RolloutMutationAuthority::new();
    let custody = admit(&authority, "rollout mutation should be admitted")?;
    let poison_target = authority.inner.clone();
    let (completed_tx, completed) = mpsc::channel();
    let poisoner = thread::spawn(move || {
        let _completion = CompletionSignal(Some(completed_tx));
        let _state = match poison_target.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        panic!("poison rollout mutation state");
    });
    let poison_cleanup = WorkerCleanup { completed, worker: poisoner };
    assert!(await_worker_cleanup("waiting for mutex poisoner", poison_cleanup).await.is_err());

    let unwind = std::panic::catch_unwind(AssertUnwindSafe(move || {
        let _custody = custody;
        panic!("release custody while unwinding");
    }));
    assert!(unwind.is_err());

    with_diagnostic_deadline("revoking after poison recovery", authority.revoke()).await?;
    assert_authority_snapshot(&authority, /*admission_closed*/ true, /*in_flight*/ 0);
    Ok(())
}

#[tokio::test]
async fn parked_mutation_drop_releases_custody_without_blocking() -> anyhow::Result<()> {
    let authority = RolloutMutationAuthority::new();
    let custody = admit(&authority, "rollout mutation should be admitted")?;
    let mut mutation = ParkedMutation::spawn(custody);
    mutation.wait_until_parked().await?;

    let mut revoke = Box::pin(authority.revoke());
    assert!(is_pending(revoke.as_mut()).await);
    drop(mutation);
    with_diagnostic_deadline("waiting for drop fallback custody release", revoke).await?;
    Ok(())
}
