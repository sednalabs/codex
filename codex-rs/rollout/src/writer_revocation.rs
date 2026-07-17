use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::command_admission::RolloutCommandAdmission;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RevocationOutcome {
    pub(crate) attempt: u64,
    pub(crate) status: RevocationStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RevocationStatus {
    Revoked,
    RetryableFailure(RetainedIoError),
    TerminalFailure(RetainedIoError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RetainedIoError {
    pub(crate) kind: io::ErrorKind,
    pub(crate) message: String,
}

#[derive(Clone)]
pub(crate) struct RolloutWriterRevocation {
    admission: RolloutCommandAdmission,
    shared: Arc<Shared>,
}

struct Shared {
    start: AsyncMutex<()>,
    lifecycle: Mutex<Lifecycle>,
    writer: Mutex<Option<JoinHandle<()>>>,
}

enum Lifecycle {
    Active {
        attempts: u64,
    },
    Revoking {
        result: watch::Receiver<Option<RevocationOutcome>>,
    },
    Revoked(RevocationOutcome),
    Failed(RevocationOutcome),
}

enum Observation {
    Start(u64),
    Wait(watch::Receiver<Option<RevocationOutcome>>),
    Done(RevocationOutcome),
}

impl RolloutWriterRevocation {
    pub(crate) fn new(admission: RolloutCommandAdmission, writer: JoinHandle<()>) -> Self {
        Self {
            admission,
            shared: Arc::new(Shared {
                start: AsyncMutex::new(()),
                lifecycle: Mutex::new(Lifecycle::Active { attempts: 0 }),
                writer: Mutex::new(Some(writer)),
            }),
        }
    }

    pub(crate) async fn revoke(
        &self,
        transfer: impl FnOnce(oneshot::Sender<io::Result<()>>),
    ) -> RevocationOutcome {
        let mut transfer = Some(transfer);
        match self.observe() {
            Observation::Wait(result) => return await_result(result).await,
            Observation::Done(outcome) => return outcome,
            Observation::Start(_) => {}
        }

        let start = self.shared.start.lock().await;
        let attempts = match self.observe() {
            Observation::Wait(result) => {
                drop(start);
                return await_result(result).await;
            }
            Observation::Done(outcome) => return outcome,
            Observation::Start(attempts) => attempts,
        };
        let terminal = self
            .admission
            .acquire_terminal()
            .await
            .expect("active revocation lifecycle must own active admission");
        let attempt = attempts
            .checked_add(1)
            .expect("rollout writer revocation attempt overflow");
        let writer = lock(&self.shared.writer)
            .take()
            .expect("active revocation lifecycle must own the writer task");
        let (ack_tx, ack_rx) = oneshot::channel();
        let (result_tx, result_rx) = watch::channel(None);
        let transition = terminal.commit(|| {
            transfer
                .take()
                .expect("revocation transfer is consumed once")(ack_tx);
        });
        *lock(&self.shared.lifecycle) = Lifecycle::Revoking {
            result: result_rx.clone(),
        };
        tokio::spawn(run_attempt(
            Arc::clone(&self.shared),
            transition,
            writer,
            ack_rx,
            result_tx,
            attempt,
        ));
        drop(start);
        await_result(result_rx).await
    }

    fn observe(&self) -> Observation {
        match &*lock(&self.shared.lifecycle) {
            Lifecycle::Active { attempts } => Observation::Start(*attempts),
            Lifecycle::Revoking { result } => Observation::Wait(result.clone()),
            Lifecycle::Revoked(outcome) | Lifecycle::Failed(outcome) => {
                Observation::Done(outcome.clone())
            }
        }
    }
}

async fn run_attempt(
    shared: Arc<Shared>,
    transition: crate::command_admission::RolloutTerminalTransition,
    mut writer: JoinHandle<()>,
    mut ack: oneshot::Receiver<io::Result<()>>,
    result_tx: watch::Sender<Option<RevocationOutcome>>,
    attempt: u64,
) {
    tokio::select! {
        biased;
        ack = &mut ack => match ack {
            Ok(Ok(())) => match writer.await {
                Ok(()) => finish_terminal(shared, transition, result_tx, revoked(attempt)),
                Err(_) => finish_terminal(shared, transition, result_tx, terminal_failure(
                    attempt, "rollout writer failed after revocation acknowledgement")),
            },
            Ok(Err(err)) => {
                let outcome = RevocationOutcome {
                    attempt,
                    status: RevocationStatus::RetryableFailure(RetainedIoError {
                        kind: err.kind(),
                        message: err.to_string(),
                    }),
                };
                transition.reopen();
                *lock(&shared.writer) = Some(writer);
                let mut lifecycle = lock(&shared.lifecycle);
                *lifecycle = Lifecycle::Active { attempts: attempt };
                result_tx.send_replace(Some(outcome));
            }
            Err(_) => {
                writer.abort();
                let _ = writer.await;
                finish_terminal(shared, transition, result_tx, terminal_failure(
                    attempt, "rollout writer dropped revocation acknowledgement"));
            }
        },
        _ = &mut writer => finish_terminal(shared, transition, result_tx, terminal_failure(
            attempt, "rollout writer exited before revocation acknowledgement")),
    }
}

fn finish_terminal(
    shared: Arc<Shared>,
    transition: crate::command_admission::RolloutTerminalTransition,
    result_tx: watch::Sender<Option<RevocationOutcome>>,
    outcome: RevocationOutcome,
) {
    transition.seal();
    *lock(&shared.lifecycle) = match outcome.status {
        RevocationStatus::Revoked => Lifecycle::Revoked(outcome.clone()),
        RevocationStatus::RetryableFailure(_) => unreachable!("retryable failure cannot seal"),
        RevocationStatus::TerminalFailure(_) => Lifecycle::Failed(outcome.clone()),
    };
    result_tx.send_replace(Some(outcome));
}

fn revoked(attempt: u64) -> RevocationOutcome {
    RevocationOutcome {
        attempt,
        status: RevocationStatus::Revoked,
    }
}

fn terminal_failure(attempt: u64, message: &str) -> RevocationOutcome {
    RevocationOutcome {
        attempt,
        status: RevocationStatus::TerminalFailure(RetainedIoError {
            kind: io::ErrorKind::BrokenPipe,
            message: message.to_string(),
        }),
    }
}

async fn await_result(mut result: watch::Receiver<Option<RevocationOutcome>>) -> RevocationOutcome {
    loop {
        if let Some(outcome) = result.borrow_and_update().clone() {
            return outcome;
        }
        result
            .changed()
            .await
            .expect("revocation result publisher must outlive its attempt");
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
#[path = "writer_revocation_tests.rs"]
mod tests;
