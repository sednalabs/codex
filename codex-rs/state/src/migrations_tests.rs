use std::borrow::Cow;
use std::collections::BTreeSet;

use sqlx::Row;
use sqlx::migrate::Migration;
use sqlx::migrate::Migrator;
use sqlx::sqlite::SqlitePoolOptions;

use super::STATE_MIGRATOR;
use super::repair_state_migration_version_collisions;

const PRE_RECENCY_MIGRATION_VERSION: i64 = 42;
const LEGACY_RECENCY_MIGRATION_VERSION: i64 = 38;
const CURRENT_RECENCY_MIGRATION_VERSION: i64 = 43;
const LEGACY_VISIBLE_SORT_INDEXES_MIGRATION_VERSION: i64 = 40;
const CURRENT_VISIBLE_SORT_INDEXES_MIGRATION_VERSION: i64 = 44;

fn migrator_through(version: i64) -> Migrator {
    Migrator {
        migrations: Cow::Owned(
            STATE_MIGRATOR
                .migrations
                .iter()
                .filter(|migration| migration.version <= version)
                .cloned()
                .collect(),
        ),
        ignore_missing: STATE_MIGRATOR.ignore_missing,
        locking: STATE_MIGRATOR.locking,
        table_name: STATE_MIGRATOR.table_name.clone(),
        create_schemas: STATE_MIGRATOR.create_schemas.clone(),
        no_tx: STATE_MIGRATOR.no_tx,
    }
}

#[test]
fn state_migration_versions_are_unique() {
    let mut seen = BTreeSet::new();
    let mut duplicates = Vec::new();
    for migration in STATE_MIGRATOR.iter() {
        if !seen.insert(migration.version) {
            duplicates.push(migration.version);
        }
    }
    assert!(
        duplicates.is_empty(),
        "duplicate state migration versions: {duplicates:?}"
    );
}

#[tokio::test]
async fn recency_migration_backfills_and_seeds_old_binary_inserts() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("in-memory database should open");
    migrator_through(PRE_RECENCY_MIGRATION_VERSION)
        .run(&pool)
        .await
        .expect("pre-recency migrations should apply");

    sqlx::query(
        r#"
INSERT INTO threads (
    id,
    rollout_path,
    created_at,
    updated_at,
    created_at_ms,
    updated_at_ms,
    source,
    model_provider,
    cwd,
    title,
    sandbox_policy,
    approval_mode
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind("00000000-0000-0000-0000-000000000001")
    .bind("/tmp/first.jsonl")
    .bind(1_700_000_000_i64)
    .bind(1_700_000_100_i64)
    .bind(1_700_000_000_123_i64)
    .bind(1_700_000_100_456_i64)
    .bind("cli")
    .bind("openai")
    .bind("/tmp")
    .bind("")
    .bind("read-only")
    .bind("on-request")
    .execute(&pool)
    .await
    .expect("legacy row should insert");

    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("recency migration should apply");

    let backfilled = sqlx::query(
        "SELECT updated_at, updated_at_ms, recency_at, recency_at_ms FROM threads WHERE id = ?",
    )
    .bind("00000000-0000-0000-0000-000000000001")
    .fetch_one(&pool)
    .await
    .expect("backfilled row should load");
    assert_eq!(backfilled.get::<i64, _>("recency_at"), 1_700_000_100);
    assert_eq!(backfilled.get::<i64, _>("recency_at_ms"), 1_700_000_100_456);

    sqlx::query(
        r#"
INSERT INTO threads (
    id,
    rollout_path,
    created_at,
    updated_at,
    created_at_ms,
    updated_at_ms,
    source,
    model_provider,
    cwd,
    title,
    sandbox_policy,
    approval_mode
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind("00000000-0000-0000-0000-000000000002")
    .bind("/tmp/second.jsonl")
    .bind(1_700_000_200_i64)
    .bind(1_700_000_300_i64)
    .bind(1_700_000_200_123_i64)
    .bind(1_700_000_300_456_i64)
    .bind("cli")
    .bind("openai")
    .bind("/tmp")
    .bind("")
    .bind("read-only")
    .bind("on-request")
    .execute(&pool)
    .await
    .expect("old-binary row should insert");

    let seeded = sqlx::query("SELECT recency_at, recency_at_ms FROM threads WHERE id = ?")
        .bind("00000000-0000-0000-0000-000000000002")
        .fetch_one(&pool)
        .await
        .expect("old-binary row should load");
    assert_eq!(seeded.get::<i64, _>("recency_at"), 1_700_000_300);
    assert_eq!(seeded.get::<i64, _>("recency_at_ms"), 1_700_000_300_456);
}

#[tokio::test]
async fn repairs_recency_migration_that_was_applied_as_version_38() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("in-memory database should open");
    migrator_through(/*version*/ 37)
        .run(&pool)
        .await
        .expect("pre-recency migrations should apply");

    let recency_migration = STATE_MIGRATOR
        .migrations
        .iter()
        .find(|migration| migration.version == CURRENT_RECENCY_MIGRATION_VERSION)
        .expect("recency migration should exist");
    let mut legacy_migrations = STATE_MIGRATOR
        .migrations
        .iter()
        .filter(|migration| migration.version <= 37)
        .cloned()
        .collect::<Vec<_>>();
    legacy_migrations.push(Migration::new(
        LEGACY_RECENCY_MIGRATION_VERSION,
        recency_migration.description.clone(),
        recency_migration.migration_type,
        recency_migration.sql.clone(),
        recency_migration.no_tx,
    ));
    let legacy_recency_migrator = Migrator::with_migrations(legacy_migrations);
    legacy_recency_migrator
        .run(&pool)
        .await
        .expect("legacy recency migration should apply as version 38");

    repair_state_migration_version_collisions(&pool, &STATE_MIGRATOR)
        .await
        .expect("legacy migration history should be repaired");
    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("current migrations should apply after repair");

    let applied = sqlx::query(
        "SELECT version, checksum FROM _sqlx_migrations WHERE version >= 38 ORDER BY version",
    )
    .fetch_all(&pool)
    .await
    .expect("applied migrations should load")
    .into_iter()
    .map(|row| {
        (
            row.get::<i64, _>("version"),
            row.get::<Vec<u8>, _>("checksum"),
        )
    })
    .collect::<Vec<_>>();
    let expected = STATE_MIGRATOR
        .migrations
        .iter()
        .filter(|migration| migration.version >= 38)
        .map(|migration| (migration.version, migration.checksum.to_vec()))
        .collect::<Vec<_>>();
    assert_eq!(applied, expected);
}

#[tokio::test]
async fn repairs_visible_sort_indexes_migration_that_was_applied_as_version_40() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("in-memory database should open");
    migrator_through(/*version*/ 39)
        .run(&pool)
        .await
        .expect("pre-visible-sort migrations should apply");

    let visible_sort_migration = STATE_MIGRATOR
        .migrations
        .iter()
        .find(|migration| migration.version == CURRENT_VISIBLE_SORT_INDEXES_MIGRATION_VERSION)
        .expect("visible sort migration should exist");
    let mut legacy_migrations = STATE_MIGRATOR
        .migrations
        .iter()
        .filter(|migration| migration.version <= 39)
        .cloned()
        .collect::<Vec<_>>();
    legacy_migrations.push(Migration::new(
        LEGACY_VISIBLE_SORT_INDEXES_MIGRATION_VERSION,
        visible_sort_migration.description.clone(),
        visible_sort_migration.migration_type,
        visible_sort_migration.sql.clone(),
        visible_sort_migration.no_tx,
    ));
    let legacy_visible_sort_migrator = Migrator::with_migrations(legacy_migrations);
    legacy_visible_sort_migrator
        .run(&pool)
        .await
        .expect("legacy visible sort migration should apply as version 40");

    repair_state_migration_version_collisions(&pool, &STATE_MIGRATOR)
        .await
        .expect("legacy visible sort migration history should be repaired");
    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("current migrations should apply after visible sort repair");

    let applied = sqlx::query(
        "SELECT version, checksum FROM _sqlx_migrations WHERE version >= 40 ORDER BY version",
    )
    .fetch_all(&pool)
    .await
    .expect("applied migrations should load")
    .into_iter()
    .map(|row| {
        (
            row.get::<i64, _>("version"),
            row.get::<Vec<u8>, _>("checksum"),
        )
    })
    .collect::<Vec<_>>();
    let expected = STATE_MIGRATOR
        .migrations
        .iter()
        .filter(|migration| migration.version >= 40)
        .map(|migration| (migration.version, migration.checksum.to_vec()))
        .collect::<Vec<_>>();
    assert_eq!(applied, expected);
}

#[tokio::test]
async fn inference_identity_authority_migration_preserves_0044_legacy_row() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("in-memory database should open");
    migrator_through(/*version*/ 44)
        .run(&pool)
        .await
        .expect("0044 schema should apply");
    sqlx::query(
        r#"
INSERT INTO threads (
    id, rollout_path, created_at, updated_at, source, model_provider,
    model, reasoning_effort, cwd, title, sandbox_policy, approval_mode
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind("00000000-0000-0000-0000-000000000045")
    .bind("/tmp/legacy-0044.jsonl")
    .bind(1_700_000_000_i64)
    .bind(1_700_000_100_i64)
    .bind("cli")
    .bind("legacy-provider")
    .bind("legacy-model")
    .bind("high")
    .bind("/tmp")
    .bind("Legacy row")
    .bind("read-only")
    .bind("on-request")
    .execute(&pool)
    .await
    .expect("0044 legacy row should insert");

    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("identity authority migration should apply");
    let row = sqlx::query(
        r#"
SELECT model_provider, model, reasoning_effort,
       configured_inference_identity_authority,
       latest_request_inference_identity_authority
FROM threads WHERE id = ?
        "#,
    )
    .bind("00000000-0000-0000-0000-000000000045")
    .fetch_one(&pool)
    .await
    .expect("upgraded legacy row should load");
    assert_eq!(
        (
            row.get::<String, _>("model_provider"),
            row.get::<Option<String>, _>("model"),
            row.get::<Option<String>, _>("reasoning_effort"),
            row.get::<Option<String>, _>("configured_inference_identity_authority"),
            row.get::<Option<String>, _>("latest_request_inference_identity_authority"),
        ),
        (
            "legacy-provider".to_string(),
            Some("legacy-model".to_string()),
            Some("high".to_string()),
            None,
            None,
        )
    );
}
