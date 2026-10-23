use crate::error::MailboxError;
use crate::error::MailboxResult;
use crate::store::Mailbox;
use crate::types::ReturnWhen;
use codex_protocol::ThreadId;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::sync::atomic::Ordering;

const MAX_ACTIVE_WAITS: usize = 4_096;
const MAX_PEERS_PER_WAIT: usize = 100;
const MAX_WAIT_ID_BYTES: usize = 256;

#[derive(Clone)]
pub(crate) struct ActiveWait {
    owner: String,
    peers: Vec<String>,
    mode: ReturnWhen,
    has_escape: bool,
    generation: u64,
}

/// RAII handle for one transient wait dependency. Dropping it removes only the
/// exact registration; restarting the process never re-arms persisted waits.
pub struct WaitRegistration {
    wait_id: String,
    generation: u64,
    waits: Weak<Mutex<HashMap<String, ActiveWait>>>,
}

/// Short-lived custody of an input publication, not a scheduler or durable queue.
/// While an input transaction can make an old dependency stale, closed-cycle
/// claims are conservatively suspended. The adapter holds no mutex across await.
pub struct InputArrival {
    mailbox: Mailbox,
}

impl Drop for InputArrival {
    fn drop(&mut self) {
        let waits = lock(&self.mailbox.waits);
        self.mailbox
            .wait_input_generation
            .fetch_add(1, Ordering::AcqRel);
        self.mailbox
            .pending_input_arrivals
            .fetch_sub(1, Ordering::AcqRel);
        drop(waits);
        self.mailbox.notify_changed();
    }
}

impl WaitRegistration {
    pub fn wait_id(&self) -> &str {
        &self.wait_id
    }
}

impl Drop for WaitRegistration {
    fn drop(&mut self) {
        if let Some(waits) = self.waits.upgrade() {
            let mut waits = lock(&waits);
            if waits
                .get(&self.wait_id)
                .is_some_and(|wait| wait.generation == self.generation)
            {
                waits.remove(&self.wait_id);
            }
        }
    }
}

impl Mailbox {
    /// Bracket an existing native enqueue or mailbox transaction before it can
    /// make input eligible. A scalar in-flight count avoids a shadow agent registry.
    pub fn begin_input_arrival(&self) -> InputArrival {
        let mut waits = lock(&self.waits);
        waits.clear();
        self.wait_input_generation.fetch_add(1, Ordering::AcqRel);
        self.pending_input_arrivals.fetch_add(1, Ordering::AcqRel);
        drop(waits);
        InputArrival {
            mailbox: self.clone(),
        }
    }

    /// Capture before the authoritative selection/input rescan.
    pub fn wait_input_generation(&self) -> u64 {
        self.wait_input_generation.load(Ordering::Acquire)
    }

    /// Invalidate active dependency edges for an owner when eligible input is
    /// about to become available. Stale RAII handles cannot remove replacements.
    pub fn invalidate_waits(&self, owner: ThreadId) {
        let owner = owner.to_string();
        let mut waits = lock(&self.waits);
        waits.retain(|_, wait| wait.owner != owner);
    }

    /// Register only currently active peer dependencies. The caller sets
    /// `has_escape` for a finite deadline or any already-eligible input.
    /// A new registration is rejected only when the graph proves it closed.
    pub fn begin_wait(
        &self,
        wait_id: String,
        owner: ThreadId,
        peers: Vec<ThreadId>,
        mode: ReturnWhen,
        has_escape: bool,
    ) -> MailboxResult<WaitRegistration> {
        self.begin_wait_inner(
            wait_id,
            owner,
            peers,
            mode,
            has_escape,
            /*input_generation*/ None,
        )
    }

    /// Register only if no eligible-input publication raced the caller's rescan.
    pub fn begin_wait_at(
        &self,
        wait_id: String,
        owner: ThreadId,
        peers: Vec<ThreadId>,
        mode: ReturnWhen,
        has_escape: bool,
        input_generation: u64,
    ) -> MailboxResult<WaitRegistration> {
        self.begin_wait_inner(
            wait_id,
            owner,
            peers,
            mode,
            has_escape,
            Some(input_generation),
        )
    }

    fn begin_wait_inner(
        &self,
        wait_id: String,
        owner: ThreadId,
        peers: Vec<ThreadId>,
        mode: ReturnWhen,
        has_escape: bool,
        input_generation: Option<u64>,
    ) -> MailboxResult<WaitRegistration> {
        if wait_id.is_empty() || wait_id.len() > MAX_WAIT_ID_BYTES {
            return Err(MailboxError::InvalidInput(
                "invalid wait registration ID".into(),
            ));
        }
        if peers.len() > MAX_PEERS_PER_WAIT {
            return Err(MailboxError::Backpressure("wait dependency count"));
        }
        let peers = peers
            .into_iter()
            .map(|peer| peer.to_string())
            .collect::<Vec<_>>();
        if peers.iter().collect::<HashSet<_>>().len() != peers.len() {
            return Err(MailboxError::InvalidInput(
                "duplicate wait dependency".into(),
            ));
        }
        let mut waits = lock(&self.waits);
        if input_generation.is_some_and(|generation| {
            generation != self.wait_input_generation.load(Ordering::Acquire)
        }) {
            return Err(MailboxError::StaleWaitSnapshot);
        }
        let has_escape = has_escape || self.pending_input_arrivals.load(Ordering::Acquire) != 0;
        if waits.contains_key(&wait_id) {
            return Err(MailboxError::IdempotencyConflict);
        }
        if waits.len() >= MAX_ACTIVE_WAITS {
            return Err(MailboxError::Backpressure("active wait registration count"));
        }
        let generation = self.wait_generation.fetch_add(1, Ordering::Relaxed);
        waits.insert(
            wait_id.clone(),
            ActiveWait {
                owner: owner.to_string(),
                peers,
                mode,
                has_escape,
                generation,
            },
        );
        if is_closed_owner(&waits, &owner.to_string()) {
            waits.remove(&wait_id);
            return Err(MailboxError::ClosedPeerCycle);
        }
        Ok(WaitRegistration {
            wait_id,
            generation,
            waits: Arc::downgrade(&self.waits),
        })
    }
}

fn is_closed_owner(waits: &HashMap<String, ActiveWait>, owner: &str) -> bool {
    let mut by_owner: HashMap<&str, Vec<&ActiveWait>> = HashMap::new();
    for wait in waits.values() {
        by_owner.entry(wait.owner.as_str()).or_default().push(wait);
    }
    let mut closed: HashSet<&str> = by_owner
        .iter()
        .filter(|(_, owner_waits)| {
            owner_waits
                .iter()
                .all(|wait| !wait.has_escape && !wait.peers.is_empty())
        })
        .map(|(owner, _)| *owner)
        .collect();
    loop {
        let previous = closed.clone();
        let previous_len = closed.len();
        closed.retain(|candidate| {
            let Some(owner_waits) = by_owner.get(candidate) else {
                return false;
            };
            owner_waits.iter().all(|wait| match wait.mode {
                // Any completes when one alternative is ready, so every peer
                // must itself be blocked for this waiter to remain trapped.
                ReturnWhen::Any => wait
                    .peers
                    .iter()
                    .all(|peer| previous.contains(peer.as_str())),
                // All remains blocked while at least one required peer is
                // blocked; other completed peers do not break the dependency.
                ReturnWhen::All => wait
                    .peers
                    .iter()
                    .any(|peer| previous.contains(peer.as_str())),
            })
        });
        if closed.len() == previous_len {
            break;
        }
    }
    closed.contains(owner)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
