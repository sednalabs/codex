use super::MutationHook;
use super::RolloutMutationAuthority;
use super::lock_unpoisoned;

pub(crate) fn set_after_acquire_hook(authority: &RolloutMutationAuthority, hook: MutationHook) {
    *lock_unpoisoned(&authority.inner.after_acquire) = Some(hook);
}

pub(crate) fn is_revoked(authority: &RolloutMutationAuthority) -> bool {
    lock_unpoisoned(&authority.inner.state).revoked
}
