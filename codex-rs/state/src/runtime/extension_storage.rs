use codex_extension_api::ExtensionStorageId;
use sqlx::Row;
use sqlx::SqlitePool;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

mod automatic_turns;

pub(crate) const USAGE_LEDGER_STORAGE_ID: ExtensionStorageId =
    ExtensionStorageId::new("usage-ledger");
pub(crate) const PHASE2_ATTESTATION_STORAGE_ID: ExtensionStorageId =
    ExtensionStorageId::new("memories.phase2-attestation");

const EXTENSION_MIGRATIONS_TABLE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS extension_migrations (
    namespace TEXT NOT NULL,
    version INTEGER NOT NULL,
    description TEXT NOT NULL,
    checksum TEXT NOT NULL,
    applied_at INTEGER NOT NULL,
    PRIMARY KEY(namespace, version)
)
"#;

const PHASE2_ATTESTATION_ROOTS_SQL: &str =
    include_str!("../../migrations/0024_phase2_attestation_roots.sql");
const PHASE2_ATTESTED_BASELINES_SQL: &str =
    include_str!("../../migrations/0038_phase2_attested_baselines.sql");

const STATE_EXTENSION_MIGRATIONS: &[ExtensionStorageMigration] = &[
    ExtensionStorageMigration {
        storage_id: PHASE2_ATTESTATION_STORAGE_ID,
        version: 1,
        description: "phase2_attestation_roots",
        sql: PHASE2_ATTESTATION_ROOTS_SQL,
    },
    ExtensionStorageMigration {
        storage_id: PHASE2_ATTESTATION_STORAGE_ID,
        version: 2,
        description: "phase2_attested_baselines",
        sql: PHASE2_ATTESTED_BASELINES_SQL,
    },
];

#[derive(Clone, Copy, Debug)]
struct ExtensionStorageMigration {
    storage_id: ExtensionStorageId,
    version: i64,
    description: &'static str,
    sql: &'static str,
}

pub(super) async fn run_state_extension_migrations(pool: &SqlitePool) -> anyhow::Result<()> {
    run_extension_storage_migrations(pool, STATE_EXTENSION_MIGRATIONS).await
}

pub(super) fn state_extension_migration_phase() -> &'static str {
    "migrate_state_extensions"
}

async fn run_extension_storage_migrations(
    pool: &SqlitePool,
    migrations: &[ExtensionStorageMigration],
) -> anyhow::Result<()> {
    sqlx::query(EXTENSION_MIGRATIONS_TABLE_SQL)
        .execute(pool)
        .await?;

    for migration in migrations {
        run_extension_storage_migration(pool, *migration).await?;
    }

    Ok(())
}

async fn run_extension_storage_migration(
    pool: &SqlitePool,
    migration: ExtensionStorageMigration,
) -> anyhow::Result<()> {
    let checksum = checksum(migration.sql);
    let existing = sqlx::query(
        r#"
SELECT checksum
FROM extension_migrations
WHERE namespace = ? AND version = ?
        "#,
    )
    .bind(migration.storage_id.as_str())
    .bind(migration.version)
    .fetch_optional(pool)
    .await?;
    if let Some(existing) = existing {
        let existing_checksum: String = existing.try_get("checksum")?;
        anyhow::ensure!(
            existing_checksum == checksum,
            "extension storage migration checksum mismatch for {} version {}",
            migration.storage_id.as_str(),
            migration.version
        );
        return Ok(());
    }

    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    for statement in migration
        .sql
        .split(';')
        .map(str::trim)
        .filter(|statement| !statement.is_empty())
    {
        sqlx::query(statement).execute(&mut *tx).await?;
    }
    sqlx::query(
        r#"
INSERT INTO extension_migrations (
    namespace,
    version,
    description,
    checksum,
    applied_at
) VALUES (?, ?, ?, ?, ?)
        "#,
    )
    .bind(migration.storage_id.as_str())
    .bind(migration.version)
    .bind(migration.description)
    .bind(checksum)
    .bind(current_epoch_seconds())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(())
}

fn checksum(sql: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET;
    for byte in sql.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("fnv1a64:{hash:016x}")
}

fn current_epoch_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::StateRuntime;
    use crate::runtime::test_support::unique_temp_dir;
    use codex_utils_absolute_path::test_support::PathExt;
    use pretty_assertions::assert_eq;
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::sqlite::SqlitePoolOptions;

    #[tokio::test]
    async fn extension_storage_migrations_use_namespace_versions() -> anyhow::Result<()> {
        let codex_home = unique_temp_dir();
        let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
        let runtime = StateRuntime::init(sqlite.clone(), "test-provider".to_string()).await?;
        drop(runtime);
        let state_db_path = sqlite.state_db_path();

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(state_db_path)
                    .create_if_missing(false),
            )
            .await?;

        let extension_versions = sqlx::query(
            "SELECT namespace, version FROM extension_migrations ORDER BY namespace, version",
        )
        .fetch_all(&pool)
        .await?
        .into_iter()
        .map(|row| {
            Ok::<_, sqlx::Error>((
                row.try_get::<String, _>("namespace")?,
                row.try_get::<i64, _>("version")?,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(
            extension_versions,
            vec![
                (PHASE2_ATTESTATION_STORAGE_ID.as_str().to_string(), 1),
                (PHASE2_ATTESTATION_STORAGE_ID.as_str().to_string(), 2),
            ]
        );

        let core_version_one_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM _sqlx_migrations WHERE version = 1)")
                .fetch_one(&pool)
                .await?;
        assert!(
            core_version_one_exists,
            "core migration version 1 should coexist with extension migration version 1"
        );

        pool.close().await;
        let _ = tokio::fs::remove_dir_all(codex_home).await;
        Ok(())
    }
}
