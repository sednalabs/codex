use std::borrow::Cow;

use sqlx::SqlitePool;
use sqlx::migrate::Migrator;

pub(crate) static STATE_MIGRATOR: Migrator = sqlx::migrate!("./migrations");
pub(crate) static LOGS_MIGRATOR: Migrator = sqlx::migrate!("./logs_migrations");
pub(crate) static USAGE_MIGRATOR: Migrator = sqlx::migrate!("./usage_migrations");
pub(crate) static GOALS_MIGRATOR: Migrator = sqlx::migrate!("./goals_migrations");
pub(crate) static MEMORIES_MIGRATOR: Migrator = sqlx::migrate!("./memory_migrations");
pub(crate) static THREAD_HISTORY_MIGRATOR: Migrator = sqlx::migrate!("./thread_history_migrations");

/// Allow an older Codex binary to open a database that has already been
/// migrated by a newer binary running in parallel.
///
/// We intentionally ignore applied migration versions that are newer than the
/// embedded migration set. Known migration versions are still validated by
/// checksum, so this only relaxes the "database is ahead of me" case.
fn runtime_migrator(base: &'static Migrator) -> Migrator {
    Migrator {
        migrations: Cow::Borrowed(base.migrations.as_ref()),
        ignore_missing: true,
        locking: base.locking,
        no_tx: base.no_tx,
        table_name: base.table_name.clone(),
        create_schemas: base.create_schemas.clone(),
    }
}

pub(crate) fn runtime_state_migrator() -> Migrator {
    runtime_migrator(&STATE_MIGRATOR)
}

pub(crate) fn runtime_logs_migrator() -> Migrator {
    runtime_migrator(&LOGS_MIGRATOR)
}

pub(crate) fn runtime_usage_migrator() -> Migrator {
    runtime_migrator(&USAGE_MIGRATOR)
}

pub(crate) fn runtime_goals_migrator() -> Migrator {
    runtime_migrator(&GOALS_MIGRATOR)
}

pub(crate) fn runtime_memories_migrator() -> Migrator {
    runtime_migrator(&MEMORIES_MIGRATOR)
}

// The paginated history projector will call this when it takes ownership of opening the database.
#[allow(dead_code)]
pub(crate) fn runtime_thread_history_migrator() -> Migrator {
    runtime_migrator(&THREAD_HISTORY_MIGRATOR)
}

const LEGACY_RECENCY_MIGRATION_VERSION: i64 = 38;
const CURRENT_RECENCY_MIGRATION_VERSION: i64 = 43;
const LEGACY_VISIBLE_SORT_INDEXES_MIGRATION_VERSION: i64 = 40;
const CURRENT_VISIBLE_SORT_INDEXES_MIGRATION_VERSION: i64 = 44;
const LEGACY_REMOTE_CONTROL_ENABLED_MIGRATION_VERSION: i64 = 41;
const CURRENT_REMOTE_CONTROL_ENABLED_MIGRATION_VERSION: i64 = 46;
const LEGACY_EXTERNAL_AGENT_CONFIG_IMPORTS_MIGRATION_VERSION: i64 = 42;
const CURRENT_EXTERNAL_AGENT_CONFIG_IMPORTS_MIGRATION_VERSION: i64 = 47;
const LEGACY_EXTERNAL_AGENT_CONFIG_IMPORTS_PROVIDER_ID_MIGRATION_VERSION: i64 = 44;
const CURRENT_EXTERNAL_AGENT_CONFIG_IMPORTS_PROVIDER_ID_MIGRATION_VERSION: i64 = 49;

const MIGRATION_VERSION_REPAIRS: &[(i64, i64)] = &[
    (
        LEGACY_RECENCY_MIGRATION_VERSION,
        CURRENT_RECENCY_MIGRATION_VERSION,
    ),
    (
        LEGACY_VISIBLE_SORT_INDEXES_MIGRATION_VERSION,
        CURRENT_VISIBLE_SORT_INDEXES_MIGRATION_VERSION,
    ),
    (
        LEGACY_REMOTE_CONTROL_ENABLED_MIGRATION_VERSION,
        CURRENT_REMOTE_CONTROL_ENABLED_MIGRATION_VERSION,
    ),
    (
        LEGACY_EXTERNAL_AGENT_CONFIG_IMPORTS_MIGRATION_VERSION,
        CURRENT_EXTERNAL_AGENT_CONFIG_IMPORTS_MIGRATION_VERSION,
    ),
    (
        LEGACY_EXTERNAL_AGENT_CONFIG_IMPORTS_PROVIDER_ID_MIGRATION_VERSION,
        CURRENT_EXTERNAL_AGENT_CONFIG_IMPORTS_PROVIDER_ID_MIGRATION_VERSION,
    ),
];

pub(crate) async fn repair_state_migration_version_collisions(
    pool: &SqlitePool,
    migrator: &Migrator,
) -> anyhow::Result<()> {
    for (legacy_version, current_version) in MIGRATION_VERSION_REPAIRS {
        repair_migration_version(pool, migrator, *legacy_version, *current_version).await?;
    }
    Ok(())
}

async fn repair_migration_version(
    pool: &SqlitePool,
    migrator: &Migrator,
    legacy_version: i64,
    current_version: i64,
) -> anyhow::Result<()> {
    let Some(current_migration) = migrator
        .migrations
        .iter()
        .find(|migration| migration.version == current_version)
    else {
        return Ok(());
    };
    let migrations_table_exists = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(pool)
    .await?
    .is_some();
    if !migrations_table_exists {
        return Ok(());
    }

    let legacy_migration_needs_repair = sqlx::query_scalar::<_, i64>(
        r#"
SELECT 1
FROM _sqlx_migrations
WHERE version = ?
  AND checksum = ?
  AND NOT EXISTS (
      SELECT 1 FROM _sqlx_migrations WHERE version = ?
  )
        "#,
    )
    .bind(legacy_version)
    .bind(current_migration.checksum.as_ref())
    .bind(current_migration.version)
    .fetch_optional(pool)
    .await?
    .is_some();
    if !legacy_migration_needs_repair {
        return Ok(());
    }

    sqlx::query(
        r#"
UPDATE _sqlx_migrations
SET version = ?, description = ?
WHERE version = ?
  AND checksum = ?
  AND NOT EXISTS (
      SELECT 1 FROM _sqlx_migrations WHERE version = ?
  )
        "#,
    )
    .bind(current_migration.version)
    .bind(current_migration.description.as_ref())
    .bind(legacy_version)
    .bind(current_migration.checksum.as_ref())
    .bind(current_migration.version)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
#[path = "migrations_tests.rs"]
mod tests;
