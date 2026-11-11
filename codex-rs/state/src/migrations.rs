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
    let known_extension = database == "state"
        && migrator.migrations.len() == 62
        && migrator.migrations.last().is_some_and(|m| m.version == 62)
        && applied.len() == 69;
    for (index, (version, description, success, checksum)) in applied.iter().enumerate() {
        anyhow::ensure!(
            *success,
            "{database} migration {version} was not successful"
        );
        let migration = migrator.migrations.get(index);
        if known_extension && index >= 62 {
            let (expected_description, expected_checksum) = W14939_STATE_EXTENSIONS[index - 62];
            let actual_checksum: String =
                checksum.iter().map(|byte| format!("{byte:02x}")).collect();
            anyhow::ensure!(
                *version == 8_000_000_000_000_000 + (index - 62) as i64
                    && description == expected_description
                    && actual_checksum == expected_checksum,
                "unrecognized {database} migration identity at version {version}"
            );
            continue;
        }
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
    if known_extension {
        validate_w14939_extension_schema(connection).await?;
    }
    Ok(())
}

// These are the seven exact successful identities observed after the complete
// upstream 1–62 prefix. No partial or different extension history is admitted.
const W14939_STATE_EXTENSIONS: [(&str, &str); 7] = [
    (
        "phase2 attested baselines",
        "c021f9cc67108fe6dfbbc58a40d20ffeb716724f1283681eb23e6f639b48a1739a7eb1ea222ed3679941d48f72101284",
    ),
    (
        "threads configured identity provenance",
        "dd42ba9960d4e01c554bfac6a27c67f3c21e80bdc8e0dd928a90d208e1e680976a3ea356a9f9360f9760a5ca42659053",
    ),
    (
        "thread dynamic tools state",
        "448d0f4ab51b856fc182d97e5e408861630fc387d7316473426771c298581881016018a34d45145e0e503179dc7efd84",
    ),
    (
        "phase2 attestation roots",
        "f0392a5480c11466f48752cdad6e1bcd05fafa3c17ba0e54f04b02dbd9b5bf9976c107d8159abc225e1a4dc0191065bf",
    ),
    (
        "thread dynamic tools persist on resume",
        "d391b088bc785fcc6d8e61abbf2702e65175ec0e2d30df51e4ff6e51d1ed5659839058ef82d046683ebb8009c5ba1317",
    ),
    (
        "thread dynamic tools capability json",
        "f1db36e9fbf40560e028c5c606f821cbf6040932a7a37d3bf47da118c7622c97789a25636376b9ed1da38e3ba8b3856d",
    ),
    (
        "backfill thread spawn edges",
        "e3dcfd70c67f1ec352fd635f7e633d246e1d8ad70668eb482ca201c5688b435028b6bbe603d5eb23c4397c6f2184b7d6",
    ),
];

async fn validate_w14939_extension_schema(connection: &mut SqliteConnection) -> anyhow::Result<()> {
    // Verify extension objects before accepting the ledger. Do not replay any
    // SQL: 0058 overlaps the separately recorded 0027/0028 ALTER statements.
    for (table, column, ty, required, default) in [
        (
            "threads",
            "configured_identity_provenance",
            "INTEGER",
            true,
            Some("0"),
        ),
        (
            "thread_dynamic_tools",
            "persist_on_resume",
            "INTEGER",
            true,
            Some("1"),
        ),
        (
            "thread_dynamic_tools",
            "capability_json",
            "TEXT",
            false,
            None,
        ),
        (
            "thread_dynamic_tools",
            "namespace_description",
            "TEXT",
            false,
            None,
        ),
    ] {
        let actual: Option<(String, bool, Option<String>)> = sqlx::query_as(
            "SELECT upper(type), [notnull], dflt_value FROM pragma_table_info(?) WHERE name = ?",
        )
        .bind(table)
        .bind(column)
        .fetch_optional(&mut *connection)
        .await?;
        anyhow::ensure!(
            actual
                .as_ref()
                .is_some_and(|(t, n, d)| t == ty && *n == required && d.as_deref() == default),
            "state extension schema mismatch for {table}.{column}"
        );
    }
    type Column<'a> = (&'a str, &'a str, bool, Option<&'a str>, i64);
    let tables: [(&str, &[Column<'_>]); 3] = [
        (
            "phase2_attestation_roots",
            &[
                ("memory_root_key", "TEXT", false, None, 1),
                ("required_since", "INTEGER", true, None, 0),
                ("updated_at", "INTEGER", true, None, 0),
            ],
        ),
        (
            "phase2_attested_baselines",
            &[
                ("memory_root_key", "TEXT", true, None, 1),
                ("output_tree_sha256", "TEXT", true, None, 2),
                ("schema_version", "INTEGER", true, None, 0),
                ("selection_sha256", "TEXT", true, None, 0),
                ("prepared_inputs_sha256", "TEXT", true, None, 0),
                ("consolidator_sha256", "TEXT", true, None, 0),
                ("completion_watermark", "INTEGER", true, None, 0),
                ("selected_count", "INTEGER", true, None, 0),
                ("attested_at", "INTEGER", true, None, 0),
            ],
        ),
        (
            "thread_spawn_edges",
            &[
                ("parent_thread_id", "TEXT", true, None, 0),
                ("child_thread_id", "TEXT", true, None, 1),
                ("status", "TEXT", true, None, 0),
            ],
        ),
    ];
    for (table, expected) in tables {
        let actual: Vec<(String, String, bool, Option<String>, i64)> = sqlx::query_as(
            "SELECT name, upper(type), [notnull], dflt_value, pk FROM pragma_table_info(?) ORDER BY cid",
        )
        .bind(table)
        .fetch_all(&mut *connection)
        .await?;
        anyhow::ensure!(
            actual.len() == expected.len()
                && actual.iter().zip(expected).all(|(a, e)| a.0 == e.0
                    && a.1 == e.1
                    && a.2 == e.2
                    && a.3.as_deref() == e.3
                    && a.4 == e.4),
            "state extension schema mismatch for {table}"
        );
    }
    let index: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_index_info('idx_phase2_attested_baselines_root_attested_at') ORDER BY seqno")
        .fetch_all(&mut *connection).await?;
    anyhow::ensure!(
        index == ["memory_root_key", "attested_at"],
        "state extension attestation index mismatch"
    );
    let index_table: Option<String> = sqlx::query_scalar("SELECT tbl_name FROM sqlite_master WHERE type = 'index' AND name = 'idx_phase2_attested_baselines_root_attested_at'")
        .fetch_optional(&mut *connection).await?;
    anyhow::ensure!(
        index_table.as_deref() == Some("phase2_attested_baselines"),
        "state extension attestation index owner mismatch"
    );
    let threads_sql: Option<String> = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'threads'",
    )
    .fetch_optional(&mut *connection)
    .await?;
    anyhow::ensure!(
        threads_sql
            .is_some_and(|sql| sql.contains("CHECK (configured_identity_provenance IN (0, 1, 2))")),
        "state extension provenance constraint mismatch"
    );
    Ok(())
}

#[cfg(test)]
#[path = "migrations_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "migration_identity_tests.rs"]
mod identity_tests;
