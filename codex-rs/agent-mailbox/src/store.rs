use crate::error::MailboxResult;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use tokio::sync::watch;

#[derive(Clone)]
pub struct Mailbox {
    // Read-before-write transactions use BEGIN IMMEDIATE so concurrent custody
    // writes cannot invalidate a deferred WAL snapshot during its write upgrade.
    pub(crate) pool: SqlitePool,
    change_tx: watch::Sender<u64>,
    pub(crate) waits: Arc<Mutex<HashMap<String, crate::wait_graph::ActiveWait>>>,
    pub(crate) wait_generation: Arc<AtomicU64>,
    pub(crate) wait_input_generation: Arc<AtomicU64>,
    pub(crate) pending_input_arrivals: Arc<AtomicUsize>,
    pub(crate) runtime_faults: Arc<Mutex<crate::runtime_fault::RuntimeFaults>>,
}

impl Mailbox {
    /// Open the mailbox's independent `mailbox_1.sqlite` database.
    pub async fn open(config: &codex_state::SqliteConfig) -> MailboxResult<Self> {
        let path = config.home().join("mailbox_1.sqlite");
        let pool = config
            .open_read_write_pool(&path)
            .await
            .map_err(|error| crate::error::MailboxError::Open(format!("{error:#}")))?;
        crate::schema::initialize(&pool).await?;
        let (change_tx, _) = watch::channel(0_u64);
        Ok(Self {
            pool,
            change_tx,
            waits: Arc::new(Mutex::new(HashMap::new())),
            wait_generation: Arc::new(AtomicU64::new(1)),
            wait_input_generation: Arc::new(AtomicU64::new(0)),
            pending_input_arrivals: Arc::new(AtomicUsize::new(0)),
            runtime_faults: Arc::new(Mutex::new(crate::runtime_fault::RuntimeFaults::default())),
        })
    }

    /// Subscribe before querying a snapshot to avoid missing local mailbox writes.
    /// A receiver wake is only a reason to rescan; it is not delivery proof.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.change_tx.subscribe()
    }

    /// Advisory wake after native producer custody changes. Consumers must rescan
    /// native eligibility and durable records; this is not a result or an ack.
    pub fn notify_native_custody_changed(&self) {
        self.notify_changed();
    }

    pub(crate) fn notify_changed(&self) {
        self.change_tx.send_modify(|generation| {
            *generation = generation.wrapping_add(1);
        });
    }
}
