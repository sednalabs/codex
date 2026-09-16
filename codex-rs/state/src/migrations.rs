use std::borrow::Cow;
use std::collections::BTreeMap;

use anyhow::bail;
use sqlx::Row;
use sqlx::SqlitePool;
use sqlx::migrate::Migrator;

pub(crate) static STATE_MIGRATOR: Migrator = sqlx::migrate!("./migrations");
pub(crate) static LOGS_MIGRATOR: Migrator = sqlx::migrate!("./logs_migrations");
pub(crate) static USAGE_MIGRATOR: Migrator = sqlx::migrate!("./usage_migrations");
pub(crate) static GOALS_MIGRATOR: Migrator = sqlx::migrate!("./goals_migrations");
pub(crate) static MEMORIES_MIGRATOR: Migrator = sqlx::migrate!("./memory_migrations");
pub(crate) static QUEUE_MIGRATOR: Migrator = sqlx::migrate!("./queue_migrations");
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

pub(crate) fn runtime_queue_migrator() -> Migrator {
    runtime_migrator(&QUEUE_MIGRATOR)
}

// The paginated history projector will call this when it takes ownership of opening the database.
#[allow(dead_code)]
pub(crate) fn runtime_thread_history_migrator() -> Migrator {
    runtime_migrator(&THREAD_HISTORY_MIGRATOR)
}

#[derive(Clone, Copy)]
struct HistoricalStateMigration {
    source_version: i64,
    canonical_version: i64,
}

// The upstream 1..55 state-migration spine is immutable. These mappings accept
// only a historical record whose checksum is exactly that of the embedded
// canonical target; a version number alone is never sufficient evidence.
const HISTORICAL_STATE_MIGRATIONS: &[HistoricalStateMigration] = &[
    HistoricalStateMigration {
        source_version: 24,
        canonical_version: 9001,
    },
    HistoricalStateMigration {
        source_version: 25,
        canonical_version: 24,
    },
    HistoricalStateMigration {
        source_version: 26,
        canonical_version: 25,
    },
    HistoricalStateMigration {
        source_version: 27,
        canonical_version: 9002,
    },
    HistoricalStateMigration {
        source_version: 28,
        canonical_version: 9003,
    },
    HistoricalStateMigration {
        source_version: 29,
        canonical_version: 26,
    },
    HistoricalStateMigration {
        source_version: 30,
        canonical_version: 27,
    },
    HistoricalStateMigration {
        source_version: 31,
        canonical_version: 28,
    },
    HistoricalStateMigration {
        source_version: 32,
        canonical_version: 29,
    },
    HistoricalStateMigration {
        source_version: 33,
        canonical_version: 30,
    },
    HistoricalStateMigration {
        source_version: 34,
        canonical_version: 31,
    },
    HistoricalStateMigration {
        source_version: 35,
        canonical_version: 32,
    },
    HistoricalStateMigration {
        source_version: 36,
        canonical_version: 33,
    },
    HistoricalStateMigration {
        source_version: 37,
        canonical_version: 34,
    },
    HistoricalStateMigration {
        source_version: 38,
        canonical_version: 9004,
    },
    HistoricalStateMigration {
        source_version: 38,
        canonical_version: 39,
    },
    HistoricalStateMigration {
        source_version: 39,
        canonical_version: 35,
    },
    HistoricalStateMigration {
        source_version: 40,
        canonical_version: 36,
    },
    HistoricalStateMigration {
        source_version: 41,
        canonical_version: 37,
    },
    HistoricalStateMigration {
        source_version: 42,
        canonical_version: 38,
    },
    HistoricalStateMigration {
        source_version: 43,
        canonical_version: 39,
    },
    HistoricalStateMigration {
        source_version: 44,
        canonical_version: 36,
    },
    HistoricalStateMigration {
        source_version: 45,
        canonical_version: 9005,
    },
    HistoricalStateMigration {
        source_version: 46,
        canonical_version: 37,
    },
    HistoricalStateMigration {
        source_version: 47,
        canonical_version: 38,
    },
    HistoricalStateMigration {
        source_version: 48,
        canonical_version: 43,
    },
    HistoricalStateMigration {
        source_version: 49,
        canonical_version: 44,
    },
    HistoricalStateMigration {
        source_version: 50,
        canonical_version: 9006,
    },
];

const FIRST_AFFECTED_VERSION: i64 = 24;
const LAST_UPSTREAM_VERSION: i64 = 55;
const FIRST_FORK_VERSION: i64 = 9001;
const LAST_FORK_VERSION: i64 = 9006;
const TEMPORARY_VERSION_BASE: i64 = 9_000_000_000_000;

#[derive(Debug)]
struct PlannedMigrationRemap {
    source_version: i64,
    source_checksum: Vec<u8>,
    canonical_version: i64,
    canonical_description: String,
}

pub(crate) async fn repair_state_migration_version_collisions(
    pool: &SqlitePool,
    migrator: &Migrator,
) -> anyhow::Result<()> {
    if !migrations_table_exists(pool).await? {
        return Ok(());
    }

    let rows = sqlx::query(
        r#"
SELECT version, checksum
FROM _sqlx_migrations
WHERE (version BETWEEN ? AND ?) OR (version BETWEEN ? AND ?)
ORDER BY version
        "#,
    )
    .bind(FIRST_AFFECTED_VERSION)
    .bind(LAST_UPSTREAM_VERSION)
    .bind(FIRST_FORK_VERSION)
    .bind(LAST_FORK_VERSION)
    .fetch_all(pool)
    .await?;

    let mut final_versions = BTreeMap::<i64, i64>::new();
    let mut remaps = Vec::new();
    for row in rows {
        let source_version = row.get::<i64, _>("version");
        let source_checksum = row.get::<Vec<u8>, _>("checksum");
        let candidates = matching_canonical_migrations(migrator, source_version, &source_checksum)?;
        if candidates.len() != 1 {
            bail!(
                "state migration history at version {source_version} has {} checksum-qualified canonical targets",
                candidates.len()
            );
        }
        let canonical = candidates[0];
        if final_versions
            .insert(canonical.version, source_version)
            .is_some()
        {
            bail!(
                "state migration history maps multiple records to canonical version {}",
                canonical.version
            );
        }
        if canonical.version != source_version {
            remaps.push(PlannedMigrationRemap {
                source_version,
                source_checksum,
                canonical_version: canonical.version,
                canonical_description: canonical.description.to_string(),
            });
        }
    }

    if remaps.is_empty() {
        return Ok(());
    }

    let temporary_end = TEMPORARY_VERSION_BASE + i64::try_from(remaps.len())?;
    let temporary_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM _sqlx_migrations WHERE version > ? AND version <= ?",
    )
    .bind(TEMPORARY_VERSION_BASE)
    .bind(temporary_end)
    .fetch_one(pool)
    .await?;
    if temporary_count != 0 {
        bail!("state migration history contains reserved temporary remap versions");
    }

    let mut transaction = pool.begin().await?;
    for (index, remap) in remaps.iter().enumerate() {
        let temporary_version = TEMPORARY_VERSION_BASE + i64::try_from(index)? + 1;
        let result = sqlx::query(
            "UPDATE _sqlx_migrations SET version = ? WHERE version = ? AND checksum = ?",
        )
        .bind(temporary_version)
        .bind(remap.source_version)
        .bind(&remap.source_checksum)
        .execute(&mut *transaction)
        .await?;
        if result.rows_affected() != 1 {
            bail!(
                "state migration history changed while remapping version {}",
                remap.source_version
            );
        }
    }
    for (index, remap) in remaps.iter().enumerate() {
        let temporary_version = TEMPORARY_VERSION_BASE + i64::try_from(index)? + 1;
        let result = sqlx::query(
            "UPDATE _sqlx_migrations SET version = ?, description = ? WHERE version = ? AND checksum = ?",
        )
        .bind(remap.canonical_version)
        .bind(&remap.canonical_description)
        .bind(temporary_version)
        .bind(&remap.source_checksum)
        .execute(&mut *transaction)
        .await?;
        if result.rows_affected() != 1 {
            bail!(
                "state migration history failed to finalize canonical version {}",
                remap.canonical_version
            );
        }
    }
    transaction.commit().await?;
    Ok(())
}

fn matching_canonical_migrations<'a>(
    migrator: &'a Migrator,
    source_version: i64,
    source_checksum: &[u8],
) -> anyhow::Result<Vec<&'a sqlx::migrate::Migration>> {
    let mut target_versions = vec![source_version];
    target_versions.extend(
        HISTORICAL_STATE_MIGRATIONS
            .iter()
            .filter(|repair| repair.source_version == source_version)
            .map(|repair| repair.canonical_version),
    );
    target_versions.sort_unstable();
    target_versions.dedup();

    let candidates = target_versions
        .into_iter()
        .filter_map(|version| {
            migrator
                .iter()
                .find(|migration| migration.version == version)
        })
        .filter(|migration| migration.checksum.as_ref() == source_checksum)
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        bail!("state migration history at version {source_version} has an unknown checksum");
    }
    Ok(candidates)
}

async fn migrations_table_exists(pool: &SqlitePool) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(pool)
    .await?
    .is_some())
}

#[cfg(test)]
#[path = "migrations_tests.rs"]
mod tests;
