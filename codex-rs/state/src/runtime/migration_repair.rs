use anyhow::Context;
use sqlx::SqlitePool;
use sqlx::migrate::Migration;
use sqlx::migrate::Migrator;

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
