use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

use tokio::sync::watch;

/// Revocable admission for filesystem work that can outlive a cancelled caller.
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
        if state.admission_closed {
            return Err(MutationAdmissionError::AdmissionClosed);
        }
        let next = state
            .in_flight
            .checked_add(1)
            .ok_or(MutationAdmissionError::CounterOverflow)?;
        let previous = state.in_flight;
        state.in_flight = next;
        self.inner.publish_count(previous, next);
        Ok(RolloutMutationCustody {
            authority: Arc::clone(&self.inner),
        })
    }

    pub(crate) async fn revoke(&self) {
        let mut in_flight = self.inner.in_flight.subscribe();
        self.inner.lock_state().admission_closed = true;
        loop {
            if *in_flight.borrow_and_update() == 0 {
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

    fn publish_count(&self, previous: usize, next: usize) {
        assert_eq!(
            self.in_flight.send_replace(next),
            previous,
            "rollout mutation count drift"
        );
    }

    fn release_custody(&self) {
        let mut state = self.lock_state();
        let previous = state.in_flight;
        let next = previous
            .checked_sub(1)
            .expect("rollout mutation custody underflow");
        state.in_flight = next;
        self.publish_count(previous, next);
    }
}

impl Drop for RolloutMutationCustody {
    fn drop(&mut self) {
        self.authority.release_custody();
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::task::Poll;

    use super::*;

    #[tokio::test]
    async fn revoke_drains_exact_admitted_mutation_and_closes_future_admission() {
        let authority = RolloutMutationAuthority::new();
        let custody = authority.admit().expect("admit mutation");
        let mut revoke = Box::pin(authority.revoke());
        std::future::poll_fn(|cx| match revoke.as_mut().poll(cx) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(()) => panic!("revoke completed before admitted mutation drained"),
        })
        .await;
        drop(custody);
        revoke.await;
        assert_eq!(
            authority.admit().err(),
            Some(MutationAdmissionError::AdmissionClosed)
        );
    }
}
