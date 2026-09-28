use anyhow::Context;
use sqlx::SqlitePool;
use sqlx::Row;
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
        source_description: "phase2_attestation_roots",
        target_version: 9000,
    },
    MigrationHistoryMove {
        source_version: 25,
        source_description: "remote_control_enrollments",
        target_version: 24,
    },
    MigrationHistoryMove {
        source_version: 26,
        source_description: "thread_timestamps_millis",
        target_version: 25,
    },
    MigrationHistoryMove {
        source_version: 27,
        source_description: "thread_dynamic_tools_persist_on_resume",
        target_version: 9001,
    },
    MigrationHistoryMove {
        source_version: 28,
        source_description: "thread_dynamic_tools_capability_json",
        target_version: 9002,
    },
    MigrationHistoryMove {
        source_version: 29,
        source_description: "thread_dynamic_tools_namespace",
        target_version: 26,
    },
    MigrationHistoryMove {
        source_version: 30,
        source_description: "threads_cwd_sort_indexes",
        target_version: 27,
    },
    MigrationHistoryMove {
        source_version: 31,
        source_description: "device_key_bindings",
        target_version: 28,
    },
    MigrationHistoryMove {
        source_version: 32,
        source_description: "thread_goals",
        target_version: 29,
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
    for repair in COLUMN_MIGRATION_REPAIRS {
        repair_column_migration(pool, migrator, repair).await?;
    }
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
        WHERE version BETWEEN 24 AND 32 OR version IN (9000, 9001, 9002)
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

    for migration_move in &moves {
        if rows.iter().any(|row| {
            row.version == migration_move.target_version
                && !moves.iter().any(|item| item.source_version == row.version)
        }) {
            anyhow::bail!(
                "state DB migration repair would overwrite existing migration {}; refusing automatic repair",
                migration_move.target_version
            );
        }
    }

    let mut tx = pool.begin().await?;
    for migration_move in &moves {
        sqlx::query("UPDATE _sqlx_migrations SET version = ? WHERE version = ?")
            .bind(temporary_repair_version(migration_move.source_version))
            .bind(migration_move.source_version)
            .execute(&mut *tx)
            .await?;
    }
    for migration_move in moves {
        let migration = migration_by_version(migrator, migration_move.target_version)
            .with_context(|| format!("embedded state migration {} is missing", migration_move.target_version))?;
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
    use super::repair_state_migrations;
    use crate::migrations::STATE_MIGRATOR;
    use crate::runtime::test_support::unique_temp_dir;
    use codex_utils_absolute_path::test_support::PathExt;
    use sqlx::Row;

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
}
