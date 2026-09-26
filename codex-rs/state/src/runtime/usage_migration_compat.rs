use anyhow::Context;
use sqlx::QueryBuilder;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use sqlx::migrate::Migration;
use sqlx::migrate::Migrator;
use std::borrow::Cow;

// These are the checksums recorded by the shipped old-main usage database.
// They are intentionally exact: this is a compatibility map for two known
// historical variants, not a general SQLx checksum bypass.
const OLD_MAIN_0001_CHECKSUM: &[u8] = &[
    0xbc, 0x75, 0x42, 0xf2, 0x22, 0xc1, 0xa7, 0x8d, 0x2b, 0x18, 0xee, 0x26, 0xf0, 0x03, 0x79, 0xd9,
    0xde, 0x56, 0x32, 0x4c, 0xd6, 0xbb, 0x80, 0x29, 0x2e, 0x9c, 0x49, 0x8a, 0x67, 0xb9, 0xda, 0x5c,
];
const OLD_MAIN_0005_CHECKSUM: &[u8] = &[
    0x7a, 0x63, 0xe3, 0xcb, 0xca, 0x4d, 0x27, 0x51, 0x23, 0x18, 0xd4, 0xb0, 0x87, 0x4c, 0x64, 0xb9,
    0x9b, 0x86, 0x7c, 0x7e, 0x72, 0x5c, 0xb1, 0xde, 0x41, 0xa1, 0xb3, 0x4e, 0xa7, 0x59, 0xe9, 0x0c,
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
    use crate::migrations::USAGE_MIGRATOR;
    use crate::migrations::runtime_usage_migrator;
    use sqlx::SqlitePool;
    use sqlx::raw_sql;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn test_pool() -> SqlitePool {
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory usage database should open")
    }
    #[tokio::test]
    async fn preserves_known_old_main_checksums_in_memory() {
        let pool = test_pool().await;
        sqlx::query(
            "CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY, checksum BLOB NOT NULL)",
        )
        .execute(&pool)
        .await
        .expect("migration metadata table should be created");
        sqlx::query("INSERT INTO _sqlx_migrations (version, checksum) VALUES (?, ?), (?, ?)")
            .bind(1_i64)
            .bind(OLD_MAIN_0001_CHECKSUM)
            .bind(5_i64)
            .bind(OLD_MAIN_0005_CHECKSUM)
            .execute(&pool)
            .await
            .expect("known historical checksums should be inserted");

        let migrator = migrator_for_usage_database(&pool, &USAGE_MIGRATOR)
            .await
            .expect("known variants should be accepted");
        for (version, expected) in [
            (1_i64, OLD_MAIN_0001_CHECKSUM),
            (5_i64, OLD_MAIN_0005_CHECKSUM),
        ] {
            let migration = migrator
                .iter()
                .find(|migration| migration.version == version)
                .expect("migration should be embedded");
            assert_eq!(migration.checksum.as_ref(), expected);
        }
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
    async fn leaves_unknown_checksums_for_sqlx_to_reject() {
        let pool = test_pool().await;
        sqlx::query(
            "CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY, checksum BLOB NOT NULL)",
        )
        .execute(&pool)
        .await
        .expect("migration metadata table should be created");
        let unknown = vec![0x5a; 32];
        sqlx::query("INSERT INTO _sqlx_migrations (version, checksum) VALUES (?, ?)")
            .bind(5_i64)
            .bind(&unknown)
            .execute(&pool)
            .await
            .expect("unknown checksum should be inserted");

        let migrator = migrator_for_usage_database(&pool, &USAGE_MIGRATOR)
            .await
            .expect("classification should not itself fail");
        let migration = migrator
            .iter()
            .find(|migration| migration.version == 5)
            .expect("migration should be embedded");
        assert_ne!(migration.checksum.as_ref(), unknown.as_slice());
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
        for version in [6_i64, 8, 9, 10, 11, 12] {
            sqlx::query(
                "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (?, ?, 1, ?, 0)",
            )
            .bind(version)
            .bind(format!("old-main-usage-{version}"))
            .bind(vec![version as u8; 32])
            .execute(&pool)
            .await
            .expect("unknown old-main migration should be retained");
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
            .expect("old-main usage history should upgrade");

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
}
