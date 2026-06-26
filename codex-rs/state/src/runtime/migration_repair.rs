use anyhow::Context;
use sqlx::SqlitePool;
use sqlx::migrate::Migration;
use sqlx::migrate::Migrator;

use crate::migrations::repair_state_migration_version_collisions;

struct ColumnMigrationRepair {
    version: i64,
    table_name: &'static str,
    column_name: &'static str,
}

const COLUMN_MIGRATION_REPAIRS: &[ColumnMigrationRepair] = &[ColumnMigrationRepair {
    version: 33,
    table_name: "threads",
    column_name: "thread_source",
}];

pub(super) async fn repair_state_migrations(
    pool: &SqlitePool,
    migrator: &Migrator,
) -> anyhow::Result<()> {
    repair_state_migration_version_collisions(pool, migrator).await?;
    for repair in COLUMN_MIGRATION_REPAIRS {
        repair_column_migration(pool, migrator, repair).await?;
    }
    Ok(())
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
