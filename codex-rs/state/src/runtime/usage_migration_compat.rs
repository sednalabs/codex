use anyhow::Context;
use anyhow::ensure;
use sqlx::QueryBuilder;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use sqlx::migrate::Migration;
use sqlx::migrate::Migrator;
use std::borrow::Cow;

// These are the SHA-384 checksums recorded by the shipped old-main usage
// database. They are intentionally exact: this is a compatibility map for
// known historical variants, not a general SQLx checksum bypass.
const OLD_MAIN_0001_CHECKSUM: &[u8] = &[
    0xd5, 0x10, 0x0b, 0x3e, 0x67, 0xe9, 0x8f, 0xac, 0xdf, 0x91, 0xe0, 0x63, 0x34, 0x5e, 0xb8, 0x83,
    0x0f, 0xc3, 0x1c, 0xde, 0x63, 0x40, 0x7e, 0x8f, 0xa6, 0xe0, 0xc0, 0x20, 0x2d, 0xd2, 0x3d, 0xf7,
    0xb3, 0xdb, 0x1d, 0x4d, 0xcf, 0x5c, 0xf8, 0x71, 0xf5, 0x5a, 0x6d, 0x20, 0x44, 0x04, 0xd5, 0xa7,
];
const OLD_MAIN_0005_CHECKSUM: &[u8] = &[
    0x81, 0x7a, 0x9d, 0x40, 0x2e, 0xeb, 0xf0, 0x82, 0x7a, 0x55, 0x82, 0xa6, 0x10, 0xdb, 0xff, 0xdc,
    0x71, 0xa2, 0x7c, 0xc5, 0x8e, 0xef, 0xe5, 0xfd, 0x36, 0x85, 0xe7, 0x05, 0x80, 0x4a, 0xc1, 0x5e,
    0x22, 0xea, 0x39, 0x3e, 0xf6, 0x5b, 0xc8, 0xeb, 0x39, 0x36, 0x1e, 0x15, 0xaf, 0x22, 0xe9, 0x8c,
];

const ORIGINAL_0009_CHECKSUM: &[u8] = &[
    0x70, 0xfb, 0x40, 0xc4, 0xb8, 0xf9, 0x56, 0x84, 0x68, 0xc9, 0x8f, 0xcc, 0x96, 0x4c, 0xb1, 0x4e,
    0x5b, 0xfe, 0x05, 0xfd, 0x16, 0x5c, 0xe3, 0x6d, 0x6f, 0xba, 0x56, 0x45, 0xaa, 0x9a, 0xd4, 0x2d,
    0x26, 0xf7, 0xb0, 0x56, 0xfa, 0xd1, 0x7f, 0x76, 0x89, 0x95, 0x40, 0x26, 0xfe, 0xbf, 0xe2, 0xab,
];

struct HistoricalVariant {
    version: i64,
    checksum: &'static [u8],
}

const OLD_MAIN_VARIANTS: &[HistoricalVariant] = &[
    HistoricalVariant {
        version: 1,
        checksum: OLD_MAIN_0001_CHECKSUM,
    },
    HistoricalVariant {
        version: 5,
        checksum: OLD_MAIN_0005_CHECKSUM,
    },
    HistoricalVariant {
        version: 9,
        checksum: ORIGINAL_0009_CHECKSUM,
    },
];

/// Build a usage migrator whose expected checksums include only the exact
/// historical variants that are already recorded in the database.
///
/// SQLx validates every known migration checksum. P7's 0001 and 0005 differ
/// from the shipped old-main files, so a canonical migrator would reject an
/// otherwise usable database before the additive compatibility migration can
/// restore the status-gated views. We keep the historical checksum in
/// `_sqlx_migrations` and only adjust the in-memory expected checksum for a
/// known old-main variant. The original 0009 used a different name for dormant
/// principal columns. Preserve that data only when both original column shapes
/// are recognized; native provider/thread usage APIs do not consume them.
/// Any other checksum mismatch is left for SQLx to reject.
pub(crate) async fn migrator_for_usage_database(
    pool: &SqlitePool,
    base: &Migrator,
) -> anyhow::Result<Migrator> {
    let table_name = base.table_name.as_ref();
    if !migration_table_exists(pool, table_name).await? {
        return Ok(clone_migrator(base, base.migrations.to_vec()));
    }

    let mut migrations = base.migrations.to_vec();
    for variant in OLD_MAIN_VARIANTS {
        let Some(recorded_checksum) = recorded_checksum(pool, table_name, variant.version).await?
        else {
            continue;
        };
        if recorded_checksum == variant.checksum {
            if variant.version == 9 {
                validate_original_principal_columns(pool).await?;
            }
            let migration = migrations
                .iter_mut()
                .find(|migration| migration.version == variant.version)
                .with_context(|| {
                    format!(
                        "known old-main usage migration {} is absent from the embedded set",
                        variant.version
                    )
                })?;
            migration.checksum = Cow::Borrowed(variant.checksum);
        }
    }

    Ok(clone_migrator(base, migrations))
}

async fn validate_original_principal_columns(pool: &SqlitePool) -> anyhow::Result<()> {
    for table in [
        "usage_automatic_turn_eligibility",
        "usage_automatic_turns",
    ] {
        let table_type = sqlx::query_scalar::<_, String>(
            "SELECT type FROM pragma_table_list WHERE schema = 'main' AND name = ?",
        )
        .bind(table)
        .fetch_optional(pool)
        .await?;
        ensure!(
            table_type.as_deref() == Some("table"),
            "unsupported original usage migration 9 principal table schema in {table}"
        );
        let columns = sqlx::query_as::<_, (String, String, i64, Option<String>, i64, i64)>(
            r#"SELECT name, type, "notnull", dflt_value, pk, hidden
               FROM pragma_table_xinfo(?, 'main')
               WHERE name IN ('connection_principal', 'principal_identity')"#,
        )
        .bind(table)
        .fetch_all(pool)
        .await?;
        ensure!(
            matches!(
                columns.as_slice(),
                [(name, column_type, 0, None, 0, 0)]
                    if name == "connection_principal" && column_type == "TEXT"
            ),
            "unsupported original usage migration 9 principal column schema in {table}"
        );
    }
    Ok(())
}

fn clone_migrator(base: &Migrator, migrations: Vec<Migration>) -> Migrator {
    Migrator {
        migrations: Cow::Owned(migrations),
        ignore_missing: base.ignore_missing,
        locking: base.locking,
        table_name: base.table_name.clone(),
        create_schemas: base.create_schemas.clone(),
        no_tx: base.no_tx,
    }
}

async fn migration_table_exists(pool: &SqlitePool, table_name: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?",
    )
    .bind(table_name)
    .fetch_optional(pool)
    .await?
    .is_some())
}

async fn recorded_checksum(
    pool: &SqlitePool,
    table_name: &str,
    version: i64,
) -> anyhow::Result<Option<Vec<u8>>> {
    let quoted_table_name = format!("\"{}\"", table_name.replace('"', "\"\""));
    let mut query = QueryBuilder::<Sqlite>::new("SELECT checksum FROM ");
    query
        .push(quoted_table_name)
        .push(" WHERE version = ")
        .push_bind(version);
    let row = query.build().fetch_optional(pool).await?;
    row.map(|row| row.try_get::<Vec<u8>, _>("checksum"))
        .transpose()
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::OLD_MAIN_0001_CHECKSUM;
    use super::OLD_MAIN_0005_CHECKSUM;
    use super::ORIGINAL_0009_CHECKSUM;
    use super::clone_migrator;
    use super::migrator_for_usage_database;
    use crate::ProviderCallUsageRecord;
    use crate::ProviderCallUsageWriteOutcome;
    use crate::SqliteConfig;
    use crate::StateRuntime;
    use crate::UsageThreadRecord;
    use crate::migrations::USAGE_MIGRATOR;
    use crate::migrations::runtime_usage_migrator;
    use crate::runtime::test_support::unique_temp_dir;
    use codex_utils_absolute_path::test_support::PathExt;
    use sqlx::AssertSqlSafe;
    use sqlx::SqlSafeStr;
    use sqlx::SqlitePool;
    use sqlx::migrate::MigrateError;
    use sqlx::migrate::Migration;
    use sqlx::raw_sql;

    async fn test_pool() -> SqlitePool {
        let codex_home = unique_temp_dir();
        tokio::fs::create_dir_all(&codex_home)
            .await
            .expect("create usage database directory");
        let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
        sqlite
            .open_read_write_pool(&codex_home.join("usage.sqlite"))
            .await
            .expect("usage database should open")
    }

    fn shipped_main_migration(version: i64) -> Migration {
        let current = USAGE_MIGRATOR
            .migrations
            .iter()
            .find(|migration| migration.version == version)
            .expect("migration should be embedded");
        // Reconstruct the shipped LF bytes independently of the checkout's line endings.
        let sql = match version {
            1 => include_str!("../../usage_migrations/0001_usage_tables.sql")
                .replace("\r\n", "\n")
                .replace(
                    ");\nCREATE TABLE IF NOT EXISTS usage_spawn_requests",
                    ");\n\nCREATE TABLE IF NOT EXISTS usage_spawn_requests",
                ),
            5 => include_str!("../../usage_migrations/0005_usage_codex_credits.sql")
                .replace("\r\n", "\n")
                .replace("CASE WHEN p.status = 'ok'\n                  AND", "CASE WHEN")
                .replace(
                    "        WHEN status IS NULL\n          OR status <> 'ok'\n          OR total_tokens",
                    "        WHEN status IS NULL\n          OR total_tokens",
                )
                .replace(
                    "        WHEN status = 'ok'\n         AND matching_policy_count",
                    "        WHEN matching_policy_count",
                ),
            9 => include_str!(
                "../../usage_migrations/0009_usage_automatic_turn_provenance_hardening.sql"
            )
            .replace("\r\n", "\n")
            .replace("principal_identity", "connection_principal"),
            _ => panic!("unsupported historical migration fixture: {version}"),
        };
        Migration::new(
            current.version,
            current.description.clone(),
            current.migration_type,
            AssertSqlSafe(sql).into_sql_str(),
            current.no_tx,
        )
    }

    #[test]
    fn shipped_main_checksums_are_sqlx_sha384_values() {
        assert_eq!(
            shipped_main_migration(/*version*/ 1).checksum.as_ref(),
            OLD_MAIN_0001_CHECKSUM
        );
        assert_eq!(
            shipped_main_migration(/*version*/ 5).checksum.as_ref(),
            OLD_MAIN_0005_CHECKSUM
        );
        assert_eq!(
            shipped_main_migration(/*version*/ 9).checksum.as_ref(),
            ORIGINAL_0009_CHECKSUM
        );
    }

    #[tokio::test]
    async fn respects_custom_migration_table_name() {
        let pool = test_pool().await;
        sqlx::query(
            "CREATE TABLE custom_sqlx_migrations (version BIGINT PRIMARY KEY, checksum BLOB NOT NULL)",
        )
        .execute(&pool)
        .await
        .expect("custom migration metadata table should be created");
        sqlx::query("INSERT INTO custom_sqlx_migrations (version, checksum) VALUES (?, ?)")
            .bind(1_i64)
            .bind(OLD_MAIN_0001_CHECKSUM)
            .execute(&pool)
            .await
            .expect("custom historical checksum should be inserted");

        let mut base = runtime_usage_migrator();
        base.table_name = "custom_sqlx_migrations".to_owned().into();
        let migrator = migrator_for_usage_database(&pool, &base)
            .await
            .expect("custom migration table should be recognized");
        let migration = migrator
            .iter()
            .find(|migration| migration.version == 1)
            .expect("migration should be embedded");
        assert_eq!(migration.checksum.as_ref(), OLD_MAIN_0001_CHECKSUM);
    }

    #[tokio::test]
    async fn upgrades_old_main_views_without_rewriting_history() {
        let pool = test_pool().await;
        USAGE_MIGRATOR
            .run(&pool)
            .await
            .expect("fresh usage schema should apply");

        // Recreate the old-main view semantics from the exact compatibility
        // migration, removing only the five P2C status gates. This synthetic
        // fixture keeps the old schema and migration history observable while
        // avoiding access to a live database.
        let old_main_views = include_str!(
            "../../usage_migrations/0015_usage_codex_credit_status_compatibility.sql"
        )
        .replace(
            "CASE WHEN p.status = 'ok'\n                  AND",
            "CASE WHEN",
        )
        .replace(
            "        WHEN status IS NULL\n          OR status <> 'ok'\n          OR total_tokens",
            "        WHEN status IS NULL\n          OR total_tokens",
        )
        .replace(
            "        WHEN status = 'ok'\n         AND matching_policy_count",
            "        WHEN matching_policy_count",
        );
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 15")
            .execute(&pool)
            .await
            .expect("compatibility migration should be made pending");
        let old_main_views: &'static str = Box::leak(old_main_views.into_boxed_str());
        raw_sql(old_main_views)
            .execute(&pool)
            .await
            .expect("old-main views should be installed");
        sqlx::query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = 1")
            .bind(OLD_MAIN_0001_CHECKSUM)
            .execute(&pool)
            .await
            .expect("old-main 0001 checksum should be recorded");
        sqlx::query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = 5")
            .bind(OLD_MAIN_0005_CHECKSUM)
            .execute(&pool)
            .await
            .expect("old-main 0005 checksum should be recorded");
        // Use absent versions to model retained newer ledger rows without colliding with the
        // embedded migrations already applied to this synthetic fixture.
        for version in [6_i64, 22, 23, 24, 25, 26] {
            sqlx::query(
                "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (?, ?, 1, ?, 0)",
            )
            .bind(version)
            .bind(format!("synthetic-future-usage-{version}"))
            .bind(vec![version as u8; 32])
            .execute(&pool)
            .await
            .expect("synthetic unknown migration row should be inserted");
        }
        sqlx::query(
            "INSERT INTO usage_threads (thread_id, root_thread_id, source) VALUES (?, ?, ?)",
        )
        .bind("old-main-thread")
        .bind("old-main-thread")
        .bind("cli")
        .execute(&pool)
        .await
        .expect("fixture thread should be inserted");
        sqlx::query(
            "INSERT INTO usage_provider_calls (provider_call_id, thread_id, provider, requested_model, actual_model_used, requested_service_tier, actual_service_tier, actual_service_tier_source, fast_mode_used, billing_surface, account_plan, started_at, input_tokens_uncached, input_tokens_cached, output_tokens, total_tokens, status) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind("old-main-error")
        .bind("old-main-thread")
        .bind("openai")
        .bind("gpt-5.6-luna")
        .bind("gpt-5.6-luna")
        .bind("default")
        .bind("default")
        .bind("runtime_contract")
        .bind(0_i64)
        .bind("chatgpt_credits")
        .bind("plus")
        .bind("2026-08-01T00:00:00Z")
        .bind(10_i64)
        .bind(2_i64)
        .bind(4_i64)
        .bind(16_i64)
        .bind("error")
        .execute(&pool)
        .await
        .expect("old-main usage call should be inserted");

        let migrator = migrator_for_usage_database(&pool, &runtime_usage_migrator())
            .await
            .expect("known old-main variants should be recognized");
        migrator
            .run(&pool)
            .await
            .expect("synthetic old-main views with newer ledger rows should upgrade");

        for version in [6_i64, 22, 23, 24, 25, 26] {
            let checksum = sqlx::query_scalar::<_, Vec<u8>>(
                "SELECT checksum FROM _sqlx_migrations WHERE version = ?",
            )
            .bind(version)
            .fetch_one(&pool)
            .await
            .expect("unknown newer migration history should be retained");
            assert_eq!(checksum, vec![version as u8; 32]);
        }

        let pricing = sqlx::query_as::<_, (String, Option<f64>, Option<String>)>(
            "SELECT pricing_status, estimated_total_credits, credit_source FROM usage_provider_call_credit_estimates WHERE provider_call_id = ?",
        )
        .bind("old-main-error")
        .fetch_one(&pool)
        .await
        .expect("upgraded usage estimate should be readable");
        assert_eq!(pricing, ("provider_usage_missing".to_string(), None, None));

        sqlx::query(
            "UPDATE usage_provider_calls SET provider_reported_credits = ? WHERE provider_call_id = ?",
        )
        .bind(7.5_f64)
        .bind("old-main-error")
        .execute(&pool)
        .await
        .expect("provider-reported credit should be added to the fixture");
        let provider_reported = sqlx::query_as::<_, (String, Option<f64>, Option<String>)>(
            "SELECT pricing_status, estimated_total_credits, credit_source FROM usage_provider_call_credit_estimates WHERE provider_call_id = ?",
        )
        .bind("old-main-error")
        .fetch_one(&pool)
        .await
        .expect("provider-reported estimate should be readable");
        assert_eq!(
            provider_reported,
            (
                "provider_reported".to_string(),
                Some(7.5),
                Some("provider_reported".to_string())
            )
        );

        for (version, expected) in [
            (1_i64, OLD_MAIN_0001_CHECKSUM),
            (5_i64, OLD_MAIN_0005_CHECKSUM),
        ] {
            let checksum = sqlx::query_scalar::<_, Vec<u8>>(
                "SELECT checksum FROM _sqlx_migrations WHERE version = ?",
            )
            .bind(version)
            .fetch_one(&pool)
            .await
            .expect("historical checksum should remain present");
            assert_eq!(checksum, expected);
        }
    }

    type LedgerRow = (i64, String, String, bool, Vec<u8>, i64);

    async fn ledger_rows(pool: &SqlitePool) -> Vec<LedgerRow> {
        sqlx::query_as(
            "SELECT version, description, CAST(installed_on AS TEXT), success, checksum, execution_time
             FROM _sqlx_migrations ORDER BY version",
        )
        .fetch_all(pool)
        .await
        .expect("migration history should be readable")
    }

    async fn test_sqlite() -> SqliteConfig {
        let codex_home = unique_temp_dir();
        tokio::fs::create_dir_all(&codex_home)
            .await
            .expect("create usage database directory");
        SqliteConfig::new_for_testing(codex_home.as_path().abs())
    }

    async fn shipped_generation(sqlite: &SqliteConfig) -> SqlitePool {
        let pool = sqlite
            .open_read_write_pool(&sqlite.usage_db_path())
            .await
            .expect("shipped usage database should open");
        let mut migrations = USAGE_MIGRATOR
            .iter()
            .filter(|migration| migration.version <= 19)
            .map(|migration| match migration.version {
                1 | 5 | 9 => shipped_main_migration(migration.version),
                _ => migration.clone(),
            })
            .collect::<Vec<_>>();
        // Recover the exact historical migration absent from the native embedded set.
        let lineage_sql = concat!(
            "-- Persist the unambiguous relationship kind and spawn request on each thread.\n",
            "-- Both fields are nullable so older ledgers remain readable and can be upgraded\n",
            "-- without reconstructing historical provenance.\n",
            "ALTER TABLE usage_threads ADD COLUMN lineage_edge_kind TEXT;\n",
            "ALTER TABLE usage_threads ADD COLUMN spawn_request_id TEXT;\n",
            "\n",
            "CREATE INDEX IF NOT EXISTS usage_threads_root_thread_idx\n",
            "ON usage_threads(root_thread_id);\n",
            "\n",
            "CREATE INDEX IF NOT EXISTS usage_threads_spawn_request_idx\n",
            "ON usage_threads(spawn_request_id);\n",
        );
        let first = USAGE_MIGRATOR
            .iter()
            .next()
            .expect("usage migration set should not be empty");
        let lineage = Migration::new(
            /*version*/ 6,
            "usage thread lineage telemetry".to_owned().into(),
            first.migration_type,
            AssertSqlSafe(lineage_sql.to_owned()).into_sql_str(),
            /*no_tx*/ false,
        );
        assert_eq!(
            lineage.checksum.as_ref(),
            &[
                0x58, 0xb4, 0x6b, 0x49, 0x7e, 0xf0, 0x5c, 0x8f, 0x21, 0x6b, 0xd7, 0x24, 0x53, 0xd1, 0xdb, 0x8d,
                0x2e, 0x24, 0xc8, 0x20, 0x86, 0xd8, 0xb1, 0xa8, 0x2f, 0xf8, 0x6a, 0x14, 0x26, 0xd4, 0x41, 0x62,
                0xd8, 0x45, 0xef, 0xee, 0x1f, 0xe3, 0x9f, 0xeb, 0x10, 0x2f, 0x75, 0xa2, 0x62, 0xaa, 0xbb, 0x0d,
            ]
        );
        migrations.push(lineage);
        migrations.sort_by_key(|migration| migration.version);
        clone_migrator(&runtime_usage_migrator(), migrations)
            .run(&pool)
            .await
            .expect("original shipped SQL should apply");
        let ledger = ledger_rows(&pool).await;
        assert_eq!(
            ledger.iter().map(|row| row.0).collect::<Vec<_>>(),
            (1_i64..=19).collect::<Vec<_>>()
        );
        for (version, checksum) in [
            (1_i64, OLD_MAIN_0001_CHECKSUM),
            (5_i64, OLD_MAIN_0005_CHECKSUM),
            (9_i64, ORIGINAL_0009_CHECKSUM),
        ] {
            assert_eq!(
                ledger
                    .iter()
                    .find(|row| row.0 == version)
                    .expect("original migration should be recorded")
                    .4
                    .as_slice(),
                checksum
            );
        }
        raw_sql(
            "INSERT INTO usage_threads
                 (thread_id, root_thread_id, source, lineage_edge_kind, spawn_request_id)
             VALUES ('shipped-thread', 'shipped-thread', 'cli', 'fork', 'shipped-spawn');
             INSERT INTO usage_automatic_turn_eligibility
                 (thread_id, trigger_turn_id, generation, next_attempt, max_attempts,
                  capability, event_occurrence_id, connection_principal)
             VALUES ('shipped-thread', 'trigger', 0, 1, 2,
                     'fixture-capability', 'event', 'fixture-principal');
             INSERT INTO usage_automatic_turns
                 (thread_id, client_user_message_id, trigger_turn_id, turn_id, origin,
                  generation, capability, attempt, max_attempts, provenance_source,
                  event_occurrence_id, connection_principal)
             VALUES ('shipped-thread', 'message', 'trigger', 'turn', 'fixture',
                     0, 'fixture-capability', 1, 2, 'server', 'event', 'fixture-principal');",
        )
        .execute(&pool)
        .await
        .expect("shipped data should be stored under the original column names");
        pool
    }

    async fn principal_schema(pool: &SqlitePool) -> Vec<(String, String)> {
        sqlx::query_as(
            "SELECT name, sql FROM sqlite_master
             WHERE type = 'table'
               AND name IN ('usage_automatic_turn_eligibility', 'usage_automatic_turns')
             ORDER BY name",
        )
        .fetch_all(pool)
        .await
        .expect("principal table definitions should be readable")
    }

    async fn assert_shipped_data(pool: &SqlitePool) {
        let principals = sqlx::query_scalar::<_, String>(
            "SELECT connection_principal FROM usage_automatic_turn_eligibility
             UNION ALL SELECT connection_principal FROM usage_automatic_turns",
        )
        .fetch_all(pool)
        .await
        .expect("original principal values should remain readable");
        assert_eq!(principals, vec!["fixture-principal"; 2]);
        let lineage = sqlx::query_as::<_, (String, String)>(
            "SELECT lineage_edge_kind, spawn_request_id FROM usage_threads
             WHERE thread_id = 'shipped-thread'",
        )
        .fetch_one(pool)
        .await
        .expect("historical lineage data should remain readable");
        assert_eq!(lineage, ("fork".to_owned(), "shipped-spawn".to_owned()));
        let renamed_columns = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM pragma_table_xinfo('usage_automatic_turn_eligibility')
             WHERE name = 'principal_identity'",
        )
        .fetch_one(pool)
        .await
        .expect("original schema should be inspectable");
        assert_eq!(renamed_columns, 0);
    }

    #[tokio::test]
    async fn runtime_opens_shipped_generation_and_preserves_data_and_history_on_reopen() {
        let sqlite = test_sqlite().await;
        let pool = shipped_generation(&sqlite).await;
        let original = ledger_rows(&pool).await;
        pool.close().await;
        let mut upgraded_ledger = None;
        for iteration in 0..3 {
            let runtime = StateRuntime::init(sqlite.clone(), "openai".to_owned())
                .await
                .expect("native runtime should open the original shipped generation");
            let pool = runtime.usage_pool();
            let ledger = ledger_rows(&pool).await;
            assert_eq!(&ledger[..original.len()], original.as_slice());
            assert_eq!(
                ledger[original.len()..]
                    .iter()
                    .map(|row| row.0)
                    .collect::<Vec<_>>(),
                vec![20, 21]
            );
            if let Some(expected) = &upgraded_ledger {
                assert_eq!(&ledger, expected);
            } else {
                upgraded_ledger = Some(ledger);
            }
            assert_shipped_data(&pool).await;
            if iteration == 0 {
                runtime
                    .record_usage_thread(&UsageThreadRecord {
                        thread_id: "shipped-thread".to_owned(),
                        parent_thread_id: None,
                        root_thread_id: Some("shipped-thread".to_owned()),
                        fork_parent_thread_id: None,
                        source: Some("cli".to_owned()),
                    })
                    .await
                    .expect("native thread usage API should preserve the historical row");
                let record = ProviderCallUsageRecord {
                    provider_call_id: "fixture-call".to_owned(),
                    thread_id: "shipped-thread".to_owned(),
                    turn_id: "fixture-turn".to_owned(),
                    provider: "openai".to_owned(),
                    provider_account_scope: None,
                    requested_model: "gpt-5.6-luna".to_owned(),
                    actual_model_used: Some("gpt-5.6-luna".to_owned()),
                    response_id: "fixture-response".to_owned(),
                    requested_service_tier: None,
                    actual_service_tier: None,
                    actual_service_tier_source: None,
                    fast_mode_requested: None,
                    fast_mode_used: None,
                    billing_surface: None,
                    account_plan: None,
                    started_at: "2026-01-01T00:00:00Z".to_owned(),
                    completed_at: "2026-01-01T00:00:01Z".to_owned(),
                    input_tokens_uncached: Some(10),
                    input_tokens_cached: Some(2),
                    input_tokens_cache_write: Some(0),
                    output_tokens: Some(4),
                    total_tokens: Some(16),
                    status: "ok",
                };
                assert_eq!(
                    runtime
                        .record_provider_call_usage(&record)
                        .await
                        .expect("native provider usage API should write"),
                    ProviderCallUsageWriteOutcome::Inserted
                );
                assert_eq!(
                    runtime
                        .record_provider_call_usage(&record)
                        .await
                        .expect("native provider usage API should deduplicate"),
                    ProviderCallUsageWriteOutcome::Duplicate
                );
            }
            let calls = sqlx::query_as::<_, (String, i64)>(
                "SELECT provider_call_id, total_tokens FROM usage_provider_calls
                 WHERE request_id = 'fixture-response'",
            )
            .fetch_all(&*pool)
            .await
            .expect("native-written usage should remain queryable");
            assert_eq!(calls, vec![("fixture-call".to_owned(), 16)]);
            let estimates = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM usage_provider_call_credit_estimates
                 WHERE provider_call_id = 'fixture-call'",
            )
            .fetch_one(&*pool)
            .await
            .expect("current credit view should query the native-written row");
            assert_eq!(estimates, 1);
            assert_shipped_data(&pool).await;
            runtime.close().await;
        }
    }

    #[tokio::test]
    async fn shipped_generation_unknown_checksums_preserve_pending_migrations_and_data() {
        for version in [1_i64, 5, 9, 19] {
            let sqlite = test_sqlite().await;
            let pool = shipped_generation(&sqlite).await;
            sqlx::query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = ?")
                .bind(vec![0x5a; 48])
                .bind(version)
                .execute(&pool)
                .await
                .expect("unknown checksum fixture should be stored");
            let before = ledger_rows(&pool).await;
            let schema = principal_schema(&pool).await;
            pool.close().await;
            let error = sqlite
                .open_usage_db(&runtime_usage_migrator(), /*telemetry_override*/ None)
                .await
                .expect_err("unknown checksums must reject the shipped generation");
            let migration_error = error
                .chain()
                .find_map(|source| source.downcast_ref::<MigrateError>());
            assert!(matches!(
                migration_error,
                Some(MigrateError::VersionMismatch(rejected)) if *rejected == version
            ));
            let preserved = sqlite
                .open_read_only_pool(&sqlite.usage_db_path(), /*busy_timeout*/ None)
                .await
                .expect("rejected usage should remain readable");
            assert_eq!(ledger_rows(&preserved).await, before);
            assert_eq!(principal_schema(&preserved).await, schema);
            assert_shipped_data(&preserved).await;
            preserved.close().await;
        }
    }

    #[tokio::test]
    async fn shipped_generation_unknown_principal_schema_rejects_before_migration() {
        for mutation in [
            "ALTER TABLE usage_automatic_turn_eligibility
             ADD COLUMN principal_identity TEXT",
            "ALTER TABLE usage_automatic_turns ADD COLUMN principal_identity TEXT",
            "ALTER TABLE usage_automatic_turn_eligibility
             RENAME COLUMN connection_principal TO unexpected_principal",
            "ALTER TABLE usage_automatic_turns
             RENAME COLUMN connection_principal TO unexpected_principal",
            "ALTER TABLE usage_automatic_turns
             RENAME COLUMN connection_principal TO unexpected_principal;
             ALTER TABLE usage_automatic_turns ADD COLUMN connection_principal INTEGER",
            "ALTER TABLE usage_automatic_turn_eligibility RENAME TO fixture_eligibility;
             CREATE VIEW usage_automatic_turn_eligibility
             AS SELECT * FROM fixture_eligibility",
        ] {
            let sqlite = test_sqlite().await;
            let pool = shipped_generation(&sqlite).await;
            raw_sql(mutation)
                .execute(&pool)
                .await
                .expect("unknown principal schema fixture should apply");
            let before = ledger_rows(&pool).await;
            let schema = principal_schema(&pool).await;
            pool.close().await;
            let error = sqlite
                .open_usage_db(&runtime_usage_migrator(), /*telemetry_override*/ None)
                .await
                .expect_err("unknown principal schema must be rejected");
            let message = format!("{error:#}");
            assert!(message.contains("unsupported original usage migration 9 principal"));
            let preserved = sqlite
                .open_read_only_pool(&sqlite.usage_db_path(), /*busy_timeout*/ None)
                .await
                .expect("rejected usage should remain readable");
            assert_eq!(ledger_rows(&preserved).await, before);
            assert_eq!(principal_schema(&preserved).await, schema);
            preserved.close().await;
        }
    }

    #[tokio::test]
    async fn runtime_usage_open_preserves_canonical_and_historical_ledgers_on_reopen() {
        for historical in [false, true] {
            let sqlite = test_sqlite().await;
            let migrator = runtime_usage_migrator();
            let pool = sqlite
                .open_usage_db(&migrator, /*telemetry_override*/ None)
                .await
                .expect("canonical usage database should initialize");
            let canonical = ledger_rows(&pool).await;
            for version in [1_i64, 5] {
                let recorded = canonical
                    .iter()
                    .find(|row| row.0 == version)
                    .expect("canonical migration should be recorded");
                let expected = USAGE_MIGRATOR
                    .iter()
                    .find(|migration| migration.version == version)
                    .expect("canonical migration should be embedded");
                assert_eq!(recorded.4.as_slice(), expected.checksum.as_ref());
            }

            if historical {
                for (version, checksum) in [
                    (1_i64, OLD_MAIN_0001_CHECKSUM),
                    (5_i64, OLD_MAIN_0005_CHECKSUM),
                ] {
                    sqlx::query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = ?")
                        .bind(checksum)
                        .bind(version)
                        .execute(&pool)
                        .await
                        .expect("known historical checksum fixture should be recorded");
                }
            }
            let before = ledger_rows(&pool).await;
            pool.close().await;

            for _ in 0..2 {
                let reopened = sqlite
                    .open_usage_db(&migrator, /*telemetry_override*/ None)
                    .await
                    .expect("recognized usage database should reopen");
                assert_eq!(ledger_rows(&reopened).await, before);
                reopened.close().await;
            }
        }
    }

    #[tokio::test]
    async fn runtime_usage_open_rejects_unknown_checksums_without_rewriting_history() {
        for version in [1_i64, 5] {
            let sqlite = test_sqlite().await;
            let migrator = runtime_usage_migrator();
            let pool = sqlite
                .open_usage_db(&migrator, /*telemetry_override*/ None)
                .await
                .expect("canonical usage database should initialize");
            let unknown = vec![0x5a; 48];
            sqlx::query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = ?")
                .bind(&unknown)
                .bind(version)
                .execute(&pool)
                .await
                .expect("unknown checksum fixture should be recorded");
            let before = ledger_rows(&pool).await;
            pool.close().await;

            let error = sqlite
                .open_usage_db(&migrator, /*telemetry_override*/ None)
                .await
                .expect_err("unknown migration checksum must be rejected");
            let migration_error = error
                .chain()
                .find_map(|source| source.downcast_ref::<MigrateError>());
            assert!(matches!(
                migration_error,
                Some(MigrateError::VersionMismatch(rejected)) if *rejected == version
            ));
            let preserved = sqlite
                .open_read_only_pool(&sqlite.usage_db_path(), /*busy_timeout*/ None)
                .await
                .expect("rejected database should remain readable");
            assert_eq!(ledger_rows(&preserved).await, before);
            preserved.close().await;
        }
    }
}
