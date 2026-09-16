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
use crate::migrations::runtime_queue_migrator;
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
use codex_history::RolloutItem;
use codex_protocol::ThreadId;
use serde_json::Value;
use sqlx::QueryBuilder;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::SqliteConnection;
use sqlx::SqlitePool;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::time::Duration;
use std::time::Instant;
use tracing::warn;

mod backfill;
mod configured_identity_provenance;
mod extension_storage;
mod external_agent_config_imports;
mod goal_execution_lease;
mod goals;
mod logs;
mod memories;
mod memory_versions;
pub(crate) mod migration_repair;
mod phase2_attestation;
mod projects;
mod queued_items;
mod recovery;
mod remote_control;
mod rollout_migration;
#[cfg(test)]
pub(crate) mod test_support;
mod thread_attachments;
mod thread_section_order;
mod thread_sections;
mod threads;
pub mod usage;

pub use configured_identity_provenance::ConfiguredIdentityProvenance;
pub use external_agent_config_imports::ExternalAgentConfigImportDetailsRecord;
pub use external_agent_config_imports::ExternalAgentConfigImportFailureRecord;
pub use external_agent_config_imports::ExternalAgentConfigImportHistoryRecord;
pub use external_agent_config_imports::ExternalAgentConfigImportSuccessRecord;
pub use goal_execution_lease::GoalExecutionLease;
pub use goal_execution_lease::GoalExecutionLeaseError;
pub use goals::GoalAccountingMode;
pub use goals::GoalAccountingOutcome;
pub use goals::GoalStore;
pub use goals::GoalUpdate;
pub use memories::MemoryStore;
pub use queued_items::SqliteQueueStore;
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
#[derive(Clone)]
pub struct StateRuntime {
    sqlite: SqliteConfig,
    default_provider: String,
    pool: Arc<sqlx::SqlitePool>,
    logs_pool: Arc<sqlx::SqlitePool>,
    usage_pool: Arc<sqlx::SqlitePool>,
    thread_goals: GoalStore,
    memories: MemoryStore,
    memories_v2: Arc<tokio::sync::OnceCell<MemoryStore>>,
    thread_queue: SqliteQueueStore,
    thread_updated_at_millis: Arc<AtomicI64>,
    thread_recency_at_millis: Arc<AtomicI64>,
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
        let state_migrator = runtime_state_migrator();
        let logs_migrator = runtime_logs_migrator();
        let usage_migrator = runtime_usage_migrator();
        let goals_migrator = runtime_goals_migrator();
        let memories_migrator = runtime_memories_migrator();
        let queue_migrator = runtime_queue_migrator();
        let queue_path = sqlite.queue_db_path();
        let has_memories_v2 = tokio::fs::try_exists(sqlite.memories_v2_db_path()).await?;
        // The legacy state database can still own thread_goals until canonical
        // migration 34 drops that table. Prepare the destination first so the
        // state bridge can copy user rows before it permits that drop.
        let goals_pool = match sqlite
            .open_goals_db(&goals_migrator, telemetry_override)
            .await
        {
            Ok(db) => Arc::new(db),
            Err(err) => {
                warn!("failed to open goals db at {}: {err}", goals_path.display());
                return Err(err);
            }
        };
        let pool = match sqlite
            .open_state_db(&state_migrator, telemetry_override, goals_pool.as_ref())
            .await
        {
            Ok(db) => Arc::new(db),
            Err(err) => {
                warn!("failed to open state db at {}: {err}", state_path.display());
                goals_pool.close().await;
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
            close_sqlite_pools(&[pool.as_ref(), goals_pool.as_ref()]).await;
            return Err(err);
        }
        let logs_pool = match sqlite
            .open_logs_db(&logs_migrator, telemetry_override)
            .await
        {
            Ok(db) => Arc::new(db),
            Err(err) => {
                warn!("failed to open logs db at {}: {err}", logs_path.display());
                close_sqlite_pools(&[pool.as_ref(), goals_pool.as_ref()]).await;
                return Err(err);
            }
        };
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
        let queue_pool = match sqlite
            .open_queue_db(&queue_migrator, telemetry_override)
            .await
        {
            Ok(db) => Arc::new(db),
            Err(err) => {
                warn!("failed to open queue db at {}: {err}", queue_path.display());
                close_sqlite_pools(&[
                    pool.as_ref(),
                    logs_pool.as_ref(),
                    goals_pool.as_ref(),
                    memories_pool.as_ref(),
                    usage_pool.as_ref(),
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
                queue_pool.as_ref(),
            ])
            .await;
            return Err(err);
        }
        let started = Instant::now();
        let thread_timestamp_millis_result: anyhow::Result<(Option<i64>, Option<i64>)> =
            sqlx::query_as(
                "SELECT (SELECT MAX(updated_at_ms) FROM threads), (SELECT MAX(recency_at_ms) FROM threads)",
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
                        queue_pool.as_ref(),
                    ])
                    .await;
                    return Err(err);
                }
            };
        let thread_updated_at_millis = thread_updated_at_millis.unwrap_or(0);
        let thread_recency_at_millis = thread_recency_at_millis.unwrap_or(0);
        let runtime = Arc::new(Self {
            thread_goals: GoalStore::new(Arc::clone(&goals_pool)),
            memories: MemoryStore::new(Arc::clone(&memories_pool), Arc::clone(&pool)),
            memories_v2: Arc::new(tokio::sync::OnceCell::new()),
            thread_queue: SqliteQueueStore::new(queue_pool),
            pool,
            logs_pool,
            usage_pool,
            sqlite,
            default_provider,
            thread_updated_at_millis: Arc::new(AtomicI64::new(thread_updated_at_millis)),
            thread_recency_at_millis: Arc::new(AtomicI64::new(thread_recency_at_millis)),
        });
        // Existing v2 state must participate in startup corruption recovery.
        // Keep creation lazy for users who have never used v2.
        if has_memories_v2
            && let Err(err) = runtime
                .memories_for_version(codex_protocol::MemoryVersion::V2)
                .await
        {
            runtime.close().await;
            return Err(err);
        }
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

    /// Try to acquire the explicit execution lease for a thread goal.
    pub async fn try_acquire_goal_execution_lease(
        &self,
        thread_id: ThreadId,
    ) -> Result<GoalExecutionLease, GoalExecutionLeaseError> {
        let goals_db_path = self
            .sqlite
            .goals_db_path()
            .canonicalize()
            .map_err(GoalExecutionLeaseError::Io)?;
        GoalExecutionLease::acquire(&goals_db_path, thread_id)
    }

    pub fn memories(&self) -> &MemoryStore {
        &self.memories
    }

    /// Return the durable, SQLite-backed user-message queue.
    pub fn thread_queue(&self) -> &SqliteQueueStore {
        &self.thread_queue
    }

    /// Close all SQLite pools and wait for outstanding pool workers to exit.
    pub async fn close(&self) {
        self.thread_queue.close().await;
        self.memories.close().await;
        if let Some(memories) = self.memories_v2.get() {
            memories.close().await;
        }
        self.thread_goals.close().await;
        self.usage_pool.close().await;
        self.logs_pool.close().await;
        self.pool.close().await;
    }

    pub async fn clear_memory_data_in_sqlite_home(sqlite: &SqliteConfig) -> anyhow::Result<bool> {
        let mut cleared = false;
        for version in [
            codex_protocol::MemoryVersion::V1,
            codex_protocol::MemoryVersion::V2,
        ] {
            let path = match version {
                codex_protocol::MemoryVersion::V1 => sqlite.memories_db_path(),
                codex_protocol::MemoryVersion::V2 => sqlite.memories_v2_db_path(),
            };
            if !tokio::fs::try_exists(path).await? {
                continue;
            }
            let pool = match version {
                codex_protocol::MemoryVersion::V1 => {
                    sqlite
                        .open_memories_db(
                            &runtime_memories_migrator(),
                            /*telemetry_override*/ None,
                        )
                        .await?
                }
                codex_protocol::MemoryVersion::V2 => sqlite.open_memories_v2_db().await?,
            };
            let result = memories::clear_memory_data_in_pool(&pool).await;
            pool.close().await;
            result?;
            cleared = true;
        }
        Ok(cleared)
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

/// Integrity-check rows, including those emitted before interruption.
#[derive(Debug, Eq, PartialEq)]
pub enum SqliteIntegrityCheck {
    Complete(Vec<String>),
    TimedOut(Vec<String>),
}

/// Run SQLite's built-in integrity check against an existing database file.
pub async fn sqlite_integrity_check(
    sqlite: &SqliteConfig,
    path: &Path,
    deadline: Option<Instant>,
) -> anyhow::Result<SqliteIntegrityCheck> {
    let pool = sqlite
        .open_read_only_pool(
            path,
            deadline.map(|deadline| deadline.saturating_duration_since(Instant::now())),
        )
        .await?;
    let mut connection = pool.acquire().await?;
    if let Some(deadline) = deadline {
        // Lock waits do not invoke the progress handler; share the remaining budget.
        QueryBuilder::<Sqlite>::new("PRAGMA busy_timeout = ")
            .push(
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis(),
            )
            .build()
            .execute(&mut *connection)
            .await?;
        // Interrupt SQLite itself: dropping a timed-out future leaves its worker scanning.
        connection
            .lock_handle()
            .await?
            .set_progress_handler(/*num_ops*/ 1_000, move || Instant::now() < deadline);
    }
    let mut rows = Vec::<String>::new();
    // Keep corruption rows even when a later step interrupts the scan.
    let result = sqlx::query::<Sqlite>("PRAGMA integrity_check")
        .try_map(|row| {
            rows.push(row.try_get(/*index*/ 0)?);
            Ok(())
        })
        .fetch_all(&mut *connection)
        .await;
    drop(connection);
    pool.close().await;
    match result {
        Ok(_) => Ok(SqliteIntegrityCheck::Complete(rows)),
        Err(sqlx::Error::Database(error))
            if deadline.is_some()
                && error.code().is_some_and(|code| {
                    matches!(
                        code.parse::<i32>().ok().map(|code| code & 0xff),
                        Some(libsqlite3_sys::SQLITE_INTERRUPT | libsqlite3_sys::SQLITE_BUSY)
                    )
                }) =>
        {
            Ok(SqliteIntegrityCheck::TimedOut(rows))
        }
        Err(error) => Err(error.into()),
    }
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
    use super::SqliteIntegrityCheck;
    use super::StateRuntime;
    use super::runtime_state_migrator;
    use super::sqlite_integrity_check;
    use super::test_support::test_thread_metadata;
    use super::test_support::unique_temp_dir;
    use crate::DB_INIT_METRIC;
    use crate::DbTelemetry;
    use crate::migrations::STATE_MIGRATOR;
    use codex_protocol::ThreadId;
    use codex_utils_absolute_path::test_support::PathExt;
    use pretty_assertions::assert_eq;
    use sqlx::SqlitePool;
    use sqlx::migrate::MigrateError;
    use sqlx::migrate::Migrator;
    use std::borrow::Cow;
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    use std::io;
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use std::time::Instant;

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

        fn histogram(&self, _name: &str, _value: i64, _tags: &[(&str, &str)]) {}
    }

    fn tags_to_map(tags: &[(&str, &str)]) -> BTreeMap<String, String> {
        tags.iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    async fn open_db_pool(path: &Path) -> SqlitePool {
        crate::SqliteConfig::new_for_testing(path.parent().unwrap_or(path).abs())
            .open_read_write_pool(path)
            .await
            .expect("open sqlite pool")
    }

    #[tokio::test]
    async fn sqlite_integrity_check_can_be_interrupted_and_retried() {
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
        sqlx::query("CREATE TABLE sample (id INTEGER PRIMARY KEY, value INTEGER, other INTEGER)")
            .execute(&pool)
            .await
            .expect("create sample table");
        sqlx::query("CREATE UNIQUE INDEX sample_value ON sample(value)")
            .execute(&pool)
            .await
            .expect("create sample index");
        sqlx::query(
            "WITH RECURSIVE rows(id) AS (SELECT 1 UNION ALL SELECT id + 1 FROM rows WHERE id < 2048) INSERT INTO sample SELECT id, id, CASE WHEN id = 1 THEN 0 ELSE id END FROM rows",
        )
        .execute(&pool)
        .await
        .expect("populate enough rows to invoke the progress handler");
        pool.close().await;

        assert_eq!(
            sqlite_integrity_check(&sqlite, &path, Some(Instant::now()))
                .await
                .expect("interrupt integrity check"),
            SqliteIntegrityCheck::TimedOut(Vec::new()),
        );

        let result = sqlite_integrity_check(&sqlite, &path, /*deadline*/ None)
            .await
            .expect("integrity check should run");

        assert_eq!(
            result,
            SqliteIntegrityCheck::Complete(vec!["ok".to_string()])
        );

        let pool = sqlite
            .open_read_write_pool(&path)
            .await
            .expect("reopen sqlite db");
        let mut connection = pool.acquire().await.expect("acquire writer");
        // WAL readers do not wait on this writer, so use a rollback journal.
        sqlx::query("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE")
            .execute(&mut *connection)
            .await
            .expect("hold an exclusive lock");
        let started = Instant::now();
        let result =
            sqlite_integrity_check(&sqlite, &path, Some(started + Duration::from_millis(50)))
                .await
                .expect("interrupt lock wait");
        let elapsed = started.elapsed();
        sqlx::query("ROLLBACK")
            .execute(&mut *connection)
            .await
            .expect("release lock");
        assert_eq!(result, SqliteIntegrityCheck::TimedOut(Vec::new()));
        assert!(elapsed < Duration::from_secs(1), "lock wait: {elapsed:?}");

        // Misdescribe one index entry so SQLite emits an error before its first progress callback.
        sqlx::query(
            "PRAGMA writable_schema=ON; UPDATE sqlite_schema SET sql='CREATE UNIQUE INDEX sample_value ON sample(other)' WHERE name='sample_value'; PRAGMA writable_schema=OFF",
        )
        .execute(&mut *connection)
        .await
        .expect("introduce an index mismatch");
        drop(connection);
        pool.close().await;
        assert_eq!(
            sqlite_integrity_check(&sqlite, &path, Some(Instant::now()))
                .await
                .expect("retain corruption before interruption"),
            SqliteIntegrityCheck::TimedOut(vec![
                "row 1 missing from index sample_value".to_string()
            ]),
        );
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
        let goals_pool = sqlite
            .open_goals_db(
                &crate::migrations::runtime_goals_migrator(),
                /*telemetry_override*/ None,
            )
            .await
            .expect("open goals database before state");
        let tolerant_pool = sqlite
            .open_state_db(
                &tolerant_migrator,
                /*telemetry_override*/ None,
                &goals_pool,
            )
            .await
            .expect("runtime migrator should tolerate newer applied migrations");
        tolerant_pool.close().await;
        goals_pool.close().await;

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
            "open_queue",
            "migrate_queue",
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

    #[tokio::test]
    async fn init_restores_independent_thread_timestamp_maxima() {
        let codex_home = unique_temp_dir();
        let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
        let runtime = StateRuntime::init(sqlite.clone(), "test-provider".to_string())
            .await
            .expect("state runtime should initialize");

        for (thread_id, updated_at_ms, recency_at_ms) in [
            ("00000000-0000-0000-0000-000000000101", 3_000, 1_000),
            ("00000000-0000-0000-0000-000000000102", 1_000, 4_000),
        ] {
            let thread_id = ThreadId::from_string(thread_id).expect("valid thread id");
            runtime
                .upsert_thread(&test_thread_metadata(
                    &codex_home,
                    thread_id,
                    codex_home.clone(),
                ))
                .await
                .expect("thread should be stored");
            sqlx::query("UPDATE threads SET updated_at_ms = ?, recency_at_ms = ? WHERE id = ?")
                .bind(updated_at_ms)
                .bind(recency_at_ms)
                .bind(thread_id.to_string())
                .execute(runtime.pool.as_ref())
                .await
                .expect("thread timestamps should be updated");
        }

        runtime.close().await;
        drop(runtime);

        let runtime = StateRuntime::init(sqlite, "test-provider".to_string())
            .await
            .expect("state runtime should restore thread timestamps");
        assert_eq!(
            (
                runtime.thread_updated_at_millis.load(Ordering::Relaxed),
                runtime.thread_recency_at_millis.load(Ordering::Relaxed),
            ),
            (3_000, 4_000)
        );

        runtime.close().await;
        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }
}
