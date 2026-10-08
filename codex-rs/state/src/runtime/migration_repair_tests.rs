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
    let before_data = thread_data(&pool).await;
    run_state_migrations(&pool, &STATE_MIGRATOR)
        .await
        .expect("recognized lineage should bridge and migrate");
    assert_all_applied(&pool).await;
    assert_eq!(thread_data(&pool).await, before_data);
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
    assert_eq!(thread_data(&reopened).await, before_data);
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
    for last in [3_i64, 4, 14, 23, 55, 58, 59, 60] {
        let (sqlite, pool) = fixture().await;
        upstream_through(last)
            .run(&pool)
            .await
            .expect("upstream prefix should apply");
        seed_thread(&pool).await;
        bridge_state_migrations(&pool, &STATE_MIGRATOR)
            .await
            .expect("reserve aliases on an upstream prefix");
        bridge_state_migrations(&pool, &STATE_MIGRATOR)
            .await
            .expect("interrupted pre-migrator prefix remains recognized");
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

    for last in [29_i64, 41, 50] {
        let (sqlite, pool) = fixture().await;
        shifted_fork_through(last)
            .run(&pool)
            .await
            .expect("shifted fork prefix should apply");
        seed_thread(&pool).await;
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

async fn seed_thread(pool: &SqlitePool) {
    sqlx::query(
        "INSERT INTO threads (
            id, rollout_path, created_at, updated_at, source, model_provider,
            cwd, title, sandbox_policy, approval_mode
        ) VALUES (
            'bridge-preservation-thread', '/synthetic/bridge.jsonl',
            1700000000, 1700000001, 'cli', 'synthetic-provider',
            '/synthetic', 'preserved bridge data', 'read-only', 'on-request'
        )",
    )
    .execute(pool)
    .await
    .expect("seed synthetic thread");
}

async fn thread_data(pool: &SqlitePool) -> Vec<(String, String, i64)> {
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'threads')",
    )
    .fetch_one(pool)
    .await
    .expect("thread table presence");
    if !exists {
        return Vec::new();
    }
    sqlx::query_as("SELECT id, title, updated_at FROM threads ORDER BY id")
        .fetch_all(pool)
        .await
        .expect("synthetic thread preimage")
}

type BridgePreimage = (
    bool,
    Vec<(i64, String, String, bool, Vec<u8>, i64)>,
    Vec<(i64, i64, String, Vec<u8>, i64)>,
    Vec<(String, String, Option<String>)>,
    Vec<(String, String, i64)>,
);

async fn preimage(pool: &SqlitePool) -> BridgePreimage {
    let has_ledger = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = '_sqlx_migrations')",
    )
    .fetch_one(pool)
    .await
    .expect("ledger presence");
    let ledger = if has_ledger {
        sqlx::query_as(
            "SELECT version, description, CAST(installed_on AS TEXT), success, checksum, execution_time
             FROM _sqlx_migrations ORDER BY version",
        )
        .fetch_all(pool)
        .await
        .expect("complete ledger preimage")
    } else {
        Vec::new()
    };
    let has_receipts = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'state_migration_rekey_receipts')",
    )
    .fetch_one(pool)
    .await
    .expect("receipt presence");
    let receipts = if has_receipts {
        sqlx::query_as(
            "SELECT old_version, new_version, old_description, checksum, execution_time
             FROM state_migration_rekey_receipts ORDER BY old_version",
        )
        .fetch_all(pool)
        .await
        .expect("complete receipt preimage")
    } else {
        Vec::new()
    };
    (
        has_ledger,
        ledger,
        receipts,
        schema(pool).await,
        thread_data(pool).await,
    )
}

#[tokio::test]
async fn interrupted_aggregate_fork_sequence_reserves_both_aliases() {
    let (sqlite, pool) = fixture().await;
    with_migrations(
        STATE_MIGRATOR
            .iter()
            .filter(|migration| migration.version <= FORK_58)
            .cloned()
            .collect(),
    )
    .run(&pool)
    .await
    .expect("raw sequence through the aggregate fork migration should apply");
    seed_thread(&pool).await;
    bridge_migrate_reopen(sqlite, pool).await;
}

#[tokio::test]
async fn canonical_gaps_and_55_59_60_effect_mismatches_reject_without_bridge_writes() {
    for (defect, through, expected_error) in [
        ("unknown_9999", 60, "unknown identity"),
        ("gap_55", 60, "gap at 55"),
        ("gap_56", 60, "gap at 56"),
        ("gap_58", 60, "gap at 58"),
        ("gap_59", 60, "gap at 59"),
        ("missing_attachments", 55, "migration 55"),
        ("early_attachments", 54, "migration 55"),
        ("missing_59", 59, "migration 59"),
        ("wrong_59_owner", 59, "migration 59"),
        ("wrong_59_order", 59, "migration 59"),
        ("unique_59", 59, "migration 59"),
        ("partial_59", 59, "migration 59"),
        ("early_59", 58, "migration 59"),
        ("missing_60", 60, "migration 60"),
        ("type_60", 60, "migration 60"),
        ("nullable_60", 60, "migration 60"),
        ("pk_60", 60, "migration 60"),
        ("fk_60", 60, "migration 60"),
        ("extra_60", 60, "migration 60"),
        ("default_60", 60, "migration 60"),
        ("early_60", 59, "migration 60"),
        ("missing_early_tools", 14, "state migration 4"),
        ("no_ledger_stray_threads", 0, "state migration 1"),
    ] {
        let (_sqlite, pool) = fixture().await;
        if through == 0 {
            sqlx::query(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, title TEXT, updated_at INTEGER);
                 INSERT INTO threads VALUES ('stray-thread', 'preserve stray data', 1700000001)",
            )
            .execute(&pool)
            .await
            .expect("malformed purported fresh schema");
        } else {
            upstream_through(through)
                .run(&pool)
                .await
                .expect("canonical fixture prefix");
            seed_thread(&pool).await;
        }
        match defect {
            "unknown_9999" => {
                sqlx::query(
                    "INSERT INTO _sqlx_migrations
                     (version, description, success, checksum, execution_time)
                     VALUES (9999, 'unknown future migration', TRUE, X'01020304', 1)",
                )
                .execute(&pool)
                .await
                .expect("synthetic unknown migration");
            }
            "gap_55" | "gap_56" | "gap_58" | "gap_59" => {
                let version = match defect {
                    "gap_55" => 55_i64,
                    "gap_56" => 56,
                    "gap_58" => 58,
                    "gap_59" => 59,
                    _ => unreachable!(),
                };
                sqlx::query("DELETE FROM _sqlx_migrations WHERE version = ?")
                    .bind(version)
                    .execute(&pool)
                    .await
                    .expect("remove canonical predecessor");
            }
            "missing_attachments" => {
                sqlx::query("DROP TABLE thread_attachments")
                    .execute(&pool)
                    .await
                    .expect("remove recorded attachment effect");
            }
            "early_attachments" | "early_59" | "early_60" => {
                let version = match defect {
                    "early_attachments" => 55_i64,
                    "early_59" => 59,
                    "early_60" => 60,
                    _ => unreachable!(),
                };
                sqlx::query(
                    embedded(&STATE_MIGRATOR, version)
                        .expect("effect SQL")
                        .sql
                        .as_ref(),
                )
                .execute(&pool)
                .await
                .expect("apply schema effect without its ledger row");
            }
            "missing_59" | "wrong_59_owner" | "wrong_59_order" | "unique_59" | "partial_59" => {
                sqlx::query("DROP INDEX idx_thread_attachments_identity_thread")
                    .execute(&pool)
                    .await
                    .expect("remove recorded reverse index");
                let replacement = match defect {
                    "missing_59" => None,
                    "wrong_59_owner" => Some(
                        "CREATE INDEX idx_thread_attachments_identity_thread ON threads(source, cwd, id)",
                    ),
                    "wrong_59_order" => Some(
                        "CREATE INDEX idx_thread_attachments_identity_thread ON thread_attachments(thread_id, identity_key, attachment_type)",
                    ),
                    "unique_59" => Some(
                        "CREATE UNIQUE INDEX idx_thread_attachments_identity_thread ON thread_attachments(attachment_type, identity_key, thread_id)",
                    ),
                    "partial_59" => Some(
                        "CREATE INDEX idx_thread_attachments_identity_thread ON thread_attachments(attachment_type, identity_key, thread_id) WHERE thread_id IS NOT NULL",
                    ),
                    _ => unreachable!(),
                };
                if let Some(sql) = replacement {
                    sqlx::query(sql)
                        .execute(&pool)
                        .await
                        .expect("synthetic reverse index mismatch");
                }
            }
            "missing_60" | "type_60" | "nullable_60" | "pk_60" | "fk_60" | "extra_60"
            | "default_60" => {
                sqlx::query("DROP TABLE guardian_review_feedback")
                    .execute(&pool)
                    .await
                    .expect("remove recorded feedback table");
                let replacement = match defect {
                    "missing_60" => None,
                    "type_60" => Some(
                        "CREATE TABLE guardian_review_feedback (id TEXT PRIMARY KEY NOT NULL, thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE, record TEXT NOT NULL)",
                    ),
                    "nullable_60" => Some(
                        "CREATE TABLE guardian_review_feedback (id TEXT PRIMARY KEY NOT NULL, thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE, record BLOB)",
                    ),
                    "pk_60" => Some(
                        "CREATE TABLE guardian_review_feedback (id TEXT NOT NULL, thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE, record BLOB NOT NULL)",
                    ),
                    "fk_60" => Some(
                        "CREATE TABLE guardian_review_feedback (id TEXT PRIMARY KEY NOT NULL, thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE RESTRICT, record BLOB NOT NULL)",
                    ),
                    "extra_60" => Some(
                        "CREATE TABLE guardian_review_feedback (id TEXT PRIMARY KEY NOT NULL, thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE, record BLOB NOT NULL, extra TEXT)",
                    ),
                    "default_60" => Some(
                        "CREATE TABLE guardian_review_feedback (id TEXT PRIMARY KEY NOT NULL, thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE, record BLOB NOT NULL DEFAULT X'01')",
                    ),
                    _ => unreachable!(),
                };
                if let Some(sql) = replacement {
                    sqlx::query(sql)
                        .execute(&pool)
                        .await
                        .expect("synthetic feedback shape mismatch");
                }
            }
            "missing_early_tools" => {
                sqlx::query("DROP TABLE thread_dynamic_tools")
                    .execute(&pool)
                    .await
                    .expect("remove early core table");
            }
            "no_ledger_stray_threads" => {}
            _ => unreachable!(),
        }
        let before = preimage(&pool).await;
        let error = run_state_migrations(&pool, &STATE_MIGRATOR)
            .await
            .expect_err("malformed lineage must reject before bridge writes");
        assert!(
            error.to_string().contains(expected_error),
            "{defect}: expected {expected_error}, got {error:#}"
        );
        assert_eq!(preimage(&pool).await, before, "{defect}");
        pool.close().await;
    }
}

#[tokio::test]
async fn cancelling_after_bridge_writes_rolls_back_on_the_same_connection() {
    let home = unique_temp_dir();
    tokio::fs::create_dir_all(&home)
        .await
        .expect("synthetic cancellation fixture directory");
    let sqlite = crate::SqliteConfig::new_for_testing(home.as_path().abs());
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(sqlite.state_db_path())
                .create_if_missing(true),
        )
        .await
        .expect("one-connection synthetic pool");
    old_fork_56_58()
        .run(&pool)
        .await
        .expect("historical input for transactional rekeying");
    seed_thread(&pool).await;
    let before = preimage(&pool).await;
    let (written, writes_observed) = tokio::sync::oneshot::channel();
    let task_pool = pool.clone();
    let task = tokio::spawn(async move {
        bridge_transaction(&task_pool, &STATE_MIGRATOR, async {
            written.send(()).expect("notify after actual bridge writes");
            std::future::pending::<()>().await;
        })
        .await
    });
    writes_observed
        .await
        .expect("bridge reached its write seam");
    task.abort();
    assert!(
        task.await
            .expect_err("bridge task should abort")
            .is_cancelled()
    );

    let mut transaction = pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .expect("same pooled connection must be reusable after cancellation");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM _sqlx_migrations WHERE version < 0",)
            .fetch_one(&mut *transaction)
            .await
            .expect("temporary rekey versions must be rolled back"),
        0
    );
    transaction.rollback().await.expect("release writer slot");
    assert_eq!(preimage(&pool).await, before);
    bridge_migrate_reopen(sqlite, pool).await;
}

#[tokio::test]
async fn shifted_identity_cannot_hide_malformed_canonical_tail() {
    for missing_57 in [false, true] {
        let (_sqlite, pool) = fixture().await;
        with_migrations(
            STATE_MIGRATOR
                .iter()
                .filter(|migration| migration.version <= FORK_9000)
                .cloned()
                .collect(),
        )
        .run(&pool)
        .await
        .expect("canonical through 60 and fork through F3 should apply");
        seed_thread(&pool).await;
        let shifted = embedded(&STATE_MIGRATOR, FORK_9000).expect("known shifted 24 target");
        sqlx::query("UPDATE _sqlx_migrations SET description = ?, checksum = ? WHERE version = 24")
            .bind(shifted.description.as_ref())
            .bind(shifted.checksum.as_ref())
            .execute(&pool)
            .await
            .expect("make the exact recognized shifted 24 identity");
        if missing_57 {
            sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 57")
                .execute(&pool)
                .await
                .expect("make an additional canonical tail gap");
        }
        let before = preimage(&pool).await;
        let error = run_state_migrations(&pool, &STATE_MIGRATOR)
            .await
            .expect_err("a shifted row must not hide malformed canonical extension");
        // Both variants first lack classified canonical 24. The extra missing
        // 57 variant also reproduces the independently hidden tail gap.
        assert!(
            error.to_string().contains("gap at 24"),
            "missing_57={missing_57}: {error:#}"
        );
        assert_eq!(preimage(&pool).await, before, "missing_57={missing_57}");
        pool.close().await;
    }
}
