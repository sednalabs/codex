use std::future::Future;
use std::future::poll_fn;
use std::pin::Pin;
use std::sync::mpsc;
use std::task::Poll;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context;
use tokio::sync::oneshot;

use super::MutationAdmissionClosed;
use super::RolloutMutationAuthority;
use super::RolloutMutationCustody;

const DIAGNOSTIC_DEADLINE: Duration = Duration::from_secs(10);

struct ParkedMutation {
    entered: Option<oneshot::Receiver<()>>,
    release: Option<mpsc::Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl ParkedMutation {
    fn spawn(custody: RolloutMutationCustody) -> Self {
        let (entered_tx, entered) = oneshot::channel();
        let (release, release_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let _ = entered_tx.send(());
            let _ = release_rx.recv();
            drop(custody);
        });
        Self {
            entered: Some(entered),
            release: Some(release),
            worker: Some(worker),
        }
    }

    async fn wait_until_parked(&mut self) -> anyhow::Result<()> {
        let entered = self.entered.take().context("parked mutation awaited once")?;
        with_diagnostic_deadline("waiting for parked rollout mutation", entered)
            .await??;
        Ok(())
    }

    fn release(mut self) -> anyhow::Result<()> {
        self.release_worker()
    }

    fn release_worker(&mut self) -> anyhow::Result<()> {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if self.worker.take().is_some_and(|worker| worker.join().is_err()) {
            anyhow::bail!("parked rollout mutation panicked");
        }
        Ok(())
    }
}

impl Drop for ParkedMutation {
    fn drop(&mut self) {
        let result = self.release_worker();
        if !thread::panicking() {
            assert!(result.is_ok(), "parked rollout mutation panicked");
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

async fn is_pending<F>(mut future: Pin<&mut F>) -> bool
where
    F: Future<Output = ()> + ?Sized,
{
    poll_fn(move |context| {
        Poll::Ready(matches!(future.as_mut().poll(context), Poll::Pending))
    })
    .await
}

#[tokio::test]
async fn revoke_waits_for_every_custody_and_wakes_every_waiter() -> anyhow::Result<()> {
    let authority = RolloutMutationAuthority::new();
    let first_custody = authority
        .admit()
        .map_err(|_| anyhow::anyhow!("first rollout mutation should be admitted"))?;
    let second_custody = authority
        .admit()
        .map_err(|_| anyhow::anyhow!("second rollout mutation should be admitted"))?;
    let mut first_mutation = ParkedMutation::spawn(first_custody);
    let mut second_mutation = ParkedMutation::spawn(second_custody);
    first_mutation.wait_until_parked().await?;
    second_mutation.wait_until_parked().await?;

    let mut first_revoke = Box::pin(authority.revoke());
    let mut second_revoke = Box::pin(authority.revoke());
    assert!(is_pending(first_revoke.as_mut()).await);
    assert!(is_pending(second_revoke.as_mut()).await);
    assert!(matches!(authority.admit(), Err(MutationAdmissionClosed)));

    first_mutation.release()?;
    assert!(is_pending(first_revoke.as_mut()).await);
    assert!(is_pending(second_revoke.as_mut()).await);

    second_mutation.release()?;
    with_diagnostic_deadline("waiting for the first revoker", first_revoke).await?;
    with_diagnostic_deadline("waiting for the second revoker", second_revoke).await?;

    assert!(matches!(authority.admit(), Err(MutationAdmissionClosed)));
    with_diagnostic_deadline("repeating one-way revocation", authority.revoke()).await?;
    assert!(matches!(authority.admit(), Err(MutationAdmissionClosed)));
    Ok(())
}
