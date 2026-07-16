use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

use tokio::sync::watch;

#[derive(Clone)]
pub(crate) struct RolloutMutationAuthority {
    inner: Arc<AuthorityInner>,
}

struct AuthorityInner {
    state: Mutex<AuthorityState>,
    in_flight: watch::Sender<usize>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct AuthorityState {
    admission_closed: bool,
    in_flight: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MutationAdmissionError {
    AdmissionClosed,
    CounterOverflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MutationCountUnderflow;

/// Custody must be moved into and retained by the actual non-cancellable
/// side-effecting continuation. Keeping it in a cancellable outer future does
/// not fence detached work after that future is dropped.
#[must_use = "dropping custody releases one admitted rollout mutation"]
pub(crate) struct RolloutMutationCustody {
    authority: Arc<AuthorityInner>,
}

impl RolloutMutationAuthority {
    pub(crate) fn new() -> Self {
        let (in_flight, _receiver) = watch::channel(0);
        Self {
            inner: Arc::new(AuthorityInner {
                state: Mutex::new(AuthorityState::default()),
                in_flight,
            }),
        }
    }

    pub(crate) fn admit(&self) -> Result<RolloutMutationCustody, MutationAdmissionError> {
        let mut state = self.inner.lock_state();
        self.inner.assert_count_synchronized(state.in_flight);
        let previous = state.in_flight;
        let next = state.admit()?;
        self.inner.publish_count(previous, next);
        Ok(RolloutMutationCustody {
            authority: Arc::clone(&self.inner),
        })
    }

    pub(crate) async fn revoke(&self) {
        let mut in_flight = self.inner.in_flight.subscribe();
        {
            let mut state = self.inner.lock_state();
            self.inner.assert_count_synchronized(state.in_flight);
            state.admission_closed = true;
        }
        loop {
            let remaining = *in_flight.borrow_and_update();
            if remaining == 0 {
                return;
            }
            if in_flight.changed().await.is_err() {
                unreachable!("rollout mutation authority owns the watch sender");
            }
        }
    }
}

impl AuthorityState {
    fn admit(&mut self) -> Result<usize, MutationAdmissionError> {
        if self.admission_closed {
            return Err(MutationAdmissionError::AdmissionClosed);
        }
        let next = self
            .in_flight
            .checked_add(1)
            .ok_or(MutationAdmissionError::CounterOverflow)?;
        self.in_flight = next;
        Ok(next)
    }

    fn release(&mut self) -> Result<usize, MutationCountUnderflow> {
        let next = self
            .in_flight
            .checked_sub(1)
            .ok_or(MutationCountUnderflow)?;
        self.in_flight = next;
        Ok(next)
    }
}

impl AuthorityInner {
    fn lock_state(&self) -> MutexGuard<'_, AuthorityState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn assert_count_synchronized(&self, state_in_flight: usize) {
        assert_eq!(
            *self.in_flight.borrow(),
            state_in_flight,
            "count signal drift"
        );
    }

    fn publish_count(&self, previous: usize, next: usize) {
        assert_eq!(
            self.in_flight.send_replace(next),
            previous,
            "count publication drift"
        );
    }

    fn release_custody(&self) -> Result<(), MutationCountUnderflow> {
        let mut state = self.lock_state();
        self.assert_count_synchronized(state.in_flight);
        let previous = state.in_flight;
        let next = state.release()?;
        self.publish_count(previous, next);
        Ok(())
    }
}

impl Drop for RolloutMutationCustody {
    fn drop(&mut self) {
        assert!(
            self.authority.release_custody().is_ok(),
            "mutation custody underflow"
        );
    }
}

#[cfg(test)]
#[path = "mutation_authority_tests.rs"]
mod tests;
