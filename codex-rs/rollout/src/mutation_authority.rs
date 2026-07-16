use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use tokio::sync::Notify;

const REVOKED_ERROR: &str = "rollout mutation authority has been revoked";

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Revocable authority for filesystem mutations performed while resuming a rollout.
///
/// Revocation closes admission to new mutations and waits for every mutation that already owns
/// custody to finish. Callers must acquire custody inside non-cancellable blocking work so dropping
/// the awaiting future cannot detach a mutation from the revocation boundary.
#[derive(Clone, Default)]
pub struct RolloutMutationAuthority {
    inner: Arc<RevocableAuthority>,
}

#[derive(Default)]
struct RevocableAuthority {
    state: Mutex<AuthorityState>,
    released: Notify,
    #[cfg(test)]
    after_acquire: Mutex<Option<MutationHook>>,
}

#[derive(Default)]
struct AuthorityState {
    revoked: bool,
    in_flight: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RolloutMutationKind {
    RepresentationMaterialization,
    AppendOpen,
}

#[cfg(test)]
pub(crate) type MutationHook = Arc<dyn Fn(RolloutMutationKind) + Send + Sync>;

#[cfg(test)]
#[path = "mutation_authority_test_support.rs"]
pub(crate) mod test_support;

impl RolloutMutationAuthority {
    pub fn new() -> Self {
        Self::default()
    }

    /// Revoke this authority and wait until all admitted filesystem mutations finish.
    pub async fn revoke(&self) {
        loop {
            let released = self.inner.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            let is_quiescent = {
                let mut state = lock_unpoisoned(&self.inner.state);
                state.revoked = true;
                state.in_flight == 0
            };
            if is_quiescent {
                return;
            }
            released.await;
        }
    }

    fn acquire_custody(&self, kind: RolloutMutationKind) -> io::Result<MutationCustody> {
        {
            let mut state = lock_unpoisoned(&self.inner.state);
            if state.revoked {
                return Err(io::Error::other(REVOKED_ERROR));
            }
            state.in_flight += 1;
        }
        let custody = MutationCustody {
            authority: Some(Arc::clone(&self.inner)),
        };
        #[cfg(test)]
        if let Some(hook) = lock_unpoisoned(&self.inner.after_acquire).clone() {
            hook(kind);
        }
        #[cfg(not(test))]
        let _ = kind;
        Ok(custody)
    }
}

#[derive(Clone)]
pub(crate) enum RolloutMutationPolicy {
    Unrestricted,
    Revocable(RolloutMutationAuthority),
}

impl RolloutMutationPolicy {
    pub(crate) fn acquire_custody(&self, kind: RolloutMutationKind) -> io::Result<MutationCustody> {
        match self {
            Self::Unrestricted => Ok(MutationCustody { authority: None }),
            Self::Revocable(authority) => authority.acquire_custody(kind),
        }
    }
}

pub(crate) struct MutationCustody {
    authority: Option<Arc<RevocableAuthority>>,
}

impl Drop for MutationCustody {
    fn drop(&mut self) {
        let Some(authority) = self.authority.take() else {
            return;
        };
        let should_notify = {
            let mut state = lock_unpoisoned(&authority.state);
            assert!(
                state.in_flight > 0,
                "mutation custody count must be positive while custody exists"
            );
            state.in_flight -= 1;
            state.revoked && state.in_flight == 0
        };
        if should_notify {
            authority.released.notify_waiters();
        }
    }
}
