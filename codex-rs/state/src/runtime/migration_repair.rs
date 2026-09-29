use anyhow::Context;
use sqlx::Row;
use sqlx::SqlitePool;
use sqlx::migrate::Migration;
use sqlx::migrate::Migrator;

#[derive(Debug)]
struct MigrationHistoryMove {
    source_version: i64,
    source_description: &'static str,
    target_version: i64,
}

const SHIFTED_STATE_MIGRATION_MOVES: &[MigrationHistoryMove] = &[
    MigrationHistoryMove {
        source_version: 24,
        source_description: "phase2 attestation roots",
        target_version: 9000,
    },
    MigrationHistoryMove {
        source_version: 25,
        source_description: "remote control enrollments",
        target_version: 24,
    },
    MigrationHistoryMove {
        source_version: 26,
        source_description: "thread timestamps millis",
        target_version: 25,
    },
    MigrationHistoryMove {
        source_version: 27,
        source_description: "thread dynamic tools persist on resume",
        target_version: 9001,
    },
    MigrationHistoryMove {
        source_version: 28,
        source_description: "thread dynamic tools capability json",
        target_version: 9002,
    },
    MigrationHistoryMove {
        source_version: 29,
        source_description: "thread dynamic tools namespace",
        target_version: 26,
    },
    MigrationHistoryMove {
        source_version: 30,
        source_description: "threads cwd sort indexes",
        target_version: 27,
    },
    MigrationHistoryMove {
        source_version: 31,
        source_description: "device key bindings",
        target_version: 28,
    },
    MigrationHistoryMove {
        source_version: 32,
        source_description: "thread goals",
        target_version: 29,
    },
    MigrationHistoryMove {
        source_version: 33,
        source_description: "threads thread source",
        target_version: 30,
    },
    MigrationHistoryMove {
        source_version: 34,
        source_description: "drop device key bindings",
        target_version: 31,
    },
    MigrationHistoryMove {
        source_version: 35,
        source_description: "threads preview",
        target_version: 32,
    },
    MigrationHistoryMove {
        source_version: 36,
        source_description: "thread goal stopped statuses",
        target_version: 33,
    },
    MigrationHistoryMove {
        source_version: 37,
        source_description: "drop thread goals",
        target_version: 34,
    },
    MigrationHistoryMove {
        source_version: 38,
        source_description: "phase2 attested baselines",
        target_version: 56,
    },
    MigrationHistoryMove {
        source_version: 39,
        source_description: "drop memory tables",
        target_version: 35,
    },
    MigrationHistoryMove {
        source_version: 40,
        source_description: "threads history mode",
        target_version: 40,
    },
    MigrationHistoryMove {
        source_version: 41,
        source_description: "threads name",
        target_version: 41,
    },
    MigrationHistoryMove {
        source_version: 42,
        source_description: "drop agent jobs",
        target_version: 42,
    },
    MigrationHistoryMove {
        source_version: 43,
        source_description: "threads recency at",
        target_version: 39,
    },
    MigrationHistoryMove {
        source_version: 44,
        source_description: "threads visible sort indexes",
        target_version: 36,
    },
    MigrationHistoryMove {
        source_version: 45,
        source_description: "threads configured identity provenance",
        target_version: 57,
    },
    MigrationHistoryMove {
        source_version: 46,
        source_description: "remote control enrollments enabled",
        target_version: 37,
    },
    MigrationHistoryMove {
        source_version: 47,
        source_description: "external agent config imports",
        target_version: 38,
    },
    MigrationHistoryMove {
        source_version: 48,
        source_description: "threads is pinned",
        target_version: 43,
    },
    MigrationHistoryMove {
        source_version: 49,
        source_description: "external agent config imports provider id",
        target_version: 44,
    },
    MigrationHistoryMove {
        source_version: 50,
        source_description: "backfill thread spawn edges",
        target_version: 9003,
    },
];

struct ColumnMigrationRepair {
    version: i64,
    table_name: &'static str,
    column_name: &'static str,
}

const COLUMN_MIGRATION_REPAIRS: &[ColumnMigrationRepair] = &[ColumnMigrationRepair {
    version: 30,
    table_name: "threads",
    column_name: "thread_source",
}];

pub(crate) async fn repair_state_migrations(
    pool: &SqlitePool,
    migrator: &Migrator,
) -> anyhow::Result<()> {
    repair_shifted_state_migrations(pool, migrator).await?;
    repair_dynamic_tool_state_overlap(pool, migrator).await?;
    for repair in COLUMN_MIGRATION_REPAIRS {
        repair_column_migration(pool, migrator, repair).await?;
    }
    Ok(())
}

async fn repair_dynamic_tool_state_overlap(
    pool: &SqlitePool,
    migrator: &Migrator,
) -> anyhow::Result<()> {
    let migration = migration_by_version(migrator, /*version*/ 58)
        .with_context(|| "embedded state migration 58 is missing")?;
    let migration_9001 = migration_by_version(migrator, /*version*/ 9001)
        .with_context(|| "embedded state migration 9001 is missing")?;
    let migration_9002 = migration_by_version(migrator, /*version*/ 9002)
        .with_context(|| "embedded state migration 9002 is missing")?;
    let migration_58_row = migration_record(pool, /*version*/ 58).await?;
    let persist_on_resume =
        column_exists(pool, "thread_dynamic_tools", "persist_on_resume").await?;
    let capability_json = column_exists(pool, "thread_dynamic_tools", "capability_json").await?;
    let namespace_description =
        column_exists(pool, "thread_dynamic_tools", "namespace_description").await?;

    let migration_9001_row = migration_record(pool, /*version*/ 9001).await?;
    let migration_9002_row = migration_record(pool, /*version*/ 9002).await?;

    if let Some(row) = migration_58_row {
        validate_canonical_migration_row(&row, migration, /*version*/ 58)?;
        if !(persist_on_resume && capability_json && namespace_description) {
            anyhow::bail!(
                "state DB migration 58 is recorded but its columns are incomplete; refusing automatic repair"
            );
        }
        match (migration_9001_row.as_ref(), migration_9002_row.as_ref()) {
            (Some(row), Some(row_2)) => {
                validate_canonical_migration_row(row, migration_9001, /*version*/ 9001)?;
                validate_canonical_migration_row(row_2, migration_9002, /*version*/ 9002)?;
            }
            (None, None) => {
                mark_alias_migrations_applied(pool, migration_9001, migration_9002).await?;
            }
            _ => anyhow::bail!(
                "state DB dynamic tool alias migration history is partial; refusing automatic repair"
            ),
        }
        return Ok(());
    }

    match (persist_on_resume, capability_json, namespace_description) {
        (false, false, false) => {
            match (migration_9001_row.as_ref(), migration_9002_row.as_ref()) {
                (Some(row), Some(row_2)) => {
                    validate_canonical_migration_row(row, migration_9001, /*version*/ 9001)?;
                    validate_canonical_migration_row(row_2, migration_9002, /*version*/ 9002)?;
                }
                (None, None) => {
                    mark_alias_migrations_applied(pool, migration_9001, migration_9002).await?;
                }
                _ => anyhow::bail!(
                    "state DB dynamic tool alias migration history is partial; refusing automatic repair"
                ),
            }
            return Ok(());
        }
        (true, false, _) | (false, true, _) | (false, false, true) => {
            anyhow::bail!(
                "state DB thread dynamic tool migration overlap is partial; refusing automatic repair"
            )
        }
        (true, true, _) => {}
    }

    for (version, expected) in [(9001, migration_9001), (9002, migration_9002)] {
        let row = migration_record(pool, version)
            .await?
            .with_context(|| format!("state DB migration {version} history is missing"))?;
        validate_canonical_migration_row(&row, expected, /*version*/ version)?;
    }

    let mut tx = pool.begin().await?;
    if !namespace_description {
        sqlx::query("ALTER TABLE thread_dynamic_tools ADD COLUMN namespace_description TEXT")
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query(
        r#"
        INSERT INTO _sqlx_migrations (
            version,
            description,
            success,
            checksum,
            execution_time
        )
        SELECT ?, ?, TRUE, ?, 0
        WHERE NOT EXISTS (
            SELECT 1
            FROM _sqlx_migrations
            WHERE version = ?
        )
        "#,
    )
    .bind(migration.version)
    .bind(migration.description.as_ref())
    .bind(migration.checksum.as_ref().to_vec())
    .bind(migration.version)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

#[derive(Debug)]
struct AppliedMigrationRow {
    version: i64,
    description: String,
    success: bool,
    checksum: Vec<u8>,
}

async fn repair_shifted_state_migrations(
    pool: &SqlitePool,
    migrator: &Migrator,
) -> anyhow::Result<()> {
    if !table_exists(pool, "_sqlx_migrations").await? {
        return Ok(());
    }

    let rows = sqlx::query(
        r#"
        SELECT version, description, success, checksum
        FROM _sqlx_migrations
        WHERE version BETWEEN 24 AND 50 OR version IN (9000, 9001, 9002, 9003)
        ORDER BY version
        "#,
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| {
        Ok(AppliedMigrationRow {
            version: row.try_get("version")?,
            description: row.try_get("description")?,
            success: row.try_get("success")?,
            checksum: row.try_get("checksum")?,
        })
    })
    .collect::<anyhow::Result<Vec<_>>>()?;

    let mut moves = Vec::new();
    for row in &rows {
        if migration_checksum_matches(migrator, row.version, &row.checksum) {
            if !row.success {
                anyhow::bail!(
                    "state DB migration {} is marked unsuccessful; refusing automatic repair",
                    row.version
                );
            }
            let migration = migration_by_version(migrator, row.version)
                .with_context(|| format!("embedded state migration {} is missing", row.version))?;
            if row.description != migration.description.as_ref() {
                anyhow::bail!(
                    "state DB migration {} has unexpected description; refusing automatic repair",
                    row.version
                );
            }
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
        if row.description != migration_move.source_description {
            anyhow::bail!(
                "state DB migration {} has unexpected description; refusing automatic repair",
                row.version
            );
        }
        if !migration_checksum_matches(migrator, migration_move.target_version, &row.checksum) {
            anyhow::bail!(
                "state DB migration {} has an unknown checksum; refusing automatic repair",
                row.version
            );
        }
        moves.push(migration_move);
    }
    if moves.is_empty() {
        return Ok(());
    }

    // A repair is only safe when the observed legacy ledger is a complete
    // prefix.  Validate the whole known cohort before opening the mutation
    // transaction so a hole or altered historical identity cannot cause any
    // source row to be moved or deleted.
    validate_legacy_migration_cohort(&rows, migrator, &moves)?;

    let mut verified_target_collisions = Vec::new();
    for migration_move in &moves {
        let Some(target_row) = rows.iter().find(|row| {
            row.version == migration_move.target_version
                && !moves.iter().any(|item| item.source_version == row.version)
        }) else {
            continue;
        };
        let target_migration = migration_by_version(migrator, migration_move.target_version)
            .with_context(|| {
                format!(
                    "embedded state migration {} is missing",
                    migration_move.target_version
                )
            })?;
        validate_canonical_migration_row(
            target_row,
            target_migration,
            migration_move.target_version,
        )?;
        if target_row.checksum
            != rows
                .iter()
                .find(|row| row.version == migration_move.source_version)
                .expect("source row was validated above")
                .checksum
        {
            anyhow::bail!(
                "state DB migration {} collision is not byte-equivalent to source {}; refusing automatic repair",
                migration_move.target_version,
                migration_move.source_version
            );
        }
        verified_target_collisions.push(migration_move.target_version);
    }

    let mut tx = pool.begin().await?;
    for target_version in verified_target_collisions {
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version = ?")
            .bind(target_version)
            .execute(&mut *tx)
            .await?;
    }
    for migration_move in &moves {
        sqlx::query("UPDATE _sqlx_migrations SET version = ? WHERE version = ?")
            .bind(temporary_repair_version(migration_move.source_version))
            .bind(migration_move.source_version)
            .execute(&mut *tx)
            .await?;
    }
    for migration_move in moves {
        let migration = migration_by_version(migrator, migration_move.target_version)
            .with_context(|| {
                format!(
                    "embedded state migration {} is missing",
                    migration_move.target_version
                )
            })?;
        sqlx::query(
            "UPDATE _sqlx_migrations SET version = ?, description = ?, checksum = ? WHERE version = ?",
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

fn validate_legacy_migration_cohort(
    rows: &[AppliedMigrationRow],
    migrator: &Migrator,
    moves: &[&MigrationHistoryMove],
) -> anyhow::Result<()> {
    let highest_source_version = moves
        .iter()
        .map(|migration_move| migration_move.source_version)
        .max()
        .expect("moves is non-empty");

    for migration_move in SHIFTED_STATE_MIGRATION_MOVES
        .iter()
        .filter(|migration_move| migration_move.source_version <= highest_source_version)
    {
        let source_is_exact = rows.iter().any(|row| {
            row.version == migration_move.source_version
                && row.success
                && row.description == migration_move.source_description
                && migration_checksum_matches(
                    migrator,
                    migration_move.target_version,
                    &row.checksum,
                )
        });
        let target_is_exact = rows
            .iter()
            .find(|row| row.version == migration_move.target_version && row.success);
        let target_is_equivalent = target_is_exact.is_some_and(|row| {
            migration_by_version(migrator, migration_move.target_version).is_some_and(|migration| {
                row.description == migration.description.as_ref()
                    && row.checksum == migration.checksum.as_ref()
            })
        });

        if !source_is_exact && !target_is_equivalent {
            anyhow::bail!(
                "state DB legacy migration cohort is incomplete through version {}; migration {} is missing its exact historical row or canonical target; refusing automatic repair",
                highest_source_version,
                migration_move.source_version,
            );
        }
    }
    Ok(())
}

fn migration_checksum_matches(migrator: &Migrator, version: i64, checksum: &[u8]) -> bool {
    migration_by_version(migrator, version)
        .is_some_and(|migration| migration.checksum.as_ref() == checksum)
}

fn temporary_repair_version(version: i64) -> i64 {
    -9_000_000 - version
}

async fn repair_column_migration(
    pool: &SqlitePool,
    migrator: &Migrator,
    repair: &ColumnMigrationRepair,
) -> anyhow::Result<()> {
    if !column_exists(pool, repair.table_name, repair.column_name).await? {
        return Ok(());
    }

    if migration_record_exists(pool, repair.version).await? {
        return Ok(());
    }

    let migration = migration_by_version(migrator, repair.version)
        .with_context(|| format!("embedded state migration {} is missing", repair.version))?;
    mark_migration_applied(pool, migration).await
}

fn migration_by_version(migrator: &Migrator, version: i64) -> Option<&Migration> {
    migrator
        .iter()
        .find(|migration| migration.version == version)
}

async fn migration_record_exists(pool: &SqlitePool, version: i64) -> anyhow::Result<bool> {
    if !table_exists(pool, "_sqlx_migrations").await? {
        return Ok(false);
    }

    let exists = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS(
            SELECT 1
            FROM _sqlx_migrations
            WHERE version = ?
        )
        "#,
    )
    .bind(version)
    .fetch_optional(pool)
    .await?
    .unwrap_or(false);
    Ok(exists)
}

async fn migration_record(
    pool: &SqlitePool,
    version: i64,
) -> anyhow::Result<Option<AppliedMigrationRow>> {
    if !table_exists(pool, "_sqlx_migrations").await? {
        return Ok(None);
    }

    let row = sqlx::query(
        r#"
        SELECT version, description, success, checksum
        FROM _sqlx_migrations
        WHERE version = ?
        "#,
    )
    .bind(version)
    .fetch_optional(pool)
    .await?
    .map(|row| -> anyhow::Result<AppliedMigrationRow> {
        Ok(AppliedMigrationRow {
            version: row.try_get("version")?,
            description: row.try_get("description")?,
            success: row.try_get("success")?,
            checksum: row.try_get("checksum")?,
        })
    })
    .transpose()?;
    Ok(row)
}

fn validate_canonical_migration_row(
    row: &AppliedMigrationRow,
    migration: &Migration,
    version: i64,
) -> anyhow::Result<()> {
    if row.version != version
        || !row.success
        || row.description != migration.description.as_ref()
        || row.checksum != migration.checksum.as_ref()
    {
        anyhow::bail!(
            "state DB migration {version} history is not canonical; refusing automatic repair"
        );
    }
    Ok(())
}

async fn table_exists(pool: &SqlitePool, table_name: &str) -> anyhow::Result<bool> {
    let exists = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS(
            SELECT 1
            FROM sqlite_schema
            WHERE type = 'table' AND name = ?
        )
        "#,
    )
    .bind(table_name)
    .fetch_one(pool)
    .await?;
    Ok(exists)
}

async fn column_exists(
    pool: &SqlitePool,
    table_name: &str,
    column_name: &str,
) -> anyhow::Result<bool> {
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?) WHERE name = ?)",
    )
    .bind(table_name)
    .bind(column_name)
    .fetch_one(pool)
    .await?;
    Ok(exists)
}

async fn mark_migration_applied(pool: &SqlitePool, migration: &Migration) -> anyhow::Result<()> {
    ensure_migrations_table(pool).await?;
    sqlx::query(
        r#"
        INSERT INTO _sqlx_migrations (
            version,
            description,
            success,
            checksum,
            execution_time
        )
        SELECT ?, ?, TRUE, ?, 0
        WHERE NOT EXISTS (
            SELECT 1
            FROM _sqlx_migrations
            WHERE version = ?
        )
        "#,
    )
    .bind(migration.version)
    .bind(migration.description.as_ref())
    .bind(migration.checksum.as_ref().to_vec())
    .bind(migration.version)
    .execute(pool)
    .await?;
    Ok(())
}

async fn mark_alias_migrations_applied(
    pool: &SqlitePool,
    migration_9001: &Migration,
    migration_9002: &Migration,
) -> anyhow::Result<()> {
    ensure_migrations_table(pool).await?;
    let mut tx = pool.begin().await?;
    for migration in [migration_9001, migration_9002] {
        sqlx::query(
            r#"
            INSERT INTO _sqlx_migrations (
                version,
                description,
                success,
                checksum,
                execution_time
            )
            SELECT ?, ?, TRUE, ?, 0
            WHERE NOT EXISTS (
                SELECT 1
                FROM _sqlx_migrations
                WHERE version = ?
            )
            "#,
        )
        .bind(migration.version)
        .bind(migration.description.as_ref())
        .bind(migration.checksum.as_ref().to_vec())
        .bind(migration.version)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn ensure_migrations_table(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS _sqlx_migrations (
            version BIGINT PRIMARY KEY,
            description TEXT NOT NULL,
            installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            success BOOLEAN NOT NULL,
            checksum BLOB NOT NULL,
            execution_time BIGINT NOT NULL
        )
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::SHIFTED_STATE_MIGRATION_MOVES;
    use super::repair_state_migrations;
    use super::temporary_repair_version;
    use crate::migrations::STATE_MIGRATOR;
    use crate::runtime::test_support::unique_temp_dir;
    use codex_utils_absolute_path::test_support::PathExt;
    use sqlx::Row;
    use sqlx::migrate::Migrator;
    use std::borrow::Cow;

    async fn ledger_snapshot(pool: &sqlx::SqlitePool) -> Vec<(i64, String, bool, Vec<u8>, i64)> {
        sqlx::query(
            "SELECT version, description, success, checksum, execution_time FROM _sqlx_migrations ORDER BY version",
        )
        .fetch_all(pool)
        .await
        .expect("migration ledger should be readable")
        .into_iter()
        .map(|row| {
            (
                row.get("version"),
                row.get("description"),
                row.get("success"),
                row.get("checksum"),
                row.get("execution_time"),
            )
        })
        .collect()
    }

    fn known_good_legacy_migrator(include_backfill: bool) -> Migrator {
        let mut migrations = STATE_MIGRATOR
            .migrations
            .iter()
            .filter(|migration| migration.version <= 23)
            .cloned()
            .collect::<Vec<_>>();
        for migration_move in SHIFTED_STATE_MIGRATION_MOVES {
            if !include_backfill && migration_move.source_version == 50 {
                continue;
            }
            let migration = STATE_MIGRATOR
                .migrations
                .iter()
                .find(|migration| migration.version == migration_move.target_version)
                .expect("mapped migration should be embedded");
            migrations.push(sqlx::migrate::Migration::new(
                migration_move.source_version,
                migration.description.clone(),
                migration.migration_type,
                migration.sql.clone(),
                migration.no_tx,
            ));
        }
        migrations.sort_by_key(|migration| migration.version);
        Migrator {
            migrations: Cow::Owned(migrations),
            ignore_missing: STATE_MIGRATOR.ignore_missing,
            locking: STATE_MIGRATOR.locking,
            no_tx: STATE_MIGRATOR.no_tx,
            table_name: STATE_MIGRATOR.table_name.clone(),
            create_schemas: STATE_MIGRATOR.create_schemas.clone(),
        }
    }

    async fn fresh_full_legacy_state() -> (std::path::PathBuf, sqlx::SqlitePool) {
        let sqlite_home = unique_temp_dir();
        tokio::fs::create_dir_all(&sqlite_home)
            .await
            .expect("sqlite home should be created");
        let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
        let pool = sqlite
            .open_read_write_pool(&sqlite.state_db_path())
            .await
            .expect("database should open");
        known_good_legacy_migrator(true)
            .run(&pool)
            .await
            .expect("legacy migrations should apply");
        (sqlite_home, pool)
    }

    #[tokio::test]
    async fn rejects_incomplete_legacy_cohort_without_mutation() {
        let (sqlite_home, pool) = fresh_full_legacy_state().await;
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 30")
            .execute(&pool)
            .await
            .expect("middle row should be removable");
        let before = ledger_snapshot(&pool).await;
        let error = repair_state_migrations(&pool, &STATE_MIGRATOR)
            .await
            .expect_err("hole must fail closed");
        assert!(
            error
                .to_string()
                .contains("legacy migration cohort is incomplete")
        );
        assert_eq!(ledger_snapshot(&pool).await, before);
        pool.close().await;
        let _ = std::fs::remove_dir_all(sqlite_home);
    }

    #[tokio::test]
    async fn rejects_altered_legacy_identity_without_mutation() {
        for (column, value, expected) in [
            (
                "description",
                "altered historical description",
                "unexpected description",
            ),
            ("checksum", "00", "unknown checksum"),
        ] {
            let (sqlite_home, pool) = fresh_full_legacy_state().await;
            if column == "description" {
                sqlx::query("UPDATE _sqlx_migrations SET description = ? WHERE version = 30")
                    .bind(value)
                    .execute(&pool)
                    .await
                    .expect("description should be altered");
            } else {
                sqlx::query("UPDATE _sqlx_migrations SET checksum = X'00' WHERE version = 30")
                    .execute(&pool)
                    .await
                    .expect("checksum should be altered");
            }
            let before = ledger_snapshot(&pool).await;
            let error = repair_state_migrations(&pool, &STATE_MIGRATOR)
                .await
                .expect_err("altered identity must fail closed");
            assert!(error.to_string().contains(expected));
            assert_eq!(ledger_snapshot(&pool).await, before);
            pool.close().await;
            let _ = std::fs::remove_dir_all(sqlite_home);
        }
    }

    #[tokio::test]
    async fn rejects_noncanonical_target_collision_without_mutation() {
        let (sqlite_home, pool) = fresh_full_legacy_state().await;
        let target = STATE_MIGRATOR
            .iter()
            .find(|migration| migration.version == 9000)
            .expect("collision target migration should be embedded");
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (?, ?, FALSE, ?, 0)",
        )
            .bind(target.version)
            .bind(target.description.as_ref())
            .bind(target.checksum.as_ref())
            .execute(&pool)
            .await
            .expect("unsuccessful target collision should be inserted");
        let before = ledger_snapshot(&pool).await;
        let error = repair_state_migrations(&pool, &STATE_MIGRATOR)
            .await
            .expect_err("noncanonical collision must fail closed");
        assert!(error.to_string().contains("marked unsuccessful"));
        assert_eq!(ledger_snapshot(&pool).await, before);
        pool.close().await;
        let _ = std::fs::remove_dir_all(sqlite_home);
    }

    #[tokio::test]
    async fn repairs_known_good_full_legacy_ledger_and_reopens_idempotently() {
        let sqlite_home = unique_temp_dir();
        tokio::fs::create_dir_all(&sqlite_home)
            .await
            .expect("sqlite home should be created");
        let _cleanup = scopeguard::guard(sqlite_home.clone(), |path| {
            let _ = std::fs::remove_dir_all(path);
        });
        let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
        let state_path = sqlite.state_db_path();
        let pool = sqlite
            .open_read_write_pool(&state_path)
            .await
            .expect("database should open");
        let legacy_without_backfill = known_good_legacy_migrator(false);
        legacy_without_backfill
            .run(&pool)
            .await
            .expect("legacy migrations through 49 should apply");
        for (id, rollout_path, source) in [
            ("parent", "/tmp/parent.jsonl", "{}"),
            (
                "child",
                "/tmp/child.jsonl",
                r#"{"subagent":{"thread_spawn":{"parent_thread_id":"parent"}}}"#,
            ),
        ] {
            sqlx::query(
                r#"
INSERT INTO threads (
    id, rollout_path, created_at, updated_at, source, model_provider, cwd,
    title, sandbox_policy, approval_mode, first_user_message
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                "#,
            )
            .bind(id)
            .bind(rollout_path)
            .bind(1_700_000_000_i64)
            .bind(1_700_000_001_i64)
            .bind(source)
            .bind("openai")
            .bind("/tmp")
            .bind(id)
            .bind("read-only")
            .bind("on-request")
            .bind(id)
            .execute(&pool)
            .await
            .expect("representative thread should insert");
        }
        let legacy = known_good_legacy_migrator(true);
        legacy
            .run(&pool)
            .await
            .expect("legacy migration 50 should apply");
        // Reproduce the deployed partial repair that preceded product 46240:
        // only the old24..32 prefix was remapped, while old33/34 remained in
        // the ledger. Migration 30 is then recorded canonically because its
        // column already exists from legacy migration 33. The unchanged
        // migrator applies migration 31 normally before failing at 32.
        for migration_move in SHIFTED_STATE_MIGRATION_MOVES
            .iter()
            .filter(|migration_move| migration_move.source_version <= 32)
        {
            sqlx::query("UPDATE _sqlx_migrations SET version = ? WHERE version = ?")
                .bind(temporary_repair_version(migration_move.source_version))
                .bind(migration_move.source_version)
                .execute(&pool)
                .await
                .expect("partial repair should reserve source version");
            let migration = STATE_MIGRATOR
                .migrations
                .iter()
                .find(|migration| migration.version == migration_move.target_version)
                .expect("partial repair target should be embedded");
            sqlx::query(
                "UPDATE _sqlx_migrations SET version = ?, description = ?, checksum = ? WHERE version = ?",
            )
            .bind(migration_move.target_version)
            .bind(migration.description.as_ref())
            .bind(migration.checksum.as_ref())
            .bind(temporary_repair_version(migration_move.source_version))
            .execute(&pool)
            .await
            .expect("partial repair should canonicalize source version");
        }
        for version in [30_i64] {
            let migration = STATE_MIGRATOR
                .migrations
                .iter()
                .find(|migration| migration.version == version)
                .expect("canonical migration should be embedded");
            sqlx::query(
                "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (?, ?, TRUE, ?, 0)",
            )
            .bind(version)
            .bind(migration.description.as_ref())
            .bind(migration.checksum.as_ref())
            .execute(&pool)
            .await
            .expect("partial repair should record canonical migration");
        }
        // Keep the failed migrator's database: migration 32 encounters the
        // preview column created by legacy migration 35.
        let failure = STATE_MIGRATOR
            .run(&pool)
            .await
            .expect_err("migration 32 should fail on existing preview column");
        assert!(failure.to_string().contains("preview"));
        for version in [30_i64, 31, 33, 34] {
            assert_eq!(
                sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM _sqlx_migrations WHERE version = ?",
                )
                .bind(version)
                .fetch_one(&pool)
                .await
                .expect("post-failure ledger row should be queryable"),
                1,
                "post-46240 ledger row {version} should remain",
            );
        }
        repair_state_migrations(&pool, &STATE_MIGRATOR)
            .await
            .expect("full legacy ledger should repair");
        STATE_MIGRATOR
            .run(&pool)
            .await
            .expect("current migrations should apply");
        for migration in STATE_MIGRATOR.iter() {
            let row = sqlx::query(
                "SELECT description, success, checksum FROM _sqlx_migrations WHERE version = ?",
            )
            .bind(migration.version)
            .fetch_one(&pool)
            .await
            .expect("every embedded migration should be recorded");
            assert_eq!(
                row.get::<String, _>("description"),
                migration.description.as_ref()
            );
            assert!(row.get::<bool, _>("success"));
            assert_eq!(
                row.get::<Vec<u8>, _>("checksum"),
                migration.checksum.to_vec()
            );
        }
        for index_name in [
            "idx_threads_section_recency_at_ms",
            "idx_threads_section_position",
        ] {
            let sql = sqlx::query_scalar::<_, String>(
                "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = ?",
            )
            .bind(index_name)
            .fetch_one(&pool)
            .await
            .expect("current section index should exist")
            .to_ascii_lowercase();
            assert!(!sql.contains("preview <> ''"));
            assert!(sql.contains("thread_section_id is not null"));
            if index_name.ends_with("recency_at_ms") {
                assert!(sql.contains("recency_at_ms desc, id desc"));
            } else {
                assert!(sql.contains("section_position asc, id asc"));
            }
        }
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM thread_spawn_edges WHERE parent_thread_id = 'parent' AND child_thread_id = 'child'")
                .fetch_one(&pool)
                .await
                .expect("spawn edge should exist"),
            1
        );
        assert!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM _sqlx_migrations WHERE version = 9003"
            )
            .fetch_one(&pool)
            .await
            .expect("alias row should exist")
                == 1
        );
        pool.close().await;
        let reopened = sqlite
            .open_read_write_pool(&state_path)
            .await
            .expect("database should reopen");
        repair_state_migrations(&reopened, &STATE_MIGRATOR)
            .await
            .expect("reopen repair should be idempotent");
        STATE_MIGRATOR
            .run(&reopened)
            .await
            .expect("reopen migrations should be idempotent");
        reopened.close().await;
    }

    #[tokio::test]
    async fn repairs_deployed_thread_source_schema_with_embedded_migration_metadata() {
        let sqlite_home = unique_temp_dir();
        tokio::fs::create_dir_all(&sqlite_home)
            .await
            .expect("sqlite home should be created");
        let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
            let _ = std::fs::remove_dir_all(sqlite_home);
        });
        let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
        let pool = sqlite
            .open_read_write_pool(&sqlite.state_db_path())
            .await
            .expect("sqlite database should open");
        sqlx::query("CREATE TABLE threads (thread_source TEXT)")
            .execute(&pool)
            .await
            .expect("deployed thread schema should be created");

        repair_state_migrations(&pool, &STATE_MIGRATOR)
            .await
            .expect("thread source migration should be repaired");

        let row =
            sqlx::query("SELECT description, checksum FROM _sqlx_migrations WHERE version = 30")
                .fetch_one(&pool)
                .await
                .expect("repaired migration row should exist");
        let embedded = STATE_MIGRATOR
            .iter()
            .find(|migration| migration.version == 30)
            .expect("thread source migration should be embedded");
        assert_eq!(
            row.get::<String, _>("description"),
            embedded.description.as_ref()
        );
        assert_eq!(
            row.get::<Vec<u8>, _>("checksum"),
            embedded.checksum.to_vec()
        );
    }

    #[tokio::test]
    async fn repairs_empty_state_before_first_migration_and_reopens_idempotently() {
        let sqlite_home = unique_temp_dir();
        tokio::fs::create_dir_all(&sqlite_home)
            .await
            .expect("sqlite home should be created");
        let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
            let _ = std::fs::remove_dir_all(sqlite_home);
        });
        let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
        let state_path = sqlite.state_db_path();
        let pool = sqlite
            .open_read_write_pool(&state_path)
            .await
            .expect("empty state database should open");

        repair_state_migrations(&pool, &STATE_MIGRATOR)
            .await
            .expect("empty state migration aliases should be recorded");
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM _sqlx_migrations WHERE version IN (9001, 9002)",
            )
            .fetch_one(&pool)
            .await
            .expect("alias migration rows should exist"),
            2
        );
        STATE_MIGRATOR
            .run(&pool)
            .await
            .expect("empty state migrations should complete");
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM _sqlx_migrations WHERE version IN (9001, 9002)",
            )
            .fetch_one(&pool)
            .await
            .expect("alias migration rows should remain canonical"),
            2
        );
        pool.close().await;

        let reopened = sqlite
            .open_read_write_pool(&state_path)
            .await
            .expect("reopened empty state database should open");
        repair_state_migrations(&reopened, &STATE_MIGRATOR)
            .await
            .expect("empty migration repair should be idempotent after reopen");
        STATE_MIGRATOR
            .run(&reopened)
            .await
            .expect("empty migrations should be idempotent after reopen");
        reopened.close().await;
    }

    #[tokio::test]
    async fn repairs_fresh_pre_migration_58_state_and_reopens_idempotently() {
        let sqlite_home = unique_temp_dir();
        tokio::fs::create_dir_all(&sqlite_home)
            .await
            .expect("sqlite home should be created");
        let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
            let _ = std::fs::remove_dir_all(sqlite_home);
        });
        let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
        let state_path = sqlite.state_db_path();
        let pool = sqlite
            .open_read_write_pool(&state_path)
            .await
            .expect("sqlite database should open");
        let pre_migration_58_migrator = Migrator {
            migrations: Cow::Owned(
                STATE_MIGRATOR
                    .migrations
                    .iter()
                    .filter(|migration| migration.version <= 57)
                    .cloned()
                    .collect(),
            ),
            ignore_missing: STATE_MIGRATOR.ignore_missing,
            locking: STATE_MIGRATOR.locking,
            no_tx: STATE_MIGRATOR.no_tx,
            table_name: STATE_MIGRATOR.table_name.clone(),
            create_schemas: STATE_MIGRATOR.create_schemas.clone(),
        };
        pre_migration_58_migrator
            .run(&pool)
            .await
            .expect("pre-58 state schema should apply");

        repair_state_migrations(&pool, &STATE_MIGRATOR)
            .await
            .expect("fresh state migration aliases should be recorded");
        for version in [9001_i64, 9002] {
            let row = sqlx::query(
                "SELECT success, description, checksum FROM _sqlx_migrations WHERE version = ?",
            )
            .bind(version)
            .fetch_one(&pool)
            .await
            .expect("fresh alias migration row should exist");
            let migration = STATE_MIGRATOR
                .migrations
                .iter()
                .find(|migration| migration.version == version)
                .expect("alias migration should be embedded");
            assert!(row.get::<bool, _>("success"));
            assert_eq!(
                row.get::<String, _>("description"),
                migration.description.as_ref()
            );
            assert_eq!(
                row.get::<Vec<u8>, _>("checksum"),
                migration.checksum.to_vec()
            );
        }
        STATE_MIGRATOR
            .run(&pool)
            .await
            .expect("fresh state migrations should complete");
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM pragma_table_info('thread_dynamic_tools') WHERE name IN ('persist_on_resume', 'capability_json', 'namespace_description')",
            )
            .fetch_one(&pool)
            .await
            .expect("dynamic tool columns should be queryable"),
            3
        );
        pool.close().await;

        let reopened = sqlite
            .open_read_write_pool(&state_path)
            .await
            .expect("reopened state database should open");
        repair_state_migrations(&reopened, &STATE_MIGRATOR)
            .await
            .expect("fresh migration repair should be idempotent after reopen");
        STATE_MIGRATOR
            .run(&reopened)
            .await
            .expect("fresh migrations should be idempotent after reopen");
        reopened.close().await;
    }

    #[tokio::test]
    async fn repairs_known_good_pre_migration_24_state_and_reopens_idempotently() {
        let sqlite_home = unique_temp_dir();
        tokio::fs::create_dir_all(&sqlite_home)
            .await
            .expect("sqlite home should be created");
        let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
            let _ = std::fs::remove_dir_all(sqlite_home);
        });
        let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
        let state_path = sqlite.state_db_path();
        let pool = sqlite
            .open_read_write_pool(&state_path)
            .await
            .expect("sqlite database should open");
        let pre_repair_migrator = Migrator {
            migrations: Cow::Owned(
                STATE_MIGRATOR
                    .migrations
                    .iter()
                    .filter(|migration| migration.version <= 23)
                    .cloned()
                    .collect(),
            ),
            ignore_missing: STATE_MIGRATOR.ignore_missing,
            locking: STATE_MIGRATOR.locking,
            no_tx: STATE_MIGRATOR.no_tx,
            table_name: STATE_MIGRATOR.table_name.clone(),
            create_schemas: STATE_MIGRATOR.create_schemas.clone(),
        };
        pre_repair_migrator
            .run(&pool)
            .await
            .expect("pre-repair state schema should apply");
        sqlx::query(
            r#"
INSERT INTO threads (
    id, rollout_path, created_at, updated_at, source, model_provider, cwd,
    title, sandbox_policy, approval_mode, first_user_message
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind("thread-preserved")
        .bind("/tmp/legacy.jsonl")
        .bind(1_700_000_000_i64)
        .bind(1_700_000_001_i64)
        .bind("cli")
        .bind("openai")
        .bind("/tmp")
        .bind("legacy title")
        .bind("read-only")
        .bind("on-request")
        .bind("legacy first message")
        .execute(&pool)
        .await
        .expect("representative thread should insert");
        sqlx::query(
            r#"
INSERT INTO thread_dynamic_tools (
    thread_id, position, name, description, input_schema, defer_loading
) VALUES (?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind("thread-preserved")
        .bind(0_i64)
        .bind("legacy-tool")
        .bind("legacy description")
        .bind(r#"{"type":"object"}"#)
        .bind(0_i64)
        .execute(&pool)
        .await
        .expect("representative dynamic tool should insert");

        for target_version in [9000_i64, 24, 25, 9001, 9002, 26, 27, 28, 29] {
            let migration = STATE_MIGRATOR
                .migrations
                .iter()
                .find(|migration| migration.version == target_version)
                .expect("historical target migration should be embedded");
            sqlx::raw_sql(migration.sql.as_ref())
                .execute(&pool)
                .await
                .expect("historical target schema should be present");
        }

        let historical_rows = [
            (24_i64, "phase2 attestation roots", 9000_i64),
            (25, "remote control enrollments", 24),
            (26, "thread timestamps millis", 25),
            (27, "thread dynamic tools persist on resume", 9001),
            (28, "thread dynamic tools capability json", 9002),
            (29, "thread dynamic tools namespace", 26),
            (30, "threads cwd sort indexes", 27),
            (31, "device key bindings", 28),
            (32, "thread goals", 29),
        ];
        for (source_version, description, target_version) in historical_rows {
            let checksum = STATE_MIGRATOR
                .migrations
                .iter()
                .find(|migration| migration.version == target_version)
                .expect("target migration should be embedded")
                .checksum
                .to_vec();
            sqlx::query(
                r#"
INSERT INTO _sqlx_migrations (
    version, description, success, checksum, execution_time
) VALUES (?, ?, TRUE, ?, 0)
                "#,
            )
            .bind(source_version)
            .bind(description)
            .bind(checksum)
            .execute(&pool)
            .await
            .expect("historical migration row should insert");
        }

        repair_state_migrations(&pool, &STATE_MIGRATOR)
            .await
            .expect("known-good historical migration rows should repair");
        STATE_MIGRATOR
            .run(&pool)
            .await
            .expect("forward state migrations should complete");

        let applied = sqlx::query(
            "SELECT version, description, success, checksum FROM _sqlx_migrations WHERE version IN (24, 25, 26, 27, 28, 29, 58, 9000, 9001, 9002) ORDER BY version",
        )
        .fetch_all(&pool)
            .await
            .expect("repaired migration rows should load");
        assert_eq!(applied.len(), 10);
        for row in applied {
            let version = row.get::<i64, _>("version");
            let migration = STATE_MIGRATOR
                .migrations
                .iter()
                .find(|migration| migration.version == version)
                .expect("canonical migration should be embedded");
            assert!(row.get::<bool, _>("success"));
            assert_eq!(
                row.get::<String, _>("description"),
                migration.description.as_ref()
            );
            assert_eq!(
                row.get::<Vec<u8>, _>("checksum"),
                migration.checksum.to_vec()
            );
        }
        let thread = sqlx::query(
            "SELECT title, first_user_message, created_at_ms, updated_at_ms, preview, thread_source FROM threads WHERE id = ?",
        )
        .bind("thread-preserved")
        .fetch_one(&pool)
        .await
        .expect("representative thread should survive forward migration");
        assert_eq!(thread.get::<String, _>("title"), "legacy title");
        assert_eq!(
            thread.get::<String, _>("first_user_message"),
            "legacy first message"
        );
        assert_eq!(thread.get::<i64, _>("created_at_ms"), 1_700_000_000_000);
        assert_eq!(thread.get::<i64, _>("updated_at_ms"), 1_700_000_001_000);
        assert_eq!(thread.get::<String, _>("preview"), "legacy first message");
        assert_eq!(thread.get::<Option<String>, _>("thread_source"), None);
        let dynamic_tool = sqlx::query(
            "SELECT namespace, namespace_description, persist_on_resume, capability_json FROM thread_dynamic_tools WHERE thread_id = ?",
        )
        .bind("thread-preserved")
        .fetch_one(&pool)
        .await
        .expect("representative dynamic tool should survive forward migration");
        assert_eq!(dynamic_tool.get::<Option<String>, _>("namespace"), None);
        assert_eq!(
            dynamic_tool.get::<Option<String>, _>("namespace_description"),
            None
        );
        assert_eq!(dynamic_tool.get::<i64, _>("persist_on_resume"), 1);
        assert_eq!(
            dynamic_tool.get::<Option<String>, _>("capability_json"),
            None
        );
        pool.close().await;

        let reopened = sqlite
            .open_read_write_pool(&state_path)
            .await
            .expect("reopened state database should open");
        repair_state_migrations(&reopened, &STATE_MIGRATOR)
            .await
            .expect("second migration repair should be idempotent");
        STATE_MIGRATOR
            .run(&reopened)
            .await
            .expect("second forward migration should be idempotent");
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM threads WHERE id = ?")
                .bind("thread-preserved")
                .fetch_one(&reopened)
                .await
                .expect("preserved thread should remain queryable"),
            1
        );
        reopened.close().await;
    }
}
