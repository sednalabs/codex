use super::STATE_MIGRATOR;
use super::THREAD_HISTORY_MIGRATOR;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use sqlx::SqlitePool;
use sqlx::migrate::Migration;
use sqlx::migrate::Migrator;

type Ledger = Vec<(i64, String, bool, Vec<u8>, i64, String)>;
type Schema = Vec<(String, String, Option<String>)>;

#[tokio::test]
async fn thread_history_prefixes_upgrade_and_unknown_identities_fail_closed() {
    for through in [4_i64, 8] {
        let home = tempfile::TempDir::new().unwrap();
        let sqlite = crate::SqliteConfig::new_for_testing(home.path().abs());
        let prefix = Migrator::with_migrations(
            THREAD_HISTORY_MIGRATOR
                .migrations
                .iter()
                .filter(|migration| migration.version <= through)
                .cloned()
                .collect(),
        );
        let pool = sqlite
            .open_thread_history_db(&prefix, /*telemetry_override*/ None)
            .await
            .unwrap();
        sqlx::query("INSERT INTO thread_turns (thread_id, turn_id, rollout_ordinal, status) VALUES ('synthetic-thread', 'turn-1', 10, 'completed')")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO thread_items (thread_id, turn_id, item_id, rollout_ordinal, created_at_ms, item_type, item_json) VALUES ('synthetic-thread', 'turn-1', 'item-1', 11, 1000, 'userMessage', '{\"text\":\"preserve-me\"}')")
            .execute(&pool).await.unwrap();
        pool.close().await;

        let pool = sqlite
            .open_thread_history_db(&THREAD_HISTORY_MIGRATOR, /*telemetry_override*/ None)
            .await
            .unwrap();
        let witness: String =
            sqlx::query_scalar("SELECT item_json FROM thread_items WHERE item_id = 'item-1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(witness, "{\"text\":\"preserve-me\"}");
        sqlx::query("INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (9999, 'unknown', 1, x'01', 0)")
            .execute(&pool).await.unwrap();
        let before: Ledger = sqlx::query_as("SELECT version, description, success, checksum, execution_time, CAST(installed_on AS TEXT) FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool).await.unwrap();
        pool.close().await;

        let error = sqlite
            .open_thread_history_db(&THREAD_HISTORY_MIGRATOR, /*telemetry_override*/ None)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("thread history migration history at version 9999"));
        let pool = sqlite
            .open_read_write_pool(&sqlite.thread_history_db_path())
            .await
            .unwrap();
        let after: Ledger = sqlx::query_as("SELECT version, description, success, checksum, execution_time, CAST(installed_on AS TEXT) FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool).await.unwrap();
        assert_eq!(after, before);
        let witness: String =
            sqlx::query_scalar("SELECT item_json FROM thread_items WHERE item_id = 'item-1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(witness, "{\"text\":\"preserve-me\"}");
        pool.close().await;
    }
}

async fn snapshot(pool: &SqlitePool) -> (Ledger, Schema, String) {
    let ledger = sqlx::query_as("SELECT version, description, success, checksum, execution_time, CAST(installed_on AS TEXT) FROM _sqlx_migrations ORDER BY version")
        .fetch_all(pool).await.unwrap();
    let schema = sqlx::query_as("SELECT type, name, sql FROM sqlite_master ORDER BY type, name")
        .fetch_all(pool)
        .await
        .unwrap();
    let witness = sqlx::query_scalar("SELECT title FROM threads WHERE id = 'synthetic-thread'")
        .fetch_one(pool)
        .await
        .unwrap();
    (ledger, schema, witness)
}

#[tokio::test]
async fn fork_history_conflicts_reject_without_semantic_changes() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../tests/fixtures/w14939-sedna-migrations.json"
    ))
    .unwrap();
    for through in [39_i64, 45] {
        let sqlite_home = crate::runtime::test_support::unique_temp_dir();
        tokio::fs::create_dir_all(&sqlite_home).await.unwrap();
        let _cleanup = scopeguard::guard(sqlite_home.clone(), |home| {
            let _ = std::fs::remove_dir_all(home);
        });
        let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
        let pool = sqlite
            .open_read_write_pool(&sqlite.state_db_path())
            .await
            .unwrap();
        sqlx::raw_sql("CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY, description TEXT NOT NULL, installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP, success BOOLEAN NOT NULL, checksum BLOB NOT NULL, execution_time BIGINT NOT NULL)")
            .execute(&pool).await.unwrap();
        for migration in fixture["migrations"].as_array().unwrap() {
            let filename = migration["filename"].as_str().unwrap();
            let version: i64 = filename.split('_').next().unwrap().parse().unwrap();
            if version > through {
                break;
            }
            // The SQL is embedded public migration fixture data, never user input.
            sqlx::raw_sql(sqlx::AssertSqlSafe(migration["sql"].as_str().unwrap()))
                .execute(&pool)
                .await
                .unwrap();
            let hex = migration["sha384"].as_str().unwrap().as_bytes();
            let checksum: Vec<u8> = hex
                .chunks_exact(2)
                .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
                .collect();
            let description = filename
                .trim_end_matches(".sql")
                .split_once('_')
                .unwrap()
                .1
                .replace('_', " ");
            sqlx::query("INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (?, ?, 1, ?, 0)")
                .bind(version).bind(description).bind(checksum).execute(&pool).await.unwrap();
        }
        sqlx::query("INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode) VALUES ('synthetic-thread', '/synthetic', 1, 2, 'cli', 'test', '/synthetic', 'preserve-me', 'read-only', 'never')")
            .execute(&pool).await.unwrap();
        let before = snapshot(&pool).await;
        pool.close().await;

        let error = sqlite
            .open_state_db(&STATE_MIGRATOR, /*telemetry_override*/ None)
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("unrecognized state migration identity at version 24")
        );
        let pool = sqlite
            .open_read_write_pool(&sqlite.state_db_path())
            .await
            .unwrap();
        assert_eq!(snapshot(&pool).await, before);
        pool.close().await;
    }
}

#[tokio::test]
async fn recorded_source_prefixes_migrate_without_losing_witness() {
    for fixture in [
        include_str!("../tests/fixtures/w14939-oci-source-migrations.json"),
        include_str!("../tests/fixtures/w14939-workstation-source-migrations.json"),
    ] {
        let fixture: Value = serde_json::from_str(fixture).unwrap();
        for through in [39_i64, 45, 62] {
            let sqlite_home = crate::runtime::test_support::unique_temp_dir();
            tokio::fs::create_dir_all(&sqlite_home).await.unwrap();
            let _cleanup = scopeguard::guard(sqlite_home.clone(), |home| {
                let _ = std::fs::remove_dir_all(home);
            });
            let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
            let pool = sqlite
                .open_read_write_pool(&sqlite.state_db_path())
                .await
                .unwrap();
            sqlx::raw_sql("CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY, description TEXT NOT NULL, installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP, success BOOLEAN NOT NULL, checksum BLOB NOT NULL, execution_time BIGINT NOT NULL)")
                .execute(&pool).await.unwrap();
            for migration in fixture["migrations"].as_array().unwrap() {
                let filename = migration["filename"].as_str().unwrap();
                let version: i64 = filename.split('_').next().unwrap().parse().unwrap();
                if version > through {
                    break;
                }
                sqlx::raw_sql(sqlx::AssertSqlSafe(migration["sql"].as_str().unwrap()))
                    .execute(&pool)
                    .await
                    .unwrap();
                let checksum: Vec<u8> = migration["sha384"]
                    .as_str()
                    .unwrap()
                    .as_bytes()
                    .chunks_exact(2)
                    .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
                    .collect();
                let description = filename
                    .trim_end_matches(".sql")
                    .split_once('_')
                    .unwrap()
                    .1
                    .replace('_', " ");
                sqlx::query("INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (?, ?, 1, ?, 0)")
                    .bind(version).bind(description).bind(checksum).execute(&pool).await.unwrap();
            }
            sqlx::query("INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode) VALUES ('synthetic-thread', '/synthetic', 1, 2, 'cli', 'test', '/synthetic', 'preserve-me', 'read-only', 'never')")
                .execute(&pool).await.unwrap();
            let before: Ledger = sqlx::query_as("SELECT version, description, success, checksum, execution_time, CAST(installed_on AS TEXT) FROM _sqlx_migrations ORDER BY version").fetch_all(&pool).await.unwrap();
            pool.close().await;
            let pool = sqlite
                .open_state_db(&STATE_MIGRATOR, /*telemetry_override*/ None)
                .await
                .unwrap();
            let after: Ledger = sqlx::query_as("SELECT version, description, success, checksum, execution_time, CAST(installed_on AS TEXT) FROM _sqlx_migrations WHERE version <= ? ORDER BY version").bind(through).fetch_all(&pool).await.unwrap();
            assert_eq!(after, before);
            let witness: String =
                sqlx::query_scalar("SELECT title FROM threads WHERE id = 'synthetic-thread'")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(witness, "preserve-me");
            pool.close().await;
        }
    }
}

#[tokio::test]
async fn unknown_high_state_ledger_fails_without_changing_synthetic_effects() {
    // Six public Sedna migrations have names matching the reported high-version
    // descriptions, but their actual identities have not been established.
    // The remaining migration and all witness data are deliberately synthetic.
    const FIRST_HIGH_VERSION: i64 = 8_000_000_000_000_000;
    let sqlite_home = tempfile::TempDir::new().unwrap();
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.path().abs());
    let pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .unwrap();
    STATE_MIGRATOR.run(&pool).await.unwrap();
    sqlx::query("INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode) VALUES ('synthetic-thread', '/synthetic', 1, 2, 'cli', 'test', '/synthetic', 'preserve-me', 'read-only', 'never')")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode) VALUES ('synthetic-child', '/synthetic-child', 1, 2, ?, 'test', '/synthetic', 'child', 'read-only', 'never')")
        .bind(r#"{"subagent":{"thread_spawn":{"parent_thread_id":"synthetic-thread"}}}"#)
        .execute(&pool).await.unwrap();
    let fixture: Value = serde_json::from_str(include_str!(
        "../tests/fixtures/w14939-sedna-migrations.json"
    ))
    .unwrap();
    for prefix in ["0038_", "0045_", "0024_", "0027_", "0028_", "0050_"] {
        let migration = fixture["migrations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["filename"].as_str().unwrap().starts_with(prefix))
            .unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(migration["sql"].as_str().unwrap()))
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO phase2_attestation_roots VALUES ('synthetic-root', 1, 2)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO phase2_attested_baselines VALUES ('synthetic-root', 'synthetic-tree', 1, 'selection', 'inputs', 'consolidator', 2, 3, 4)")
        .execute(&pool).await.unwrap();
    sqlx::query(
        "UPDATE threads SET configured_identity_provenance = 2 WHERE id = 'synthetic-thread'",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO thread_dynamic_tools (thread_id, position, name, description, input_schema, persist_on_resume, capability_json) VALUES ('synthetic-thread', 0, 'synthetic-tool', 'synthetic', '{}', 0, '{\"synthetic\":true}')")
        .execute(&pool).await.unwrap();
    sqlx::query("CREATE TABLE synthetic_extension_effects (version INTEGER PRIMARY KEY, marker TEXT NOT NULL)")
        .execute(&pool).await.unwrap();
    let public_candidates = [
        Some("0038_"),
        Some("0045_"),
        None,
        Some("0024_"),
        Some("0027_"),
        Some("0028_"),
        Some("0050_"),
    ];
    for (offset, candidate) in public_candidates.into_iter().enumerate() {
        let offset = offset as i64;
        let version = FIRST_HIGH_VERSION + offset;
        let (description, checksum) = if let Some(prefix) = candidate {
            let entry = fixture["migrations"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["filename"].as_str().unwrap().starts_with(prefix))
                .unwrap();
            let filename = entry["filename"].as_str().unwrap();
            let description = filename
                .trim_end_matches(".sql")
                .split_once('_')
                .unwrap()
                .1
                .replace('_', " ");
            let checksum = entry["sha384"]
                .as_str()
                .unwrap()
                .as_bytes()
                .chunks_exact(2)
                .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
                .collect::<Vec<_>>();
            (description, checksum)
        } else {
            ("thread dynamic tools state".to_string(), vec![2_u8; 48])
        };
        sqlx::query("INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (?, ?, 1, ?, 0)")
            .bind(version)
            .bind(description)
            .bind(checksum)
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO synthetic_extension_effects VALUES (?, ?)")
            .bind(version)
            .bind(format!("synthetic-marker-{offset}"))
            .execute(&pool)
            .await
            .unwrap();
    }
    let before = snapshot(&pool).await;
    let effects_before: Vec<(i64, String)> =
        sqlx::query_as("SELECT version, marker FROM synthetic_extension_effects ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    pool.close().await;

    let error = crate::StateRuntime::init(sqlite.clone(), "synthetic-provider".to_string())
        .await
        .err()
        .expect("unknown state identities must reject startup");
    assert!(format!("{error:#}").contains(&format!(
        "unrecognized or incomplete state migration history at version {FIRST_HIGH_VERSION}"
    )));
    assert!(!crate::is_sqlite_corruption_error(&error));
    let pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .unwrap();
    assert_eq!(snapshot(&pool).await, before);
    let effects_after: Vec<(i64, String)> =
        sqlx::query_as("SELECT version, marker FROM synthetic_extension_effects ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(effects_after, effects_before);
    let identity: i64 = sqlx::query_scalar(
        "SELECT configured_identity_provenance FROM threads WHERE id = 'synthetic-thread'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(identity, 2);
    let tool: (bool, String) = sqlx::query_as("SELECT persist_on_resume, capability_json FROM thread_dynamic_tools WHERE thread_id = 'synthetic-thread'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(tool, (false, r#"{"synthetic":true}"#.to_string()));
    let edge: (String, String, String) = sqlx::query_as("SELECT parent_thread_id, child_thread_id, status FROM thread_spawn_edges WHERE child_thread_id = 'synthetic-child'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(
        edge,
        (
            "synthetic-thread".to_string(),
            "synthetic-child".to_string(),
            "open".to_string()
        )
    );
    let roots: (String, i64, i64) = sqlx::query_as(
        "SELECT memory_root_key, required_since, updated_at FROM phase2_attestation_roots",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(roots, ("synthetic-root".to_string(), 1, 2));
    let baselines: (String, String, i64) = sqlx::query_as(
        "SELECT memory_root_key, output_tree_sha256, attested_at FROM phase2_attested_baselines",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        baselines,
        (
            "synthetic-root".to_string(),
            "synthetic-tree".to_string(),
            4
        )
    );
    pool.close().await;
}

#[tokio::test]
async fn sqlx_applies_a_pending_low_version_without_rewriting_unknown_high_rows() {
    // Exercise SQLx in isolation, bypassing the production preflight: this is
    // not permission to open a database with an unidentified migration.
    let home = tempfile::TempDir::new().unwrap();
    let sqlite = crate::SqliteConfig::new_for_testing(home.path().abs());
    let pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .unwrap();
    let prefix = Migrator::with_migrations(
        STATE_MIGRATOR
            .migrations
            .iter()
            .filter(|migration| migration.version <= 61)
            .cloned()
            .collect(),
    );
    prefix.run(&pool).await.unwrap();
    sqlx::query("CREATE TABLE synthetic_extension (marker TEXT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO synthetic_extension VALUES ('preserve-me')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (8000000000000000, 'synthetic unknown', 1, X'0102', 17)").execute(&pool).await.unwrap();
    let before: Ledger = sqlx::query_as("SELECT version, description, success, checksum, execution_time, CAST(installed_on AS TEXT) FROM _sqlx_migrations ORDER BY version").fetch_all(&pool).await.unwrap();
    super::runtime_state_migrator().run(&pool).await.unwrap();
    let after: Ledger = sqlx::query_as("SELECT version, description, success, checksum, execution_time, CAST(installed_on AS TEXT) FROM _sqlx_migrations WHERE version != 62 ORDER BY version").fetch_all(&pool).await.unwrap();
    assert_eq!(after, before);
    let applied: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM _sqlx_migrations WHERE version = 62 AND success = 1)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(applied);
    let marker: String = sqlx::query_scalar("SELECT marker FROM synthetic_extension")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(marker, "preserve-me");
    pool.close().await;
}

#[tokio::test]
async fn individual_fork_identities_39_and_45_are_not_upstream_identities() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../tests/fixtures/w14939-sedna-migrations.json"
    ))
    .unwrap();
    for version in [39_i64, 45] {
        let sqlite_home = crate::runtime::test_support::unique_temp_dir();
        tokio::fs::create_dir_all(&sqlite_home).await.unwrap();
        let _cleanup = scopeguard::guard(sqlite_home.clone(), |home| {
            let _ = std::fs::remove_dir_all(home);
        });
        let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
        let pool = sqlite
            .open_read_write_pool(&sqlite.state_db_path())
            .await
            .unwrap();
        STATE_MIGRATOR.run(&pool).await.unwrap();
        sqlx::query("INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode) VALUES ('synthetic-thread', '/synthetic', 1, 2, 'cli', 'test', '/synthetic', 'preserve-me', 'read-only', 'never')")
            .execute(&pool).await.unwrap();
        let item = fixture["migrations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| {
                m["filename"]
                    .as_str()
                    .unwrap()
                    .starts_with(&format!("{version:04}_"))
            })
            .unwrap();
        let checksum: Vec<u8> = item["sha384"]
            .as_str()
            .unwrap()
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect();
        sqlx::query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = ?")
            .bind(checksum)
            .bind(version)
            .execute(&pool)
            .await
            .unwrap();
        let before = snapshot(&pool).await;
        pool.close().await;

        let error = sqlite
            .open_state_db(&STATE_MIGRATOR, /*telemetry_override*/ None)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains(&format!(
            "unrecognized state migration identity at version {version}"
        )));
        let pool = sqlite
            .open_read_write_pool(&sqlite.state_db_path())
            .await
            .unwrap();
        assert_eq!(snapshot(&pool).await, before);
        pool.close().await;
    }
}

#[tokio::test]
async fn mixed_legacy_recency_and_unknown_identity_reject_before_rekey() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home).await.unwrap();
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |home| {
        let _ = std::fs::remove_dir_all(home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .unwrap();
    let recency = STATE_MIGRATOR
        .migrations
        .iter()
        .find(|m| m.version == 39)
        .unwrap();
    let mut legacy: Vec<_> = STATE_MIGRATOR
        .migrations
        .iter()
        .filter(|m| m.version <= 37)
        .cloned()
        .collect();
    legacy.push(Migration::new(
        38,
        recency.description.clone(),
        recency.migration_type,
        recency.sql.clone(),
        recency.no_tx,
    ));
    Migrator::with_migrations(legacy).run(&pool).await.unwrap();
    sqlx::query("INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (9999, 'unknown', 1, X'010203', 0)")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode) VALUES ('synthetic-thread', '/synthetic', 1, 2, 'cli', 'test', '/synthetic', 'preserve-me', 'read-only', 'never')")
        .execute(&pool).await.unwrap();
    let before = snapshot(&pool).await;
    pool.close().await;

    assert!(
        sqlite
            .open_state_db(&STATE_MIGRATOR, /*telemetry_override*/ None)
            .await
            .is_err()
    );
    let pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .unwrap();
    assert_eq!(snapshot(&pool).await, before);
    pool.close().await;
}

#[tokio::test]
async fn untracked_schema_is_not_adopted_as_a_fresh_database() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home).await.unwrap();
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |home| {
        let _ = std::fs::remove_dir_all(home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TABLE unknown_state (value TEXT); INSERT INTO unknown_state VALUES ('preserve-me');")
        .execute(&pool).await.unwrap();
    pool.close().await;
    let error = sqlite
        .open_state_db(&STATE_MIGRATOR, /*telemetry_override*/ None)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("schema exists without a recognized migration history"));
    let pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .unwrap();
    let value: String = sqlx::query_scalar("SELECT value FROM unknown_state")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(value, "preserve-me");
    let ledger_exists: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE name = '_sqlx_migrations'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(ledger_exists, 0);
    pool.close().await;
}

#[tokio::test]
async fn recognized_legacy_recency_history_migrates_and_reopens() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home).await.unwrap();
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |home| {
        let _ = std::fs::remove_dir_all(home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .unwrap();
    let recency = STATE_MIGRATOR
        .migrations
        .iter()
        .find(|m| m.version == 39)
        .unwrap();
    let mut legacy: Vec<_> = STATE_MIGRATOR
        .migrations
        .iter()
        .filter(|m| m.version <= 37)
        .cloned()
        .collect();
    legacy.push(Migration::new(
        38,
        recency.description.clone(),
        recency.migration_type,
        recency.sql.clone(),
        recency.no_tx,
    ));
    Migrator::with_migrations(legacy).run(&pool).await.unwrap();
    sqlx::query("INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode) VALUES ('synthetic-thread', '/synthetic', 1, 2, 'cli', 'test', '/synthetic', 'preserve-me', 'read-only', 'never')")
        .execute(&pool).await.unwrap();
    pool.close().await;
    let pool = sqlite
        .open_state_db(&STATE_MIGRATOR, /*telemetry_override*/ None)
        .await
        .unwrap();
    let first = snapshot(&pool).await;
    let applied: Vec<i64> = first.0.iter().map(|row| row.0).collect();
    assert_eq!(
        applied,
        STATE_MIGRATOR
            .migrations
            .iter()
            .map(|m| m.version)
            .collect::<Vec<_>>()
    );
    assert_eq!(first.2, "preserve-me");
    pool.close().await;
    let pool = sqlite
        .open_state_db(&STATE_MIGRATOR, /*telemetry_override*/ None)
        .await
        .unwrap();
    assert_eq!(snapshot(&pool).await, first);
    pool.close().await;
}
