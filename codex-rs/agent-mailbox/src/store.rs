use crate::error::MailboxResult;
use crate::schema;
use sqlx::SqlitePool;
use tokio::sync::watch;

#[derive(Clone)]
pub struct Mailbox {
    pub(crate) pool: SqlitePool,
    change_tx: watch::Sender<u64>,
}

impl Mailbox {
    /// Open the mailbox's independent `mailbox_1.sqlite` database.
    pub async fn open(config: &codex_state::SqliteConfig) -> MailboxResult<Self> {
        let path = config.home().join("mailbox_1.sqlite");
        let pool = config.open_read_write_pool(&path).await?;
        crate::schema::initialize(&pool).await?;
        let (change_tx, _) = watch::channel(0_u64);
        Ok(Self { pool, change_tx })
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
