use crate::AgentJob;
use crate::AgentJobCreateParams;
use crate::AgentJobItem;
use crate::AgentJobItemCreateParams;
use crate::AgentJobItemStatus;
use crate::AgentJobProgress;
use crate::AgentJobStatus;
use crate::GOALS_DB_FILENAME;
use crate::LOGS_DB_FILENAME;
use crate::LogEntry;
use crate::LogQuery;
use crate::LogRow;
use crate::MEMORIES_DB_FILENAME;
use crate::STATE_DB_FILENAME;
use crate::SortKey;
use crate::SqliteConfig;
use crate::THREAD_HISTORY_DB_FILENAME;
use crate::ThreadMetadata;
use crate::ThreadMetadataBuilder;
use crate::ThreadsPage;
use crate::USAGE_DB_FILENAME;
use crate::apply_rollout_item;
use crate::migrations::runtime_goals_migrator;
use crate::migrations::runtime_logs_migrator;
use crate::migrations::runtime_memories_migrator;
use crate::migrations::runtime_state_migrator;
use crate::migrations::runtime_thread_history_migrator;
use crate::migrations::runtime_usage_migrator;
use crate::model::AgentJobRow;
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
use codex_utils_absolute_path::AbsolutePathBuf;
use serde_json::Value;
use sqlx::QueryBuilder;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::SqliteConnection;
use sqlx::SqlitePool;
use sqlx::migrate::Migrator;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::time::Duration;
use std::time::Instant;
use tracing::warn;

mod agent_jobs;
mod backfill;
mod configured_identity_provenance;
mod extension_storage;
mod external_agent_config_imports;
mod goals;
mod logs;
mod memories;
mod migration_repair;
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
pub use goals::GoalAccountingMode;
pub use goals::GoalAccountingOutcome;
pub use goals::GoalStore;
pub use goals::GoalUpdate;
pub use memories::MemoryStore;
pub use recovery::RuntimeDbBackup;
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

const STATE_DB_CURRENT_FILENAME: &str = "state_5.sqlite";
const LOGS_DB_CURRENT_FILENAME: &str = "logs_2.sqlite";
const USAGE_DB_CURRENT_FILENAME: &str = "usage_1.sqlite";

#[derive(Clone, Copy)]
struct RuntimeDbSpec {
    label: &'static str,
    filename: &'static str,
    kind: DbKind,
    open_phase: &'static str,
    repair_phase: Option<&'static str>,
    migrate_phase: &'static str,
}

impl RuntimeDbSpec {
    fn path(self, codex_home: &Path) -> PathBuf {
        codex_home.join(self.filename)
    }
}

const STATE_DB: RuntimeDbSpec = RuntimeDbSpec {
    label: "state DB",
    filename: STATE_DB_CURRENT_FILENAME,
    kind: DbKind::State,
    open_phase: "open_state",
    repair_phase: Some("repair_state_migrations"),
    migrate_phase: "migrate_state",
};

const LOGS_DB: RuntimeDbSpec = RuntimeDbSpec {
    label: "log DB",
    filename: LOGS_DB_CURRENT_FILENAME,
    kind: DbKind::Logs,
    open_phase: "open_logs",
    repair_phase: None,
    migrate_phase: "migrate_logs",
};

const GOALS_DB: RuntimeDbSpec = RuntimeDbSpec {
    label: "goals DB",
    filename: GOALS_DB_FILENAME,
    kind: DbKind::Goals,
    open_phase: "open_goals",
    repair_phase: None,
    migrate_phase: "migrate_goals",
};

const USAGE_DB: RuntimeDbSpec = RuntimeDbSpec {
    label: "usage DB",
    filename: USAGE_DB_CURRENT_FILENAME,
    kind: DbKind::Usage,
    open_phase: "open_usage",
    repair_phase: None,
    migrate_phase: "migrate_usage",
};

const MEMORIES_DB: RuntimeDbSpec = RuntimeDbSpec {
    label: "memories DB",
    filename: MEMORIES_DB_FILENAME,
    kind: DbKind::Memories,
    open_phase: "open_memories",
    repair_phase: None,
    migrate_phase: "migrate_memories",
};

const THREAD_HISTORY_DB: RuntimeDbSpec = RuntimeDbSpec {
    label: "thread history DB",
    filename: THREAD_HISTORY_DB_FILENAME,
    kind: DbKind::ThreadHistory,
    open_phase: "open_thread_history",
    repair_phase: None,
    migrate_phase: "migrate_thread_history",
};

const RUNTIME_DBS: [RuntimeDbSpec; 6] = [
    STATE_DB,
    LOGS_DB,
    GOALS_DB,
    MEMORIES_DB,
    USAGE_DB,
    THREAD_HISTORY_DB,
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeDbPath {
    pub label: &'static str,
    pub path: PathBuf,
}

#[derive(Clone)]
pub struct StateRuntime {
    codex_home: PathBuf,
    default_provider: String,
    pool: Arc<sqlx::SqlitePool>,
    logs_pool: Arc<sqlx::SqlitePool>,
    usage_pool: Arc<sqlx::SqlitePool>,
    thread_goals: GoalStore,
    memories: MemoryStore,
    thread_updated_at_millis: Arc<AtomicI64>,
    thread_recency_at_millis: Arc<AtomicI64>,
}

impl StateRuntime {
    /// Initialize the state runtime using the provided Codex home and default provider.
    ///
    /// This opens (and migrates) the SQLite databases under `codex_home`.
    /// Logs and paginated thread history live in dedicated files to reduce
    /// lock contention with the rest of the state store.
    pub async fn init(codex_home: PathBuf, default_provider: String) -> anyhow::Result<Arc<Self>> {
        Self::init_inner(
            codex_home,
            default_provider,
            /*telemetry_override*/ None,
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn init_with_telemetry_for_tests(
        codex_home: PathBuf,
        default_provider: String,
        telemetry_override: &dyn DbTelemetry,
    ) -> anyhow::Result<Arc<Self>> {
        Self::init_inner(codex_home, default_provider, Some(telemetry_override)).await
    }

    async fn init_inner(
        codex_home: PathBuf,
        default_provider: String,
        telemetry_override: Option<&dyn DbTelemetry>,
    ) -> anyhow::Result<Arc<Self>> {
        let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(codex_home.clone())?);
        tokio::fs::create_dir_all(&codex_home).await?;
        let current_state_name = state_db_filename();
        let current_logs_name = logs_db_filename();
        remove_legacy_db_files(
            &codex_home,
            current_state_name.as_str(),
            STATE_DB_FILENAME,
            "state",
        )
        .await;
        remove_legacy_db_files(
            &codex_home,
            current_logs_name.as_str(),
            LOGS_DB_FILENAME,
            "logs",
        )
        .await;
        let usage_name = usage_db_filename();
        remove_legacy_db_files(&codex_home, usage_name.as_str(), USAGE_DB_FILENAME, "usage").await;
        let state_migrator = runtime_state_migrator();
        let logs_migrator = runtime_logs_migrator();
        let usage_migrator = runtime_usage_migrator();
        let goals_migrator = runtime_goals_migrator();
        let memories_migrator = runtime_memories_migrator();
        let state_path = STATE_DB.path(codex_home.as_path());
        let logs_path = LOGS_DB.path(codex_home.as_path());
        let goals_path = GOALS_DB.path(codex_home.as_path());
        let usage_path = USAGE_DB.path(codex_home.as_path());
        let memories_path = MEMORIES_DB.path(codex_home.as_path());
        let pool = match open_state_sqlite(
            &sqlite,
            &state_path,
            &state_migrator,
            telemetry_override,
        )
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
        let logs_pool =
            match open_logs_sqlite(&sqlite, &logs_path, &logs_migrator, telemetry_override).await {
                Ok(db) => Arc::new(db),
                Err(err) => {
                    warn!("failed to open logs db at {}: {err}", logs_path.display());
                    close_sqlite_pools(&[pool.as_ref()]).await;
                    return Err(err);
                }
            };
        let goals_pool = match open_goals_sqlite(
            &sqlite,
            &goals_path,
            &goals_migrator,
            telemetry_override,
        )
        .await
        {
            Ok(db) => Arc::new(db),
            Err(err) => {
                warn!("failed to open goals db at {}: {err}", goals_path.display());
                close_sqlite_pools(&[pool.as_ref(), logs_pool.as_ref()]).await;
                return Err(err);
            }
        };
        let memories_pool = match open_memories_sqlite(
            &sqlite,
            &memories_path,
            &memories_migrator,
            telemetry_override,
        )
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
        let usage_pool = match open_usage_sqlite(
            &sqlite,
            &usage_path,
            &usage_migrator,
            telemetry_override,
        )
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
        let runtime = Arc::new(Self {
            thread_goals: GoalStore::new(Arc::clone(&goals_pool)),
            memories: MemoryStore::new(Arc::clone(&memories_pool), Arc::clone(&pool)),
            pool,
            logs_pool,
            usage_pool,
            codex_home,
            default_provider,
            thread_updated_at_millis: Arc::new(AtomicI64::new(thread_updated_at_millis)),
            thread_recency_at_millis: Arc::new(AtomicI64::new(thread_recency_at_millis)),
        });
        if let Err(err) = runtime.run_logs_startup_maintenance().await {
            warn!("logs startup maintenance failed; continuing runtime initialization: {err}");
        }
        Ok(runtime)
    }

    /// Return the configured Codex home directory for this runtime.
    pub fn codex_home(&self) -> &Path {
        self.codex_home.as_path()
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

    pub fn memories(&self) -> &MemoryStore {
        &self.memories
    }

    /// Close all SQLite pools and wait for outstanding pool workers to exit.
    pub async fn close(&self) {
        self.memories.close().await;
        self.thread_goals.close().await;
        self.usage_pool.close().await;
        self.logs_pool.close().await;
        self.pool.close().await;
    }

    pub async fn clear_memory_data_in_sqlite_home(sqlite_home: &Path) -> anyhow::Result<bool> {
        let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(sqlite_home)?);
        let memories_path = MEMORIES_DB.path(sqlite_home);
        if !tokio::fs::try_exists(&memories_path).await? {
            return Ok(false);
        }

        let memories_migrator = runtime_memories_migrator();
        let pool = open_memories_sqlite(
            &sqlite,
            &memories_path,
            &memories_migrator,
            /*telemetry_override*/ None,
        )
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

async fn open_state_sqlite(
    sqlite: &SqliteConfig,
    path: &Path,
    migrator: &Migrator,
    telemetry_override: Option<&dyn DbTelemetry>,
) -> anyhow::Result<SqlitePool> {
    // New state DBs should use incremental auto-vacuum, but retrofitting an
    // existing DB requires a full VACUUM. Do not attempt that during process
    // startup: it is maintenance work that can contend with foreground writers.
    open_sqlite(sqlite, path, migrator, STATE_DB, telemetry_override).await
}

async fn open_logs_sqlite(
    sqlite: &SqliteConfig,
    path: &Path,
    migrator: &Migrator,
    telemetry_override: Option<&dyn DbTelemetry>,
) -> anyhow::Result<SqlitePool> {
    open_sqlite(sqlite, path, migrator, LOGS_DB, telemetry_override).await
}

async fn open_goals_sqlite(
    sqlite: &SqliteConfig,
    path: &Path,
    migrator: &Migrator,
    telemetry_override: Option<&dyn DbTelemetry>,
) -> anyhow::Result<SqlitePool> {
    open_sqlite(sqlite, path, migrator, GOALS_DB, telemetry_override).await
}

async fn open_usage_sqlite(
    sqlite: &SqliteConfig,
    path: &Path,
    migrator: &Migrator,
    telemetry_override: Option<&dyn DbTelemetry>,
) -> anyhow::Result<SqlitePool> {
    open_sqlite(sqlite, path, migrator, USAGE_DB, telemetry_override).await
}

async fn open_memories_sqlite(
    sqlite: &SqliteConfig,
    path: &Path,
    migrator: &Migrator,
    telemetry_override: Option<&dyn DbTelemetry>,
) -> anyhow::Result<SqlitePool> {
    open_sqlite(sqlite, path, migrator, MEMORIES_DB, telemetry_override).await
}

/// Open and migrate the rebuildable paginated thread-history database.
pub async fn open_thread_history_db(sqlite_home: &Path) -> anyhow::Result<SqlitePool> {
    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(sqlite_home)?);
    let migrator = runtime_thread_history_migrator();
    open_sqlite(
        &sqlite,
        thread_history_db_path(sqlite_home).as_path(),
        &migrator,
        THREAD_HISTORY_DB,
        /*telemetry_override*/ None,
    )
    .await
}

async fn open_sqlite(
    sqlite: &SqliteConfig,
    path: &Path,
    migrator: &Migrator,
    spec: RuntimeDbSpec,
    telemetry_override: Option<&dyn DbTelemetry>,
) -> anyhow::Result<SqlitePool> {
    let started = Instant::now();
    let pool_result = sqlite
        .open_read_write_pool(path)
        .await
        .map_err(anyhow::Error::from);
    crate::telemetry::record_init_result(
        telemetry_override,
        spec.kind,
        spec.open_phase,
        started.elapsed(),
        &pool_result,
    );
    let pool = pool_result
        .map_err(|source| recovery::RuntimeDbInitError::new(spec.label, "open", path, source))?;
    if let Some(repair_phase) = spec.repair_phase {
        let started = Instant::now();
        let repair_result = migration_repair::repair_state_migrations(&pool, migrator).await;
        crate::telemetry::record_init_result(
            telemetry_override,
            spec.kind,
            repair_phase,
            started.elapsed(),
            &repair_result,
        );
        if let Err(source) = repair_result {
            pool.close().await;
            return Err(
                recovery::RuntimeDbInitError::new(spec.label, "repair", path, source).into(),
            );
        }
    }
    let started = Instant::now();
    let migrate_result = migrator.run(&pool).await.map_err(anyhow::Error::from);
    crate::telemetry::record_init_result(
        telemetry_override,
        spec.kind,
        spec.migrate_phase,
        started.elapsed(),
        &migrate_result,
    );
    if let Err(source) = migrate_result {
        pool.close().await;
        return Err(recovery::RuntimeDbInitError::new(spec.label, "migrate", path, source).into());
    }
    Ok(pool)
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

pub fn state_db_filename() -> String {
    STATE_DB.filename.to_string()
}

pub fn state_db_path(codex_home: &Path) -> PathBuf {
    STATE_DB.path(codex_home)
}

pub fn logs_db_filename() -> String {
    LOGS_DB.filename.to_string()
}

pub fn logs_db_path(codex_home: &Path) -> PathBuf {
    LOGS_DB.path(codex_home)
}

pub fn goals_db_filename() -> String {
    GOALS_DB.filename.to_string()
}

pub fn goals_db_path(codex_home: &Path) -> PathBuf {
    GOALS_DB.path(codex_home)
}

pub fn memories_db_filename() -> String {
    MEMORIES_DB.filename.to_string()
}

pub fn memories_db_path(codex_home: &Path) -> PathBuf {
    MEMORIES_DB.path(codex_home)
}

pub fn thread_history_db_filename() -> String {
    THREAD_HISTORY_DB.filename.to_string()
}

pub fn thread_history_db_path(codex_home: &Path) -> PathBuf {
    THREAD_HISTORY_DB.path(codex_home)
}

pub fn runtime_db_paths(codex_home: &Path) -> Vec<RuntimeDbPath> {
    RUNTIME_DBS
        .iter()
        .map(|spec| RuntimeDbPath {
            label: spec.label,
            path: spec.path(codex_home),
        })
        .collect()
}

/// Run SQLite's built-in integrity check against an existing database file.
pub async fn sqlite_integrity_check(path: &Path) -> anyhow::Result<Vec<String>> {
    let sqlite =
        SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(path.parent().unwrap_or(path))?);
    let pool = sqlite.open_read_only_pool(path).await?;
    let rows = sqlx::query_scalar::<_, String>("PRAGMA integrity_check")
        .fetch_all(&pool)
        .await?;
    pool.close().await;
    Ok(rows)
}

pub fn usage_db_filename() -> String {
    USAGE_DB.filename.to_string()
}

pub fn usage_db_path(codex_home: &Path) -> PathBuf {
    USAGE_DB.path(codex_home)
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
    use super::logs_db_filename;
    use super::open_state_sqlite;
    use super::runtime_state_migrator;
    use super::sqlite_integrity_check;
    use super::state_db_path;
    use super::test_support::unique_temp_dir;
    use super::usage_db_filename;
    use crate::DB_INIT_METRIC;
    use crate::DbTelemetry;
    use crate::LOGS_DB_FILENAME;
    use crate::LOGS_DB_VERSION;
    use crate::USAGE_DB_FILENAME;
    use crate::USAGE_DB_VERSION;
    use crate::migrations::STATE_MIGRATOR;
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

    async fn open_db_pool(path: &Path) -> SqlitePool {
        crate::SqliteConfig::new_for_testing(path.parent().unwrap_or(path).to_path_buf())
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
        let path = state_db_path(codex_home.as_path());
        let pool = crate::SqliteConfig::new_for_testing(codex_home.clone())
            .open_read_write_pool(&path)
            .await
            .expect("open sqlite db");
        sqlx::query("CREATE TABLE sample (id INTEGER PRIMARY KEY)")
            .execute(&pool)
            .await
            .expect("create sample table");
        pool.close().await;

        let result = sqlite_integrity_check(&path)
            .await
            .expect("integrity check should run");

        assert_eq!(result, vec!["ok".to_string()]);
        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn open_state_sqlite_tolerates_newer_applied_migrations() {
        let codex_home = unique_temp_dir();
        tokio::fs::create_dir_all(&codex_home)
            .await
            .expect("create codex home");
        let state_path = state_db_path(codex_home.as_path());
        let pool = crate::SqliteConfig::new_for_testing(codex_home.clone())
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
        let tolerant_pool = open_state_sqlite(
            &crate::SqliteConfig::new_for_testing(codex_home.clone()),
            state_path.as_path(),
            &tolerant_migrator,
            /*telemetry_override*/ None,
        )
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
        let state_path = state_db_path(codex_home.as_path());
        let sqlite = crate::SqliteConfig::new_for_testing(codex_home.clone());
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
        let tolerant_pool = open_state_sqlite(
            &sqlite,
            state_path.as_path(),
            &tolerant_migrator,
            /*telemetry_override*/ None,
        )
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

        let current_logs_name = logs_db_filename();
        let current_usage_name = usage_db_filename();
        let previous_logs_version = LOGS_DB_VERSION.saturating_sub(1);
        let previous_usage_version = USAGE_DB_VERSION.saturating_sub(1);
        let unversioned_logs_name = format!("{LOGS_DB_FILENAME}.sqlite");
        let unversioned_usage_name = format!("{USAGE_DB_FILENAME}.sqlite");

        for suffix in ["", "-wal", "-shm", "-journal"] {
            let legacy_logs_path = codex_home.join(format!("{unversioned_logs_name}{suffix}"));
            tokio::fs::write(legacy_logs_path, b"legacy")
                .await
                .expect("write legacy logs file");
            let old_logs_path = codex_home.join(format!(
                "{LOGS_DB_FILENAME}_{previous_logs_version}.sqlite{suffix}"
            ));
            tokio::fs::write(old_logs_path, b"old_logs")
                .await
                .expect("write old logs file");
            let legacy_usage_path = codex_home.join(format!("{unversioned_usage_name}{suffix}"));
            tokio::fs::write(legacy_usage_path, b"legacy")
                .await
                .expect("write legacy usage file");
            let old_usage_path = codex_home.join(format!(
                "{USAGE_DB_FILENAME}_{previous_usage_version}.sqlite{suffix}"
            ));
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

        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
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
            let old_logs_path = codex_home.join(format!(
                "{LOGS_DB_FILENAME}_{previous_logs_version}.sqlite{suffix}"
            ));
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
            let old_usage_path = codex_home.join(format!(
                "{USAGE_DB_FILENAME}_{previous_usage_version}.sqlite{suffix}"
            ));
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
            codex_home.clone(),
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
