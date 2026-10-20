use anyhow::Context;
use sqlx::QueryBuilder;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use sqlx::migrate::Migration;
use sqlx::migrate::Migrator;
use std::borrow::Cow;

// These are the SHA-384 checksums recorded by the shipped old-main usage
// database. They are intentionally exact: this is a compatibility map for
// two known historical variants, not a general SQLx checksum bypass.
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
];

/// Build a usage migrator whose expected checksums include only the exact
/// historical variants that are already recorded in the database.
///
/// SQLx validates every known migration checksum. P7's 0001 and 0005 differ
/// from the shipped old-main files, so a canonical migrator would reject an
/// otherwise usable database before the additive compatibility migration can
/// restore the status-gated views. We keep the historical checksum in
/// `_sqlx_migrations` and only adjust the in-memory expected checksum for a
/// known old-main variant. Any other mismatch is left for SQLx to reject.
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
    use super::migrator_for_usage_database;
    use crate::SqliteConfig;
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
