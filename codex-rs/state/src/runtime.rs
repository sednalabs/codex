use crate::LogEntry;
use crate::LogQuery;
use crate::LogRow;
use crate::SortKey;
use crate::SqliteConfig;
use crate::ThreadMetadata;
use crate::ThreadMetadataBuilder;
use crate::ThreadsPage;
use crate::apply_rollout_item;
use crate::migrations::runtime_goals_migrator;
use crate::migrations::runtime_logs_migrator;
use crate::migrations::runtime_memories_migrator;
use crate::migrations::runtime_state_migrator;
use crate::migrations::runtime_thread_history_migrator;
use crate::migrations::runtime_usage_migrator;
use crate::model::ThreadRow;
use crate::model::anchor_from_item;
use crate::model::datetime_to_epoch_millis;
use crate::model::datetime_to_epoch_seconds;
use crate::model::epoch_millis_to_datetime;
use crate::paths::file_modified_time_utc;
use crate::telemetry::DbKind;
use crate::telemetry::DbTelemetry;
use chrono::DateTime;
use chrono::Utc;
use codex_extension_api::ExtensionStorageId;
use codex_protocol::ThreadId;
use codex_protocol::protocol::RolloutItem;
use serde_json::Value;
use sqlx::QueryBuilder;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::SqliteConnection;
use sqlx::SqlitePool;
use std::collections::BTreeSet;
use std::fs::File;
use std::fs::OpenOptions;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use tracing::warn;
use uuid::Uuid;

mod backfill;
mod configured_identity_provenance;
mod extension_storage;
mod external_agent_config_imports;
mod goal_owner_admissions;
mod goals;
mod logs;
mod memories;
pub(crate) mod migration_repair;
mod phase2_attestation;
mod recovery;
mod remote_control;
#[cfg(test)]
pub(crate) mod test_support;
mod threads;
pub mod usage;

pub use configured_identity_provenance::ConfiguredIdentityProvenance;
pub use external_agent_config_imports::ExternalAgentConfigImportDetailsRecord;
pub use external_agent_config_imports::ExternalAgentConfigImportFailureRecord;
pub use external_agent_config_imports::ExternalAgentConfigImportHistoryRecord;
pub use external_agent_config_imports::ExternalAgentConfigImportSuccessRecord;
pub use goal_owner_admissions::GoalOwnerAdmissionAccountContextFingerprint;
pub use goal_owner_admissions::GoalOwnerAdmissionAcquireResult;
pub use goal_owner_admissions::GoalOwnerAdmissionAuthority;
pub use goal_owner_admissions::GoalOwnerAdmissionContinuationAuthority;
pub use goal_owner_admissions::GoalOwnerAdmissionDenialClass;
pub use goal_owner_admissions::GoalOwnerAdmissionLease;
pub use goal_owner_admissions::GoalOwnerAdmissionObservation;
pub use goal_owner_admissions::GoalOwnerAdmissionPhase;
pub use goal_owner_admissions::GoalOwnerAdmissionRecord;
pub use goal_owner_admissions::GoalOwnerAdmissionRetirementReason;
pub use goal_owner_admissions::GoalOwnerAdmissionStore;
pub use goal_owner_admissions::GoalOwnerAdmissionTerminalDisposition;
pub use goal_owner_admissions::GoalOwnerAdmissionTerminalOutcome;
pub use goal_owner_admissions::GoalOwnerDispatchFenceCapability;
pub use goal_owner_admissions::canonical_provider_id;
pub use goals::GoalAccountingMode;
pub use goals::GoalAccountingOutcome;
pub use goals::GoalStore;
pub use goals::GoalUpdate;
pub use memories::MemoryStore;
pub use recovery::RuntimeDbBackup;
pub(super) use recovery::RuntimeDbInitError;
pub use recovery::backup_runtime_db_for_fresh_start;
pub use recovery::is_sqlite_corruption_error;
pub use recovery::runtime_db_path_for_corruption_error;
pub use recovery::sqlite_error_detail_is_corruption;
pub use recovery::sqlite_error_detail_is_lock;
pub use remote_control::RemoteControlEnrollmentRecord;
pub use threads::ThreadFilterOptions;

// "Partition" is the retained-log-content bucket we cap at 10 MiB:
// - one bucket per non-null thread_id
// - one bucket per threadless (thread_id IS NULL) non-null process_uuid
// - one bucket for threadless rows with process_uuid IS NULL
// This budget tracks each row's persisted rendered log body plus non-body
// metadata, rather than the exact sum of all persisted SQLite column bytes.
const LOG_PARTITION_SIZE_LIMIT_BYTES: i64 = 10 * 1024 * 1024;
const LOG_PARTITION_ROW_LIMIT: i64 = 1_000;
const STATE_DB_BASENAME: &str = "state";
const LOGS_DB_BASENAME: &str = "logs";
const USAGE_DB_BASENAME: &str = "usage";

/// One opaque, revocable capability shared by coupled goal/admission stores.
/// A lock loser receives no capability and is therefore unable to mutate
/// either side of the continuation protocol.
#[derive(Debug)]
pub(crate) struct RuntimeOwnerCapability {
    revoked: AtomicBool,
    active: AtomicUsize,
    idle: tokio::sync::Notify,
}

pub(crate) struct RuntimeOwnerCapabilityGuard {
    capability: Arc<RuntimeOwnerCapability>,
    // Keep the process-lifetime lock alive for the full mutation guard, even
    // if the StateRuntime handle itself is dropped while the mutation waits.
    _owner_lease: Option<Arc<RuntimeOwnerLease>>,
}

impl RuntimeOwnerCapability {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            revoked: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            idle: tokio::sync::Notify::new(),
        })
    }

    fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
    }

    async fn wait_quiescent(&self) {
        loop {
            // Register the notification before sampling the counter so a
            // final guard release cannot race the waiter and leave it asleep
            // after the capability has become quiescent.
            let notified = self.idle.notified();
            if self.active.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }

    fn enter(
        self: &Arc<Self>,
        owner_lease: Option<Arc<RuntimeOwnerLease>>,
    ) -> anyhow::Result<RuntimeOwnerCapabilityGuard> {
        if self.revoked.load(Ordering::Acquire) {
            anyhow::bail!("runtime owner capability has been revoked")
        }
        self.active.fetch_add(1, Ordering::AcqRel);
        if self.revoked.load(Ordering::Acquire) {
            self.active.fetch_sub(1, Ordering::AcqRel);
            self.idle.notify_waiters();
            anyhow::bail!("runtime owner capability has been revoked")
        }
        Ok(RuntimeOwnerCapabilityGuard {
            capability: Arc::clone(self),
            _owner_lease: owner_lease,
        })
    }

    fn is_active(&self) -> bool {
        !self.revoked.load(Ordering::Acquire)
    }
}

impl Drop for RuntimeOwnerCapabilityGuard {
    fn drop(&mut self) {
        if self.capability.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.capability.idle.notify_waiters();
        }
    }
}

struct RuntimeOwnerLease {
    store: GoalOwnerAdmissionStore,
    owner_id: Uuid,
    capability: Arc<RuntimeOwnerCapability>,
    _process_lock: RuntimeProcessLock,
    released: AtomicBool,
}

/// Process-lifetime locks used for goal-runtime ownership. The v2 identity
/// lock is the canonical cross-alias lock; the database and adjacent locks
/// are retained as bridges for pathname replacement and older runtimes that
/// only acquired `goals_*.runtime.lock`.
struct RuntimeProcessLock {
    _v2_identity: File,
    _database: File,
    _adjacent: File,
    database_dev: u64,
    database_ino: u64,
}

impl RuntimeProcessLock {
    #[cfg(unix)]
    fn path_matches(&self, goals_path: &Path) -> std::io::Result<bool> {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(goals_path)?;
        Ok(metadata.dev() == self.database_dev && metadata.ino() == self.database_ino)
    }

    #[cfg(not(unix))]
    fn path_matches(&self, _goals_path: &Path) -> std::io::Result<bool> {
        Ok(false)
    }
}

impl RuntimeOwnerLease {
    async fn release(&self) {
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        self.capability.revoke();
        self.capability.wait_quiescent().await;
        if let Err(error) = self.store.release_runtime_owner(self.owner_id).await {
            warn!(%error, "failed to release durable runtime owner");
        }
    }
}

impl Drop for RuntimeOwnerLease {
    fn drop(&mut self) {
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        // Drop is synchronous and must not schedule a stale asynchronous
        // owner-row write after pools or a replacement runtime are live. The
        // kernel releases the process lock; the next owner replaces the stale
        // durable row only after acquiring that lock.
        self.capability.revoke();
    }
}

/// Acquire an OS lock whose lifetime is exactly the owning process/runtime.
/// Unix advisory locks are released by the kernel on process death, which is
/// the death proof required before replacing the durable owner audit row.
#[cfg(unix)]
fn try_acquire_runtime_process_lock(
    goals_path: &Path,
) -> anyhow::Result<Option<RuntimeProcessLock>> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    let database = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(goals_path)?;
    // Keep the v2 lock identity byte-for-byte compatible: it is keyed by the
    // device/inode pair in the system temporary directory, so hard-link
    // aliases converge on the same lock before any migration is attempted.
    let metadata = database.metadata()?;
    let lock_name = format!(
        ".codex-goals-runtime-{:x}-{:x}.lock",
        metadata.dev(),
        metadata.ino()
    );
    let v2_identity = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(std::env::temp_dir().join(lock_name))?;
    let result = unsafe { libc::flock(v2_identity.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Ok(None);
        }
        return Err(error.into());
    }

    // The identity lock alone cannot prevent a replacement at the configured
    // pathname from being treated as a new database. Couple it to the file
    // and the historical adjacent lock so a live predecessor remains visible
    // across rename/recreate and mixed-version upgrades.
    let result = unsafe { libc::flock(database.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Ok(None);
        }
        return Err(error.into());
    }
    let adjacent_path = goals_path.with_extension("runtime.lock");
    let adjacent = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(adjacent_path)?;
    let adjacent_result =
        unsafe { libc::flock(adjacent.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if adjacent_result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Ok(None);
        }
        return Err(error.into());
    }
    Ok(Some(RuntimeProcessLock {
        _v2_identity: v2_identity,
        _database: database,
        _adjacent: adjacent,
        database_dev: metadata.dev(),
        database_ino: metadata.ino(),
    }))
}

#[cfg(not(unix))]
fn try_acquire_runtime_process_lock(
    _goals_path: &Path,
) -> anyhow::Result<Option<RuntimeProcessLock>> {
    // Do not claim recovery authority without a process-lifetime lock/death
    // proof on platforms where this implementation has not been verified.
    Ok(None)
}

#[derive(Clone)]
pub struct StateRuntime {
    sqlite: SqliteConfig,
    default_provider: String,
    pool: Arc<sqlx::SqlitePool>,
    logs_pool: Arc<sqlx::SqlitePool>,
    usage_pool: Arc<sqlx::SqlitePool>,
    thread_goals: GoalStore,
    goal_owner_admissions: GoalOwnerAdmissionStore,
    memories: MemoryStore,
    thread_updated_at_millis: Arc<AtomicI64>,
    thread_recency_at_millis: Arc<AtomicI64>,
    runtime_owner: Option<Arc<RuntimeOwnerLease>>,
}

impl StateRuntime {
    /// Initialize the state runtime using the provided SQLite configuration and default provider.
    ///
    /// This opens (and migrates) the SQLite databases under the configured
    /// `sqlite_home`.
    /// Logs and paginated thread history live in dedicated files to reduce
    /// lock contention with the rest of the state store.
    pub async fn init(sqlite: SqliteConfig, default_provider: String) -> anyhow::Result<Arc<Self>> {
        Self::init_inner(sqlite, default_provider, /*telemetry_override*/ None).await
    }

    #[cfg(test)]
    pub(crate) async fn init_with_telemetry_for_tests(
        sqlite: SqliteConfig,
        default_provider: String,
        telemetry_override: &dyn DbTelemetry,
    ) -> anyhow::Result<Arc<Self>> {
        Self::init_inner(sqlite, default_provider, Some(telemetry_override)).await
    }

    async fn init_inner(
        sqlite: SqliteConfig,
        default_provider: String,
        telemetry_override: Option<&dyn DbTelemetry>,
    ) -> anyhow::Result<Arc<Self>> {
        tokio::fs::create_dir_all(sqlite.home()).await?;
        let state_path = sqlite.state_db_path();
        let logs_path = sqlite.logs_db_path();
        let goals_path = sqlite.goals_db_path();
        let memories_path = sqlite.memories_db_path();
        let usage_path = sqlite.usage_db_path();
        // Ownership arbitration must precede legacy cleanup and goals-schema
        // migration. A losing runtime may inspect the existing DB read-only,
        // but it must not mutate it while another owner holds the lock.
        let process_lock = try_acquire_runtime_process_lock(&goals_path)?;
        if process_lock.is_some() {
            remove_legacy_db_files(
                sqlite.home(),
                database_filename(&state_path)?,
                STATE_DB_BASENAME,
                "state",
            )
            .await;
            remove_legacy_db_files(
                sqlite.home(),
                database_filename(&logs_path)?,
                LOGS_DB_BASENAME,
                "logs",
            )
            .await;
            remove_legacy_db_files(
                sqlite.home(),
                database_filename(&usage_path)?,
                USAGE_DB_BASENAME,
                "usage",
            )
            .await;
        }
        let state_migrator = runtime_state_migrator();
        let logs_migrator = runtime_logs_migrator();
        let usage_migrator = runtime_usage_migrator();
        let goals_migrator = runtime_goals_migrator();
        let memories_migrator = runtime_memories_migrator();
        let pool = match sqlite
            .open_state_db(&state_migrator, telemetry_override)
            .await
        {
            Ok(db) => Arc::new(db),
            Err(err) => {
                warn!("failed to open state db at {}: {err}", state_path.display());
                return Err(err);
            }
        };
        let started = Instant::now();
        let extension_migrations_result =
            extension_storage::run_state_extension_migrations(pool.as_ref()).await;
        crate::telemetry::record_init_result(
            telemetry_override,
            DbKind::State,
            extension_storage::state_extension_migration_phase(),
            started.elapsed(),
            &extension_migrations_result,
        );
        if let Err(err) = extension_migrations_result {
            close_sqlite_pools(&[pool.as_ref()]).await;
            return Err(err);
        }
        let logs_pool = match sqlite
            .open_logs_db(&logs_migrator, telemetry_override)
            .await
        {
            Ok(db) => Arc::new(db),
            Err(err) => {
                warn!("failed to open logs db at {}: {err}", logs_path.display());
                close_sqlite_pools(&[pool.as_ref()]).await;
                return Err(err);
            }
        };
        let goals_pool_result = if process_lock.is_some() {
            if !process_lock
                .as_ref()
                .expect("process lock present")
                .path_matches(&goals_path)?
            {
                close_sqlite_pools(&[pool.as_ref(), logs_pool.as_ref()]).await;
                return Err(anyhow::anyhow!(
                    "goal database pathname changed after ownership lock acquisition"
                ));
            }
            sqlite
                .open_goals_db(&goals_migrator, telemetry_override)
                .await
        } else {
            sqlite
                .open_read_only_pool(&goals_path)
                .await
                .map_err(Into::into)
        };
        let goals_pool = match goals_pool_result {
            Ok(db) => Arc::new(db),
            Err(err) => {
                warn!("failed to open goals db at {}: {err}", goals_path.display());
                close_sqlite_pools(&[pool.as_ref(), logs_pool.as_ref()]).await;
                return Err(err);
            }
        };
        if let Some(process_lock) = process_lock.as_ref() {
            if !process_lock.path_matches(&goals_path)? {
                close_sqlite_pools(&[pool.as_ref(), logs_pool.as_ref(), goals_pool.as_ref()]).await;
                return Err(anyhow::anyhow!(
                    "goal database pathname changed during ownership-bound open"
                ));
            }
        }
        let memories_pool = match sqlite
            .open_memories_db(&memories_migrator, telemetry_override)
            .await
        {
            Ok(db) => Arc::new(db),
            Err(err) => {
                warn!(
                    "failed to open memories db at {}: {err}",
                    memories_path.display()
                );
                close_sqlite_pools(&[pool.as_ref(), logs_pool.as_ref(), goals_pool.as_ref()]).await;
                return Err(err);
            }
        };
        let usage_pool = match sqlite
            .open_usage_db(&usage_migrator, telemetry_override)
            .await
        {
            Ok(db) => Arc::new(db),
            Err(err) => {
                warn!("failed to open usage db at {}: {err}", usage_path.display());
                close_sqlite_pools(&[
                    pool.as_ref(),
                    logs_pool.as_ref(),
                    goals_pool.as_ref(),
                    memories_pool.as_ref(),
                ])
                .await;
                return Err(err);
            }
        };
        let started = Instant::now();
        let backfill_state_result = ensure_backfill_state_row_in_pool(pool.as_ref()).await;
        crate::telemetry::record_init_result(
            telemetry_override,
            DbKind::State,
            "ensure_backfill_state",
            started.elapsed(),
            &backfill_state_result,
        );
        if let Err(err) = backfill_state_result {
            close_sqlite_pools(&[
                pool.as_ref(),
                logs_pool.as_ref(),
                goals_pool.as_ref(),
                usage_pool.as_ref(),
                memories_pool.as_ref(),
            ])
            .await;
            return Err(err);
        }
        let started = Instant::now();
        let thread_timestamp_millis_result: anyhow::Result<(Option<i64>, Option<i64>)> =
            sqlx::query_as(
                "SELECT MAX(threads.updated_at_ms), MAX(threads.recency_at_ms) FROM threads",
            )
            .fetch_one(pool.as_ref())
            .await
            .map_err(anyhow::Error::from);
        crate::telemetry::record_init_result(
            telemetry_override,
            DbKind::State,
            "post_init_query",
            started.elapsed(),
            &thread_timestamp_millis_result,
        );
        let (thread_updated_at_millis, thread_recency_at_millis) =
            match thread_timestamp_millis_result {
                Ok(value) => value,
                Err(err) => {
                    close_sqlite_pools(&[
                        pool.as_ref(),
                        logs_pool.as_ref(),
                        goals_pool.as_ref(),
                        usage_pool.as_ref(),
                        memories_pool.as_ref(),
                    ])
                    .await;
                    return Err(err);
                }
            };
        let thread_updated_at_millis = thread_updated_at_millis.unwrap_or(0);
        let thread_recency_at_millis = thread_recency_at_millis.unwrap_or(0);
        let owner_capability = RuntimeOwnerCapability::new();
        let bootstrap_goal_owner_admissions = GoalOwnerAdmissionStore::with_capability(
            Arc::clone(&goals_pool),
            Arc::clone(&owner_capability),
            None,
        );
        let runtime_owner = if let Some(process_lock) = process_lock {
            let owner_id = Uuid::now_v7();
            if let Err(err) = bootstrap_goal_owner_admissions
                .claim_runtime_owner(owner_id)
                .await
            {
                close_sqlite_pools(&[
                    pool.as_ref(),
                    logs_pool.as_ref(),
                    goals_pool.as_ref(),
                    usage_pool.as_ref(),
                    memories_pool.as_ref(),
                ])
                .await;
                return Err(err);
            }
            if let Err(err) = bootstrap_goal_owner_admissions
                .recover_in_flight_on_open_as_owner(owner_id)
                .await
            {
                let _ = bootstrap_goal_owner_admissions
                    .release_runtime_owner(owner_id)
                    .await;
                close_sqlite_pools(&[
                    pool.as_ref(),
                    logs_pool.as_ref(),
                    goals_pool.as_ref(),
                    usage_pool.as_ref(),
                    memories_pool.as_ref(),
                ])
                .await;
                return Err(err);
            }
            Some(Arc::new(RuntimeOwnerLease {
                store: bootstrap_goal_owner_admissions.clone(),
                owner_id,
                capability: Arc::clone(&owner_capability),
                _process_lock: process_lock,
                released: AtomicBool::new(false),
            }))
        } else {
            owner_capability.revoke();
            warn!(
                "another StateRuntime owns the goals database or process lock is unavailable; admission recovery is disabled"
            );
            None
        };
        let capability = runtime_owner
            .as_ref()
            .map(|owner| Arc::clone(&owner.capability));
        let goal_owner_admissions = if let Some(capability) = capability.clone() {
            GoalOwnerAdmissionStore::with_capability(
                Arc::clone(&goals_pool),
                capability,
                runtime_owner.clone(),
            )
        } else {
            GoalOwnerAdmissionStore::read_only(Arc::clone(&goals_pool))
        };
        let runtime = Arc::new(Self {
            thread_goals: GoalStore::with_capability(
                Arc::clone(&goals_pool),
                capability,
                runtime_owner.clone(),
            ),
            goal_owner_admissions,
            memories: MemoryStore::new(Arc::clone(&memories_pool), Arc::clone(&pool)),
            pool,
            logs_pool,
            usage_pool,
            sqlite,
            default_provider,
            thread_updated_at_millis: Arc::new(AtomicI64::new(thread_updated_at_millis)),
            thread_recency_at_millis: Arc::new(AtomicI64::new(thread_recency_at_millis)),
            runtime_owner,
        });
        if let Err(err) = runtime.run_logs_startup_maintenance().await {
            warn!("logs startup maintenance failed; continuing runtime initialization: {err}");
        }
        Ok(runtime)
    }

    /// Return the SQLite configuration for this runtime.
    pub fn sqlite(&self) -> &SqliteConfig {
        &self.sqlite
    }

    pub fn usage_pool(&self) -> Arc<SqlitePool> {
        Arc::clone(&self.usage_pool)
    }

    pub fn extension_storage_pool(
        &self,
        storage_id: ExtensionStorageId,
    ) -> Option<Arc<SqlitePool>> {
        if storage_id == extension_storage::USAGE_LEDGER_STORAGE_ID {
            return Some(Arc::clone(&self.usage_pool));
        }
        if storage_id == extension_storage::PHASE2_ATTESTATION_STORAGE_ID {
            return Some(Arc::clone(&self.pool));
        }
        None
    }

    pub(crate) fn usage_ledger_pool(&self) -> Arc<SqlitePool> {
        self.extension_storage_pool(extension_storage::USAGE_LEDGER_STORAGE_ID)
            .unwrap_or_else(|| Arc::clone(&self.usage_pool))
    }

    pub(crate) fn phase2_attestation_pool(&self) -> Arc<SqlitePool> {
        self.extension_storage_pool(extension_storage::PHASE2_ATTESTATION_STORAGE_ID)
            .unwrap_or_else(|| Arc::clone(&self.pool))
    }

    pub fn thread_goals(&self) -> &GoalStore {
        &self.thread_goals
    }

    /// Durable authority ledger for one exact goal-owner admission per thread.
    pub fn goal_owner_admissions(&self) -> &GoalOwnerAdmissionStore {
        &self.goal_owner_admissions
    }

    /// Whether this runtime holds the process-lifetime goal database lease.
    /// Callers must gate recovery and admission mutations on this capability;
    /// a read-only runtime may still inspect durable state for diagnostics.
    pub fn owns_goal_runtime(&self) -> bool {
        self.runtime_owner.is_some()
    }

    pub fn memories(&self) -> &MemoryStore {
        &self.memories
    }

    /// Close all SQLite pools and wait for outstanding pool workers to exit.
    pub async fn close(&self) {
        if let Some(runtime_owner) = &self.runtime_owner {
            runtime_owner.release().await;
        }
        self.memories.close().await;
        self.thread_goals.close().await;
        self.usage_pool.close().await;
        self.logs_pool.close().await;
        self.pool.close().await;
    }

    pub async fn clear_memory_data_in_sqlite_home(sqlite: &SqliteConfig) -> anyhow::Result<bool> {
        let memories_path = sqlite.memories_db_path();
        if !tokio::fs::try_exists(&memories_path).await? {
            return Ok(false);
        }

        let memories_migrator = runtime_memories_migrator();
        let pool = sqlite
            .open_memories_db(&memories_migrator, /*telemetry_override*/ None)
            .await?;
        memories::clear_memory_data_in_pool(&pool).await?;
        pool.close().await;
        Ok(true)
    }
}

async fn close_sqlite_pools(pools: &[&SqlitePool]) {
    for pool in pools {
        pool.close().await;
    }
}

/// Open and migrate the rebuildable paginated thread-history database.
pub async fn open_thread_history_db(sqlite: &SqliteConfig) -> anyhow::Result<SqlitePool> {
    let migrator = runtime_thread_history_migrator();
    sqlite
        .open_thread_history_db(&migrator, /*telemetry_override*/ None)
        .await
}

pub(super) async fn ensure_backfill_state_row_in_pool(
    pool: &sqlx::SqlitePool,
) -> anyhow::Result<()> {
    // Eagerly check if the operation would have no effect to avoid blocking waiting for a SQLite
    // writer for no reason in the hot startup path.
    if sqlx::query_scalar::<_, i64>("SELECT 1 FROM backfill_state WHERE id = 1")
        .fetch_optional(pool)
        .await?
        .is_some()
    {
        return Ok(());
    }

    sqlx::query(
        r#"
INSERT INTO backfill_state (id, status, last_watermark, last_success_at, updated_at)
VALUES (?, ?, NULL, NULL, ?)
ON CONFLICT(id) DO NOTHING
            "#,
    )
    .bind(1_i64)
    .bind(crate::BackfillStatus::Pending.as_str())
    .bind(Utc::now().timestamp())
    .execute(pool)
    .await?;
    Ok(())
}

/// Run SQLite's built-in integrity check against an existing database file.
pub async fn sqlite_integrity_check(
    sqlite: &SqliteConfig,
    path: &Path,
) -> anyhow::Result<Vec<String>> {
    let pool = sqlite.open_read_only_pool(path).await?;
    let rows = sqlx::query_scalar::<_, String>("PRAGMA integrity_check")
        .fetch_all(&pool)
        .await?;
    pool.close().await;
    Ok(rows)
}

fn database_filename(path: &Path) -> anyhow::Result<&str> {
    path.file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("database path has no UTF-8 filename: {}", path.display()))
}

async fn remove_legacy_db_files(
    codex_home: &Path,
    current_name: &str,
    base_name: &str,
    db_label: &str,
) {
    let mut entries = match tokio::fs::read_dir(codex_home).await {
        Ok(entries) => entries,
        Err(err) => {
            warn!(
                "failed to read codex_home for {db_label} db cleanup {}: {err}",
                codex_home.display(),
            );
            return;
        }
    };
    let mut legacy_paths = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        if !entry
            .file_type()
            .await
            .map(|file_type| file_type.is_file())
            .unwrap_or(false)
        {
            continue;
        }
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if !should_remove_db_file(file_name.as_ref(), current_name, base_name) {
            continue;
        }

        legacy_paths.push(entry.path());
    }

    // On Windows, SQLite can keep the main database file undeletable until the
    // matching `-wal` / `-shm` sidecars are removed. Remove the longest
    // sidecar-style paths first so the main file is attempted last.
    legacy_paths.sort_by_key(|path| std::cmp::Reverse(path.as_os_str().len()));
    for legacy_path in legacy_paths {
        let mut result = tokio::fs::remove_file(&legacy_path).await;
        for _ in 0..3 {
            if result.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
            result = tokio::fs::remove_file(&legacy_path).await;
        }
        if let Err(err) = result {
            warn!(
                "failed to remove legacy {db_label} db file {}: {err}",
                legacy_path.display(),
            );
        }
    }
}

fn should_remove_db_file(file_name: &str, current_name: &str, base_name: &str) -> bool {
    let mut normalized_name = file_name;
    for suffix in ["-wal", "-shm", "-journal"] {
        if let Some(stripped) = file_name.strip_suffix(suffix) {
            normalized_name = stripped;
            break;
        }
    }
    if normalized_name == current_name {
        return false;
    }
    let unversioned_name = format!("{base_name}.sqlite");
    if normalized_name == unversioned_name {
        return true;
    }

    let Some(version_with_extension) = normalized_name.strip_prefix(&format!("{base_name}_"))
    else {
        return false;
    };
    let Some(version_suffix) = version_with_extension.strip_suffix(".sqlite") else {
        return false;
    };
    !version_suffix.is_empty() && version_suffix.chars().all(|ch| ch.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::StateRuntime;
    use super::runtime_state_migrator;
    use super::sqlite_integrity_check;
    use super::test_support::unique_temp_dir;
    #[cfg(unix)]
    use super::try_acquire_runtime_process_lock;
    use crate::DB_INIT_METRIC;
    use crate::DbTelemetry;
    use crate::migrations::STATE_MIGRATOR;
    use codex_utils_absolute_path::test_support::PathExt;
    use pretty_assertions::assert_eq;
    use sqlx::SqlitePool;
    use sqlx::migrate::MigrateError;
    use sqlx::migrate::Migrator;
    use std::borrow::Cow;
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    #[cfg(unix)]
    use std::fs;
    use std::io;
    #[cfg(unix)]
    use std::os::fd::AsRawFd;
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::Duration;

    #[derive(Default)]
    struct TestTelemetry {
        counters: Mutex<Vec<MetricEvent>>,
    }

    #[derive(Debug, Eq, PartialEq)]
    struct MetricEvent {
        name: String,
        tags: BTreeMap<String, String>,
    }

    impl TestTelemetry {
        fn counters(&self) -> Vec<MetricEvent> {
            self.counters
                .lock()
                .expect("telemetry lock")
                .iter()
                .map(|event| MetricEvent {
                    name: event.name.clone(),
                    tags: event.tags.clone(),
                })
                .collect()
        }
    }

    impl DbTelemetry for TestTelemetry {
        fn counter(&self, name: &str, _inc: i64, tags: &[(&str, &str)]) {
            self.counters
                .lock()
                .expect("telemetry lock")
                .push(MetricEvent {
                    name: name.to_string(),
                    tags: tags_to_map(tags),
                });
        }

        fn record_duration(
            &self,
            _name: &str,
            _duration: std::time::Duration,
            _tags: &[(&str, &str)],
        ) {
        }
    }

    fn tags_to_map(tags: &[(&str, &str)]) -> BTreeMap<String, String> {
        tags.iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn runtime_process_lock_is_alias_stable_across_hardlinks() {
        let root = unique_temp_dir();
        let first_parent = root.join("first");
        let second_parent = root.join("second");
        fs::create_dir_all(&first_parent).expect("create first lock-test directory");
        fs::create_dir_all(&second_parent).expect("create second lock-test directory");
        let first = first_parent.join("goals.sqlite");
        let alias = second_parent.join("goals-alias.sqlite");
        fs::File::create(&first).expect("create goals database fixture");
        fs::hard_link(&first, &alias).expect("create hardlink alias");

        let lock = try_acquire_runtime_process_lock(&first)
            .expect("acquire first database alias")
            .expect("first alias should own the process lock");
        assert!(
            try_acquire_runtime_process_lock(&alias)
                .expect("probe hardlink alias")
                .is_none(),
            "a hardlink alias must resolve to the same process lock"
        );
        drop(lock);
        assert!(
            try_acquire_runtime_process_lock(&alias)
                .expect("reacquire hardlink alias")
                .is_some(),
            "the alias should acquire only after the first lock is released"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn runtime_process_lock_respects_legacy_adjacent_lock() {
        let root = unique_temp_dir();
        fs::create_dir_all(&root).expect("create legacy-lock test directory");
        let goals = root.join("goals.sqlite");
        fs::File::create(&goals).expect("create goals database fixture");
        let adjacent = goals.with_extension("runtime.lock");
        let legacy = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&adjacent)
            .expect("create legacy adjacent lock");
        let result = unsafe { libc::flock(legacy.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(result, 0, "acquire legacy adjacent lock fixture");
        assert!(
            try_acquire_runtime_process_lock(&goals)
                .expect("probe legacy adjacent lock")
                .is_none(),
            "new ownership must respect the parent DB-adjacent lock"
        );
        drop(legacy);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn runtime_process_lock_rejects_pathname_replacement_while_owner_is_live() {
        let root = unique_temp_dir();
        fs::create_dir_all(&root).expect("create replacement-lock test directory");
        let goals = root.join("goals.sqlite");
        let moved = root.join("goals.sqlite.previous");
        fs::File::create(&goals).expect("create goals database fixture");

        let lock = try_acquire_runtime_process_lock(&goals)
            .expect("acquire original database lock")
            .expect("original database should own the process lock");
        fs::rename(&goals, &moved).expect("move live database pathname");
        fs::File::create(&goals).expect("create replacement database pathname");

        assert!(
            try_acquire_runtime_process_lock(&goals)
                .expect("probe replacement pathname")
                .is_none(),
            "a replacement pathname must not bypass the live adjacent lock bridge"
        );

        drop(lock);
        assert!(
            try_acquire_runtime_process_lock(&goals)
                .expect("reacquire replacement pathname")
                .is_some(),
            "the replacement pathname should acquire after the predecessor exits"
        );
        let _ = fs::remove_dir_all(root);
    }

    async fn open_db_pool(path: &Path) -> SqlitePool {
        crate::SqliteConfig::new_for_testing(path.parent().unwrap_or(path).abs())
            .open_read_write_pool(path)
            .await
            .expect("open sqlite pool")
    }

    #[tokio::test]
    async fn sqlite_integrity_check_reports_ok_for_valid_db() {
        let codex_home = unique_temp_dir();
        tokio::fs::create_dir_all(&codex_home)
            .await
            .expect("create codex home");
        let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
        let path = sqlite.state_db_path();
        let pool = sqlite
            .open_read_write_pool(&path)
            .await
            .expect("open sqlite db");
        sqlx::query("CREATE TABLE sample (id INTEGER PRIMARY KEY)")
            .execute(&pool)
            .await
            .expect("create sample table");
        pool.close().await;

        let result = sqlite_integrity_check(&sqlite, &path)
            .await
            .expect("integrity check should run");

        assert_eq!(result, vec!["ok".to_string()]);
        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn cloned_store_keeps_runtime_owner_lock_until_it_is_dropped() {
        let codex_home = unique_temp_dir();
        let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
        let runtime = StateRuntime::init(sqlite.clone(), "test-provider".to_string())
            .await
            .expect("initialize owning runtime");
        assert!(runtime.owns_goal_runtime());
        let cloned_store = runtime.goal_owner_admissions().clone();
        drop(runtime);

        let blocked_runtime = StateRuntime::init(sqlite.clone(), "test-provider".to_string())
            .await
            .expect("initialize read-only successor while clone is live");
        assert!(
            !blocked_runtime.owns_goal_runtime(),
            "a cloned store must retain the process lock after its runtime is dropped"
        );
        blocked_runtime.close().await;
        drop(cloned_store);

        let successor = StateRuntime::init(sqlite.clone(), "test-provider".to_string())
            .await
            .expect("initialize successor after cloned store drops");
        assert!(successor.owns_goal_runtime());
        successor.close().await;
        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn open_state_sqlite_tolerates_newer_applied_migrations() {
        let codex_home = unique_temp_dir();
        tokio::fs::create_dir_all(&codex_home)
            .await
            .expect("create codex home");
        let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
        let state_path = sqlite.state_db_path();
        let pool = sqlite
            .open_read_write_pool(&state_path)
            .await
            .expect("open state db");
        STATE_MIGRATOR
            .run(&pool)
            .await
            .expect("apply current state schema");
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(9_999_i64)
        .bind("future migration")
        .bind(true)
        .bind(vec![1_u8, 2, 3, 4])
        .bind(1_i64)
        .execute(&pool)
        .await
        .expect("insert future migration record");
        pool.close().await;

        let strict_pool = open_db_pool(state_path.as_path()).await;
        let strict_err = STATE_MIGRATOR
            .run(&strict_pool)
            .await
            .expect_err("strict migrator should reject newer applied migrations");
        assert!(matches!(strict_err, MigrateError::VersionMissing(9_999)));
        strict_pool.close().await;

        let tolerant_migrator = runtime_state_migrator();
        let tolerant_pool = sqlite
            .open_state_db(&tolerant_migrator, /*telemetry_override*/ None)
            .await
            .expect("runtime migrator should tolerate newer applied migrations");
        tolerant_pool.close().await;

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn open_state_sqlite_marks_existing_thread_source_migration_applied() {
        let codex_home = unique_temp_dir();
        tokio::fs::create_dir_all(&codex_home)
            .await
            .expect("create codex home");
        let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
        let state_path = sqlite.state_db_path();
        let pool = sqlite
            .open_read_write_pool(&state_path)
            .await
            .expect("open state db");
        let partial_migrator = Migrator {
            migrations: Cow::Owned(
                STATE_MIGRATOR
                    .iter()
                    .filter(|migration| migration.version <= 32)
                    .cloned()
                    .collect(),
            ),
            ignore_missing: STATE_MIGRATOR.ignore_missing,
            locking: STATE_MIGRATOR.locking,
            no_tx: STATE_MIGRATOR.no_tx,
            table_name: STATE_MIGRATOR.table_name.clone(),
            create_schemas: STATE_MIGRATOR.create_schemas.clone(),
        };
        partial_migrator
            .run(&pool)
            .await
            .expect("apply state schema before thread_source migration");
        sqlx::query("ALTER TABLE threads ADD COLUMN thread_source TEXT")
            .execute(&pool)
            .await
            .expect("simulate column applied without migration record");
        pool.close().await;

        let strict_pool = open_db_pool(state_path.as_path()).await;
        let strict_err = STATE_MIGRATOR
            .run(&strict_pool)
            .await
            .expect_err("strict migrator should try to add the existing column again");
        assert!(strict_err.to_string().contains("duplicate column name"));
        strict_pool.close().await;

        let tolerant_migrator = runtime_state_migrator();
        let tolerant_pool = sqlite
            .open_state_db(&tolerant_migrator, /*telemetry_override*/ None)
            .await
            .expect("runtime migrator should repair the missing migration record");

        let applied: (String, bool, Vec<u8>) = sqlx::query_as(
            "SELECT description, success, checksum FROM _sqlx_migrations WHERE version = 33",
        )
        .fetch_one(&tolerant_pool)
        .await
        .expect("migration 33 should be recorded");
        let migration = tolerant_migrator
            .iter()
            .find(|migration| migration.version == 33)
            .expect("embedded migration 33");
        assert_eq!(
            applied,
            (
                migration.description.to_string(),
                true,
                migration.checksum.as_ref().to_vec(),
            )
        );
        tolerant_pool.close().await;

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    async fn remove_dir_all_with_retry(path: &Path) -> io::Result<()> {
        let mut last_err = None;
        for attempt in 0..5 {
            match tokio::fs::remove_dir_all(path).await {
                Ok(()) => return Ok(()),
                Err(err) if attempt < 4 => {
                    last_err = Some(err);
                    tokio::time::sleep(Duration::from_millis(25 * (attempt + 1) as u64)).await;
                }
                Err(err) => return Err(err),
            }
        }

        Err(last_err.unwrap_or_else(|| io::Error::other("cleanup retry loop exhausted")))
    }

    #[tokio::test]
    async fn init_removes_legacy_logs_and_usage_db_files() {
        let codex_home = unique_temp_dir();
        tokio::fs::create_dir_all(&codex_home)
            .await
            .expect("create codex_home");

        let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
        let current_logs_name = sqlite
            .logs_db_path()
            .file_name()
            .expect("logs database filename")
            .to_owned();
        let current_usage_name = sqlite
            .usage_db_path()
            .file_name()
            .expect("usage database filename")
            .to_owned();
        let unversioned_logs_name = "logs.sqlite";
        let unversioned_usage_name = "usage.sqlite";

        for suffix in ["", "-wal", "-shm", "-journal"] {
            let legacy_logs_path = codex_home.join(format!("{unversioned_logs_name}{suffix}"));
            tokio::fs::write(legacy_logs_path, b"legacy")
                .await
                .expect("write legacy logs file");
            let old_logs_path = codex_home.join(format!("logs_1.sqlite{suffix}"));
            tokio::fs::write(old_logs_path, b"old_logs")
                .await
                .expect("write old logs file");
            let legacy_usage_path = codex_home.join(format!("{unversioned_usage_name}{suffix}"));
            tokio::fs::write(legacy_usage_path, b"legacy")
                .await
                .expect("write legacy usage file");
            let old_usage_path = codex_home.join(format!("usage_0.sqlite{suffix}"));
            tokio::fs::write(old_usage_path, b"old_usage")
                .await
                .expect("write old usage file");
        }

        let logs_backup_path = codex_home.join("logs.sqlite_backup");
        tokio::fs::write(&logs_backup_path, b"keep")
            .await
            .expect("write logs backup");
        let usage_backup_path = codex_home.join("usage.sqlite_backup");
        tokio::fs::write(&usage_backup_path, b"keep")
            .await
            .expect("write usage backup");

        let runtime = StateRuntime::init(sqlite.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        for suffix in ["", "-wal", "-shm", "-journal"] {
            let legacy_logs_path = codex_home.join(format!("{unversioned_logs_name}{suffix}"));
            assert_eq!(
                tokio::fs::try_exists(&legacy_logs_path)
                    .await
                    .expect("check legacy logs path"),
                false
            );
            let old_logs_path = codex_home.join(format!("logs_1.sqlite{suffix}"));
            assert_eq!(
                tokio::fs::try_exists(&old_logs_path)
                    .await
                    .expect("check old logs path"),
                false
            );

            let legacy_usage_path = codex_home.join(format!("{unversioned_usage_name}{suffix}"));
            assert_eq!(
                tokio::fs::try_exists(&legacy_usage_path)
                    .await
                    .expect("check legacy usage path"),
                false
            );
            let old_usage_path = codex_home.join(format!("usage_0.sqlite{suffix}"));
            assert_eq!(
                tokio::fs::try_exists(&old_usage_path)
                    .await
                    .expect("check old usage path"),
                false
            );
        }

        assert_eq!(
            tokio::fs::try_exists(codex_home.join(current_logs_name))
                .await
                .expect("check current logs db path"),
            true
        );
        assert_eq!(
            tokio::fs::try_exists(codex_home.join(current_usage_name))
                .await
                .expect("check current usage db path"),
            true
        );
        assert_eq!(
            tokio::fs::try_exists(&logs_backup_path)
                .await
                .expect("check logs backup path"),
            true
        );
        assert_eq!(
            tokio::fs::try_exists(&usage_backup_path)
                .await
                .expect("check usage backup path"),
            true
        );

        drop(runtime);
        remove_dir_all_with_retry(&codex_home)
            .await
            .expect("failed to clean up temp directory");
    }

    #[tokio::test]
    async fn init_records_successful_sqlite_init_phases_to_explicit_telemetry() {
        let codex_home = unique_temp_dir();
        let telemetry = TestTelemetry::default();

        let runtime = StateRuntime::init_with_telemetry_for_tests(
            crate::SqliteConfig::new_for_testing(codex_home.as_path().abs()),
            "test-provider".to_string(),
            &telemetry,
        )
        .await
        .expect("state runtime should initialize");

        let phases = telemetry
            .counters()
            .into_iter()
            .filter(|event| event.name == DB_INIT_METRIC)
            .filter(|event| event.tags.get("status").map(String::as_str) == Some("success"))
            .filter_map(|event| event.tags.get("phase").cloned())
            .collect::<BTreeSet<_>>();
        let expected = [
            "open_state",
            "repair_state_migrations",
            "migrate_state",
            "migrate_state_extensions",
            "open_logs",
            "migrate_logs",
            "open_goals",
            "migrate_goals",
            "open_memories",
            "migrate_memories",
            "open_usage",
            "migrate_usage",
            "ensure_backfill_state",
            "post_init_query",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
        assert_eq!(phases, expected);

        runtime.close().await;
        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }
}
