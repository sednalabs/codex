use std::borrow::Cow;

use sqlx::migrate::Migrator;
use sqlx_sqlite::SqliteConnection;

pub(crate) static STATE_MIGRATOR: Migrator = sqlx_macros::migrate!("./migrations");
pub(crate) static LOGS_MIGRATOR: Migrator = sqlx_macros::migrate!("./logs_migrations");
pub(crate) static GOALS_MIGRATOR: Migrator = sqlx_macros::migrate!("./goals_migrations");
pub(crate) static MEMORIES_MIGRATOR: Migrator = sqlx_macros::migrate!("./memory_migrations");
pub(crate) static QUEUE_MIGRATOR: Migrator = sqlx_macros::migrate!("./queue_migrations");
pub(crate) static THREAD_HISTORY_MIGRATOR: Migrator =
    sqlx_macros::migrate!("./thread_history_migrations");

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
    // State additionally validates the entire known ledger before migration;
    // unknown newer identities are rejected rather than treated as compatible.
    runtime_migrator(&STATE_MIGRATOR)
}

pub(crate) fn runtime_logs_migrator() -> Migrator {
    runtime_migrator(&LOGS_MIGRATOR)
}

pub(crate) fn runtime_goals_migrator() -> Migrator {
    runtime_migrator(&GOALS_MIGRATOR)
}

pub(crate) fn runtime_memories_migrator() -> Migrator {
    runtime_migrator(&MEMORIES_MIGRATOR)
}

pub(crate) fn runtime_queue_migrator() -> Migrator {
    runtime_migrator(&QUEUE_MIGRATOR)
}

// The paginated history projector will call this when it takes ownership of opening the database.
#[allow(dead_code)]
pub(crate) fn runtime_thread_history_migrator() -> Migrator {
    runtime_migrator(&THREAD_HISTORY_MIGRATOR)
}

pub(crate) async fn repair_legacy_recency_migration_version(
    connection: &mut SqliteConnection,
    migrator: &Migrator,
) -> anyhow::Result<()> {
    let Some(recency_migration) = migrator
        .migrations
        .iter()
        .find(|migration| migration.version == 39)
    else {
        return Ok(());
    };
    let migrations_table_exists = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(&mut *connection)
    .await?
    .is_some();
    if !migrations_table_exists {
        return Ok(());
    }

    let legacy_recency_needs_repair = sqlx::query_scalar::<_, i64>(
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
    .bind(38_i64)
    .bind(recency_migration.checksum.as_ref())
    .bind(recency_migration.version)
    .fetch_optional(&mut *connection)
    .await?
    .is_some();
    if !legacy_recency_needs_repair {
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
    .bind(recency_migration.version)
    .bind(recency_migration.description.as_ref())
    .bind(38_i64)
    .bind(recency_migration.checksum.as_ref())
    .bind(recency_migration.version)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

/// Reject identities we cannot relate to the embedded migration history.
/// For state, validate the entire ledger before the legacy recency rekey writes.
pub(crate) async fn validate_migration_identities(
    connection: &mut SqliteConnection,
    migrator: &Migrator,
    database: &'static str,
    allow_legacy_recency: bool,
) -> anyhow::Result<()> {
    let exists = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(&mut *connection)
    .await?
    .is_some();
    let applied = if exists {
        sqlx::query_as::<_, (i64, String, bool, Vec<u8>)>(
            "SELECT version, description, success, checksum FROM _sqlx_migrations ORDER BY version",
        )
        .fetch_all(&mut *connection)
        .await?
    } else {
        Vec::new()
    };
    if applied.is_empty() {
        let objects = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' AND name != '_sqlx_migrations'",
        )
        .fetch_one(&mut *connection)
        .await?;
        anyhow::ensure!(
            objects == 0,
            "{database} schema exists without a recognized migration history"
        );
        return Ok(());
    }
    let legacy_recency = allow_legacy_recency
        .then(|| migrator.migrations.iter().find(|m| m.version == 39))
        .flatten();
    for (index, (version, description, success, checksum)) in applied.iter().enumerate() {
        anyhow::ensure!(
            *success,
            "{database} migration {version} was not successful"
        );
        let migration = migrator.migrations.get(index);
        anyhow::ensure!(
            migration.is_some_and(|m| m.version == *version),
            "unrecognized or incomplete {database} migration history at version {version}"
        );
        if *version == 38
            && index + 1 == applied.len()
            && legacy_recency.is_some_and(|m| {
                m.description.as_ref() == description.as_str()
                    && m.checksum.as_ref() == checksum.as_slice()
            })
        {
            continue;
        }
        anyhow::ensure!(
            migration.is_some_and(|m| {
                m.description.as_ref() == description.as_str()
                    && m.checksum.as_ref() == checksum.as_slice()
            }),
            "unrecognized {database} migration identity at version {version}"
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "migrations_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "migration_identity_tests.rs"]
mod identity_tests;
