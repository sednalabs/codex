use std::borrow::Cow;
use std::collections::BTreeSet;

use codex_utils_absolute_path::test_support::PathExt;
use sqlx::Connection;
use sqlx::Row;
use sqlx::migrate::Migration;
use sqlx::migrate::Migrator;

use super::STATE_MIGRATOR;
use super::THREAD_HISTORY_MIGRATOR;
use super::repair_state_migration_version_collisions;

const PRE_RECENCY_MIGRATION_VERSION: i64 = 42;
const LEGACY_RECENCY_MIGRATION_VERSION: i64 = 38;
const CURRENT_RECENCY_MIGRATION_VERSION: i64 = 43;
const LEGACY_VISIBLE_SORT_INDEXES_MIGRATION_VERSION: i64 = 40;
const CURRENT_VISIBLE_SORT_INDEXES_MIGRATION_VERSION: i64 = 44;
const LEGACY_REMOTE_CONTROL_ENABLED_MIGRATION_VERSION: i64 = 41;
const CURRENT_REMOTE_CONTROL_ENABLED_MIGRATION_VERSION: i64 = 46;
const PRE_CONFIGURED_IDENTITY_PROVENANCE_MIGRATION_VERSION: i64 = 44;
const LEGACY_EXTERNAL_AGENT_CONFIG_IMPORTS_MIGRATION_VERSION: i64 = 42;
const CURRENT_EXTERNAL_AGENT_CONFIG_IMPORTS_MIGRATION_VERSION: i64 = 47;
const CURRENT_PINNED_THREADS_MIGRATION_VERSION: i64 = 48;
const LEGACY_EXTERNAL_AGENT_CONFIG_IMPORTS_PROVIDER_ID_MIGRATION_VERSION: i64 = 44;
const CURRENT_EXTERNAL_AGENT_CONFIG_IMPORTS_PROVIDER_ID_MIGRATION_VERSION: i64 = 49;
const DEPLOYED_ORIGIN_MAIN_MIGRATION_VERSION: i64 = 45;

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

fn origin_main_migrator() -> Migrator {
    let remote_control_enabled_migration = STATE_MIGRATOR
        .migrations
        .iter()
        .find(|migration| migration.version == CURRENT_REMOTE_CONTROL_ENABLED_MIGRATION_VERSION)
        .expect("remote control enabled migration should exist");
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
        .filter(|migration| migration.version < LEGACY_REMOTE_CONTROL_ENABLED_MIGRATION_VERSION)
        .cloned()
        .collect::<Vec<_>>();
    migrations.push(Migration::new(
        LEGACY_REMOTE_CONTROL_ENABLED_MIGRATION_VERSION,
        remote_control_enabled_migration.description.clone(),
        remote_control_enabled_migration.migration_type,
        remote_control_enabled_migration.sql.clone(),
        remote_control_enabled_migration.no_tx,
    ));
    migrations.push(Migration::new(
        LEGACY_EXTERNAL_AGENT_CONFIG_IMPORTS_MIGRATION_VERSION,
        external_imports_migration.description.clone(),
        external_imports_migration.migration_type,
        external_imports_migration.sql.clone(),
        external_imports_migration.no_tx,
    ));
    migrations.extend(
        STATE_MIGRATOR
            .migrations
            .iter()
            .filter(|migration| {
                migration.version > LEGACY_EXTERNAL_AGENT_CONFIG_IMPORTS_MIGRATION_VERSION
                    && migration.version <= DEPLOYED_ORIGIN_MAIN_MIGRATION_VERSION
            })
            .cloned(),
    );
    Migrator::with_migrations(migrations)
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
        .filter(|migration| {
            migration.version < LEGACY_EXTERNAL_AGENT_CONFIG_IMPORTS_MIGRATION_VERSION
        })
        .cloned()
        .collect::<Vec<_>>();
    migrations.push(Migration::new(
        LEGACY_EXTERNAL_AGENT_CONFIG_IMPORTS_MIGRATION_VERSION,
        external_imports_migration.description.clone(),
        external_imports_migration.migration_type,
        external_imports_migration.sql.clone(),
        external_imports_migration.no_tx,
    ));
    migrations.extend(
        STATE_MIGRATOR
            .migrations
            .iter()
            .filter(|migration| {
                migration.version > LEGACY_EXTERNAL_AGENT_CONFIG_IMPORTS_MIGRATION_VERSION
                    && migration.version
                        < LEGACY_EXTERNAL_AGENT_CONFIG_IMPORTS_PROVIDER_ID_MIGRATION_VERSION
            })
            .cloned(),
    );
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
async fn pinned_threads_migration_defaults_existing_and_legacy_rows_to_unpinned() {
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
        .expect("pre-pin migrations should apply");

    for thread_id in [
        "00000000-0000-0000-0000-000000000043",
        "00000000-0000-0000-0000-000000000044",
    ] {
        if thread_id.ends_with("44") {
            STATE_MIGRATOR
                .run(&pool)
                .await
                .expect("pin migration should apply");
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

    let pinned_values = sqlx::query_scalar::<_, bool>("SELECT is_pinned FROM threads ORDER BY id")
        .fetch_all(&pool)
        .await
        .expect("pin states should load");
    assert_eq!(pinned_values, vec![false, false]);

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
        .open_read_only_pool(&sqlite.state_db_path())
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
    migrator_through(/*version*/ 40)
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
        .filter(|migration| migration.version <= 40)
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
        .open_read_only_pool(&state_path)
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
