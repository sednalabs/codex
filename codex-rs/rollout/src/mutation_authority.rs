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

#[derive(Default)]
struct AuthorityState {
    admission_closed: bool,
    in_flight: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MutationAdmissionClosed;

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

    pub(crate) fn admit(&self) -> Result<RolloutMutationCustody, MutationAdmissionClosed> {
        let mut state = self.inner.lock_state();
        if state.admission_closed {
            return Err(MutationAdmissionClosed);
        }

        let previous = state.in_flight;
        state.in_flight += 1;
        let observed_previous = self.inner.in_flight.send_replace(state.in_flight);
        debug_assert_eq!(observed_previous, previous);

        Ok(RolloutMutationCustody {
            authority: Arc::clone(&self.inner),
        })
    }

    pub(crate) async fn revoke(&self) {
        let mut in_flight = self.inner.in_flight.subscribe();
        self.inner.lock_state().admission_closed = true;

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

impl AuthorityInner {
    fn lock_state(&self) -> MutexGuard<'_, AuthorityState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn release_custody(&self) {
        let mut state = self.lock_state();
        let previous = state.in_flight;
        debug_assert!(state.in_flight > 0);
        state.in_flight -= 1;
        let observed_previous = self.in_flight.send_replace(state.in_flight);
        debug_assert_eq!(observed_previous, previous);
    }
}

impl Drop for RolloutMutationCustody {
    fn drop(&mut self) {
        self.authority.release_custody();
    }
}

#[cfg(test)]
#[path = "mutation_authority_tests.rs"]
mod tests;
