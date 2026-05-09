use crate::AgentJob;
use crate::AgentJobCreateParams;
use crate::AgentJobItem;
use crate::AgentJobItemCreateParams;
use crate::AgentJobItemStatus;
use crate::AgentJobProgress;
use crate::AgentJobStatus;
use crate::LOGS_DB_FILENAME;
use crate::LOGS_DB_VERSION;
use crate::LogEntry;
use crate::LogQuery;
use crate::LogRow;
use crate::STATE_DB_FILENAME;
use crate::STATE_DB_VERSION;
use crate::SortKey;
use crate::ThreadMetadata;
use crate::ThreadMetadataBuilder;
use crate::ThreadsPage;
use crate::USAGE_DB_FILENAME;
use crate::USAGE_DB_VERSION;
use crate::apply_rollout_item;
use crate::migrations::runtime_logs_migrator;
use crate::migrations::runtime_state_migrator;
use crate::migrations::runtime_usage_migrator;
use crate::model::AgentJobRow;
use crate::model::ThreadGoalRow;
use crate::model::ThreadRow;
use crate::model::anchor_from_item;
use crate::model::datetime_to_epoch_millis;
use crate::model::datetime_to_epoch_seconds;
use crate::model::epoch_millis_to_datetime;
use crate::paths::file_modified_time_utc;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::protocol::RolloutItem;
use log::LevelFilter;
use serde_json::Value;
use sqlx::ConnectOptions;
use sqlx::QueryBuilder;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::SqliteConnection;
use sqlx::SqlitePool;
use sqlx::migrate::Migrator;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::sqlite::SqliteJournalMode;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::sqlite::SqliteSynchronous;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::time::Duration;
use tracing::warn;

mod agent_jobs;
mod backfill;
mod device_key;
#[cfg(test)]
mod device_key_tests;
mod goals;
mod logs;
mod memories;
mod phase2_attestation;
mod remote_control;
#[cfg(test)]
mod test_support;
mod threads;
pub mod usage;

pub use device_key::DeviceKeyBindingRecord;
pub use goals::ThreadGoalAccountingMode;
pub use goals::ThreadGoalAccountingOutcome;
pub use goals::ThreadGoalUpdate;
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

#[derive(Clone)]
pub struct StateRuntime {
    codex_home: PathBuf,
    default_provider: String,
    pool: Arc<sqlx::SqlitePool>,
    logs_pool: Arc<sqlx::SqlitePool>,
    usage_pool: Arc<sqlx::SqlitePool>,
    thread_updated_at_millis: Arc<AtomicI64>,
}

impl StateRuntime {
    /// Initialize the state runtime using the provided Codex home and default provider.
    ///
    /// This opens (and migrates) the SQLite databases under `codex_home`,
    /// keeping logs in a dedicated file to reduce lock contention with the
    /// rest of the state store.
    pub async fn init(codex_home: PathBuf, default_provider: String) -> anyhow::Result<Arc<Self>> {
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
        let state_path = state_db_path(codex_home.as_path());
        let logs_path = logs_db_path(codex_home.as_path());
        let usage_path = usage_db_path(codex_home.as_path());
        let state_migrator = runtime_state_migrator();
        let logs_migrator = runtime_logs_migrator();
        let usage_migrator = runtime_usage_migrator();
        let pool = match open_state_sqlite(&state_path, &state_migrator).await {
            Ok(db) => Arc::new(db),
            Err(err) => {
                warn!("failed to open state db at {}: {err}", state_path.display());
                return Err(err);
            }
        };
        let logs_pool = match open_sqlite(&logs_path, &logs_migrator).await {
            Ok(db) => Arc::new(db),
            Err(err) => {
                warn!("failed to open logs db at {}: {err}", logs_path.display());
                return Err(err);
            }
        };
        ensure_incremental_auto_vacuum(logs_pool.as_ref()).await?;
        let usage_pool = match open_sqlite(&usage_path, &usage_migrator).await {
            Ok(db) => Arc::new(db),
            Err(err) => {
                warn!("failed to open usage db at {}: {err}", usage_path.display());
                return Err(err);
            }
        };
        let thread_updated_at_millis: Option<i64> =
            sqlx::query_scalar("SELECT MAX(threads.updated_at_ms) FROM threads")
                .fetch_one(pool.as_ref())
                .await?;
        let thread_updated_at_millis = thread_updated_at_millis.unwrap_or(0);
        let runtime = Arc::new(Self {
            pool,
            logs_pool,
            usage_pool,
            codex_home,
            default_provider,
            thread_updated_at_millis: Arc::new(AtomicI64::new(thread_updated_at_millis)),
        });
        runtime.run_logs_startup_maintenance().await?;
        Ok(runtime)
    }

    /// Return the configured Codex home directory for this runtime.
    pub fn codex_home(&self) -> &Path {
        self.codex_home.as_path()
    }

    pub fn usage_pool(&self) -> Arc<SqlitePool> {
        Arc::clone(&self.usage_pool)
    }
}

async fn open_state_sqlite(path: &Path, migrator: &Migrator) -> anyhow::Result<SqlitePool> {
    let pool = open_sqlite_pool(path).await?;
    repair_known_shifted_state_migrations(&pool, migrator).await?;
    migrator.run(&pool).await?;
    Ok(pool)
}

async fn open_sqlite(path: &Path, migrator: &Migrator) -> anyhow::Result<SqlitePool> {
    let pool = open_sqlite_pool(path).await?;
    migrator.run(&pool).await?;
    Ok(pool)
}

async fn open_sqlite_pool(path: &Path) -> anyhow::Result<SqlitePool> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5))
        .log_statements(LevelFilter::Off);
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;
    Ok(pool)
}

#[derive(Debug)]
struct MigrationHistoryMove {
    source_version: i64,
    target_version: i64,
}

const SHIFTED_STATE_MIGRATION_MOVES: &[MigrationHistoryMove] = &[
    MigrationHistoryMove {
        source_version: 24,
        target_version: 9000,
    },
    MigrationHistoryMove {
        source_version: 25,
        target_version: 24,
    },
    MigrationHistoryMove {
        source_version: 26,
        target_version: 25,
    },
    MigrationHistoryMove {
        source_version: 27,
        target_version: 9001,
    },
    MigrationHistoryMove {
        source_version: 28,
        target_version: 9002,
    },
    MigrationHistoryMove {
        source_version: 29,
        target_version: 26,
    },
    MigrationHistoryMove {
        source_version: 30,
        target_version: 27,
    },
    MigrationHistoryMove {
        source_version: 31,
        target_version: 28,
    },
    MigrationHistoryMove {
        source_version: 32,
        target_version: 29,
    },
];

#[derive(Debug)]
struct AppliedMigrationRow {
    version: i64,
    success: bool,
    checksum: Vec<u8>,
}

async fn repair_known_shifted_state_migrations(
    pool: &SqlitePool,
    migrator: &Migrator,
) -> anyhow::Result<()> {
    let migrations_table_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations')",
    )
    .fetch_one(pool)
    .await?;
    if !migrations_table_exists {
        return Ok(());
    }

    let applied_rows = sqlx::query(
        r#"
SELECT version, success, checksum
FROM _sqlx_migrations
WHERE version IN (24, 25, 26, 27, 28, 29, 30, 31, 32, 9000, 9001, 9002)
ORDER BY version ASC
        "#,
    )
    .fetch_all(pool)
    .await?;
    let applied_rows = applied_rows
        .into_iter()
        .map(|row| {
            Ok(AppliedMigrationRow {
                version: row.try_get("version")?,
                success: row.try_get("success")?,
                checksum: row.try_get("checksum")?,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    if applied_rows.is_empty() {
        return Ok(());
    }

    let mut shifted_moves = Vec::new();
    for row in &applied_rows {
        if migration_checksum_matches(migrator, row.version, row.checksum.as_slice()) {
            continue;
        }

        let Some(migration_move) = SHIFTED_STATE_MIGRATION_MOVES
            .iter()
            .find(|migration_move| migration_move.source_version == row.version)
        else {
            anyhow::bail!(
                "state DB migration history contains unknown shifted migration version {}; refusing automatic repair",
                row.version
            );
        };

        if !row.success {
            anyhow::bail!(
                "state DB migration {} is marked unsuccessful; refusing automatic repair",
                row.version
            );
        }

        if !migration_checksum_matches(
            migrator,
            migration_move.target_version,
            row.checksum.as_slice(),
        ) {
            anyhow::bail!(
                "state DB migration {} has an unknown checksum; refusing automatic repair",
                row.version
            );
        }

        shifted_moves.push(migration_move);
    }

    if shifted_moves.is_empty() {
        return Ok(());
    }

    for migration_move in &shifted_moves {
        if let Some(collision) = applied_rows
            .iter()
            .find(|row| row.version == migration_move.target_version)
            && !shifted_moves
                .iter()
                .any(|shifted| shifted.source_version == collision.version)
        {
            anyhow::bail!(
                "state DB migration repair would overwrite existing migration {}; refusing automatic repair",
                migration_move.target_version
            );
        }
    }

    let mut tx = pool.begin().await?;
    for migration_move in &shifted_moves {
        let temporary_version = temporary_repair_version(migration_move.source_version);
        sqlx::query("UPDATE _sqlx_migrations SET version = ? WHERE version = ?")
            .bind(temporary_version)
            .bind(migration_move.source_version)
            .execute(&mut *tx)
            .await?;
    }
    for migration_move in shifted_moves {
        let migration = migration_for_version(migrator, migration_move.target_version)?;
        sqlx::query(
            r#"
UPDATE _sqlx_migrations
SET version = ?, description = ?, checksum = ?
WHERE version = ?
            "#,
        )
        .bind(migration_move.target_version)
        .bind(migration.description.as_ref())
        .bind(migration.checksum.as_ref())
        .bind(temporary_repair_version(migration_move.source_version))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

fn migration_checksum_matches(migrator: &Migrator, version: i64, checksum: &[u8]) -> bool {
    migrator
        .iter()
        .find(|migration| migration.version == version)
        .is_some_and(|migration| migration.checksum.as_ref() == checksum)
}

fn migration_for_version(
    migrator: &Migrator,
    version: i64,
) -> anyhow::Result<&sqlx::migrate::Migration> {
    migrator
        .iter()
        .find(|migration| migration.version == version)
        .ok_or_else(|| anyhow::anyhow!("state migration {version} is not embedded"))
}

fn temporary_repair_version(version: i64) -> i64 {
    -9_000_000 - version
}

async fn ensure_incremental_auto_vacuum(pool: &SqlitePool) -> anyhow::Result<()> {
    let mut conn = pool.acquire().await?;
    let auto_vacuum = sqlx::query_scalar::<_, i64>("PRAGMA auto_vacuum")
        .fetch_one(&mut *conn)
        .await?;
    if auto_vacuum == 2 {
        return Ok(());
    }

    sqlx::query("PRAGMA auto_vacuum = INCREMENTAL")
        .execute(&mut *conn)
        .await?;
    sqlx::query("VACUUM").execute(&mut *conn).await?;
    Ok(())
}

fn db_filename(base_name: &str, version: u32) -> String {
    format!("{base_name}_{version}.sqlite")
}

pub fn state_db_filename() -> String {
    db_filename(STATE_DB_FILENAME, STATE_DB_VERSION)
}

pub fn state_db_path(codex_home: &Path) -> PathBuf {
    codex_home.join(state_db_filename())
}

pub fn logs_db_filename() -> String {
    db_filename(LOGS_DB_FILENAME, LOGS_DB_VERSION)
}

pub fn logs_db_path(codex_home: &Path) -> PathBuf {
    codex_home.join(logs_db_filename())
}

pub fn usage_db_filename() -> String {
    db_filename(USAGE_DB_FILENAME, USAGE_DB_VERSION)
}

pub fn usage_db_path(codex_home: &Path) -> PathBuf {
    codex_home.join(usage_db_filename())
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
    use super::open_sqlite_pool;
    use super::repair_known_shifted_state_migrations;
    use super::runtime_state_migrator;
    use super::state_db_path;
    use super::test_support::unique_temp_dir;
    use super::usage_db_filename;
    use crate::LOGS_DB_FILENAME;
    use crate::LOGS_DB_VERSION;
    use crate::USAGE_DB_FILENAME;
    use crate::USAGE_DB_VERSION;
    use crate::migrations::STATE_MIGRATOR;
    use pretty_assertions::assert_eq;
    use sqlx::SqlitePool;
    use sqlx::migrate::Migrator;
    use std::borrow::Cow;
    use std::io;
    use std::path::Path;
    use std::time::Duration;

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

    fn state_migrator_for_versions(versions: &[i64]) -> Migrator {
        let migrations = versions
            .iter()
            .map(|version| {
                STATE_MIGRATOR
                    .iter()
                    .find(|migration| migration.version == *version)
                    .unwrap_or_else(|| panic!("missing migration {version}"))
                    .clone()
            })
            .collect::<Vec<_>>();
        Migrator {
            migrations: Cow::Owned(migrations),
            ignore_missing: false,
            locking: STATE_MIGRATOR.locking,
            no_tx: STATE_MIGRATOR.no_tx,
        }
    }

    async fn applied_state_migration_versions(pool: &SqlitePool) -> Vec<i64> {
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version ASC")
            .fetch_all(pool)
            .await
            .expect("load applied migration versions")
    }

    async fn shift_current_history_to_bad(pool: &SqlitePool, source_versions: &[i64]) {
        let moves = [
            (9000_i64, 24_i64),
            (24, 25),
            (25, 26),
            (9001, 27),
            (9002, 28),
            (26, 29),
            (27, 30),
            (28, 31),
            (29, 32),
        ];
        let mut tx = pool.begin().await.expect("begin shift tx");
        for (target, source) in moves {
            if !source_versions.contains(&source) {
                continue;
            }
            sqlx::query("UPDATE _sqlx_migrations SET version = ? WHERE version = ?")
                .bind(-source)
                .bind(target)
                .execute(&mut *tx)
                .await
                .expect("move to temporary migration version");
        }
        for (target, source) in moves {
            if !source_versions.contains(&source) {
                continue;
            }
            sqlx::query("UPDATE _sqlx_migrations SET version = ? WHERE version = ?")
                .bind(source)
                .bind(-source)
                .execute(&mut *tx)
                .await
                .expect("move to shifted migration version");
        }
        tx.commit().await.expect("commit shifted history");
    }

    #[tokio::test]
    async fn init_accepts_upstream_0_129_state_db() {
        let codex_home = unique_temp_dir();
        tokio::fs::create_dir_all(&codex_home)
            .await
            .expect("create codex_home");
        let state_path = state_db_path(codex_home.as_path());
        let pool = open_sqlite_pool(state_path.as_path())
            .await
            .expect("open state db");
        let upstream_0_129_migrator =
            state_migrator_for_versions(&(1_i64..=29_i64).collect::<Vec<_>>());
        upstream_0_129_migrator
            .run(&pool)
            .await
            .expect("apply upstream 0.129 state migrations");
        pool.close().await;

        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime from upstream 0.129 DB");
        let versions = applied_state_migration_versions(runtime.pool.as_ref()).await;
        assert!(versions.contains(&24));
        assert!(versions.contains(&29));
        assert!(versions.contains(&9000));
        assert!(versions.contains(&9001));
        assert!(versions.contains(&9002));
        assert_eq!(
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'phase2_attestation_roots')",
            )
            .fetch_one(runtime.pool.as_ref())
            .await
            .expect("check attestation table"),
            true
        );

        drop(runtime);
        remove_dir_all_with_retry(&codex_home)
            .await
            .expect("failed to clean up temp directory");
    }

    #[tokio::test]
    async fn init_repairs_shifted_sedna_state_migration_history() {
        let codex_home = unique_temp_dir();
        tokio::fs::create_dir_all(&codex_home)
            .await
            .expect("create codex_home");
        let state_path = state_db_path(codex_home.as_path());
        let pool = open_sqlite_pool(state_path.as_path())
            .await
            .expect("open state db");
        STATE_MIGRATOR
            .run(&pool)
            .await
            .expect("apply current migrations");
        shift_current_history_to_bad(&pool, &[24, 25, 26, 27, 28, 29, 30, 31, 32]).await;
        pool.close().await;

        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime from shifted Sedna DB");
        let versions = applied_state_migration_versions(runtime.pool.as_ref()).await;
        assert!(versions.contains(&24));
        assert!(versions.contains(&25));
        assert!(versions.contains(&26));
        assert!(versions.contains(&27));
        assert!(versions.contains(&28));
        assert!(versions.contains(&29));
        assert!(versions.contains(&9000));
        assert!(versions.contains(&9001));
        assert!(versions.contains(&9002));
        assert!(!versions.contains(&30));
        assert!(!versions.contains(&31));
        assert!(!versions.contains(&32));

        drop(runtime);
        remove_dir_all_with_retry(&codex_home)
            .await
            .expect("failed to clean up temp directory");
    }

    #[tokio::test]
    async fn migration_history_repair_accepts_partial_known_shifted_prefix() {
        let codex_home = unique_temp_dir();
        tokio::fs::create_dir_all(&codex_home)
            .await
            .expect("create codex_home");
        let state_path = state_db_path(codex_home.as_path());
        let pool = open_sqlite_pool(state_path.as_path())
            .await
            .expect("open state db");
        let partial_migrator = state_migrator_for_versions(&[
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
            9000, 24, 25,
        ]);
        partial_migrator
            .run(&pool)
            .await
            .expect("apply partial current migrations");
        shift_current_history_to_bad(&pool, &[24, 25, 26]).await;
        pool.close().await;

        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime from partial shifted Sedna DB");
        let versions = applied_state_migration_versions(runtime.pool.as_ref()).await;
        assert!(versions.contains(&24));
        assert!(versions.contains(&25));
        assert!(versions.contains(&26));
        assert!(versions.contains(&29));
        assert!(versions.contains(&9000));
        assert!(versions.contains(&9001));
        assert!(versions.contains(&9002));

        drop(runtime);
        remove_dir_all_with_retry(&codex_home)
            .await
            .expect("failed to clean up temp directory");
    }

    #[tokio::test]
    async fn migration_history_repair_rejects_unknown_checksum() {
        let codex_home = unique_temp_dir();
        tokio::fs::create_dir_all(&codex_home)
            .await
            .expect("create codex_home");
        let state_path = state_db_path(codex_home.as_path());
        let pool = open_sqlite_pool(state_path.as_path())
            .await
            .expect("open state db");
        STATE_MIGRATOR
            .run(&pool)
            .await
            .expect("apply current migrations");
        shift_current_history_to_bad(&pool, &[24, 25, 26, 27, 28, 29, 30, 31, 32]).await;
        sqlx::query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = ?")
            .bind(vec![0_u8, 1, 2, 3])
            .bind(27_i64)
            .execute(&pool)
            .await
            .expect("tamper migration checksum");

        let migrator = runtime_state_migrator();
        let err = repair_known_shifted_state_migrations(&pool, &migrator)
            .await
            .expect_err("unknown checksum should be rejected");
        assert!(
            err.to_string().contains("unknown checksum"),
            "unexpected error: {err:#}"
        );
        let versions = applied_state_migration_versions(&pool).await;
        assert!(versions.contains(&24));
        assert!(versions.contains(&27));
        assert!(versions.contains(&32));
        assert!(!versions.contains(&9000));

        pool.close().await;
        remove_dir_all_with_retry(&codex_home)
            .await
            .expect("failed to clean up temp directory");
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
}
