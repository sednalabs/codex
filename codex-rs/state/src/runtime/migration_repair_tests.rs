use super::*;
use crate::migrations::STATE_MIGRATOR;
use crate::runtime::test_support::unique_temp_dir;
use codex_utils_absolute_path::test_support::PathExt;
use sqlx::migrate::Migrator;
use std::borrow::Cow;

fn with_migrations(migrations: Vec<Migration>) -> Migrator {
    Migrator {
        migrations: Cow::Owned(migrations),
        ignore_missing: STATE_MIGRATOR.ignore_missing,
        locking: STATE_MIGRATOR.locking,
        no_tx: STATE_MIGRATOR.no_tx,
        table_name: STATE_MIGRATOR.table_name.clone(),
        create_schemas: STATE_MIGRATOR.create_schemas.clone(),
    }
}

fn upstream_through(last: i64) -> Migrator {
    with_migrations(
        STATE_MIGRATOR
            .iter()
            .filter(|migration| migration.version <= last)
            .cloned()
            .collect(),
    )
}

fn old_fork_56_58() -> Migrator {
    let mut migrations = upstream_through(55).iter().cloned().collect::<Vec<_>>();
    for (old, target) in LEGACY_FORK_IDS.iter().take(3) {
        let current = embedded(&STATE_MIGRATOR, *target).expect("fork migration is embedded");
        migrations.push(Migration::new(
            *old,
            current.description.clone(),
            current.migration_type,
            current.sql.clone(),
            current.no_tx,
        ));
    }
    with_migrations(migrations)
}

fn old_fork_alias_pair() -> Migrator {
    let mut migrations = upstream_through(55).iter().cloned().collect::<Vec<_>>();
    for (old, target) in LEGACY_FORK_IDS
        .iter()
        .filter(|(old, _)| *old == 9001 || *old == 9002)
    {
        let current = embedded(&STATE_MIGRATOR, *target).expect("alias migration is embedded");
        migrations.push(Migration::new(
            *old,
            current.description.clone(),
            current.migration_type,
            current.sql.clone(),
            current.no_tx,
        ));
    }
    with_migrations(migrations)
}

fn shifted_fork_through(last: i64) -> Migrator {
    let mut migrations = upstream_through(23).iter().cloned().collect::<Vec<_>>();
    for (old, _, target) in SHIFTED.iter().filter(|(old, _, _)| *old <= last) {
        let current = embedded(&STATE_MIGRATOR, *target).expect("shifted target is embedded");
        migrations.push(Migration::new(
            *old,
            current.description.clone(),
            current.migration_type,
            current.sql.clone(),
            current.no_tx,
        ));
    }
    migrations.sort_by_key(|migration| migration.version);
    with_migrations(migrations)
}

async fn fixture() -> (crate::SqliteConfig, SqlitePool) {
    let home = unique_temp_dir();
    tokio::fs::create_dir_all(&home)
        .await
        .expect("synthetic fixture directory");
    let sqlite = crate::SqliteConfig::new_for_testing(home.as_path().abs());
    let pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .expect("synthetic state database");
    (sqlite, pool)
}

async fn ledger(pool: &SqlitePool) -> Vec<(i64, String, bool, Vec<u8>, i64)> {
    sqlx::query(
        "SELECT version, description, success, checksum, execution_time
         FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(pool)
    .await
    .expect("ledger")
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

async fn schema(pool: &SqlitePool) -> Vec<(String, String, Option<String>)> {
    sqlx::query("SELECT type, name, sql FROM sqlite_schema ORDER BY type, name")
        .fetch_all(pool)
        .await
        .expect("schema")
        .into_iter()
        .map(|row| (row.get("type"), row.get("name"), row.get("sql")))
        .collect()
}

async fn assert_all_applied(pool: &SqlitePool) {
    for migration in STATE_MIGRATOR.iter() {
        let row = sqlx::query(
            "SELECT description, success, checksum FROM _sqlx_migrations WHERE version = ?",
        )
        .bind(migration.version)
        .fetch_one(pool)
        .await
        .expect("canonical migration row");
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
}

async fn bridge_migrate_reopen(sqlite: crate::SqliteConfig, pool: SqlitePool) {
    run_state_migrations(&pool, &STATE_MIGRATOR)
        .await
        .expect("recognized lineage should bridge and migrate");
    assert_all_applied(&pool).await;
    pool.close().await;
    let reopened = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .expect("synthetic fixture should reopen");
    let before = ledger(&reopened).await;
    run_state_migrations(&reopened, &STATE_MIGRATOR)
        .await
        .expect("second bridge and migrator run should be idempotent");
    assert_eq!(ledger(&reopened).await, before);
    reopened.close().await;
}

#[tokio::test]
async fn fresh_and_upstream_prefixes_coexist_with_fork_namespace() {
    let (sqlite, pool) = fixture().await;
    bridge_state_migrations(&pool, &STATE_MIGRATOR)
        .await
        .expect("fresh pre-migrator alias marking should succeed");
    bridge_state_migrations(&pool, &STATE_MIGRATOR)
        .await
        .expect("fresh pre-migrator reopen should remain recognized");
    bridge_migrate_reopen(sqlite, pool).await;
    for last in [23_i64, 55, 58] {
        let (sqlite, pool) = fixture().await;
        upstream_through(last)
            .run(&pool)
            .await
            .expect("upstream prefix should apply");
        bridge_migrate_reopen(sqlite, pool).await;
    }
}

#[tokio::test]
async fn old_fork_56_58_and_shifted_history_rekey_without_checksum_change() {
    let (sqlite, pool) = fixture().await;
    old_fork_56_58()
        .run(&pool)
        .await
        .expect("fork prefix should apply");
    sqlx::query(
        "INSERT INTO threads (
            id, rollout_path, created_at, updated_at, source, model_provider, cwd,
            title, sandbox_policy, approval_mode, first_user_message
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("00000000-0000-0000-0000-000000000123")
    .bind("/synthetic/rollout.jsonl")
    .bind(1_700_000_000_i64)
    .bind(1_700_000_001_i64)
    .bind("cli")
    .bind("synthetic-provider")
    .bind("/synthetic")
    .bind("preserved title")
    .bind("read-only")
    .bind("on-request")
    .bind("preserved message")
    .execute(&pool)
    .await
    .expect("synthetic thread data should insert");
    let old_checksum = sqlx::query_scalar::<_, Vec<u8>>(
        "SELECT checksum FROM _sqlx_migrations WHERE version = 56",
    )
    .fetch_one(&pool)
    .await
    .expect("old fork row");
    run_state_migrations(&pool, &STATE_MIGRATOR)
        .await
        .expect("old fork rows should rekey before full migrations");
    let new_checksum =
        sqlx::query_scalar::<_, Vec<u8>>("SELECT checksum FROM _sqlx_migrations WHERE version = ?")
            .bind(FORK_56)
            .fetch_one(&pool)
            .await
            .expect("rekeyed fork row");
    assert_eq!(old_checksum, new_checksum);
    let receipt = sqlx::query_scalar::<_, i64>(
        "SELECT new_version FROM state_migration_rekey_receipts WHERE old_version = 56",
    )
    .fetch_one(&pool)
    .await
    .expect("durable mapping receipt");
    assert_eq!(receipt, FORK_56);
    assert_all_applied(&pool).await;
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT title FROM threads WHERE id = '00000000-0000-0000-0000-000000000123'",
        )
        .fetch_one(&pool)
        .await
        .expect("synthetic thread should survive"),
        "preserved title"
    );
    pool.close().await;
    let reopened = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .expect("fork fixture should reopen");
    run_state_migrations(&reopened, &STATE_MIGRATOR)
        .await
        .expect("fork second open and migration should be idempotent");
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT first_user_message FROM threads WHERE id = '00000000-0000-0000-0000-000000000123'",
        )
        .fetch_one(&reopened)
        .await
        .expect("synthetic thread should survive reopen"),
        "preserved message"
    );
    reopened.close().await;

    for last in [29_i64, 50] {
        let (sqlite, pool) = fixture().await;
        shifted_fork_through(last)
            .run(&pool)
            .await
            .expect("shifted fork prefix should apply");
        bridge_migrate_reopen(sqlite, pool).await;
    }
    let (sqlite, pool) = fixture().await;
    old_fork_alias_pair()
        .run(&pool)
        .await
        .expect("two-column alias history should apply");
    bridge_migrate_reopen(sqlite, pool).await;
}

#[tokio::test]
async fn deployed_thread_source_column_without_row_is_recognized() {
    let (sqlite, pool) = fixture().await;
    upstream_through(29)
        .run(&pool)
        .await
        .expect("pre-column upstream prefix");
    sqlx::query("ALTER TABLE threads ADD COLUMN thread_source TEXT")
        .execute(&pool)
        .await
        .expect("synthetic deployed column");
    bridge_migrate_reopen(sqlite, pool).await;
}

#[tokio::test]
async fn unknown_failed_gapped_and_partial_history_reject_without_bridge_writes() {
    for defect in [
        "checksum",
        "mixed",
        "failed",
        "gap",
        "upstream_column",
        "alias",
    ] {
        let (sqlite, pool) = fixture().await;
        upstream_through(if defect == "upstream_column" || defect == "alias" {
            55
        } else {
            58
        })
        .run(&pool)
        .await
        .expect("baseline upstream fixture");
        sqlx::query(
            "INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind("negative-bridge-preservation")
        .bind("/synthetic/negative.jsonl")
        .bind(1_700_000_000_i64)
        .bind(1_700_000_001_i64)
        .bind("cli")
        .bind("synthetic-provider")
        .bind("/synthetic")
        .bind("preserve this row")
        .bind("read-only")
        .bind("on-request")
        .execute(&pool)
        .await
        .expect("synthetic preservation row should insert");
        match defect {
            "checksum" => {
                sqlx::query("UPDATE _sqlx_migrations SET checksum = X'01' WHERE version = 56")
                    .execute(&pool)
                    .await
                    .expect("alter checksum");
            }
            "mixed" => {
                let fork = embedded(&STATE_MIGRATOR, FORK_56).expect("fork migration");
                sqlx::query(
                    "UPDATE _sqlx_migrations SET description = ?, checksum = ? WHERE version = 56",
                )
                .bind(fork.description.as_ref())
                .bind(fork.checksum.as_ref())
                .execute(&pool)
                .await
                .expect("make mixed upstream/fork identity");
            }
            "failed" => {
                sqlx::query("UPDATE _sqlx_migrations SET success = FALSE WHERE version = 56")
                    .execute(&pool)
                    .await
                    .expect("mark failed");
            }
            "gap" => {
                sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 27")
                    .execute(&pool)
                    .await
                    .expect("make middle gap");
            }
            "upstream_column" => {
                sqlx::query("ALTER TABLE threads ADD COLUMN creator_user_id TEXT")
                    .execute(&pool)
                    .await
                    .expect("make partial upstream 56 schema");
            }
            "alias" => {
                let alias = embedded(&STATE_MIGRATOR, FORK_9001).expect("alias migration");
                sqlx::query(
                    "INSERT INTO _sqlx_migrations
                     (version, description, success, checksum, execution_time)
                     VALUES (9001, ?, TRUE, ?, 0)",
                )
                .bind(alias.description.as_ref())
                .bind(alias.checksum.as_ref())
                .execute(&pool)
                .await
                .expect("make partial alias pair");
            }
            _ => unreachable!(),
        }
        let before_ledger = ledger(&pool).await;
        let before_schema = schema(&pool).await;
        let before_data = sqlx::query(
            "SELECT id, title, updated_at FROM threads WHERE id = 'negative-bridge-preservation'",
        )
        .fetch_all(&pool)
        .await
        .expect("synthetic preservation row should load");
        run_state_migrations(&pool, &STATE_MIGRATOR)
            .await
            .expect_err("unrecognized or partial history must reject");
        assert_eq!(ledger(&pool).await, before_ledger, "{defect}");
        assert_eq!(schema(&pool).await, before_schema, "{defect}");
        let after_data = sqlx::query(
            "SELECT id, title, updated_at FROM threads WHERE id = 'negative-bridge-preservation'",
        )
        .fetch_all(&pool)
        .await
        .expect("synthetic preservation row should remain readable");
        assert_eq!(after_data.len(), before_data.len(), "{defect}");
        assert_eq!(
            after_data[0].get::<String, _>("title"),
            before_data[0].get::<String, _>("title"),
            "{defect}"
        );
        assert_eq!(
            after_data[0].get::<i64, _>("updated_at"),
            before_data[0].get::<i64, _>("updated_at"),
            "{defect}"
        );
        pool.close().await;
        let _ = sqlite;
    }
}
