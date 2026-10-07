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

const T10_USAGE_MIGRATION_HISTORY: &[(i64, &str, &str)] = &[
    (
        1,
        "usage tables",
        "c77f82ba7ec24c3874c7bd91818ce685bfd3fbc56d714e67b7c35db0e534cbb27dc4abe0cc24026440b97a113536cbae",
    ),
    (
        5,
        "usage codex credits",
        "0957854d0e40df0283bf7f0187e026284d91b9d45e35f688d09f0d9fa3c4a501c3f4197b50f81b31226c803df9335bfa",
    ),
    (
        15,
        "usage codex credit status compatibility",
        "73865aae347fae7344ec0b71d80b2c49f2748fbd613da8fe7b3121bf1769ffe71cf3d820115bc2cfcbe4bfe2c26c2cdc",
    ),
    (
        16,
        "usage codex legacy model rates 20260926",
        "dca6288df3dd02d7440d7fddb4568ad743562ac1022f8bbf2563e0ee00d120d99c5ab7593e52085493b68e7c9b30ff3b",
    ),
    (
        17,
        "usage codex credit rates 20260930",
        "3227fd99dfc64e77c0ccb04dd803b85de41ca880cecdfb9cd1ea467c743921025cb0dca8fe1a55918ff8e30be9b630a8",
    ),
    (
        18,
        "usage response idempotency",
        "dc1beb8d446d292ef0e98d00a828e49804d38ff8ff498adec990d4696d51dee7678855876f887158667c5f11107b65f4",
    ),
    (
        19,
        "usage standard rate scenarios",
        "0baffd0cc7dd86d306907b40c324eb92f72e908ade298629385aa7dc399a39db47c2f8b7d3898cd08fc688ac577b89fb",
    ),
];

/// Preserve the exact T10 usage-history variants without rewriting their
/// applied SQLx migration rows. Versions absent from this binary remain
/// governed by the runtime migrator's existing ignore_missing contract.
pub(crate) async fn runtime_usage_migrator_for_history(
    pool: &SqlitePool,
    base: &Migrator,
) -> anyhow::Result<Migrator> {
    let migration_table_exists = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(pool)
    .await?
    .is_some();

    let applied = if migration_table_exists {
        sqlx::query_as::<_, (i64, String, bool, Vec<u8>)>(
            "SELECT version, description, success, checksum FROM _sqlx_migrations ORDER BY version",
        )
        .fetch_all(pool)
        .await?
    } else {
        Vec::new()
    };

    let mut t10_v5_or_later_seen = false;
    let mut t10_v15_or_later_seen = false;
    let mut skip_source_versions = Vec::new();

    for (version, description, success, checksum) in &applied {
        anyhow::ensure!(
            *success,
            "usage migration history contains an unsuccessful version {version}"
        );

        let source = base.iter().find(|migration| migration.version == *version);
        let t10 = T10_USAGE_MIGRATION_HISTORY
            .iter()
            .find(|(known_version, _, _)| known_version == version);
        let matches_source = source.is_some_and(|migration| {
            migration.description.as_ref() == description.as_str()
                && migration.checksum.as_ref() == checksum.as_slice()
        });
        let matches_t10 = t10.is_some_and(|(_, known_description, known_checksum)| {
            description.as_str() == *known_description
                && checksum_matches_hex(checksum, known_checksum)
        });

        if matches_t10 {
            t10_v5_or_later_seen |= *version == 5 || *version >= 15;
            t10_v15_or_later_seen |= *version >= 15;
            if source.is_some_and(|migration| !matches_source) {
                skip_source_versions.push(*version);
            }
            continue;
        }

        if t10.is_some() && source.is_none() {
            anyhow::bail!("usage migration history has an unrecognized T10-only version {version}");
        }
        if source.is_none() {
            // Keep the usage runtime's existing ignore_missing=true behavior
            // for genuine source-absent future versions.
            continue;
        }
        anyhow::ensure!(
            matches_source,
            "usage migration history conflicts with known version {version}"
        );
    }

    if t10_v5_or_later_seen {
        anyhow::ensure!(
            has_exact_t10_usage_migration(&applied, 1),
            "usage migration history has a T10 v5-or-later variant without its exact v1 row"
        );
    }
    if t10_v15_or_later_seen {
        anyhow::ensure!(
            has_exact_t10_usage_migration(&applied, 5)
                && has_exact_t10_usage_migration(&applied, 15),
            "usage migration history has a T10 v15-or-later variant without its exact v5/v15 rows"
        );
    }

    let migrations: Vec<_> = base
        .iter()
        .filter(|migration| !skip_source_versions.contains(&migration.version))
        .cloned()
        .collect();
    if migrations.iter().any(|migration| migration.version == 15)
        && !applied.iter().any(|(version, _, _, _)| *version == 15)
    {
        let existing_reporting_indexes = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name IN ('usage_provider_calls_started_at_idx', 'usage_provider_calls_thread_started_at_idx', 'usage_threads_reporting_parent_idx')",
        )
        .fetch_one(pool)
        .await?;
        anyhow::ensure!(
            existing_reporting_indexes == 0,
            "usage reporting indexes exist without their migration history row"
        );
    }

    Ok(Migrator {
        migrations: Cow::Owned(migrations),
        ignore_missing: base.ignore_missing,
        locking: base.locking,
        no_tx: base.no_tx,
        table_name: base.table_name.clone(),
        create_schemas: base.create_schemas.clone(),
    })
}

fn has_exact_t10_usage_migration(applied: &[(i64, String, bool, Vec<u8>)], version: i64) -> bool {
    let Some((_, description, _, checksum)) = applied.iter().find(|row| row.0 == version) else {
        return false;
    };
    let Some((_, known_description, known_checksum)) = T10_USAGE_MIGRATION_HISTORY
        .iter()
        .find(|(known_version, _, _)| *known_version == version)
    else {
        return false;
    };
    description.as_str() == *known_description && checksum_matches_hex(checksum, known_checksum)
}

fn checksum_matches_hex(checksum: &[u8], expected: &str) -> bool {
    if checksum.len() * 2 != expected.len() {
        return false;
    }
    checksum
        .iter()
        .zip(expected.as_bytes().chunks_exact(2))
        .all(|(actual, pair)| {
            hex_nibble(pair[0])
                .zip(hex_nibble(pair[1]))
                .is_some_and(|(high, low)| *actual == ((high << 4) | low))
        })
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

const USAGE_REPORTING_INDEXES: &[(&str, &str, &str)] = &[
    (
        "usage_provider_calls_started_at_idx",
        "usage_provider_calls",
        "createindexusage_provider_calls_started_at_idxonusage_provider_calls(started_at)",
    ),
    (
        "usage_provider_calls_thread_started_at_idx",
        "usage_provider_calls",
        "createindexusage_provider_calls_thread_started_at_idxonusage_provider_calls(thread_id,started_at)",
    ),
    (
        "usage_threads_reporting_parent_idx",
        "usage_threads",
        "createindexusage_threads_reporting_parent_idxonusage_threads(coalesce(nullif(parent_thread_id,''),nullif(fork_parent_thread_id,'')))",
    ),
];

/// Reject conflicting same-name indexes before a migration can make changes;
/// after migration, also require every reporting index to have its exact source
/// definition.
pub(crate) async fn validate_usage_reporting_indexes(
    pool: &SqlitePool,
    require_all: bool,
) -> anyhow::Result<()> {
    let indexes = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT name, tbl_name, sql FROM sqlite_master WHERE type = 'index' AND name IN ('usage_provider_calls_started_at_idx', 'usage_provider_calls_thread_started_at_idx', 'usage_threads_reporting_parent_idx', 'usage_provider_calls_thread_idx')",
    )
    .fetch_all(pool)
    .await?;

    for &(name, expected_table, expected_sql) in USAGE_REPORTING_INDEXES {
        let found = indexes
            .iter()
            .find(|(index_name, _, _)| index_name.as_str() == name);
        match found {
            Some((_, table, Some(sql)))
                if table.as_str() == expected_table
                    && normalize_index_sql(sql).as_str() == expected_sql => {}
            Some(_) => {
                anyhow::bail!("usage reporting index {name} exists with an unexpected definition")
            }
            None if require_all => {
                anyhow::bail!("usage reporting index {name} is missing after migration")
            }
            None => {}
        }
    }

    if let Some((_, table, Some(sql))) = indexes
        .iter()
        .find(|(name, _, _)| name.as_str() == "usage_provider_calls_thread_idx")
    {
        anyhow::ensure!(
            !require_all,
            "legacy usage thread index remains after reporting migration"
        );
        anyhow::ensure!(
            table == "usage_provider_calls"
                && normalize_index_sql(sql)
                    == "createindexusage_provider_calls_thread_idxonusage_provider_calls(thread_id)",
            "legacy usage thread index exists with an unexpected definition"
        );
    } else if indexes
        .iter()
        .any(|(name, _, _)| name.as_str() == "usage_provider_calls_thread_idx")
    {
        anyhow::bail!("legacy usage thread index has no SQL definition");
    }

    Ok(())
}

fn normalize_index_sql(sql: &str) -> String {
    let normalized = sql
        .chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    normalized.replace("createindexifnotexists", "createindex")
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
