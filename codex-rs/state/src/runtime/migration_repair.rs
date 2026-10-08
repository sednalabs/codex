//! Guarded state-ledger compatibility for the upstream-rooted migration cut.
//!
//! This adapter is needed while databases created by the fork can be opened by
//! this binary. Retire it only after the supported-profile upgrade contract
//! explicitly excludes every old ledger lineage and hosted upgrade/reopen
//! evidence proves that exclusion safe.

use anyhow::Context;
use sqlx::Row;
use sqlx::SqliteConnection;
use sqlx::SqlitePool;
use sqlx::migrate::Migration;
use sqlx::migrate::Migrator;
use std::collections::BTreeSet;

const FORK_BASE: i64 = 8_000_000_000_000_000;
const FORK_56: i64 = FORK_BASE;
const FORK_57: i64 = FORK_BASE + 1;
const FORK_58: i64 = FORK_BASE + 2;
const FORK_9000: i64 = FORK_BASE + 3;
const FORK_9001: i64 = FORK_BASE + 4;
const FORK_9002: i64 = FORK_BASE + 5;
const FORK_9003: i64 = FORK_BASE + 6;
const LEGACY_FORK_IDS: &[(i64, i64)] = &[
    (56, FORK_56),
    (57, FORK_57),
    (58, FORK_58),
    (9000, FORK_9000),
    (9001, FORK_9001),
    (9002, FORK_9002),
    (9003, FORK_9003),
];

// These are the exact deployed source identities, including the older shifted
// 24-50 sequence. The SQL bytes are supplied by the embedded canonical target.
const SHIFTED: &[(i64, &str, i64)] = &[
    (24, "phase2 attestation roots", FORK_9000),
    (25, "remote control enrollments", 24),
    (26, "thread timestamps millis", 25),
    (27, "thread dynamic tools persist on resume", FORK_9001),
    (28, "thread dynamic tools capability json", FORK_9002),
    (29, "thread dynamic tools namespace", 26),
    (30, "threads cwd sort indexes", 27),
    (31, "device key bindings", 28),
    (32, "thread goals", 29),
    (33, "threads thread source", 30),
    (34, "drop device key bindings", 31),
    (35, "threads preview", 32),
    (36, "thread goal stopped statuses", 33),
    (37, "drop thread goals", 34),
    (38, "phase2 attested baselines", FORK_56),
    (39, "drop memory tables", 35),
    (40, "threads history mode", 40),
    (41, "threads name", 41),
    (42, "drop agent jobs", 42),
    (43, "threads recency at", 39),
    (44, "threads visible sort indexes", 36),
    (45, "threads configured identity provenance", FORK_57),
    (46, "remote control enrollments enabled", 37),
    (47, "external agent config imports", 38),
    (48, "threads is pinned", 43),
    (49, "external agent config imports provider id", 44),
    (50, "backfill thread spawn edges", FORK_9003),
];

#[derive(Clone, Debug)]
struct RowIdentity {
    source: i64,
    target: i64,
    description: String,
    checksum: Vec<u8>,
    execution_time: i64,
    shifted: bool,
}

/// Run immediately before STATE_MIGRATOR::run, never against a live profile as
/// an ad-hoc command. BEGIN IMMEDIATE serializes classification and rekeying
/// with other SQLite writers; every rejection rolls back the whole bridge.
pub(crate) async fn bridge_state_migrations(
    pool: &SqlitePool,
    migrator: &Migrator,
) -> anyhow::Result<()> {
    bridge_transaction(pool, migrator, std::future::ready(())).await
}

// The per-invocation completion future is ready in production. Tests suspend
// here after real bridge writes to exercise cancellation of this transaction.
async fn bridge_transaction(
    pool: &SqlitePool,
    migrator: &Migrator,
    after_apply: impl std::future::Future<Output = ()>,
) -> anyhow::Result<()> {
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    match bridge_locked(&mut transaction, migrator).await {
        Ok(()) => {
            after_apply.await;
            transaction.commit().await.context(
                "state migration bridge commit failed; completion is uncertain and SQLx queues rollback on drop",
            )
        }
        Err(error) => {
            if let Err(rollback_error) = transaction.rollback().await {
                return Err(error.context(format!(
                    "state migration bridge rollback failed: {rollback_error}; transaction outcome is uncertain"
                )));
            }
            Err(error)
        }
    }
}

/// Run the guarded compatibility bridge before the full state migrator.
///
/// State database opens and ordinary full-state fixtures must share this
/// sequence so the SQLx migrator never bypasses the ledger/schema classifier.
pub(crate) async fn run_state_migrations(
    pool: &SqlitePool,
    migrator: &Migrator,
) -> anyhow::Result<()> {
    bridge_state_migrations(pool, migrator).await?;
    migrator.run(pool).await.map_err(anyhow::Error::from)
}

async fn bridge_locked(
    connection: &mut SqliteConnection,
    migrator: &Migrator,
) -> anyhow::Result<()> {
    validate_embedded_namespace(migrator)?;
    let ledger_exists = table_exists(connection, "_sqlx_migrations").await?;
    if ledger_exists {
        for column in [
            "version",
            "description",
            "installed_on",
            "success",
            "checksum",
            "execution_time",
        ] {
            if !column_exists(connection, "_sqlx_migrations", column).await? {
                anyhow::bail!("state migration ledger is missing column {column}");
            }
        }
    }
    let rows = if ledger_exists {
        sqlx::query(
            "SELECT version, description, success, checksum, execution_time FROM _sqlx_migrations ORDER BY version",
        )
        .fetch_all(&mut *connection)
        .await?
    } else {
        Vec::new()
    };
    let mut identities = Vec::with_capacity(rows.len());
    let mut old_fork_low = false;
    let mut upstream_56_58 = false;
    for row in rows {
        let version: i64 = row.try_get("version")?;
        let description: String = row.try_get("description")?;
        let success: bool = row.try_get("success")?;
        let checksum: Vec<u8> = row.try_get("checksum")?;
        let execution_time: i64 = row.try_get("execution_time")?;
        if !success {
            anyhow::bail!("state migration {version} failed; refusing compatibility bridge");
        }
        let (target, shifted) = classify_row(migrator, version, &description, &checksum)?;
        old_fork_low |= (56..=58).contains(&version) && target >= FORK_BASE;
        upstream_56_58 |= (56..=58).contains(&version) && target == version;
        identities.push(RowIdentity {
            source: version,
            target,
            description,
            checksum,
            execution_time,
            shifted,
        });
    }
    if old_fork_low && upstream_56_58 {
        anyhow::bail!("mixed upstream and unrekeyed fork 56-58 history");
    }
    validate_receipts(connection, migrator, &identities).await?;
    let deployed_thread_source = column_exists(connection, "threads", "thread_source").await?;
    validate_prefixes(&identities, deployed_thread_source)?;
    validate_schema(connection, &identities).await?;

    // All ledger and schema checks above are read-only. Mutations below are
    // one SQLite transaction; SQLx sees only canonical checksums on return.
    apply_bridge(connection, migrator, ledger_exists, &identities).await
}

fn embedded(migrator: &Migrator, version: i64) -> anyhow::Result<&Migration> {
    migrator
        .iter()
        .find(|migration| migration.version == version)
        .with_context(|| format!("embedded state migration {version} is missing"))
}

fn classify_row(
    migrator: &Migrator,
    source: i64,
    description: &str,
    checksum: &[u8],
) -> anyhow::Result<(i64, bool)> {
    if let Some(migration) = migrator
        .iter()
        .find(|migration| migration.version == source)
        && description == migration.description.as_ref()
        && checksum == migration.checksum.as_ref()
    {
        return Ok((source, false));
    }
    let historical_target = LEGACY_FORK_IDS
        .iter()
        .find(|(version, _)| *version == source)
        .map(|(_, target)| *target);
    if let Some(target) = historical_target {
        let migration = embedded(migrator, target)?;
        if description == migration.description.as_ref() && checksum == migration.checksum.as_ref()
        {
            return Ok((target, false));
        }
    }
    // Upstream's prior one-off recency repair used version 38 for the SQL
    // currently embedded as 39. Fold it into this guarded transaction.
    if source == 38 {
        let migration = embedded(migrator, 39)?;
        if description == migration.description.as_ref() && checksum == migration.checksum.as_ref()
        {
            return Ok((39, false));
        }
    }
    if let Some((_, expected_description, target)) =
        SHIFTED.iter().find(|(version, _, _)| *version == source)
    {
        let migration = embedded(migrator, *target)?;
        if description == *expected_description && checksum == migration.checksum.as_ref() {
            return Ok((*target, true));
        }
    }
    anyhow::bail!("state migration {source} has an unknown identity; refusing compatibility bridge")
}

fn validate_embedded_namespace(migrator: &Migrator) -> anyhow::Result<()> {
    let versions = migrator
        .iter()
        .map(|migration| migration.version)
        .collect::<BTreeSet<_>>();
    if versions.len() != migrator.iter().count() {
        anyhow::bail!("duplicate embedded state migration versions");
    }
    for version in 1..=60 {
        embedded(migrator, version)?;
    }
    for version in FORK_BASE..=FORK_9003 {
        embedded(migrator, version)?;
    }
    if migrator
        .iter()
        .any(|migration| migration.version > FORK_9003)
    {
        anyhow::bail!("state migration uses an unreviewed fork namespace version");
    }
    Ok(())
}

async fn validate_receipts(
    connection: &mut SqliteConnection,
    migrator: &Migrator,
    rows: &[RowIdentity],
) -> anyhow::Result<()> {
    if !table_exists(connection, "state_migration_rekey_receipts").await? {
        return Ok(());
    }
    for column in [
        "old_version",
        "new_version",
        "old_description",
        "checksum",
        "execution_time",
    ] {
        if !column_exists(connection, "state_migration_rekey_receipts", column).await? {
            anyhow::bail!("state migration rekey receipt is missing column {column}");
        }
    }
    let receipts = sqlx::query(
        "SELECT old_version, new_version, old_description, checksum
         FROM state_migration_rekey_receipts ORDER BY old_version",
    )
    .fetch_all(&mut *connection)
    .await?;
    let present = rows.iter().map(|row| row.target).collect::<BTreeSet<_>>();
    for receipt in receipts {
        let old_version: i64 = receipt.try_get("old_version")?;
        let new_version: i64 = receipt.try_get("new_version")?;
        let old_description: String = receipt.try_get("old_description")?;
        let checksum: Vec<u8> = receipt.try_get("checksum")?;
        let (target, _) = classify_row(migrator, old_version, &old_description, &checksum)?;
        if target != new_version || !present.contains(&new_version) {
            anyhow::bail!("state migration rekey receipt {old_version} is inconsistent");
        }
    }
    Ok(())
}

fn validate_prefixes(rows: &[RowIdentity], deployed_thread_source: bool) -> anyhow::Result<()> {
    let targets = rows.iter().map(|row| row.target).collect::<BTreeSet<_>>();
    let alias_only = targets == BTreeSet::from([FORK_9001, FORK_9002]);
    let last_early = if rows
        .iter()
        .any(|row| row.source > 23 && ![FORK_9001, FORK_9002].contains(&row.target))
    {
        if alias_only { 0 } else { 23 }
    } else {
        rows.iter()
            .filter(|row| row.source <= 23)
            .map(|row| row.source)
            .max()
            .unwrap_or(0)
    };
    for version in 1..=last_early {
        if !targets.contains(&version) {
            anyhow::bail!("state migration history has an early gap at {version}");
        }
    }
    for target in &targets {
        let matching = rows
            .iter()
            .filter(|row| row.target == *target)
            .collect::<Vec<_>>();
        if matching.len() > 1
            && matching
                .iter()
                .any(|row| row.checksum != matching[0].checksum)
        {
            anyhow::bail!("state migration {target} has conflicting historical rows");
        }
    }
    let last_shifted = rows
        .iter()
        .filter(|row| row.shifted)
        .map(|row| row.source)
        .max();
    if let Some(last_shifted) = last_shifted {
        for (source, _, target) in SHIFTED
            .iter()
            .filter(|(source, _, _)| *source <= last_shifted)
        {
            if !targets.contains(target) {
                anyhow::bail!(
                    "shifted state history through {last_shifted} is missing source {source}"
                );
            }
        }
    }
    // A partial shifted history can still lack canonical targets whose old
    // source IDs have not run. Its immutable mapping ends at canonical 44.
    // Any later canonical progress requires independent predecessor closure;
    // a shifted row must not hide gaps in an appended canonical tail.
    let shifted_core_limit = SHIFTED
        .iter()
        .map(|(_, _, target)| *target)
        .filter(|target| *target < FORK_BASE)
        .max()
        .unwrap_or(0);
    if let Some(last_core) = targets.iter().filter(|version| **version <= 60).max()
        && (last_shifted.is_none() || *last_core > shifted_core_limit)
    {
        let legacy_recency = rows.iter().any(|row| row.source == 38 && row.target == 39);
        for version in 1..=*last_core {
            if !(targets.contains(&version)
                || legacy_recency && version == 38
                || deployed_thread_source && version == 30)
            {
                anyhow::bail!("state migration history has a gap at {version}");
            }
        }
    }
    let alias_1 = targets.contains(&FORK_9001);
    let alias_2 = targets.contains(&FORK_9002);
    if alias_1 != alias_2 {
        anyhow::bail!("dynamic-tool alias pair is incomplete");
    }
    if targets.contains(&FORK_57) && !targets.contains(&FORK_56)
        || targets.contains(&FORK_58)
            && !targets.contains(&FORK_57)
            && (!alias_1
                || rows
                    .iter()
                    .any(|row| row.source == 58 && row.target == FORK_58))
    {
        anyhow::bail!("fork state migration 56-58 history has a gap");
    }
    Ok(())
}

async fn table_exists(connection: &mut SqliteConnection, name: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?)",
    )
    .bind(name)
    .fetch_one(&mut *connection)
    .await?)
}

async fn column_exists(
    connection: &mut SqliteConnection,
    table: &str,
    column: &str,
) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?) WHERE name = ?)",
    )
    .bind(table)
    .bind(column)
    .fetch_one(&mut *connection)
    .await?)
}

async fn index_exists(connection: &mut SqliteConnection, name: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'index' AND name = ?)",
    )
    .bind(name)
    .fetch_one(&mut *connection)
    .await?)
}

async fn schema_object_exists(
    connection: &mut SqliteConnection,
    name: &str,
) -> anyhow::Result<bool> {
    Ok(
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name = ?)")
            .bind(name)
            .fetch_one(&mut *connection)
            .await?,
    )
}

async fn validate_attachment_and_feedback_effects(
    connection: &mut SqliteConnection,
    applied: &BTreeSet<i64>,
) -> anyhow::Result<()> {
    let attachments = schema_object_exists(connection, "thread_attachments").await?;
    if applied.contains(&55) != attachments {
        anyhow::bail!("thread attachments table disagrees with migration 55");
    }
    if attachments {
        if !table_exists(connection, "thread_attachments").await?
            || schema_object_exists(connection, "thread_artifacts").await?
            || column_exists(connection, "thread_attachments", "artifact_type").await?
        {
            anyhow::bail!("thread attachments rename disagrees with migration 55");
        }
        for column in ["thread_id", "attachment_type", "identity_key"] {
            if !column_exists(connection, "thread_attachments", column).await? {
                anyhow::bail!("thread attachments migration 55 is missing column {column}");
            }
        }
    }
    let reverse_index = "idx_thread_attachments_identity_thread";
    if applied.contains(&59) != schema_object_exists(connection, reverse_index).await? {
        anyhow::bail!("attachment reverse index disagrees with migration 59");
    }
    if applied.contains(&59) {
        let owner = sqlx::query_scalar::<_, String>(
            "SELECT tbl_name FROM sqlite_schema WHERE type = 'index' AND name = ?",
        )
        .bind(reverse_index)
        .fetch_optional(&mut *connection)
        .await?;
        let columns = sqlx::query_scalar::<_, Option<String>>(
            "SELECT name FROM pragma_index_info(?) ORDER BY seqno",
        )
        .bind(reverse_index)
        .fetch_all(&mut *connection)
        .await?;
        let flags = sqlx::query(
            "SELECT \"unique\", partial FROM pragma_index_list('thread_attachments') WHERE name = ?",
        )
        .bind(reverse_index)
        .fetch_optional(&mut *connection)
        .await?;
        let flags_match = flags.is_some_and(|row| {
            row.get::<i64, _>("unique") == 0 && row.get::<i64, _>("partial") == 0
        });
        let expected =
            ["attachment_type", "identity_key", "thread_id"].map(|name| Some(name.to_string()));
        if owner.as_deref() != Some("thread_attachments") || columns != expected || !flags_match {
            anyhow::bail!("attachment reverse index shape disagrees with migration 59");
        }
    }
    let feedback = schema_object_exists(connection, "guardian_review_feedback").await?;
    if applied.contains(&60) != feedback {
        anyhow::bail!("guardian review feedback table disagrees with migration 60");
    }
    if feedback {
        if !table_exists(connection, "guardian_review_feedback").await? {
            anyhow::bail!("guardian review feedback is not the migration 60 table");
        }
        let columns = sqlx::query(
            "SELECT name, type, \"notnull\", dflt_value, pk
             FROM pragma_table_info('guardian_review_feedback') ORDER BY cid",
        )
        .fetch_all(&mut *connection)
        .await?
        .into_iter()
        .map(|row| {
            (
                row.get::<String, _>("name"),
                row.get::<String, _>("type"),
                row.get::<i64, _>("notnull"),
                row.get::<Option<String>, _>("dflt_value"),
                row.get::<i64, _>("pk"),
            )
        })
        .collect::<Vec<_>>();
        let expected = vec![
            ("id".to_string(), "TEXT".to_string(), 1, None, 1),
            ("thread_id".to_string(), "TEXT".to_string(), 1, None, 0),
            ("record".to_string(), "BLOB".to_string(), 1, None, 0),
        ];
        let foreign_keys = sqlx::query(
            "SELECT \"table\", \"from\", \"to\", on_delete
             FROM pragma_foreign_key_list('guardian_review_feedback') ORDER BY id, seq",
        )
        .fetch_all(&mut *connection)
        .await?;
        let foreign_key_matches = foreign_keys.len() == 1
            && foreign_keys[0].get::<String, _>("table") == "threads"
            && foreign_keys[0].get::<String, _>("from") == "thread_id"
            && foreign_keys[0].get::<String, _>("to") == "id"
            && foreign_keys[0].get::<String, _>("on_delete") == "CASCADE";
        if columns != expected || !foreign_key_matches {
            anyhow::bail!("guardian review feedback shape disagrees with migration 60");
        }
    }
    Ok(())
}

async fn validate_schema(
    connection: &mut SqliteConnection,
    rows: &[RowIdentity],
) -> anyhow::Result<()> {
    let applied = rows.iter().map(|row| row.target).collect::<BTreeSet<_>>();
    // Reservations are valid on every recognized early prefix, but never on
    // an untracked schema or a prefix missing its durable core tables.
    for (version, table) in [(1, "threads"), (4, "thread_dynamic_tools")] {
        let exists = table_exists(connection, table).await?;
        if applied.contains(&version) != exists
            || schema_object_exists(connection, table).await? != exists
        {
            anyhow::bail!("state migration {version} disagrees with table {table}");
        }
    }
    validate_attachment_and_feedback_effects(connection, &applied).await?;
    if applied == BTreeSet::from([FORK_9001, FORK_9002])
        && (table_exists(connection, "threads").await?
            || table_exists(connection, "thread_dynamic_tools").await?)
    {
        anyhow::bail!("alias-only fresh ledger disagrees with existing state schema");
    }
    for (version, table) in [
        (FORK_56, "phase2_attested_baselines"),
        (FORK_9000, "phase2_attestation_roots"),
    ] {
        if applied.contains(&version) != table_exists(connection, table).await? {
            anyhow::bail!("state migration {version} disagrees with table {table}");
        }
    }
    let configured_provenance =
        column_exists(connection, "threads", "configured_identity_provenance").await?;
    if applied.contains(&FORK_57) != configured_provenance {
        anyhow::bail!("configured identity provenance column disagrees with its migration");
    }
    let creator_user = column_exists(connection, "threads", "creator_user_id").await?;
    let creator_account = column_exists(connection, "threads", "creator_account_id").await?;
    if creator_user != creator_account || applied.contains(&56) != creator_user {
        anyhow::bail!("upstream creator identity columns disagree with migration 56");
    }
    let recency = column_exists(connection, "threads", "recency_at").await?;
    let recency_ms = column_exists(connection, "threads", "recency_at_ms").await?;
    if recency != recency_ms || applied.contains(&39) != recency {
        anyhow::bail!("thread recency columns disagree with migration 39");
    }
    let archive_indexes = [
        "idx_threads_archive_created_at_ms",
        "idx_threads_archive_updated_at_ms",
        "idx_threads_archive_recency_at_ms",
    ];
    for index in archive_indexes {
        if applied.contains(&58) != index_exists(connection, index).await? {
            anyhow::bail!("upstream archive index {index} disagrees with migration 58");
        }
    }
    let persist = column_exists(connection, "thread_dynamic_tools", "persist_on_resume").await?;
    let capability = column_exists(connection, "thread_dynamic_tools", "capability_json").await?;
    let namespace =
        column_exists(connection, "thread_dynamic_tools", "namespace_description").await?;
    if persist != capability || namespace && !persist {
        anyhow::bail!("dynamic-tool state columns are partial");
    }
    if applied.contains(&FORK_58) && !(persist && capability && namespace) {
        anyhow::bail!("fork migration 58 is recorded but its columns are incomplete");
    }
    if persist
        && !applied.contains(&FORK_58)
        && !(applied.contains(&FORK_9001) && applied.contains(&FORK_9002))
    {
        anyhow::bail!("dynamic-tool state columns have no recognized migration history");
    }
    if namespace
        && !applied.contains(&FORK_58)
        && !(applied.contains(&FORK_9001) && applied.contains(&FORK_9002))
    {
        anyhow::bail!("dynamic-tool namespace column has no recognized alias pair");
    }
    // A known deployed release wrote this column before recording migration
    // 30. It is the only missing-row column repair admitted by this bridge.
    if column_exists(connection, "threads", "thread_source").await?
        && !applied.contains(&30)
        && !applied.contains(&29)
    {
        anyhow::bail!("thread_source column appears before its known predecessor");
    }
    Ok(())
}

async fn ensure_ledger(connection: &mut SqliteConnection) -> anyhow::Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS _sqlx_migrations (
            version BIGINT PRIMARY KEY,
            description TEXT NOT NULL,
            installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            success BOOLEAN NOT NULL,
            checksum BLOB NOT NULL,
            execution_time BIGINT NOT NULL
        )",
    )
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn insert_applied(
    connection: &mut SqliteConnection,
    migrator: &Migrator,
    version: i64,
) -> anyhow::Result<()> {
    let migration = embedded(migrator, version)?;
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
         VALUES (?, ?, TRUE, ?, 0)",
    )
    .bind(version)
    .bind(migration.description.as_ref())
    .bind(migration.checksum.as_ref())
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn apply_bridge(
    connection: &mut SqliteConnection,
    migrator: &Migrator,
    ledger_exists: bool,
    rows: &[RowIdentity],
) -> anyhow::Result<()> {
    let applied = rows.iter().map(|row| row.target).collect::<BTreeSet<_>>();
    let original_versions = rows.iter().map(|row| row.source).collect::<BTreeSet<_>>();
    if !ledger_exists {
        ensure_ledger(connection).await?;
    }
    if rows.iter().any(|row| row.source != row.target) {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS state_migration_rekey_receipts (
                old_version BIGINT PRIMARY KEY,
                new_version BIGINT NOT NULL,
                old_description TEXT NOT NULL,
                checksum BLOB NOT NULL,
                execution_time BIGINT NOT NULL
            )",
        )
        .execute(&mut *connection)
        .await?;
    }
    // Temporary negative versions make overlapping shifted moves independent
    // of update order. Existing exact target rows are retained, not rewritten.
    for row in rows.iter().filter(|row| row.source != row.target) {
        sqlx::query(
            "INSERT INTO state_migration_rekey_receipts
             (old_version, new_version, old_description, checksum, execution_time)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(row.source)
        .bind(row.target)
        .bind(&row.description)
        .bind(&row.checksum)
        .bind(row.execution_time)
        .execute(&mut *connection)
        .await?;
        let temporary = -FORK_BASE - row.source;
        sqlx::query("UPDATE _sqlx_migrations SET version = ? WHERE version = ?")
            .bind(temporary)
            .bind(row.source)
            .execute(&mut *connection)
            .await?;
    }
    for row in rows.iter().filter(|row| row.source != row.target) {
        let temporary = -FORK_BASE - row.source;
        if original_versions.contains(&row.target)
            && !rows
                .iter()
                .any(|other| other.source == row.target && other.source != other.target)
        {
            sqlx::query("DELETE FROM _sqlx_migrations WHERE version = ?")
                .bind(temporary)
                .execute(&mut *connection)
                .await?;
            continue;
        }
        // A second legacy source can map to an already restored canonical
        // target. It must be checksum-equivalent (validated above).
        if sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM _sqlx_migrations WHERE version = ?)",
        )
        .bind(row.target)
        .fetch_one(&mut *connection)
        .await?
        {
            sqlx::query("DELETE FROM _sqlx_migrations WHERE version = ?")
                .bind(temporary)
                .execute(&mut *connection)
                .await?;
            continue;
        }
        let migration = embedded(migrator, row.target)?;
        sqlx::query("UPDATE _sqlx_migrations SET version = ?, description = ? WHERE version = ?")
            .bind(row.target)
            .bind(migration.description.as_ref())
            .bind(temporary)
            .execute(&mut *connection)
            .await?;
    }

    let persist = column_exists(connection, "thread_dynamic_tools", "persist_on_resume").await?;
    let namespace =
        column_exists(connection, "thread_dynamic_tools", "namespace_description").await?;
    if persist && !namespace {
        sqlx::query("ALTER TABLE thread_dynamic_tools ADD COLUMN namespace_description TEXT")
            .execute(&mut *connection)
            .await?;
    }
    if persist && !applied.contains(&FORK_58) {
        insert_applied(connection, migrator, FORK_58).await?;
    }
    for alias in [FORK_9001, FORK_9002] {
        if !applied.contains(&alias) {
            insert_applied(connection, migrator, alias).await?;
        }
    }
    if column_exists(connection, "threads", "thread_source").await? && !applied.contains(&30) {
        insert_applied(connection, migrator, 30).await?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "migration_repair_tests.rs"]
mod tests;
