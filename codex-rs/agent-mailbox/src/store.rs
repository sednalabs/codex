use crate::error::MailboxResult;
use crate::schema;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use sqlx::SqlitePool;
use tokio::sync::watch;

#[derive(Clone)]
pub struct Mailbox {
    pub(crate) pool: SqlitePool,
    change_tx: watch::Sender<u64>,
    pub(crate) waits: Arc<Mutex<HashMap<String, crate::wait_graph::ActiveWait>>>,
    pub(crate) runtime_faults: Arc<Mutex<crate::runtime_fault::RuntimeFaults>>,
}

impl Mailbox {
    /// Open the mailbox's independent `mailbox_1.sqlite` database.
    pub async fn open(config: &codex_state::SqliteConfig) -> MailboxResult<Self> {
        let path = config.home().join("mailbox_1.sqlite");
        let pool = config.open_read_write_pool(&path).await?;
        crate::schema::initialize(&pool).await?;
        let (change_tx, _) = watch::channel(0_u64);
        Ok(Self {
            pool,
            change_tx,
            waits: Arc::new(Mutex::new(HashMap::new())),
            runtime_faults: Arc::new(Mutex::new(crate::runtime_fault::RuntimeFaults::default())),
        })
    }

    /// Subscribe before querying a snapshot to avoid missing local mailbox writes.
    /// A receiver wake is only a reason to rescan; it is not delivery proof.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.change_tx.subscribe()
    }

    pub(crate) fn notify_changed(&self) {
        self.change_tx.send_modify(|generation| {
            *generation = generation.wrapping_add(1);
        });
    }

    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}
