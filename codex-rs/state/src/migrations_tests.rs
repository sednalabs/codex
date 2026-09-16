use std::borrow::Cow;
use std::collections::BTreeSet;

use codex_protocol::ThreadId;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use sqlx::Connection;
use sqlx::Row;
use sqlx::migrate::Migration;
use sqlx::migrate::Migrator;

use super::GOALS_MIGRATOR;
use super::STATE_MIGRATOR;
use super::THREAD_HISTORY_MIGRATOR;
use super::USAGE_MIGRATOR;
use super::repair_state_migration_version_collisions;

const PRE_RECENCY_MIGRATION_VERSION: i64 = 38;
const LEGACY_RECENCY_MIGRATION_VERSION: i64 = 38;
const CURRENT_RECENCY_MIGRATION_VERSION: i64 = 39;
const LEGACY_VISIBLE_SORT_INDEXES_MIGRATION_VERSION: i64 = 40;
const CURRENT_VISIBLE_SORT_INDEXES_MIGRATION_VERSION: i64 = 36;
const LEGACY_REMOTE_CONTROL_ENABLED_MIGRATION_VERSION: i64 = 41;
const CURRENT_REMOTE_CONTROL_ENABLED_MIGRATION_VERSION: i64 = 37;
const PRE_CONFIGURED_IDENTITY_PROVENANCE_MIGRATION_VERSION: i64 = 9004;
const LEGACY_EXTERNAL_AGENT_CONFIG_IMPORTS_MIGRATION_VERSION: i64 = 42;
const CURRENT_EXTERNAL_AGENT_CONFIG_IMPORTS_MIGRATION_VERSION: i64 = 38;
const CURRENT_PINNED_THREADS_MIGRATION_VERSION: i64 = 43;
const LEGACY_EXTERNAL_AGENT_CONFIG_IMPORTS_PROVIDER_ID_MIGRATION_VERSION: i64 = 44;
const CURRENT_EXTERNAL_AGENT_CONFIG_IMPORTS_PROVIDER_ID_MIGRATION_VERSION: i64 = 44;
const LEGACY_GOAL_ID: &str = "legacy-goal";
const LEGACY_GOAL_STATUS: &str = "active";
const LEGACY_GOAL_TOKEN_BUDGET: Option<i64> = Some(123);
const LEGACY_GOAL_TOKENS_USED: i64 = 17;
const LEGACY_GOAL_TIME_USED_SECONDS: i64 = 19;
const LEGACY_GOAL_CREATED_AT_MS: i64 = 1_700_000_000_000;
const LEGACY_GOAL_UPDATED_AT_MS: i64 = 1_700_000_001_000;
use crate::PINNED_THREAD_SECTION_ID;
use crate::PINNED_THREAD_SECTION_NAME;

const CUSTOM_THREAD_SECTION_ID: &str = "01984de2-8f74-7c91-a3b2-5c5e937cf317";

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

/// Maps each historical downstream migration file to the canonical embedded
/// migration containing its exact SQL. Versions 40 through 42 are already
/// upstream-identical, so they deliberately keep their own canonical number.
const DOWNSTREAM_STATE_MIGRATION_TARGETS: &[(i64, i64)] = &[
    (24, 9001),
    (25, 24),
    (26, 25),
    (27, 9002),
    (28, 9003),
    (29, 26),
    (30, 27),
    (31, 28),
    (32, 29),
    (33, 30),
    (34, 31),
    (35, 32),
    (36, 33),
    (37, 34),
    (38, 9004),
    (39, 35),
    (40, 40),
    (41, 41),
    (42, 42),
    (43, 39),
    (44, 36),
    (45, 9005),
    (46, 37),
    (47, 38),
    (48, 43),
    (49, 44),
    (50, 9006),
];

fn downstream_migrator_through(version: i64) -> Migrator {
    let mut migrations = STATE_MIGRATOR
        .iter()
        .filter(|migration| migration.version <= 23)
        .cloned()
        .collect::<Vec<_>>();
    for &(legacy_version, canonical_version) in DOWNSTREAM_STATE_MIGRATION_TARGETS {
        if legacy_version > version {
            break;
        }
        let migration = STATE_MIGRATOR
            .iter()
            .find(|migration| migration.version == canonical_version)
            .expect("canonical migration should exist");
        migrations.push(Migration::new(
            legacy_version,
            migration.description.clone(),
            migration.migration_type,
            migration.sql.clone(),
            migration.no_tx,
        ));
    }
    Migrator::with_migrations(migrations)
}

fn downstream_migrator_through_goal_status() -> Migrator {
    downstream_migrator_through(36)
}

fn origin_main_migrator() -> Migrator {
    let external_imports_migration = STATE_MIGRATOR
        .migrations
        .iter()
        .find(|migration| {
            migration.version == CURRENT_EXTERNAL_AGENT_CONFIG_IMPORTS_MIGRATION_VERSION
        })
        .expect("external agent config imports migration should exist");
    let mut migrations = STATE_MIGRATOR
        .migrations
        .iter()
        .filter(|migration| migration.version <= 35)
        .cloned()
        .collect::<Vec<_>>();
    migrations.push(Migration::new(
        LEGACY_EXTERNAL_AGENT_CONFIG_IMPORTS_MIGRATION_VERSION,
        external_imports_migration.description.clone(),
        external_imports_migration.migration_type,
        external_imports_migration.sql.clone(),
        external_imports_migration.no_tx,
    ));
    Migrator::with_migrations(migrations)
}

fn usage_migrator_through(version: i64) -> Migrator {
    Migrator {
        migrations: Cow::Owned(
            USAGE_MIGRATOR
                .migrations
                .iter()
                .filter(|migration| migration.version <= version)
                .cloned()
                .collect(),
        ),
        ignore_missing: USAGE_MIGRATOR.ignore_missing,
        locking: USAGE_MIGRATOR.locking,
        table_name: USAGE_MIGRATOR.table_name.clone(),
        create_schemas: USAGE_MIGRATOR.create_schemas.clone(),
        no_tx: USAGE_MIGRATOR.no_tx,
    }
}

fn upstream_external_agent_import_provider_migrator() -> Migrator {
    let external_imports_migration = STATE_MIGRATOR
        .migrations
        .iter()
        .find(|migration| {
            migration.version == CURRENT_EXTERNAL_AGENT_CONFIG_IMPORTS_MIGRATION_VERSION
        })
        .expect("external agent config imports migration should exist");
    let provider_id_migration = STATE_MIGRATOR
        .migrations
        .iter()
        .find(|migration| {
            migration.version == CURRENT_EXTERNAL_AGENT_CONFIG_IMPORTS_PROVIDER_ID_MIGRATION_VERSION
        })
        .expect("external agent config imports provider-id migration should exist");
    let mut migrations = STATE_MIGRATOR
        .migrations
        .iter()
        .filter(|migration| migration.version <= 43)
        .cloned()
        .collect::<Vec<_>>();
    migrations.push(Migration::new(
        LEGACY_EXTERNAL_AGENT_CONFIG_IMPORTS_MIGRATION_VERSION,
        external_imports_migration.description.clone(),
        external_imports_migration.migration_type,
        external_imports_migration.sql.clone(),
        external_imports_migration.no_tx,
    ));
    migrations.push(Migration::new(
        LEGACY_EXTERNAL_AGENT_CONFIG_IMPORTS_PROVIDER_ID_MIGRATION_VERSION,
        provider_id_migration.description.clone(),
        provider_id_migration.migration_type,
        provider_id_migration.sql.clone(),
        provider_id_migration.no_tx,
    ));
    Migrator::with_migrations(migrations)
}

async fn insert_old_binary_thread(pool: &sqlx::SqlitePool, id: &str, rollout_path: &str) {
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
    .bind(id)
    .bind(rollout_path)
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
    .execute(pool)
    .await
    .expect("old-binary-shaped thread should insert");
}

async fn insert_legacy_goal(pool: &sqlx::SqlitePool, thread_id: &str, objective: &str) {
    sqlx::query(
        r#"
INSERT INTO thread_goals (
    thread_id,
    goal_id,
    objective,
    status,
    token_budget,
    tokens_used,
    time_used_seconds,
    created_at_ms,
    updated_at_ms
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(thread_id)
    .bind(LEGACY_GOAL_ID)
    .bind(objective)
    .bind(LEGACY_GOAL_STATUS)
    .bind(LEGACY_GOAL_TOKEN_BUDGET)
    .bind(LEGACY_GOAL_TOKENS_USED)
    .bind(LEGACY_GOAL_TIME_USED_SECONDS)
    .bind(LEGACY_GOAL_CREATED_AT_MS)
    .bind(LEGACY_GOAL_UPDATED_AT_MS)
    .execute(pool)
    .await
    .expect("legacy goal should insert");
}

async fn assert_transferred_legacy_goal_row(
    sqlite: &crate::SqliteConfig,
    thread_id: &str,
    objective: &str,
) {
    let goals_pool = sqlite
        .open_read_write_pool(&sqlite.goals_db_path())
        .await
        .expect("goals database should open for transferred-row readback");
    let destination = sqlx::query(
        r#"
SELECT
    thread_id,
    goal_id,
    objective,
    status,
    token_budget,
    tokens_used,
    time_used_seconds,
    created_at_ms,
    updated_at_ms
FROM thread_goals
WHERE thread_id = ?
        "#,
    )
    .bind(thread_id)
    .fetch_one(&goals_pool)
    .await
    .expect("transferred goal row should exist");
    let destination = (
        destination.get::<String, _>("thread_id"),
        destination.get::<String, _>("goal_id"),
        destination.get::<String, _>("objective"),
        destination.get::<String, _>("status"),
        destination.get::<Option<i64>, _>("token_budget"),
        destination.get::<i64, _>("tokens_used"),
        destination.get::<i64, _>("time_used_seconds"),
        destination.get::<i64, _>("created_at_ms"),
        destination.get::<i64, _>("updated_at_ms"),
    );
    let expected = (
        thread_id.to_owned(),
        LEGACY_GOAL_ID.to_owned(),
        objective.to_owned(),
        LEGACY_GOAL_STATUS.to_owned(),
        LEGACY_GOAL_TOKEN_BUDGET,
        LEGACY_GOAL_TOKENS_USED,
        LEGACY_GOAL_TIME_USED_SECONDS,
        LEGACY_GOAL_CREATED_AT_MS,
        LEGACY_GOAL_UPDATED_AT_MS,
    );
    assert_eq!(
        destination, expected,
        "all legacy goal fields must transfer exactly"
    );
    goals_pool.close().await;
}

async fn assert_canonical_state_ledger(pool: &sqlx::SqlitePool) {
    let applied = sqlx::query("SELECT version, checksum FROM _sqlx_migrations ORDER BY version")
        .fetch_all(pool)
        .await
        .expect("state migration ledger should load")
        .into_iter()
        .map(|row| {
            (
                row.get::<i64, _>("version"),
                row.get::<Vec<u8>, _>("checksum"),
            )
        })
        .collect::<Vec<_>>();
    let expected = STATE_MIGRATOR
        .iter()
        .map(|migration| (migration.version, migration.checksum.to_vec()))
        .collect::<Vec<_>>();
    assert_eq!(applied, expected);
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
async fn configured_identity_provenance_migration_defaults_existing_and_old_binary_rows_to_unknown()
{
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
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
    migrator_through(PRE_CONFIGURED_IDENTITY_PROVENANCE_MIGRATION_VERSION)
        .run(&pool)
        .await
        .expect("pre-provenance migrations should apply");
    insert_old_binary_thread(
        &pool,
        "00000000-0000-0000-0000-000000000011",
        "/tmp/pre-v45.jsonl",
    )
    .await;

    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("configured-identity provenance migration should apply");
    let migrated_provenance: i64 =
        sqlx::query_scalar("SELECT configured_identity_provenance FROM threads WHERE id = ?")
            .bind("00000000-0000-0000-0000-000000000011")
            .fetch_one(&pool)
            .await
            .expect("migrated provenance should load");

    insert_old_binary_thread(
        &pool,
        "00000000-0000-0000-0000-000000000012",
        "/tmp/post-v45.jsonl",
    )
    .await;
    let post_v45_old_binary_provenance: i64 =
        sqlx::query_scalar("SELECT configured_identity_provenance FROM threads WHERE id = ?")
            .bind("00000000-0000-0000-0000-000000000012")
            .fetch_one(&pool)
            .await
            .expect("old-binary provenance default should load");

    assert_eq!(
        (migrated_provenance, post_v45_old_binary_provenance),
        (0, 0)
    );
    let invalid_update =
        sqlx::query("UPDATE threads SET configured_identity_provenance = 3 WHERE id = ?")
            .bind("00000000-0000-0000-0000-000000000011")
            .execute(&pool)
            .await;
    assert!(
        invalid_update.is_err(),
        "invalid provenance must be rejected"
    );

    pool.close().await;
}

#[tokio::test]
async fn thread_section_migration_preserves_legacy_pin_compatibility() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
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
    migrator_through(CURRENT_EXTERNAL_AGENT_CONFIG_IMPORTS_MIGRATION_VERSION)
        .run(&pool)
        .await
        .expect("released thread migrations should apply");

    for thread_id in [
        "00000000-0000-0000-0000-000000000043",
        "00000000-0000-0000-0000-000000000044",
    ] {
        if thread_id.ends_with("44") {
            sqlx::query("UPDATE threads SET is_pinned = 1 WHERE id = ?")
                .bind("00000000-0000-0000-0000-000000000043")
                .execute(&pool)
                .await
                .expect("legacy pin should remain writable before section migration");
            STATE_MIGRATOR
                .run(&pool)
                .await
                .expect("section migration should apply");
        }
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
        .bind(thread_id)
        .bind("/tmp/legacy.jsonl")
        .bind(1_700_000_000_i64)
        .bind(1_700_000_000_i64)
        .bind(1_700_000_000_000_i64)
        .bind(1_700_000_000_000_i64)
        .bind("cli")
        .bind("openai")
        .bind("/tmp")
        .bind("")
        .bind("read-only")
        .bind("on-request")
        .execute(&pool)
        .await
        .expect("legacy thread insert should succeed");
    }

    let registered_sections = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT id, name, appearance FROM thread_sections ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .expect("independent thread sections should load");
    assert_eq!(
        registered_sections,
        vec![(
            PINNED_THREAD_SECTION_ID.to_string(),
            PINNED_THREAD_SECTION_NAME.to_string(),
            None,
        )]
    );

    let threads = sqlx::query_as::<_, (i64, Option<String>)>(
        "SELECT is_pinned, thread_section_id FROM threads ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .expect("legacy and section-aware thread metadata should load");
    assert_eq!(threads, vec![(1, None), (0, None)]);

    sqlx::query("INSERT INTO thread_sections (id, name) VALUES (?, ?)")
        .bind(CUSTOM_THREAD_SECTION_ID)
        .bind("Custom section")
        .execute(&pool)
        .await
        .expect("custom sections should have independent persisted identities");

    let thread_id = "00000000-0000-0000-0000-000000000043";
    sqlx::query("UPDATE threads SET thread_section_id = ? WHERE id = ?")
        .bind(CUSTOM_THREAD_SECTION_ID)
        .bind(thread_id)
        .execute(&pool)
        .await
        .expect("threads should reference independently persisted sections");
    sqlx::query("UPDATE threads SET is_pinned = 0 WHERE id = ?")
        .bind(thread_id)
        .execute(&pool)
        .await
        .expect("released binaries should still update the legacy pin column");
    let thread = sqlx::query_as::<_, (i64, Option<String>)>(
        "SELECT is_pinned, thread_section_id FROM threads WHERE id = ?",
    )
    .bind(thread_id)
    .fetch_one(&pool)
    .await
    .expect("legacy pin updates should not overwrite the authoritative section");
    assert_eq!(thread, (0, Some(CUSTOM_THREAD_SECTION_ID.to_string())));

    sqlx::query("UPDATE threads SET thread_section_id = NULL WHERE id = ?")
        .bind(thread_id)
        .execute(&pool)
        .await
        .expect("threads should be removable from sections");

    let registered_sections =
        sqlx::query_as::<_, (String, String)>("SELECT id, name FROM thread_sections ORDER BY id")
            .fetch_all(&pool)
            .await
            .expect("empty sections should remain independently discoverable");
    assert_eq!(
        registered_sections,
        vec![
            (
                CUSTOM_THREAD_SECTION_ID.to_string(),
                "Custom section".to_string(),
            ),
            (
                PINNED_THREAD_SECTION_ID.to_string(),
                PINNED_THREAD_SECTION_NAME.to_string(),
            ),
        ]
    );

    let mut released_pin_migrator = migrator_through(/*version*/ 44);
    released_pin_migrator.ignore_missing = true;
    released_pin_migrator
        .run(&pool)
        .await
        .expect("released pin-capable binaries should tolerate newer migrations");

    pool.close().await;
}

#[tokio::test]
async fn thread_attachment_migration_preserves_existing_data() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
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
    migrator_through(/*version*/ 51)
        .run(&pool)
        .await
        .expect("released thread migrations should apply");
    sqlx::query("UPDATE thread_sections SET appearance = ? WHERE id = ?")
        .bind(r#"{"icon":"pin"}"#)
        .bind(PINNED_THREAD_SECTION_ID)
        .execute(&pool)
        .await
        .expect("released section appearance should remain writable");

    let thread_id = "00000000-0000-0000-0000-000000000051";
    sqlx::query(
        "INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode) VALUES (?, 'rollout.jsonl', 1, 1, 'cli', 'openai', '/tmp', '', 'read-only', 'on-request')",
    )
    .bind(thread_id)
    .execute(&pool)
    .await
    .expect("existing thread should be inserted");
    sqlx::query(
        "INSERT INTO thread_artifacts (id, thread_id, artifact_type, identity_key, payload, created_at) VALUES ('attachment-1', ?, 'pull_request', 'pr-123', '{}', 1)",
    )
    .bind(thread_id)
    .execute(&pool)
    .await
    .expect("existing attachment should be inserted using the released schema");

    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("attachment migration should apply without rewriting released migrations");
    let section = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT id, name, appearance FROM thread_sections WHERE id = ?",
    )
    .bind(PINNED_THREAD_SECTION_ID)
    .fetch_one(&pool)
    .await
    .expect("existing section metadata should remain available");
    assert_eq!(
        section,
        (
            PINNED_THREAD_SECTION_ID.to_string(),
            PINNED_THREAD_SECTION_NAME.to_string(),
            Some(r#"{"icon":"pin"}"#.to_string()),
        )
    );

    let attachment = sqlx::query_as::<_, (String, String, String, String, String, i64)>(
        "SELECT id, thread_id, attachment_type, identity_key, payload, created_at FROM thread_attachments",
    )
    .fetch_one(&pool)
    .await
    .expect("existing attachment should remain available under the renamed table and column");
    assert_eq!(
        attachment,
        (
            "attachment-1".to_string(),
            thread_id.to_string(),
            "pull_request".to_string(),
            "pr-123".to_string(),
            "{}".to_string(),
            1,
        )
    );

    let mut released_migrator = migrator_through(/*version*/ 50);
    released_migrator.ignore_missing = true;
    released_migrator
        .run(&pool)
        .await
        .expect("released binaries should tolerate the attachment rename migration");
}

#[tokio::test]
async fn thread_section_order_migration_backfills_stably() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
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
    migrator_through(/*version*/ 45)
        .run(&pool)
        .await
        .expect("pre-ordering migrations should apply");

    sqlx::query("INSERT INTO thread_sections (id, name) VALUES (?, ?)")
        .bind(CUSTOM_THREAD_SECTION_ID)
        .bind("Custom section")
        .execute(&pool)
        .await
        .expect("custom section should exist before threads reference it");

    let older = "00000000-0000-0000-0000-000000000071";
    let newer = "00000000-0000-0000-0000-000000000072";
    let pinned = "00000000-0000-0000-0000-000000000073";
    let unsectioned = "00000000-0000-0000-0000-000000000074";
    for (thread_id, recency_at_ms, section) in [
        (older, 1_700_000_001_000_i64, Some(CUSTOM_THREAD_SECTION_ID)),
        (newer, 1_700_000_002_000, Some(CUSTOM_THREAD_SECTION_ID)),
        (pinned, 1_700_000_003_000, Some(PINNED_THREAD_SECTION_ID)),
        (unsectioned, 1_700_000_004_000, None),
    ] {
        sqlx::query(
            r#"
INSERT INTO threads (
    id, rollout_path, created_at, updated_at, recency_at,
    created_at_ms, updated_at_ms, recency_at_ms, source,
    model_provider, cwd, title, preview, sandbox_policy, approval_mode, thread_section_id
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(thread_id)
        .bind("/tmp/legacy.jsonl")
        .bind(recency_at_ms / 1000)
        .bind(recency_at_ms / 1000)
        .bind(recency_at_ms / 1000)
        .bind(recency_at_ms)
        .bind(recency_at_ms)
        .bind(recency_at_ms)
        .bind("cli")
        .bind("openai")
        .bind("/tmp")
        .bind("")
        .bind("preview")
        .bind("read-only")
        .bind("on-request")
        .bind(section)
        .execute(&pool)
        .await
        .expect("legacy section row should insert");
    }

    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("section ordering migration should apply");
    let custom_order = sqlx::query_scalar::<_, String>(
        "SELECT id FROM threads WHERE thread_section_id = ? ORDER BY section_position, id",
    )
    .bind(CUSTOM_THREAD_SECTION_ID)
    .fetch_all(&pool)
    .await
    .expect("backfilled custom order should load");
    assert_eq!(custom_order, vec![newer.to_string(), older.to_string()]);
    let positions =
        sqlx::query_scalar::<_, Option<i64>>("SELECT section_position FROM threads ORDER BY id")
            .fetch_all(&pool)
            .await
            .expect("section positions should load");
    assert_eq!(
        positions,
        vec![Some(2_000_000), Some(1_000_000), Some(1_000_000), None]
    );
    let entered = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT section_entered_at_ms FROM threads ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .expect("section entry timestamps should load");
    assert_eq!(
        entered,
        vec![
            Some(1_700_000_001_000),
            Some(1_700_000_002_000),
            Some(1_700_000_003_000),
            None,
        ]
    );

    let section_position_index = sqlx::query_scalar::<_, String>(
        "SELECT name FROM sqlite_master WHERE type = 'index' AND name = ?",
    )
    .bind("idx_threads_section_position")
    .fetch_optional(&pool)
    .await
    .expect("section position index should remain inspectable");
    assert_eq!(
        section_position_index,
        Some("idx_threads_section_position".to_string())
    );

    let applied_versions = sqlx::query_scalar::<_, i64>(
        "SELECT version FROM _sqlx_migrations WHERE version IN (?, ?) ORDER BY version",
    )
    .bind(CURRENT_RECENCY_MIGRATION_VERSION)
    .bind(CURRENT_PINNED_THREADS_MIGRATION_VERSION)
    .fetch_all(&pool)
    .await
    .expect("recency and pin migrations should be recorded");
    assert_eq!(
        applied_versions,
        vec![
            CURRENT_RECENCY_MIGRATION_VERSION,
            CURRENT_PINNED_THREADS_MIGRATION_VERSION,
        ]
    );

    let pinned_index_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_threads_pinned_recency_at_ms'",
    )
    .fetch_one(&pool)
    .await
    .expect("pinned recency index should load");
    assert_eq!(pinned_index_count, 1);

    pool.close().await;
}

#[tokio::test]
async fn thread_item_update_ordinals_allow_older_writers() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let pre_update_ordinal_migrator = Migrator {
        migrations: Cow::Owned(
            THREAD_HISTORY_MIGRATOR
                .migrations
                .iter()
                .filter(|migration| migration.version < 4)
                .cloned()
                .collect(),
        ),
        ignore_missing: THREAD_HISTORY_MIGRATOR.ignore_missing,
        locking: THREAD_HISTORY_MIGRATOR.locking,
        table_name: THREAD_HISTORY_MIGRATOR.table_name.clone(),
        create_schemas: THREAD_HISTORY_MIGRATOR.create_schemas.clone(),
        no_tx: THREAD_HISTORY_MIGRATOR.no_tx,
    };
    let pool = sqlite
        .open_thread_history_db(
            &pre_update_ordinal_migrator,
            /*telemetry_override*/ None,
        )
        .await
        .expect("pre-update-ordinal migrations should apply");
    sqlx::query(
        r#"
INSERT INTO thread_items (thread_id, turn_id, item_id, rollout_ordinal, created_at_ms, item_type, item_json) VALUES
    ('thread-1', 'turn-1', 'existing-item-1', 11, 1_100, 'userMessage', '{}'),
    ('thread-1', 'turn-1', 'existing-item-2', 12, 1_200, 'userMessage', '{}')
        "#,
    )
    .execute(&pool)
    .await
    .expect("pre-migration items should be inserted");
    THREAD_HISTORY_MIGRATOR
        .run(&pool)
        .await
        .expect("update-ordinal migration should apply");
    sqlx::query(
        r#"
INSERT INTO thread_items (thread_id, turn_id, item_id, rollout_ordinal, created_at_ms, item_type, item_json) VALUES
    ('thread-1', 'turn-1', 'old-writer-item-1', 13, 1_300, 'userMessage', '{}'),
    ('thread-1', 'turn-1', 'old-writer-item-2', 14, 1_400, 'userMessage', '{}')
        "#,
    )
    .execute(&pool)
    .await
    .expect("older writers should be able to append multiple items after migration");
    let ordinals = sqlx::query_as::<_, (i64, i64)>(
        "SELECT rollout_ordinal, updated_at_ordinal FROM thread_items WHERE thread_id = ? ORDER BY rollout_ordinal",
    )
    .bind("thread-1")
    .fetch_all(&pool)
    .await
    .expect("old-writer items should load");
    assert_eq!(ordinals, vec![(11, 11), (12, 12), (13, 0), (14, 0)]);

    pool.close().await;
}

#[tokio::test]
async fn realtime_items_preserve_older_thread_history_writers() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let older_migrator = Migrator {
        migrations: Cow::Owned(
            THREAD_HISTORY_MIGRATOR
                .migrations
                .iter()
                .filter(|migration| migration.version < 5)
                .cloned()
                .collect(),
        ),
        ignore_missing: true,
        locking: THREAD_HISTORY_MIGRATOR.locking,
        table_name: THREAD_HISTORY_MIGRATOR.table_name.clone(),
        create_schemas: THREAD_HISTORY_MIGRATOR.create_schemas.clone(),
        no_tx: THREAD_HISTORY_MIGRATOR.no_tx,
    };
    let pool = sqlite
        .open_thread_history_db(&older_migrator, /*telemetry_override*/ None)
        .await
        .expect("existing thread history migrations should apply");
    sqlx::query(
        "INSERT INTO thread_items (thread_id, turn_id, item_id, rollout_ordinal, created_at_ms, item_json) VALUES ('thread-1', 'turn-1', 'existing-item', 1, 100, '{}')",
    )
    .execute(&pool)
    .await
    .expect("existing turn-scoped item should be inserted");

    THREAD_HISTORY_MIGRATOR
        .run(&pool)
        .await
        .expect("realtime item migration should apply");
    let turn_id_not_null = sqlx::query_scalar::<_, i64>(
        "SELECT \"notnull\" FROM pragma_table_info('thread_items') WHERE name = 'turn_id'",
    )
    .fetch_one(&pool)
    .await
    .expect("existing thread item schema should remain inspectable");
    assert_eq!(turn_id_not_null, 1);

    sqlx::query(
        "INSERT INTO thread_realtime_items (thread_id, item_id, rollout_ordinal, created_at_ms, item_type, item_json) VALUES ('thread-1', 'realtime-item', 2, 200, 'realtime_session_started', '{}')",
    )
    .execute(&pool)
    .await
    .expect("thread-scoped realtime item should be inserted separately");
    sqlx::query(
        "INSERT INTO thread_history_projection_state (thread_id, next_rollout_byte_offset, next_rollout_ordinal) VALUES ('thread-1', 0, 0)",
    )
    .execute(&pool)
    .await
    .expect("thread projection checkpoint should be inserted");

    let older_pool = sqlite
        .open_thread_history_db(&older_migrator, /*telemetry_override*/ None)
        .await
        .expect("older binaries should tolerate the additive realtime migration");
    sqlx::query(
        "INSERT INTO thread_items (thread_id, turn_id, item_id, rollout_ordinal, created_at_ms, item_json) VALUES ('thread-1', 'turn-1', 'older-writer-item', 3, 300, '{}')",
    )
    .execute(&older_pool)
    .await
    .expect("older binaries should continue writing ordinary turn-scoped items");
    let ordinary_items = sqlx::query_as::<_, (String, String)>(
        "SELECT item_id, turn_id FROM thread_items WHERE thread_id = ? ORDER BY rollout_ordinal",
    )
    .bind("thread-1")
    .fetch_all(&older_pool)
    .await
    .expect("older binaries should never observe turnless realtime items");
    assert_eq!(
        ordinary_items,
        vec![
            ("existing-item".to_string(), "turn-1".to_string()),
            ("older-writer-item".to_string(), "turn-1".to_string()),
        ]
    );
    sqlx::query("DELETE FROM thread_history_projection_state WHERE thread_id = ?")
        .bind("thread-1")
        .execute(&older_pool)
        .await
        .expect("older binaries should delete their known projection checkpoint");
    let remaining_realtime_items = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM thread_realtime_items WHERE thread_id = ?",
    )
    .bind("thread-1")
    .fetch_one(&pool)
    .await
    .expect("read realtime items after an older writer deleted the thread");
    assert_eq!(remaining_realtime_items, 0);

    older_pool.close().await;
    pool.close().await;
}

#[tokio::test]
async fn agent_job_tables_are_dropped_when_upgrading() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
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
    origin_main_migrator()
        .run(&pool)
        .await
        .expect("origin main migrations should apply");

    insert_old_binary_thread(
        &pool,
        "00000000-0000-0000-0000-000000000021",
        "/tmp/parent.jsonl",
    )
    .await;
    insert_old_binary_thread(
        &pool,
        "00000000-0000-0000-0000-000000000022",
        "/tmp/child.jsonl",
    )
    .await;
    sqlx::query(
        r#"
INSERT INTO thread_spawn_edges (parent_thread_id, child_thread_id, status)
VALUES (?, ?, ?)
        "#,
    )
    .bind("00000000-0000-0000-0000-000000000021")
    .bind("00000000-0000-0000-0000-000000000022")
    .bind("completed")
    .execute(&pool)
    .await
    .expect("thread spawn edge should insert");
    sqlx::query(
        r#"
INSERT INTO external_agent_config_imports (
    import_id,
    completed_at_ms,
    successes,
    failures
) VALUES (?, ?, ?, ?)
        "#,
    )
    .bind("import-1")
    .bind(1_700_000_000_123_i64)
    .bind(r#"[{"item_type":"config"}]"#)
    .bind("[]")
    .execute(&pool)
    .await
    .expect("external agent import record should insert");

    sqlx::query(
        r#"
INSERT INTO agent_jobs (
    id,
    name,
    status,
    instruction,
    input_headers_json,
    input_csv_path,
    output_csv_path,
    created_at,
    updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind("job-1")
    .bind("legacy job")
    .bind("running")
    .bind("process rows")
    .bind(r#"["path"]"#)
    .bind("/tmp/input.csv")
    .bind("/tmp/output.csv")
    .bind(1_700_000_000_i64)
    .bind(1_700_000_000_i64)
    .execute(&pool)
    .await
    .expect("legacy agent job should insert");
    sqlx::query(
        r#"
INSERT INTO agent_job_items (
    job_id,
    item_id,
    row_index,
    row_json,
    status,
    result_json,
    created_at,
    updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind("job-1")
    .bind("item-1")
    .bind(0_i64)
    .bind(r#"{"path":"secret.csv"}"#)
    .bind("completed")
    .bind(r#"{"result":"legacy"}"#)
    .bind(1_700_000_000_i64)
    .bind(1_700_000_000_i64)
    .execute(&pool)
    .await
    .expect("legacy agent job item should insert");

    pool.close().await;
    let runtime = crate::StateRuntime::init(sqlite.clone(), "test-provider".to_string())
        .await
        .expect("state runtime should repair and migrate the origin main database");
    let pool = sqlite
        .open_read_only_pool(&sqlite.state_db_path(), /*busy_timeout*/ None)
        .await
        .expect("migrated state database should reopen read-only");

    let agent_job_tables = sqlx::query_scalar::<_, String>(
        r#"
SELECT name
FROM sqlite_master
WHERE type = 'table' AND name IN ('agent_jobs', 'agent_job_items')
ORDER BY name
        "#,
    )
    .fetch_all(&pool)
    .await
    .expect("remaining agent job tables should load");
    assert_eq!(agent_job_tables, Vec::<String>::new());

    let preserved_threads = sqlx::query_as::<_, (String, String)>(
        "SELECT id, rollout_path FROM threads WHERE id IN (?, ?) ORDER BY id",
    )
    .bind("00000000-0000-0000-0000-000000000021")
    .bind("00000000-0000-0000-0000-000000000022")
    .fetch_all(&pool)
    .await
    .expect("preserved threads should load");
    let preserved_spawn_edges = sqlx::query_as::<_, (String, String, String)>(
        r#"
SELECT parent_thread_id, child_thread_id, status
FROM thread_spawn_edges
ORDER BY child_thread_id
        "#,
    )
    .fetch_all(&pool)
    .await
    .expect("preserved spawn edges should load");
    let preserved_import = sqlx::query_as::<_, (String, i64, String, String)>(
        r#"
SELECT import_id, completed_at_ms, successes, failures
FROM external_agent_config_imports
WHERE import_id = ?
        "#,
    )
    .bind("import-1")
    .fetch_one(&pool)
    .await
    .expect("preserved external import should load");
    assert_eq!(
        (preserved_threads, preserved_spawn_edges, preserved_import),
        (
            vec![
                (
                    "00000000-0000-0000-0000-000000000021".to_string(),
                    "/tmp/parent.jsonl".to_string(),
                ),
                (
                    "00000000-0000-0000-0000-000000000022".to_string(),
                    "/tmp/child.jsonl".to_string(),
                ),
            ],
            vec![(
                "00000000-0000-0000-0000-000000000021".to_string(),
                "00000000-0000-0000-0000-000000000022".to_string(),
                "completed".to_string(),
            )],
            (
                "import-1".to_string(),
                1_700_000_000_123_i64,
                r#"[{"item_type":"config"}]"#.to_string(),
                "[]".to_string(),
            ),
        )
    );

    pool.close().await;
    runtime.close().await;
}

#[tokio::test]
async fn repairs_external_agent_config_import_migration_that_was_applied_as_version_42() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
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
    origin_main_migrator()
        .run(&pool)
        .await
        .expect("origin main migrations should apply");

    repair_state_migration_version_collisions(&pool, &STATE_MIGRATOR)
        .await
        .expect("external import migration history should be repaired");
    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("current migrations should apply after external import repair");

    let applied = sqlx::query(
        "SELECT version, checksum FROM _sqlx_migrations WHERE version >= 42 ORDER BY version",
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
        .filter(|migration| migration.version >= 42)
        .map(|migration| (migration.version, migration.checksum.to_vec()))
        .collect::<Vec<_>>();
    assert_eq!(applied, expected);

    pool.close().await;
}

#[tokio::test]
async fn external_agent_config_import_provider_migration_follows_table_creation_on_fresh_database()
{
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
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

    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("fresh state migrations should apply in dependency order");

    let provider_id_column = sqlx::query_scalar::<_, String>(
        "SELECT name FROM pragma_table_info('external_agent_config_imports') WHERE name = 'provider_id'",
    )
    .fetch_one(&pool)
    .await
    .expect("external agent import table should include provider_id after migration");
    assert_eq!(provider_id_column, "provider_id");

    pool.close().await;
}

#[tokio::test]
async fn repairs_external_agent_config_import_provider_migration_that_was_applied_as_version_44() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
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

    upstream_external_agent_import_provider_migrator()
        .run(&pool)
        .await
        .expect("legacy upstream import migrations should apply");
    sqlx::query(
        r#"
INSERT INTO external_agent_config_imports (
    import_id,
    provider_id,
    completed_at_ms,
    successes,
    failures
) VALUES (?, ?, ?, ?, ?)
        "#,
    )
    .bind("import-legacy-provider")
    .bind("claude")
    .bind(1_700_000_000_123_i64)
    .bind(r#"[{"item_type":"config"}]"#)
    .bind("[]")
    .execute(&pool)
    .await
    .expect("legacy provider record should insert");

    repair_state_migration_version_collisions(&pool, &STATE_MIGRATOR)
        .await
        .expect("legacy provider migration history should be repaired");
    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("current migrations should apply after provider migration repair");

    let provider_id = sqlx::query_scalar::<_, Option<String>>(
        "SELECT provider_id FROM external_agent_config_imports WHERE import_id = ?",
    )
    .bind("import-legacy-provider")
    .fetch_one(&pool)
    .await
    .expect("legacy provider record should load");
    assert_eq!(provider_id.as_deref(), Some("claude"));

    let visible_indexes = sqlx::query_scalar::<_, String>(
        "SELECT name FROM sqlite_master WHERE type = 'index' AND name IN ('idx_threads_visible_created_at_ms', 'idx_threads_visible_updated_at_ms') ORDER BY name",
    )
    .fetch_all(&pool)
    .await
    .expect("visible thread indexes should load");
    assert_eq!(
        visible_indexes,
        vec![
            "idx_threads_visible_created_at_ms".to_string(),
            "idx_threads_visible_updated_at_ms".to_string(),
        ]
    );

    let applied_visible_checksum =
        sqlx::query_scalar::<_, Vec<u8>>("SELECT checksum FROM _sqlx_migrations WHERE version = ?")
            .bind(CURRENT_VISIBLE_SORT_INDEXES_MIGRATION_VERSION)
            .fetch_one(&pool)
            .await
            .expect("visible sort migration should be recorded at the downstream version");
    let current_visible_checksum = STATE_MIGRATOR
        .migrations
        .iter()
        .find(|migration| migration.version == CURRENT_VISIBLE_SORT_INDEXES_MIGRATION_VERSION)
        .expect("current visible sort migration should exist")
        .checksum
        .to_vec();
    assert_eq!(applied_visible_checksum, current_visible_checksum);

    let applied_provider_checksum =
        sqlx::query_scalar::<_, Vec<u8>>("SELECT checksum FROM _sqlx_migrations WHERE version = ?")
            .bind(CURRENT_EXTERNAL_AGENT_CONFIG_IMPORTS_PROVIDER_ID_MIGRATION_VERSION)
            .fetch_one(&pool)
            .await
            .expect("provider migration should be recorded at the downstream version");
    let current_provider_checksum = STATE_MIGRATOR
        .migrations
        .iter()
        .find(|migration| {
            migration.version == CURRENT_EXTERNAL_AGENT_CONFIG_IMPORTS_PROVIDER_ID_MIGRATION_VERSION
        })
        .expect("current provider migration should exist")
        .checksum
        .to_vec();
    assert_eq!(applied_provider_checksum, current_provider_checksum);

    pool.close().await;
}

#[tokio::test]
async fn recency_migration_backfills_and_seeds_old_binary_inserts() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
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

    pool.close().await;
}

#[tokio::test]
async fn repairs_recency_migration_that_was_applied_as_version_38() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
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

    pool.close().await;
}

#[tokio::test]
async fn repairs_visible_sort_indexes_migration_that_was_applied_as_version_40() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
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
    migrator_through(/*version*/ 35)
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
        .filter(|migration| migration.version <= 35)
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

    pool.close().await;
}

#[tokio::test]
async fn repairs_remote_control_enabled_migration_that_was_applied_as_version_41() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
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
    migrator_through(/*version*/ 36)
        .run(&pool)
        .await
        .expect("pre-thread-name migrations should apply");

    let remote_control_enabled_migration = STATE_MIGRATOR
        .migrations
        .iter()
        .find(|migration| migration.version == CURRENT_REMOTE_CONTROL_ENABLED_MIGRATION_VERSION)
        .expect("remote-control-enabled migration should exist");
    let mut legacy_migrations = STATE_MIGRATOR
        .migrations
        .iter()
        .filter(|migration| migration.version <= 36)
        .cloned()
        .collect::<Vec<_>>();
    legacy_migrations.push(Migration::new(
        LEGACY_REMOTE_CONTROL_ENABLED_MIGRATION_VERSION,
        remote_control_enabled_migration.description.clone(),
        remote_control_enabled_migration.migration_type,
        remote_control_enabled_migration.sql.clone(),
        remote_control_enabled_migration.no_tx,
    ));
    let legacy_remote_control_enabled_migrator = Migrator::with_migrations(legacy_migrations);
    legacy_remote_control_enabled_migrator
        .run(&pool)
        .await
        .expect("legacy remote-control-enabled migration should apply as version 41");

    repair_state_migration_version_collisions(&pool, &STATE_MIGRATOR)
        .await
        .expect("legacy remote-control-enabled migration history should be repaired");
    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("current migrations should apply after remote-control-enabled repair");

    let applied = sqlx::query(
        "SELECT version, checksum FROM _sqlx_migrations WHERE version >= 41 ORDER BY version",
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
        .filter(|migration| migration.version >= 41)
        .map(|migration| (migration.version, migration.checksum.to_vec()))
        .collect::<Vec<_>>();
    assert_eq!(applied, expected);

    pool.close().await;
}

#[tokio::test]
async fn repair_state_migration_version_collisions_succeeds_while_writer_slot_is_held() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
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
        .expect("database should open");
    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("current migrations should apply");
    let read_pool = sqlite
        .open_read_only_pool(&state_path, /*busy_timeout*/ None)
        .await
        .expect("read-only pool should open");
    let mut write_connection = pool.acquire().await.expect("write connection should open");
    let write_transaction = write_connection
        .begin_with("BEGIN IMMEDIATE")
        .await
        .expect("write transaction should acquire the writer slot");

    let repair_result =
        repair_state_migration_version_collisions(&read_pool, &STATE_MIGRATOR).await;

    write_transaction
        .rollback()
        .await
        .expect("write transaction should roll back");
    drop(write_connection);
    read_pool.close().await;
    pool.close().await;
    repair_result.expect("current migration history should not need the writer slot");
}

#[tokio::test]
async fn state_migration_repair_rejects_an_unknown_checksum_without_mutating_history() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
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
        .expect("state database should open");
    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("current state schema should apply");
    sqlx::query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = 24")
        .bind(vec![0xA5_u8; 32])
        .execute(&pool)
        .await
        .expect("test checksum should update");

    let repair = repair_state_migration_version_collisions(&pool, &STATE_MIGRATOR).await;
    assert!(
        repair.is_err(),
        "unknown migration checksum must fail closed"
    );
    let checksum = sqlx::query_scalar::<_, Vec<u8>>(
        "SELECT checksum FROM _sqlx_migrations WHERE version = 24",
    )
    .fetch_one(&pool)
    .await
    .expect("checksum should remain readable");
    assert_eq!(checksum, vec![0xA5_u8; 32]);
    pool.close().await;
}

#[tokio::test]
async fn pre_drop_state_goals_transfer_to_goals_database_before_canonical_drop() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let thread_id = "00000000-0000-0000-0000-000000000099";
    let state_pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .expect("state database should open");
    migrator_through(33)
        .run(&state_pool)
        .await
        .expect("pre-drop upstream state history should apply");
    insert_old_binary_thread(&state_pool, thread_id, "/tmp/legacy-goal.jsonl").await;
    insert_legacy_goal(&state_pool, thread_id, "preserve this user goal").await;
    state_pool.close().await;

    let runtime = crate::runtime::StateRuntime::init(sqlite.clone(), "test-provider".to_string())
        .await
        .expect("runtime should transfer goals before dropping the state table");
    let goal = runtime
        .thread_goals()
        .get_thread_goal(ThreadId::from_string(thread_id).expect("valid thread id"))
        .await
        .expect("goal query should succeed")
        .expect("transferred goal should exist");
    assert_eq!(goal.goal_id, "legacy-goal");
    assert_eq!(goal.objective, "preserve this user goal");
    assert_eq!(goal.token_budget, Some(123));
    assert_eq!(goal.tokens_used, 17);
    assert_eq!(goal.time_used_seconds, 19);
    runtime.close().await;

    let state_pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .expect("state database should reopen");
    let source_exists = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'thread_goals'",
    )
    .fetch_optional(&state_pool)
    .await
    .expect("state schema should query")
    .is_some();
    assert!(
        !source_exists,
        "canonical state migration should drop the source only after transfer"
    );
    state_pool.close().await;
}

#[tokio::test]
async fn downstream_pre_drop_state_goals_transfer_from_d32_through_d36() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let thread_id = "00000000-0000-0000-0000-000000000098";
    let state_pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .expect("state database should open");
    downstream_migrator_through_goal_status()
        .run(&state_pool)
        .await
        .expect("D32 through D36 state history should apply");
    insert_old_binary_thread(&state_pool, thread_id, "/tmp/downstream-goal.jsonl").await;
    insert_legacy_goal(&state_pool, thread_id, "preserve downstream user goal").await;
    state_pool.close().await;

    let runtime = crate::runtime::StateRuntime::init(sqlite, "test-provider".to_string())
        .await
        .expect("runtime should remap D history and transfer its goal rows");
    let goal = runtime
        .thread_goals()
        .get_thread_goal(ThreadId::from_string(thread_id).expect("valid thread id"))
        .await
        .expect("goal query should succeed")
        .expect("transferred goal should exist");
    assert_eq!(goal.objective, "preserve downstream user goal");
    assert_eq!(goal.tokens_used, 17);
    assert_eq!(goal.time_used_seconds, 19);
    runtime.close().await;
}

#[tokio::test]
async fn every_pre_drop_upstream_and_downstream_cutline_transfers_legacy_goals() {
    for (family, cutlines) in [
        ("U", &[29, 30, 31, 32, 33][..]),
        ("D", &[32, 33, 34, 35, 36][..]),
    ] {
        for &cutline in cutlines {
            let sqlite_home = crate::runtime::test_support::unique_temp_dir();
            tokio::fs::create_dir_all(&sqlite_home)
                .await
                .expect("sqlite home should be created");
            let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
                let _ = std::fs::remove_dir_all(sqlite_home);
            });
            let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
            let thread_id = format!("00000000-0000-0000-0000-{cutline:012}");
            let state_pool = sqlite
                .open_read_write_pool(&sqlite.state_db_path())
                .await
                .expect("state database should open");
            let historical_migrator = match family {
                "U" => migrator_through(cutline),
                "D" => downstream_migrator_through(cutline),
                _ => unreachable!("the fixture families are fixed"),
            };
            historical_migrator
                .run(&state_pool)
                .await
                .unwrap_or_else(|error| {
                    panic!("{family}{cutline} state history should apply: {error}")
                });
            insert_old_binary_thread(&state_pool, &thread_id, "/tmp/cutline-goal.jsonl").await;
            let objective = format!("preserve {family}{cutline} goal");
            insert_legacy_goal(&state_pool, &thread_id, &objective).await;
            state_pool.close().await;

            let runtime = crate::runtime::StateRuntime::init(
                sqlite.clone(),
                "test-provider".to_string(),
            )
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "{family}{cutline} startup should bridge goals before the state drop: {error}"
                )
            });
            assert_transferred_legacy_goal_row(&sqlite, &thread_id, &objective).await;
            let goal = runtime
                .thread_goals()
                .get_thread_goal(ThreadId::from_string(&thread_id).expect("valid thread id"))
                .await
                .expect("goal query should succeed")
                .expect("transferred goal should exist");
            assert_eq!(goal.objective, objective, "{family}{cutline}");
            assert_eq!(goal.goal_id, "legacy-goal", "{family}{cutline}");
            runtime.close().await;
        }
    }
}

#[tokio::test]
async fn full_downstream_history_converges_to_the_canonical_state_ledger() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let state_pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .expect("state database should open");
    downstream_migrator_through(50)
        .run(&state_pool)
        .await
        .expect("full downstream state history should apply");
    state_pool.close().await;

    let runtime = crate::runtime::StateRuntime::init(sqlite.clone(), "test-provider".to_string())
        .await
        .expect("runtime should converge the full downstream history");
    runtime.close().await;

    let state_pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .expect("state database should reopen");
    assert_canonical_state_ledger(&state_pool).await;
    state_pool.close().await;
}

#[tokio::test]
async fn full_upstream_history_converges_to_the_canonical_state_ledger() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let state_pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .expect("state database should open");
    migrator_through(55)
        .run(&state_pool)
        .await
        .expect("full upstream state history should apply");
    state_pool.close().await;

    let runtime = crate::runtime::StateRuntime::init(sqlite.clone(), "test-provider".to_string())
        .await
        .expect("runtime should add only downstream high migrations to upstream history");
    runtime.close().await;

    let state_pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .expect("state database should reopen");
    assert_canonical_state_ledger(&state_pool).await;
    state_pool.close().await;
}

#[tokio::test]
async fn destination_collision_preflight_does_not_mutate_the_mixed_history_ledger() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
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
        .expect("state database should open");
    downstream_migrator_through(34)
        .run(&pool)
        .await
        .expect("real downstream pre-drop state history should apply");

    // This is intentionally a corrupt mixed ledger, not an old-binary fixture:
    // it combines two real embedded checksums so two records target canonical
    // U31. The repair must reject it before its transactional remap starts.
    let u31_checksum = STATE_MIGRATOR
        .iter()
        .find(|migration| migration.version == 31)
        .expect("canonical U31 migration should exist")
        .checksum
        .to_vec();
    sqlx::query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = 31")
        .bind(u31_checksum)
        .execute(&pool)
        .await
        .expect("mixed-ledger collision should be injected");
    let before =
        sqlx::query("SELECT version, description, checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .expect("mixed ledger should load")
            .into_iter()
            .map(|row| {
                (
                    row.get::<i64, _>("version"),
                    row.get::<String, _>("description"),
                    row.get::<Vec<u8>, _>("checksum"),
                )
            })
            .collect::<Vec<_>>();

    let repair = repair_state_migration_version_collisions(&pool, &STATE_MIGRATOR).await;
    assert!(repair.is_err(), "destination collisions must fail closed");
    assert!(
        repair
            .err()
            .expect("collision should produce an error")
            .to_string()
            .contains("maps multiple records to canonical version 31")
    );
    let after =
        sqlx::query("SELECT version, description, checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .expect("mixed ledger should remain readable")
            .into_iter()
            .map(|row| {
                (
                    row.get::<i64, _>("version"),
                    row.get::<String, _>("description"),
                    row.get::<Vec<u8>, _>("checksum"),
                )
            })
            .collect::<Vec<_>>();
    assert_eq!(after, before, "collision preflight must not mutate history");
    pool.close().await;
}

#[tokio::test]
async fn goal_transfer_reopens_after_destination_commit_failure_without_losing_source() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let thread_id = "00000000-0000-0000-0000-000000000101";
    let state_pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .expect("state database should open");
    migrator_through(33)
        .run(&state_pool)
        .await
        .expect("pre-drop upstream state history should apply");
    insert_old_binary_thread(&state_pool, thread_id, "/tmp/reopen-goal.jsonl").await;
    insert_legacy_goal(&state_pool, thread_id, "retry after destination commit").await;
    state_pool.close().await;

    crate::runtime::migration_repair::fail_next_goal_transfer_after_destination_commit(thread_id);
    let first_open =
        crate::runtime::StateRuntime::init(sqlite.clone(), "test-provider".to_string()).await;
    assert!(
        first_open.is_err(),
        "injected post-commit failure should abort startup"
    );

    let state_pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .expect("state database should reopen after injected failure");
    let source_goal =
        sqlx::query_scalar::<_, String>("SELECT objective FROM thread_goals WHERE thread_id = ?")
            .bind(thread_id)
            .fetch_one(&state_pool)
            .await
            .expect("source goal must remain until canonical drop");
    assert_eq!(source_goal, "retry after destination commit");
    state_pool.close().await;

    let runtime = crate::runtime::StateRuntime::init(sqlite.clone(), "test-provider".to_string())
        .await
        .expect("retry should accept the identical committed destination goal");
    assert!(
        runtime
            .thread_goals()
            .get_thread_goal(ThreadId::from_string(thread_id).expect("valid thread id"))
            .await
            .expect("goal query should succeed")
            .is_some()
    );
    runtime.close().await;
}

#[tokio::test]
async fn concurrent_new_runtimes_serialize_goal_transfer_before_state_drop() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let thread_id = "00000000-0000-0000-0000-000000000102";
    let state_pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .expect("state database should open");
    migrator_through(33)
        .run(&state_pool)
        .await
        .expect("pre-drop upstream state history should apply");
    insert_old_binary_thread(&state_pool, thread_id, "/tmp/concurrent-goal.jsonl").await;
    insert_legacy_goal(&state_pool, thread_id, "serialize concurrent transfer").await;
    state_pool.close().await;

    let (first, second) = tokio::join!(
        crate::runtime::StateRuntime::init(sqlite.clone(), "test-provider".to_string()),
        crate::runtime::StateRuntime::init(sqlite.clone(), "test-provider".to_string())
    );
    let first = first.expect("first runtime should initialize");
    let second = second.expect("second runtime should initialize");
    assert!(
        first
            .thread_goals()
            .get_thread_goal(ThreadId::from_string(thread_id).expect("valid thread id"))
            .await
            .expect("first goal query should succeed")
            .is_some()
    );
    assert!(
        second
            .thread_goals()
            .get_thread_goal(ThreadId::from_string(thread_id).expect("valid thread id"))
            .await
            .expect("second goal query should succeed")
            .is_some()
    );
    first.close().await;
    second.close().await;
}

#[tokio::test]
async fn conflicting_pre_drop_goal_destination_fails_closed_without_dropping_source() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let thread_id = "00000000-0000-0000-0000-000000000100";
    let state_pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .expect("state database should open");
    migrator_through(33)
        .run(&state_pool)
        .await
        .expect("pre-drop upstream state history should apply");
    insert_old_binary_thread(&state_pool, thread_id, "/tmp/conflicting-goal.jsonl").await;
    insert_legacy_goal(&state_pool, thread_id, "source goal").await;
    state_pool.close().await;

    let goals_pool = sqlite
        .open_read_write_pool(&sqlite.goals_db_path())
        .await
        .expect("goals database should open");
    GOALS_MIGRATOR
        .run(&goals_pool)
        .await
        .expect("goals schema should apply");
    sqlx::query(
        r#"
INSERT INTO thread_goals (
    thread_id, goal_id, objective, status, token_budget, tokens_used,
    time_used_seconds, created_at_ms, updated_at_ms
) VALUES (?, 'different-goal', 'destination goal', 'active', NULL, 0, 0, 1, 1)
        "#,
    )
    .bind(thread_id)
    .execute(&goals_pool)
    .await
    .expect("conflicting destination goal should insert");
    goals_pool.close().await;

    let result =
        crate::runtime::StateRuntime::init(sqlite.clone(), "test-provider".to_string()).await;
    assert!(
        result.is_err(),
        "conflicting destination goal must fail closed"
    );
    let error = result
        .err()
        .expect("conflicting destination goal should return an error");
    assert!(
        error
            .to_string()
            .contains("conflicts with an existing destination goal")
    );

    let state_pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .expect("state database should reopen");
    let source_goal =
        sqlx::query_scalar::<_, String>("SELECT objective FROM thread_goals WHERE thread_id = ?")
            .bind(thread_id)
            .fetch_one(&state_pool)
            .await
            .expect("source goal must remain after conflict");
    assert_eq!(source_goal, "source goal");
    state_pool.close().await;
}

#[tokio::test]
async fn usage_automatic_turn_epochs_backfills_legacy_abort_occurrences() {
    let sqlite_home = tempfile::tempdir().expect("sqlite home should be created");
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.path().abs());
    let usage_path = sqlite.usage_db_path();
    let pool = sqlite
        .open_read_write_pool(&usage_path)
        .await
        .expect("usage database should open");
    usage_migrator_through(/*version*/ 11)
        .run(&pool)
        .await
        .expect("pre-epochs usage migrations should apply");

    sqlx::query(
        "INSERT INTO usage_automatic_turns (thread_id, client_user_message_id, trigger_turn_id, turn_id, event_occurrence_id, origin, generation, capability, attempt, max_attempts, provenance_source, outcome) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("thread-legacy")
    .bind("client-legacy")
    .bind("trigger-legacy")
    .bind("turn-legacy")
    .bind("occurrence-legacy")
    .bind("policy_retry")
    .bind(0_i64)
    .bind("capability-legacy")
    .bind(1_i64)
    .bind(3_i64)
    .bind("server_validated_client_user_message_id")
    .bind("aborted")
    .execute(&pool)
    .await
    .expect("legacy aborted row should insert");

    USAGE_MIGRATOR
        .run(&pool)
        .await
        .expect("epochs migration should apply");
    let abort_occurrence: String = sqlx::query_scalar(
        "SELECT abort_event_occurrence_id FROM usage_automatic_turns WHERE client_user_message_id = ?",
    )
    .bind("client-legacy")
    .fetch_one(&pool)
    .await
    .expect("backfilled abort occurrence should load");
    assert_eq!(abort_occurrence, "occurrence-legacy");
    pool.close().await;
}
