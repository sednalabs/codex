use std::fs::File;
use std::fs::OpenOptions;
use std::path::Path;
use std::time::Duration;
use std::time::Instant;

#[cfg(test)]
use std::sync::Mutex;

use anyhow::bail;
use sqlx::Row;
use sqlx::SqlitePool;
use sqlx::migrate::Migrator;

use crate::migrations::repair_state_migration_version_collisions;

const STATE_MIGRATION_LOCK_SUFFIX: &str = ".state-migration.lock";
const STATE_MIGRATION_LOCK_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(test)]
static FAIL_AFTER_GOAL_DESTINATION_COMMIT: Mutex<Option<String>> = Mutex::new(None);

#[cfg(test)]
pub(crate) fn fail_next_goal_transfer_after_destination_commit(thread_id: &str) {
    *FAIL_AFTER_GOAL_DESTINATION_COMMIT
        .lock()
        .expect("goal transfer failpoint lock should not be poisoned") = Some(thread_id.to_owned());
}

/// Serializes the complete legacy-state bridge and canonical SQLx migration
/// across new runtime processes. The file is deliberately retained; its path
/// is stable state-database identity and the advisory lock is released with
/// the handle.
pub(crate) struct StateMigrationLease {
    _file: File,
}

pub(crate) async fn acquire_state_migration_lease(
    state_db_path: &Path,
) -> anyhow::Result<StateMigrationLease> {
    let Some(file_name) = state_db_path.file_name() else {
        bail!("state database path has no file name");
    };
    let lock_path = state_db_path.with_file_name(format!(
        ".{}{}",
        file_name.to_string_lossy(),
        STATE_MIGRATION_LOCK_SUFFIX
    ));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    let deadline = Instant::now() + STATE_MIGRATION_LOCK_TIMEOUT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(StateMigrationLease { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                bail!("timed out waiting for the state migration lease");
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
    }
}

#[derive(Debug, PartialEq)]
struct LegacyGoalRow {
    thread_id: String,
    goal_id: String,
    objective: String,
    status: String,
    token_budget: Option<i64>,
    tokens_used: i64,
    time_used_seconds: i64,
    created_at_ms: i64,
    updated_at_ms: i64,
}

pub(crate) async fn repair_state_migrations(
    state_pool: &SqlitePool,
    goals_pool: &SqlitePool,
    migrator: &Migrator,
) -> anyhow::Result<()> {
    repair_state_migration_version_collisions(state_pool, migrator).await?;
    transfer_legacy_thread_goals(state_pool, goals_pool, migrator).await
}

async fn transfer_legacy_thread_goals(
    state_pool: &SqlitePool,
    goals_pool: &SqlitePool,
    migrator: &Migrator,
) -> anyhow::Result<()> {
    // A fresh database reaches this bridge before SQLx has created its ledger.
    // It cannot possibly carry legacy goals, so leave normal migration creation
    // to the canonical migrator. Any existing ledger continues through the
    // checksum-qualified remap path and therefore fails closed on unknown data.
    if !state_migrations_table_exists(state_pool).await? {
        return Ok(());
    }
    if !migration_is_applied(state_pool, migrator, 29).await? {
        return Ok(());
    }
    if migration_is_applied(state_pool, migrator, 34).await? {
        return Ok(());
    }
    if !legacy_thread_goals_table_exists(state_pool).await? {
        bail!("pre-drop thread_goals migration history has no source table");
    }

    let source_rows = sqlx::query(
        r#"
SELECT
    thread_id,
    goal_id,
    objective,
    status,
    token_budget,
    tokens_used,
    time_used_seconds,
    created_at_ms,
    updated_at_ms
FROM thread_goals
ORDER BY thread_id
        "#,
    )
    .fetch_all(state_pool)
    .await?
    .into_iter()
    .map(|row| LegacyGoalRow {
        thread_id: row.get("thread_id"),
        goal_id: row.get("goal_id"),
        objective: row.get("objective"),
        status: row.get("status"),
        token_budget: row.get("token_budget"),
        tokens_used: row.get("tokens_used"),
        time_used_seconds: row.get("time_used_seconds"),
        created_at_ms: row.get("created_at_ms"),
        updated_at_ms: row.get("updated_at_ms"),
    })
    .collect::<Vec<_>>();

    let mut transaction = goals_pool.begin().await?;
    for source in &source_rows {
        sqlx::query(
            r#"
INSERT INTO thread_goals (
    thread_id,
    goal_id,
    objective,
    status,
    token_budget,
    tokens_used,
    time_used_seconds,
    created_at_ms,
    updated_at_ms
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(thread_id) DO NOTHING
            "#,
        )
        .bind(&source.thread_id)
        .bind(&source.goal_id)
        .bind(&source.objective)
        .bind(&source.status)
        .bind(source.token_budget)
        .bind(source.tokens_used)
        .bind(source.time_used_seconds)
        .bind(source.created_at_ms)
        .bind(source.updated_at_ms)
        .execute(&mut *transaction)
        .await?;

        let destination = sqlx::query(
            r#"
SELECT
    thread_id,
    goal_id,
    objective,
    status,
    token_budget,
    tokens_used,
    time_used_seconds,
    created_at_ms,
    updated_at_ms
FROM thread_goals
WHERE thread_id = ?
            "#,
        )
        .bind(&source.thread_id)
        .fetch_one(&mut *transaction)
        .await?;
        let destination = LegacyGoalRow {
            thread_id: destination.get("thread_id"),
            goal_id: destination.get("goal_id"),
            objective: destination.get("objective"),
            status: destination.get("status"),
            token_budget: destination.get("token_budget"),
            tokens_used: destination.get("tokens_used"),
            time_used_seconds: destination.get("time_used_seconds"),
            created_at_ms: destination.get("created_at_ms"),
            updated_at_ms: destination.get("updated_at_ms"),
        };
        if destination != *source {
            bail!(
                "legacy thread goal {} conflicts with an existing destination goal",
                source.thread_id
            );
        }
    }
    transaction.commit().await?;
    #[cfg(test)]
    let should_fail = {
        let mut failpoint = FAIL_AFTER_GOAL_DESTINATION_COMMIT
            .lock()
            .expect("goal transfer failpoint lock should not be poisoned");
        if failpoint
            .as_deref()
            .is_some_and(|thread_id| source_rows.iter().any(|goal| goal.thread_id == thread_id))
        {
            failpoint.take();
            true
        } else {
            false
        }
    };
    if should_fail {
        bail!("injected failure after legacy goal destination commit");
    }
    Ok(())
}

async fn state_migrations_table_exists(state_pool: &SqlitePool) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(state_pool)
    .await?
    .is_some())
}

async fn migration_is_applied(
    state_pool: &SqlitePool,
    migrator: &Migrator,
    version: i64,
) -> anyhow::Result<bool> {
    let migration = migrator
        .iter()
        .find(|migration| migration.version == version)
        .ok_or_else(|| anyhow::anyhow!("embedded state migration {version} is missing"))?;
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM _sqlx_migrations WHERE version = ? AND checksum = ? AND success = TRUE",
    )
    .bind(version)
    .bind(migration.checksum.as_ref())
    .fetch_optional(state_pool)
    .await?
    .is_some())
}

async fn legacy_thread_goals_table_exists(state_pool: &SqlitePool) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'thread_goals'",
    )
    .fetch_optional(state_pool)
    .await?
    .is_some())
}
