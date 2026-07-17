use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;

const TERMINAL_EPOCH_BIT: u64 = 1;

#[derive(Clone)]
pub(crate) struct RolloutCommandAdmission {
    inner: Arc<AdmissionInner>,
}

struct AdmissionInner {
    gate: Arc<Semaphore>,
    epoch: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RolloutCommandAdmissionError {
    Terminal,
}

/// Exclusive custody for one data command's fallible channel reservation.
///
/// The owned semaphore permit is safe to retain while channel capacity is
/// awaited. Dropping a cancelled caller releases custody without admitting a
/// command.
#[must_use = "data admission must be committed or cancelled"]
pub(crate) struct RolloutDataAdmission {
    permit: OwnedSemaphorePermit,
}

/// Exclusive custody for one terminal command's fallible channel reservation.
///
/// Terminal state is not published until `commit`, so cancellation while
/// waiting for channel capacity leaves admission active.
#[must_use = "terminal admission must be committed or cancelled"]
pub(crate) struct RolloutTerminalAdmission {
    inner: Arc<AdmissionInner>,
    active_epoch: u64,
    permit: OwnedSemaphorePermit,
}

/// Custody of a committed terminal transition.
///
/// Dropping this token fails closed. The owner must explicitly reopen
/// admission after a recoverable terminal failure or seal it after success.
#[must_use = "a terminal transition must be reopened or sealed"]
pub(crate) struct RolloutTerminalTransition {
    inner: Arc<AdmissionInner>,
    terminal_epoch: u64,
}

impl RolloutCommandAdmission {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(AdmissionInner {
                gate: Arc::new(Semaphore::new(/*permits*/ 1)),
                epoch: AtomicU64::new(0),
            }),
        }
    }

    pub(crate) async fn acquire_data(
        &self,
    ) -> Result<RolloutDataAdmission, RolloutCommandAdmissionError> {
        let (permit, _active_epoch) = self.acquire_active().await?;
        Ok(RolloutDataAdmission { permit })
    }

    pub(crate) async fn acquire_terminal(
        &self,
    ) -> Result<RolloutTerminalAdmission, RolloutCommandAdmissionError> {
        let (permit, active_epoch) = self.acquire_active().await?;
        Ok(RolloutTerminalAdmission {
            inner: Arc::clone(&self.inner),
            active_epoch,
            permit,
        })
    }

    pub(crate) fn is_terminal(&self) -> bool {
        is_terminal_epoch(self.inner.epoch.load(Ordering::Acquire))
    }

    async fn acquire_active(
        &self,
    ) -> Result<(OwnedSemaphorePermit, u64), RolloutCommandAdmissionError> {
        let Ok(permit) = Arc::clone(&self.inner.gate).acquire_owned().await else {
            unreachable!("rollout command admission never closes its semaphore");
        };
        let epoch = self.inner.epoch.load(Ordering::Acquire);
        if is_terminal_epoch(epoch) {
            return Err(RolloutCommandAdmissionError::Terminal);
        }
        Ok((permit, epoch))
    }
}

impl RolloutDataAdmission {
    /// Run the already-reserved channel transfer synchronously, then release
    /// admission custody. The closure cannot contain an await point.
    pub(crate) fn commit(self, transfer: impl FnOnce()) {
        let Self { permit } = self;
        transfer();
        drop(permit);
    }
}

impl RolloutTerminalAdmission {
    /// Publish terminal admission and synchronously transfer the already-
    /// reserved command while exclusive custody is still held.
    pub(crate) fn commit(self, transfer: impl FnOnce()) -> RolloutTerminalTransition {
        let Self {
            inner,
            active_epoch,
            permit,
        } = self;
        let Some(terminal_epoch) = active_epoch.checked_add(1) else {
            panic!("rollout command admission epoch overflow");
        };
        assert_eq!(
            inner.epoch.compare_exchange(
                active_epoch,
                terminal_epoch,
                Ordering::AcqRel,
                Ordering::Acquire,
            ),
            Ok(active_epoch),
            "rollout command admission state changed while exclusively held",
        );
        transfer();
        drop(permit);
        RolloutTerminalTransition {
            inner,
            terminal_epoch,
        }
    }
}

impl RolloutTerminalTransition {
    /// Reopen admission after the terminal consumer reports a recoverable
    /// failure. The next epoch prevents a stale transition from reopening a
    /// later terminal command.
    pub(crate) fn reopen(self) {
        let Self {
            inner,
            terminal_epoch,
        } = self;
        let Some(active_epoch) = terminal_epoch.checked_add(1) else {
            panic!("rollout command admission epoch overflow");
        };
        assert_eq!(
            inner.epoch.compare_exchange(
                terminal_epoch,
                active_epoch,
                Ordering::AcqRel,
                Ordering::Acquire,
            ),
            Ok(terminal_epoch),
            "stale terminal transition attempted to reopen command admission",
        );
    }

    /// Keep admission terminal after the consumer has completed successfully.
    pub(crate) fn seal(self) {
        assert_eq!(
            self.inner.epoch.load(Ordering::Acquire),
            self.terminal_epoch,
            "terminal transition was replaced before it was sealed",
        );
    }
}

fn is_terminal_epoch(epoch: u64) -> bool {
    epoch & TERMINAL_EPOCH_BIT == TERMINAL_EPOCH_BIT
}

#[cfg(test)]
#[path = "command_admission_tests.rs"]
mod tests;
