use crate::StateRuntime;
use anyhow::ensure;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::Transaction;

/// Outcome of writing one completed provider response to the durable ledger.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderCallUsageWriteOutcome {
    Inserted,
    Duplicate,
}

/// Usage observed for one completed provider response.
///
/// `request_id` is retained as the provider response ID because the existing
/// ledger schema has no separate response-id column. `provider_call_id` is the
/// distinct local identity for this persisted call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderCallUsageRecord {
    pub provider_call_id: String,
    pub thread_id: String,
    pub turn_id: String,
    pub provider: String,
    /// Stable provider-account scope when the runtime has an authoritative value.
    /// `None` stays unknown; it is never replaced by an account plan or model.
    pub provider_account_scope: Option<String>,
    pub requested_model: String,
    pub actual_model_used: Option<String>,
    pub response_id: String,
    pub requested_service_tier: Option<String>,
    pub actual_service_tier: Option<String>,
    pub actual_service_tier_source: Option<String>,
    pub fast_mode_requested: Option<bool>,
    pub fast_mode_used: Option<bool>,
    pub billing_surface: Option<String>,
    pub account_plan: Option<String>,
    pub started_at: String,
    pub completed_at: String,
    pub input_tokens_uncached: Option<i64>,
    pub input_tokens_cached: Option<i64>,
    pub input_tokens_cache_write: Option<i64>,
    pub output_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub status: &'static str,
}

/// Authoritative thread-tree identity supplied by session lifecycle code.
///
/// The writer derives a missing root only from an already-persisted parent or
/// fork-parent row. Root sessions should provide their exact self-root; absent
/// lineage evidence remains NULL rather than being guessed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageThreadRecord {
    pub thread_id: String,
    pub parent_thread_id: Option<String>,
    pub root_thread_id: Option<String>,
    pub fork_parent_thread_id: Option<String>,
    pub source: Option<String>,
}

impl StateRuntime {
    /// Persist one already-completed response without aggregating across turn
    /// state. Missing provider usage is represented with NULL token fields.
    pub async fn record_provider_call_usage(
        &self,
        record: &ProviderCallUsageRecord,
    ) -> anyhow::Result<ProviderCallUsageWriteOutcome> {
        ensure!(
            !record.provider_call_id.trim().is_empty(),
            "provider_call_id is required"
        );
        ensure!(!record.provider.trim().is_empty(), "provider is required");
        ensure!(!record.thread_id.trim().is_empty(), "thread_id is required");
        ensure!(!record.turn_id.trim().is_empty(), "turn_id is required");
        ensure!(
            !record.response_id.trim().is_empty(),
            "response_id is required for idempotency"
        );
        ensure!(
            !record
                .provider_account_scope
                .as_ref()
                .is_some_and(|scope| scope.trim().is_empty()),
            "provider account scope cannot be empty when supplied"
        );
        ensure!(
            matches!(record.status, "ok" | "provider_usage_missing"),
            "completed response has unsupported usage status"
        );
        for count in [
            record.input_tokens_uncached,
            record.input_tokens_cached,
            record.input_tokens_cache_write,
            record.output_tokens,
            record.total_tokens,
        ]
        .into_iter()
        .flatten()
        {
            ensure!(count >= 0, "provider usage token counts cannot be negative");
        }
        ensure!(
            record.status != "provider_usage_missing"
                || (record.input_tokens_uncached.is_none()
                    && record.input_tokens_cached.is_none()
                    && record.input_tokens_cache_write.is_none()
                    && record.output_tokens.is_none()
                    && record.total_tokens.is_none()),
            "missing provider usage must retain NULL token dimensions"
        );

        let account_scope = record.provider_account_scope.as_deref().unwrap_or("");
        let mut transaction = self.usage_pool.begin().await?;
        // The first operation is a write: SQLite serializes contenders before
        // either can inspect or append a call with this response identity.
        let identity_insert = sqlx::query(
            "INSERT OR IGNORE INTO usage_response_idempotency (provider, account_scope, thread_id, response_id, provider_call_id) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&record.provider)
        .bind(account_scope)
        .bind(&record.thread_id)
        .bind(&record.response_id)
        .bind(&record.provider_call_id)
        .execute(&mut *transaction)
        .await?;

        if identity_insert.rows_affected() == 0 {
            let existing_call_id = sqlx::query_scalar::<_, String>(
                "SELECT provider_call_id FROM usage_response_idempotency WHERE provider = ? AND account_scope = ? AND thread_id = ? AND response_id = ?",
            )
            .bind(&record.provider)
            .bind(account_scope)
            .bind(&record.thread_id)
            .bind(&record.response_id)
            .fetch_one(&mut *transaction)
            .await?;
            ensure!(
                provider_call_response_matches(&mut transaction, &existing_call_id, record).await?,
                "conflicting payload for an existing provider response identity"
            );
            transaction.commit().await?;
            return Ok(ProviderCallUsageWriteOutcome::Duplicate);
        }

        // Older ledger rows predate the idempotency table. Reconcile only the
        // unscoped provider/session contract; an unknown account is not used to
        // collapse rows from different threads or providers.
        if record.provider_account_scope.is_none()
            && let Some(existing_call_id) =
                find_legacy_provider_call(&mut transaction, record).await?
        {
            ensure!(
                provider_call_response_matches(&mut transaction, &existing_call_id, record).await?,
                "conflicting legacy row for provider response identity"
            );
            sqlx::query(
                "UPDATE usage_response_idempotency SET provider_call_id = ? WHERE provider = ? AND account_scope = ? AND thread_id = ? AND response_id = ?",
            )
            .bind(&existing_call_id)
            .bind(&record.provider)
            .bind(account_scope)
            .bind(&record.thread_id)
            .bind(&record.response_id)
            .execute(&mut *transaction)
            .await?;
            transaction.commit().await?;
            return Ok(ProviderCallUsageWriteOutcome::Duplicate);
        }

        sqlx::query(
            r#"INSERT INTO usage_provider_calls (
                provider_call_id, thread_id, turn_id, provider, requested_model,
                actual_model_used, request_id, requested_service_tier,
                actual_service_tier, actual_service_tier_source,
                fast_mode_requested, fast_mode_used, billing_surface,
                account_plan, started_at, completed_at,
                input_tokens_uncached, input_tokens_cached,
                input_tokens_cache_write, output_tokens, total_tokens, status
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(&record.provider_call_id)
        .bind(&record.thread_id)
        .bind(&record.turn_id)
        .bind(&record.provider)
        .bind(&record.requested_model)
        .bind(&record.actual_model_used)
        .bind(&record.response_id)
        .bind(&record.requested_service_tier)
        .bind(&record.actual_service_tier)
        .bind(&record.actual_service_tier_source)
        .bind(
            record
                .fast_mode_requested
                .map(|value| if value { 1_i64 } else { 0_i64 }),
        )
        .bind(
            record
                .fast_mode_used
                .map(|value| if value { 1_i64 } else { 0_i64 }),
        )
        .bind(&record.billing_surface)
        .bind(&record.account_plan)
        .bind(&record.started_at)
        .bind(&record.completed_at)
        .bind(record.input_tokens_uncached)
        .bind(record.input_tokens_cached)
        .bind(record.input_tokens_cache_write)
        .bind(record.output_tokens)
        .bind(record.total_tokens)
        .bind(record.status)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(ProviderCallUsageWriteOutcome::Inserted)
    }

    /// Persist or reconcile the exact lineage for one root, child, or resumed
    /// thread. Known lineage is immutable; previously unknown fields may be
    /// filled from authoritative later evidence.
    pub async fn record_usage_thread(&self, record: &UsageThreadRecord) -> anyhow::Result<()> {
        ensure!(!record.thread_id.trim().is_empty(), "thread_id is required");
        for parent in [
            record.parent_thread_id.as_deref(),
            record.root_thread_id.as_deref(),
            record.fork_parent_thread_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            ensure!(
                !parent.trim().is_empty(),
                "thread lineage IDs cannot be empty"
            );
        }
        ensure!(
            record.parent_thread_id.as_deref() != Some(record.thread_id.as_str()),
            "thread cannot be its own parent"
        );
        ensure!(
            record.fork_parent_thread_id.as_deref() != Some(record.thread_id.as_str()),
            "thread cannot be its own fork parent"
        );

        let mut transaction = self.usage_pool.begin().await?;
        sqlx::query(
            "INSERT OR IGNORE INTO usage_threads (thread_id, parent_thread_id, root_thread_id, fork_parent_thread_id, source) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&record.thread_id)
        .bind(&record.parent_thread_id)
        .bind(&record.root_thread_id)
        .bind(&record.fork_parent_thread_id)
        .bind(&record.source)
        .execute(&mut *transaction)
        .await?;

        let existing = sqlx::query(
            "SELECT parent_thread_id, root_thread_id, fork_parent_thread_id FROM usage_threads WHERE thread_id = ?",
        )
        .bind(&record.thread_id)
        .fetch_one(&mut *transaction)
        .await?;
        let existing_parent: Option<String> = existing.try_get("parent_thread_id")?;
        let existing_root: Option<String> = existing.try_get("root_thread_id")?;
        let existing_fork_parent: Option<String> = existing.try_get("fork_parent_thread_id")?;
        ensure_lineage_compatible(&existing_parent, &record.parent_thread_id, "parent")?;
        ensure_lineage_compatible(
            &existing_fork_parent,
            &record.fork_parent_thread_id,
            "fork parent",
        )?;

        let parent_thread_id = record.parent_thread_id.clone().or(existing_parent.clone());
        let fork_parent_thread_id = record
            .fork_parent_thread_id
            .clone()
            .or(existing_fork_parent.clone());
        let parent_root = match parent_thread_id.as_deref() {
            Some(parent_thread_id) => sqlx::query_scalar::<_, Option<String>>(
                "SELECT root_thread_id FROM usage_threads WHERE thread_id = ?",
            )
            .bind(parent_thread_id)
            .fetch_optional(&mut *transaction)
            .await?
            .flatten(),
            None => None,
        };
        let fork_parent_root = match fork_parent_thread_id.as_deref() {
            Some(fork_parent_thread_id) => sqlx::query_scalar::<_, Option<String>>(
                "SELECT root_thread_id FROM usage_threads WHERE thread_id = ?",
            )
            .bind(fork_parent_thread_id)
            .fetch_optional(&mut *transaction)
            .await?
            .flatten(),
            None => None,
        };
        if let (Some(parent_root), Some(fork_parent_root)) =
            (parent_root.as_deref(), fork_parent_root.as_deref())
        {
            ensure!(
                parent_root == fork_parent_root,
                "parent and fork-parent lineage disagree on root"
            );
        }
        let root_thread_id = record
            .root_thread_id
            .clone()
            .or(existing_root.clone())
            .or(parent_root.clone())
            .or(fork_parent_root.clone());
        ensure!(
            parent_thread_id.is_none()
                || root_thread_id.as_deref() != Some(record.thread_id.as_str()),
            "non-root thread cannot identify itself as the root"
        );
        ensure_lineage_compatible(&existing_root, &root_thread_id, "root")?;
        for known_parent_root in [parent_root.as_deref(), fork_parent_root.as_deref()]
            .into_iter()
            .flatten()
        {
            let Some(root_thread_id) = root_thread_id.as_deref() else {
                continue;
            };
            ensure!(
                root_thread_id == known_parent_root,
                "thread root conflicts with its persisted parent lineage"
            );
        }

        sqlx::query(
            "UPDATE usage_threads SET parent_thread_id = COALESCE(parent_thread_id, ?), root_thread_id = COALESCE(root_thread_id, ?), fork_parent_thread_id = COALESCE(fork_parent_thread_id, ?), source = COALESCE(source, ?) WHERE thread_id = ?",
        )
        .bind(&parent_thread_id)
        .bind(&root_thread_id)
        .bind(&fork_parent_thread_id)
        .bind(&record.source)
        .bind(&record.thread_id)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }
}

fn ensure_lineage_compatible(
    existing: &Option<String>,
    incoming: &Option<String>,
    label: &str,
) -> anyhow::Result<()> {
    ensure!(
        existing.is_none() || incoming.is_none() || existing == incoming,
        "conflicting known thread {label} lineage"
    );
    Ok(())
}

async fn find_legacy_provider_call(
    transaction: &mut Transaction<'_, Sqlite>,
    record: &ProviderCallUsageRecord,
) -> anyhow::Result<Option<String>> {
    // Only an unmapped row can be adopted as pre-idempotency history.
    // New rows can have the same provider/thread/response tuple under a
    // different explicit account scope and must never satisfy an unknown
    // account lookup.
    let existing = sqlx::query_scalar::<_, String>(
        "SELECT p.provider_call_id FROM usage_provider_calls AS p WHERE p.provider = ? AND p.thread_id = ? AND p.request_id = ? AND NOT EXISTS (SELECT 1 FROM usage_response_idempotency AS idempotency WHERE idempotency.provider_call_id = p.provider_call_id) ORDER BY p.provider_call_id LIMIT 2",
    )
    .bind(&record.provider)
    .bind(&record.thread_id)
    .bind(&record.response_id)
    .fetch_all(&mut **transaction)
    .await?;
    ensure!(
        existing.len() <= 1,
        "ambiguous legacy rows for provider response identity"
    );
    Ok(existing.into_iter().next())
}

async fn provider_call_response_matches(
    transaction: &mut Transaction<'_, Sqlite>,
    provider_call_id: &str,
    record: &ProviderCallUsageRecord,
) -> anyhow::Result<bool> {
    let row = sqlx::query(
        "SELECT thread_id, turn_id, provider, requested_model, actual_model_used, request_id, requested_service_tier, actual_service_tier, actual_service_tier_source, fast_mode_requested, fast_mode_used, billing_surface, account_plan, input_tokens_uncached, input_tokens_cached, input_tokens_cache_write, output_tokens, total_tokens, status FROM usage_provider_calls WHERE provider_call_id = ?",
    )
    .bind(provider_call_id)
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(row) = row else {
        anyhow::bail!("response identity points to a missing provider-call row");
    };
    let optional_string_matches =
        |column: &str, incoming: &Option<String>| -> anyhow::Result<bool> {
            let stored: Option<String> = row.try_get(column)?;
            Ok(stored.as_deref() == incoming.as_deref())
        };
    let optional_bool_matches = |column: &str, incoming: Option<bool>| -> anyhow::Result<bool> {
        let stored: Option<i64> = row.try_get(column)?;
        Ok(stored == incoming.map(|value| if value { 1_i64 } else { 0_i64 }))
    };
    Ok(row.try_get::<String, _>("thread_id")? == record.thread_id
        && row.try_get::<Option<String>, _>("turn_id")?.as_deref() == Some(record.turn_id.as_str())
        && row.try_get::<Option<String>, _>("provider")?.as_deref()
            == Some(record.provider.as_str())
        && row
            .try_get::<Option<String>, _>("requested_model")?
            .as_deref()
            == Some(record.requested_model.as_str())
        && optional_string_matches("actual_model_used", &record.actual_model_used)?
        && row.try_get::<Option<String>, _>("request_id")?.as_deref()
            == Some(record.response_id.as_str())
        && optional_string_matches("requested_service_tier", &record.requested_service_tier)?
        && optional_string_matches("actual_service_tier", &record.actual_service_tier)?
        && optional_string_matches(
            "actual_service_tier_source",
            &record.actual_service_tier_source,
        )?
        && optional_bool_matches("fast_mode_requested", record.fast_mode_requested)?
        && optional_bool_matches("fast_mode_used", record.fast_mode_used)?
        && optional_string_matches("billing_surface", &record.billing_surface)?
        && optional_string_matches("account_plan", &record.account_plan)?
        && row.try_get::<Option<i64>, _>("input_tokens_uncached")? == record.input_tokens_uncached
        && row.try_get::<Option<i64>, _>("input_tokens_cached")? == record.input_tokens_cached
        && row.try_get::<Option<i64>, _>("input_tokens_cache_write")?
            == record.input_tokens_cache_write
        && row.try_get::<Option<i64>, _>("output_tokens")? == record.output_tokens
        && row.try_get::<Option<i64>, _>("total_tokens")? == record.total_tokens
        && row.try_get::<Option<String>, _>("status")?.as_deref() == Some(record.status))
}

#[cfg(test)]
mod tests {
    use super::ProviderCallUsageRecord;
    use super::ProviderCallUsageWriteOutcome;
    use super::UsageThreadRecord;
    use crate::SqliteConfig;
    use crate::StateRuntime;
    use crate::runtime::test_support::unique_temp_dir;
    use codex_utils_absolute_path::test_support::PathExt;
    use pretty_assertions::assert_eq;

    fn completed_record(
        provider_call_id: &str,
        thread_id: &str,
        response_id: &str,
        status: &'static str,
        usage: Option<(i64, i64, i64, i64, i64)>,
    ) -> ProviderCallUsageRecord {
        ProviderCallUsageRecord {
            provider_call_id: provider_call_id.to_string(),
            thread_id: thread_id.to_string(),
            turn_id: "turn-1".to_string(),
            provider: "openai".to_string(),
            provider_account_scope: None,
            requested_model: "gpt-6-sol".to_string(),
            actual_model_used: Some("gpt-6-luna".to_string()),
            response_id: response_id.to_string(),
            requested_service_tier: Some("default".to_string()),
            actual_service_tier: Some("default".to_string()),
            actual_service_tier_source: Some("runtime_contract".to_string()),
            fast_mode_requested: Some(false),
            fast_mode_used: Some(false),
            billing_surface: Some("chatgpt_credits".to_string()),
            account_plan: Some("plus".to_string()),
            started_at: "2026-09-30T00:00:00Z".to_string(),
            completed_at: "2026-09-30T00:00:01Z".to_string(),
            input_tokens_uncached: usage.map(|usage| usage.0),
            input_tokens_cached: usage.map(|usage| usage.1),
            input_tokens_cache_write: usage.map(|usage| usage.2),
            output_tokens: usage.map(|usage| usage.3),
            total_tokens: usage.map(|usage| usage.4),
            status,
        }
    }

    async fn account_scope_order_fixture(account_first: bool) {
        let codex_home = unique_temp_dir();
        let sqlite = SqliteConfig::new_for_testing(codex_home.as_path().abs());
        let runtime = StateRuntime::init(sqlite.clone(), "openai".to_string())
            .await
            .expect("initialize isolated state runtime");

        let mut known_account = completed_record(
            "local-call-known-account",
            "scope-thread",
            "scope-response",
            "ok",
            Some((100, 20, 7, 10, 130)),
        );
        known_account.provider_account_scope = Some("account-a".to_string());
        let unknown_account = completed_record(
            "local-call-unknown-account",
            "scope-thread",
            "scope-response",
            "ok",
            Some((100, 20, 7, 10, 130)),
        );
        let records = if account_first {
            vec![known_account, unknown_account]
        } else {
            vec![unknown_account, known_account]
        };

        for record in &records {
            assert_eq!(
                runtime
                    .record_provider_call_usage(record)
                    .await
                    .expect("different account-identity scopes remain distinct"),
                ProviderCallUsageWriteOutcome::Inserted
            );
        }
        for (index, record) in records.iter().enumerate() {
            let mut replay = record.clone();
            replay.provider_call_id = format!("replay-local-call-{index}");
            assert_eq!(
                runtime
                    .record_provider_call_usage(&replay)
                    .await
                    .expect("same scoped response replay should deduplicate"),
                ProviderCallUsageWriteOutcome::Duplicate
            );

            let mut conflict = record.clone();
            conflict.provider_call_id = format!("conflicting-local-call-{index}");
            conflict.input_tokens_uncached = Some(101);
            assert!(
                runtime.record_provider_call_usage(&conflict).await.is_err(),
                "same scoped response with changed usage must fail"
            );
        }
        let identities = sqlx::query_as::<_, (String, String)>(
            "SELECT account_scope, provider_call_id FROM usage_response_idempotency WHERE provider = ? AND thread_id = ? AND response_id = ? ORDER BY account_scope",
        )
        .bind("openai")
        .bind("scope-thread")
        .bind("scope-response")
        .fetch_all(runtime.usage_pool().as_ref())
        .await
        .expect("read the two scoped identities");
        assert_eq!(
            identities,
            vec![
                (String::new(), "local-call-unknown-account".to_string()),
                (
                    "account-a".to_string(),
                    "local-call-known-account".to_string(),
                ),
            ]
        );
        let counts = sqlx::query_as::<_, (i64, i64)>(
            "SELECT COUNT(*), COUNT(DISTINCT provider_call_id) FROM usage_provider_calls WHERE provider = ? AND thread_id = ? AND request_id = ?",
        )
        .bind("openai")
        .bind("scope-thread")
        .bind("scope-response")
        .fetch_one(runtime.usage_pool().as_ref())
        .await
        .expect("count calls without collapsing account scopes");
        assert_eq!(counts, (2, 2));
        let summary_count = sqlx::query_scalar::<_, i64>(
            "SELECT provider_call_count FROM usage_thread_credit_summary WHERE thread_id = ?",
        )
        .bind("scope-thread")
        .fetch_one(runtime.usage_pool().as_ref())
        .await
        .expect("read account-scoped call summary");
        assert_eq!(summary_count, 2);

        runtime.close().await;
        let reopened = StateRuntime::init(sqlite, "openai".to_string())
            .await
            .expect("reopen isolated state runtime");
        for record in &records {
            let mut replay = record.clone();
            replay.provider_call_id = format!(
                "reopened-replay-{}",
                record
                    .provider_account_scope
                    .as_deref()
                    .unwrap_or("unknown")
            );
            assert_eq!(
                reopened
                    .record_provider_call_usage(&replay)
                    .await
                    .expect("scope-aware response replay remains durable"),
                ProviderCallUsageWriteOutcome::Duplicate
            );
        }
        let reopened_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM usage_provider_calls WHERE provider = ? AND thread_id = ? AND request_id = ?",
        )
        .bind("openai")
        .bind("scope-thread")
        .bind("scope-response")
        .fetch_one(reopened.usage_pool().as_ref())
        .await
        .expect("reopened scoped-call count remains stable");
        assert_eq!(reopened_count, 2);

        reopened.close().await;
        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn response_identity_deduplicates_replay_and_rejects_conflicts() {
        let codex_home = unique_temp_dir();
        let sqlite = SqliteConfig::new_for_testing(codex_home.as_path().abs());
        let runtime = StateRuntime::init(sqlite.clone(), "openai".to_string())
            .await
            .expect("initialize isolated state runtime");

        let first = completed_record(
            "local-call-first",
            "root-thread",
            "response-first",
            "ok",
            Some((100, 20, 7, 10, 130)),
        );
        assert_eq!(
            runtime
                .record_provider_call_usage(&first)
                .await
                .expect("insert first response"),
            ProviderCallUsageWriteOutcome::Inserted
        );

        let mut replay = first.clone();
        replay.provider_call_id = "different-random-local-call-id".to_string();
        replay.started_at = "2026-09-30T01:00:00Z".to_string();
        replay.completed_at = "2026-09-30T01:00:01Z".to_string();
        assert_eq!(
            runtime
                .record_provider_call_usage(&replay)
                .await
                .expect("same response replay should be idempotent"),
            ProviderCallUsageWriteOutcome::Duplicate
        );

        let continuation = completed_record(
            "local-call-continuation",
            "root-thread",
            "response-continuation",
            "ok",
            Some((40, 5, 2, 3, 48)),
        );
        assert_eq!(
            runtime
                .record_provider_call_usage(&continuation)
                .await
                .expect("a continuation with another response ID is a new call"),
            ProviderCallUsageWriteOutcome::Inserted
        );
        let retry = completed_record(
            "local-call-retry",
            "root-thread",
            "response-retry",
            "ok",
            Some((11, 2, 1, 4, 17)),
        );
        assert_eq!(
            runtime
                .record_provider_call_usage(&retry)
                .await
                .expect("a retry with another response ID is a new call"),
            ProviderCallUsageWriteOutcome::Inserted
        );

        let mut conflicting_replay = replay.clone();
        conflicting_replay.input_tokens_uncached = Some(101);
        assert!(
            runtime
                .record_provider_call_usage(&conflicting_replay)
                .await
                .is_err()
        );

        let mut another_thread = first.clone();
        another_thread.provider_call_id = "local-call-another-thread".to_string();
        another_thread.thread_id = "another-thread".to_string();
        assert_eq!(
            runtime
                .record_provider_call_usage(&another_thread)
                .await
                .expect("response IDs are scoped to the thread session"),
            ProviderCallUsageWriteOutcome::Inserted
        );
        let mut another_provider = first.clone();
        another_provider.provider_call_id = "local-call-another-provider".to_string();
        another_provider.provider = "other-provider".to_string();
        assert_eq!(
            runtime
                .record_provider_call_usage(&another_provider)
                .await
                .expect("response IDs are scoped to the provider"),
            ProviderCallUsageWriteOutcome::Inserted
        );
        let mut another_account = first.clone();
        another_account.provider_call_id = "local-call-another-account".to_string();
        another_account.provider_account_scope = Some("account-a".to_string());
        assert_eq!(
            runtime
                .record_provider_call_usage(&another_account)
                .await
                .expect("known account scope remains distinct"),
            ProviderCallUsageWriteOutcome::Inserted
        );

        runtime.close().await;
        let reopened = StateRuntime::init(sqlite, "openai".to_string())
            .await
            .expect("reopen isolated state runtime");
        let counts = sqlx::query_as::<_, (i64, i64)>(
            "SELECT (SELECT COUNT(*) FROM usage_provider_calls), (SELECT COUNT(*) FROM usage_response_idempotency)",
        )
        .fetch_one(reopened.usage_pool().as_ref())
        .await
        .expect("read durable idempotency rows after reopen");
        assert_eq!(counts, (6, 6));
        let dimensions = sqlx::query_as::<_, (Option<i64>, Option<i64>, Option<i64>, Option<i64>, Option<i64>)>(
            "SELECT input_tokens_uncached, input_tokens_cached, input_tokens_cache_write, output_tokens, total_tokens FROM usage_provider_calls WHERE provider_call_id = ?",
        )
        .bind("local-call-first")
        .fetch_one(reopened.usage_pool().as_ref())
        .await
        .expect("read response-local usage dimensions");
        assert_eq!(
            dimensions,
            (Some(100), Some(20), Some(7), Some(10), Some(130))
        );

        reopened.close().await;
        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn account_scopes_do_not_alias_in_either_write_order() {
        account_scope_order_fixture(/*account_first*/ true).await;
        account_scope_order_fixture(/*account_first*/ false).await;
    }

    #[tokio::test]
    async fn unscoped_legacy_rows_are_adopted_without_cross_scope_aliasing() {
        let codex_home = unique_temp_dir();
        let sqlite = SqliteConfig::new_for_testing(codex_home.as_path().abs());
        let runtime = StateRuntime::init(sqlite, "openai".to_string())
            .await
            .expect("initialize isolated state runtime");

        let legacy = completed_record(
            "legacy-provider-call",
            "legacy-thread",
            "legacy-response",
            "ok",
            Some((100, 20, 7, 10, 130)),
        );
        assert_eq!(
            runtime
                .record_provider_call_usage(&legacy)
                .await
                .expect("seed an old-format unscoped ledger row"),
            ProviderCallUsageWriteOutcome::Inserted
        );
        sqlx::query("DELETE FROM usage_response_idempotency WHERE provider_call_id = ?")
            .bind("legacy-provider-call")
            .execute(runtime.usage_pool().as_ref())
            .await
            .expect("model a pre-idempotency row with no response mapping");

        let mut replay = legacy.clone();
        replay.provider_call_id = "legacy-replay-local-call".to_string();
        assert_eq!(
            runtime
                .record_provider_call_usage(&replay)
                .await
                .expect("adopt an unambiguous unscoped historical response"),
            ProviderCallUsageWriteOutcome::Duplicate
        );
        let mut conflict = replay.clone();
        conflict.provider_call_id = "legacy-conflicting-local-call".to_string();
        conflict.input_tokens_uncached = Some(101);
        assert!(
            runtime.record_provider_call_usage(&conflict).await.is_err(),
            "changed payload for a legacy response must fail closed"
        );

        let mut known_account = legacy.clone();
        known_account.provider_call_id = "known-account-after-legacy".to_string();
        known_account.provider_account_scope = Some("account-a".to_string());
        assert_eq!(
            runtime
                .record_provider_call_usage(&known_account)
                .await
                .expect("known account must not adopt an unknown-scope legacy row"),
            ProviderCallUsageWriteOutcome::Inserted
        );

        let identities = sqlx::query_as::<_, (String, String)>(
            "SELECT account_scope, provider_call_id FROM usage_response_idempotency WHERE provider = ? AND thread_id = ? AND response_id = ? ORDER BY account_scope",
        )
        .bind("openai")
        .bind("legacy-thread")
        .bind("legacy-response")
        .fetch_all(runtime.usage_pool().as_ref())
        .await
        .expect("read adopted and known-account identities");
        assert_eq!(
            identities,
            vec![
                (String::new(), "legacy-provider-call".to_string()),
                (
                    "account-a".to_string(),
                    "known-account-after-legacy".to_string(),
                ),
            ]
        );
        let count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM usage_provider_calls WHERE provider = ? AND thread_id = ? AND request_id = ?",
        )
        .bind("openai")
        .bind("legacy-thread")
        .bind("legacy-response")
        .fetch_one(runtime.usage_pool().as_ref())
        .await
        .expect("count adopted and account-scoped rows");
        assert_eq!(count, 2);

        runtime.close().await;
        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn thread_lineage_joins_root_and_child_responses_after_resume() {
        let codex_home = unique_temp_dir();
        let sqlite = SqliteConfig::new_for_testing(codex_home.as_path().abs());
        let runtime = StateRuntime::init(sqlite.clone(), "openai".to_string())
            .await
            .expect("initialize isolated state runtime");

        runtime
            .record_usage_thread(&UsageThreadRecord {
                thread_id: "root-thread".to_string(),
                parent_thread_id: None,
                root_thread_id: Some("root-thread".to_string()),
                fork_parent_thread_id: None,
                source: Some("cli".to_string()),
            })
            .await
            .expect("persist root lineage");
        runtime
            .record_usage_thread(&UsageThreadRecord {
                thread_id: "child-thread".to_string(),
                parent_thread_id: Some("root-thread".to_string()),
                root_thread_id: None,
                fork_parent_thread_id: Some("root-thread".to_string()),
                source: Some("subagent".to_string()),
            })
            .await
            .expect("persist child lineage from exact parent row");

        runtime
            .record_provider_call_usage(&completed_record(
                "local-call-root",
                "root-thread",
                "response-root",
                "ok",
                Some((25, 5, 1, 3, 33)),
            ))
            .await
            .expect("persist root response");
        let mut child_call = completed_record(
            "local-call-child",
            "child-thread",
            "response-child",
            "ok",
            Some((17, 4, 2, 6, 27)),
        );
        child_call.actual_model_used = None;
        runtime
            .record_provider_call_usage(&child_call)
            .await
            .expect("persist child response with unknown observed model");
        runtime.close().await;

        let resumed = StateRuntime::init(sqlite, "openai".to_string())
            .await
            .expect("reopen isolated state runtime");
        for lineage in [
            UsageThreadRecord {
                thread_id: "root-thread".to_string(),
                parent_thread_id: None,
                root_thread_id: Some("root-thread".to_string()),
                fork_parent_thread_id: None,
                source: Some("cli".to_string()),
            },
            UsageThreadRecord {
                thread_id: "child-thread".to_string(),
                parent_thread_id: Some("root-thread".to_string()),
                root_thread_id: None,
                fork_parent_thread_id: Some("root-thread".to_string()),
                source: Some("subagent".to_string()),
            },
        ] {
            resumed
                .record_usage_thread(&lineage)
                .await
                .expect("resume should preserve identical lineage");
        }
        let mut replay = child_call.clone();
        replay.provider_call_id = "local-call-child-replay".to_string();
        assert_eq!(
            resumed
                .record_provider_call_usage(&replay)
                .await
                .expect("resumed response replay should deduplicate"),
            ProviderCallUsageWriteOutcome::Duplicate
        );
        resumed
            .record_provider_call_usage(&completed_record(
                "local-call-child-next-turn",
                "child-thread",
                "response-child-next-turn",
                "ok",
                Some((9, 1, 0, 2, 12)),
            ))
            .await
            .expect("resumed child continuation should append");

        let rows = sqlx::query_as::<_, (String, Option<String>, Option<String>, Option<String>, String, Option<String>)>(
            "SELECT t.thread_id, t.parent_thread_id, t.root_thread_id, t.fork_parent_thread_id, p.request_id, p.actual_model_used FROM usage_threads AS t JOIN usage_provider_calls AS p ON p.thread_id = t.thread_id ORDER BY t.thread_id, p.request_id",
        )
        .fetch_all(resumed.usage_pool().as_ref())
        .await
        .expect("join durable response rows to thread lineage");
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0],
            (
                "child-thread".to_string(),
                Some("root-thread".to_string()),
                Some("root-thread".to_string()),
                Some("root-thread".to_string()),
                "response-child".to_string(),
                None,
            )
        );
        assert_eq!(rows[1].0, "child-thread");
        assert_eq!(rows[1].4, "response-child-next-turn");
        assert_eq!(
            rows[2],
            (
                "root-thread".to_string(),
                None,
                Some("root-thread".to_string()),
                None,
                "response-root".to_string(),
                Some("gpt-6-luna".to_string()),
            )
        );
        let missing_actual_model = sqlx::query_as::<
            _,
            (String, Option<String>, String, Option<f64>),
        >(
            "SELECT requested_model, actual_model_used, pricing_status, estimated_total_credits FROM usage_provider_call_credit_estimates WHERE provider_call_id = ?",
        )
        .bind("local-call-child")
        .fetch_one(resumed.usage_pool().as_ref())
        .await
        .expect("read missing observed-model pricing outcome");
        assert_eq!(
            missing_actual_model,
            (
                "gpt-6-sol".to_string(),
                None,
                "actual_model_missing".to_string(),
                None,
            )
        );

        resumed.close().await;
        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn completed_response_usage_survives_reopen_and_credit_views() {
        let codex_home = unique_temp_dir();
        let sqlite = SqliteConfig::new_for_testing(codex_home.as_path().abs());
        let runtime = StateRuntime::init(sqlite.clone(), "openai".to_string())
            .await
            .expect("initialize isolated state runtime");

        runtime
            .record_provider_call_usage(&completed_record(
                "local-call-priced",
                "thread-priced",
                "response-priced",
                "ok",
                Some((100_000, 0, 0, 1_000, 101_000)),
            ))
            .await
            .expect("persist fully evidenced response");
        runtime
            .record_provider_call_usage(&completed_record(
                "local-call-missing",
                "thread-missing",
                "response-missing-usage",
                "provider_usage_missing",
                /*usage*/ None,
            ))
            .await
            .expect("persist completed response with missing usage");
        runtime.close().await;

        let reopened = StateRuntime::init(sqlite, "openai".to_string())
            .await
            .expect("reopen isolated state runtime");
        let priced = sqlx::query_as::<
            _,
            (
                String,
                Option<f64>,
                Option<String>,
                String,
                String,
                Option<String>,
            ),
        >(
            "SELECT e.pricing_status, e.estimated_total_credits, e.credit_source, p.request_id, e.requested_model, e.actual_model_used FROM usage_provider_call_credit_estimates AS e JOIN usage_provider_calls AS p USING (provider_call_id) WHERE e.provider_call_id = ?",
        )
        .bind("local-call-priced")
        .fetch_one(reopened.usage_pool().as_ref())
        .await
        .expect("read priced response after reopen");
        assert_eq!(
            priced,
            (
                "priced_estimate".to_string(),
                Some(0.2625),
                Some("rate_card_estimate".to_string()),
                "response-priced".to_string(),
                "gpt-6-sol".to_string(),
                Some("gpt-6-luna".to_string()),
            )
        );
        let priced_summary = sqlx::query_as::<_, (i64, i64, i64, i64, Option<f64>)>(
            "SELECT provider_call_count, priced_call_count, unpriced_call_count, partial, estimated_total_credits FROM usage_thread_credit_summary WHERE thread_id = ?",
        )
        .bind("thread-priced")
        .fetch_one(reopened.usage_pool().as_ref())
        .await
        .expect("read complete thread summary");
        assert_eq!(priced_summary, (1, 1, 0, 0, Some(0.2625)));

        let missing = sqlx::query_as::<_, (String, Option<i64>, Option<i64>, Option<i64>, Option<i64>, Option<i64>)>(
            "SELECT pricing_status, input_tokens_uncached, input_tokens_cached, input_tokens_cache_write, output_tokens, total_tokens FROM usage_provider_call_credit_estimates WHERE provider_call_id = ?",
        )
        .bind("local-call-missing")
        .fetch_one(reopened.usage_pool().as_ref())
        .await
        .expect("read missing-usage response after reopen");
        assert_eq!(
            missing,
            (
                "provider_usage_missing".to_string(),
                None,
                None,
                None,
                None,
                None
            )
        );
        let missing_summary = sqlx::query_as::<_, (i64, i64, i64, i64, Option<f64>)>(
            "SELECT provider_call_count, priced_call_count, unpriced_call_count, partial, estimated_total_credits FROM usage_thread_credit_summary WHERE thread_id = ?",
        )
        .bind("thread-missing")
        .fetch_one(reopened.usage_pool().as_ref())
        .await
        .expect("read partial thread summary");
        assert_eq!(missing_summary, (1, 0, 1, 1, None));

        reopened.close().await;
        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn adopted_rate_card_is_effective_dated_and_fails_closed_on_unknown_modes() {
        let codex_home = unique_temp_dir();
        let sqlite = SqliteConfig::new_for_testing(codex_home.as_path().abs());
        let runtime = StateRuntime::init(sqlite.clone(), "openai".to_string())
            .await
            .expect("initialize isolated state runtime");

        let one_million_each = Some((1_000_000, 1_000_000, 0, 1_000_000, 3_000_000));
        let current_rates = [
            ("gpt-6.1-sol", "default", false, 302.5),
            ("gpt-6-sol", "default", false, 305.0),
            ("gpt-6-luna", "default", false, 15.25),
            ("gpt-6-astra", "default", false, 1_525.0),
            ("gpt-6.1-sol", "priority", true, 605.0),
            ("gpt-6-astra", "priority", true, 3_050.0),
            ("gpt-6-sol", "priority", true, 610.0),
            ("gpt-6-luna", "priority", true, 30.5),
            ("gpt-rosalind-research", "default", false, 762.5),
        ];
        for (index, (model, service_tier, fast, _)) in current_rates.iter().enumerate() {
            let call_id = format!("adopted-rate-{index}");
            let mut record = completed_record(
                &call_id,
                "adopted-rate-thread",
                &format!("response-{call_id}"),
                "ok",
                one_million_each,
            );
            record.requested_model = "gpt-6-sol".to_string();
            record.actual_model_used = Some((*model).to_string());
            record.actual_service_tier = Some((*service_tier).to_string());
            record.fast_mode_used = Some(*fast);
            record.started_at = "2026-09-29T23:52:00Z".to_string();
            runtime
                .record_provider_call_usage(&record)
                .await
                .expect("persist rate-card fixture");
        }

        let mut historical_fast = completed_record(
            "historical-fast",
            "adopted-rate-thread",
            "response-historical-fast",
            "ok",
            one_million_each,
        );
        historical_fast.actual_model_used = Some("gpt-6-sol".to_string());
        historical_fast.actual_service_tier = Some("priority".to_string());
        historical_fast.fast_mode_used = Some(true);
        historical_fast.started_at = "2026-09-29T23:51:59Z".to_string();
        runtime
            .record_provider_call_usage(&historical_fast)
            .await
            .expect("persist pre-adoption historical fixture");

        let mut mixed_tokens = completed_record(
            "mixed-token-call",
            "adopted-rate-thread",
            "response-mixed-token-call",
            "ok",
            Some((2_000_000, 3_000_000, 0, 4_000_000, 9_000_000)),
        );
        mixed_tokens.actual_model_used = Some("gpt-6.1-sol".to_string());
        mixed_tokens.started_at = "2026-09-29T23:52:00Z".to_string();
        runtime
            .record_provider_call_usage(&mixed_tokens)
            .await
            .expect("persist mixed-token fixture");

        let mut reported_credits = completed_record(
            "provider-credits-call",
            "adopted-rate-thread",
            "response-provider-credits-call",
            "ok",
            one_million_each,
        );
        reported_credits.actual_model_used = Some("gpt-6-sol".to_string());
        reported_credits.actual_service_tier = Some("priority".to_string());
        reported_credits.fast_mode_used = Some(true);
        reported_credits.started_at = "2026-09-29T23:52:00Z".to_string();
        runtime
            .record_provider_call_usage(&reported_credits)
            .await
            .expect("persist provider-credit fixture");
        sqlx::query(
            "UPDATE usage_provider_calls SET provider_reported_credits = ? WHERE provider_call_id = ?",
        )
        .bind(42.25_f64)
        .bind("provider-credits-call")
        .execute(runtime.usage_pool().as_ref())
        .await
        .expect("attach provider-reported credit evidence");

        for (call_id, model, service_tier, fast) in [
            ("unsupported-fast", "gpt-5.5", "priority", true),
            ("unsupported-ultrafast", "gpt-6-astra", "ultrafast", true),
            ("image-without-modality", "gpt-image-2", "default", false),
        ] {
            let mut record = completed_record(
                call_id,
                "adopted-rate-thread",
                &format!("response-{call_id}"),
                "ok",
                one_million_each,
            );
            record.actual_model_used = Some(model.to_string());
            record.actual_service_tier = Some(service_tier.to_string());
            record.fast_mode_used = Some(fast);
            record.started_at = "2026-09-29T23:52:00Z".to_string();
            runtime
                .record_provider_call_usage(&record)
                .await
                .expect("persist partial-coverage fixture");
        }
        runtime.close().await;

        let reopened = StateRuntime::init(sqlite, "openai".to_string())
            .await
            .expect("reopen isolated state runtime");
        let mut expected = current_rates
            .iter()
            .enumerate()
            .map(|(index, (_, _, _, credits))| {
                (
                    format!("adopted-rate-{index}"),
                    "priced_estimate".to_string(),
                    Some(*credits),
                )
            })
            .collect::<Vec<_>>();
        expected.extend([
            (
                "historical-fast".to_string(),
                "priced_estimate".to_string(),
                Some(762.5),
            ),
            (
                "mixed-token-call".to_string(),
                "priced_estimate".to_string(),
                Some(1_107.5),
            ),
            (
                "provider-credits-call".to_string(),
                "provider_reported".to_string(),
                Some(42.25),
            ),
            (
                "unsupported-fast".to_string(),
                "fast_rate_unknown".to_string(),
                None,
            ),
            (
                "unsupported-ultrafast".to_string(),
                "fast_rate_unknown".to_string(),
                None,
            ),
            (
                "image-without-modality".to_string(),
                "model_rate_missing".to_string(),
                None,
            ),
        ]);
        for (call_id, pricing_status, credits) in expected {
            let actual = sqlx::query_as::<_, (String, Option<f64>)>(
                "SELECT pricing_status, estimated_total_credits FROM usage_provider_call_credit_estimates WHERE provider_call_id = ?",
            )
            .bind(&call_id)
            .fetch_one(reopened.usage_pool().as_ref())
            .await
            .expect("read effective credit result after reopen");
            assert_eq!(actual, (pricing_status, credits), "call {call_id}");
        }

        let actual_identity = sqlx::query_as::<_, (String, String)>(
            "SELECT requested_model, pricing_model FROM usage_provider_call_credit_estimates WHERE provider_call_id = ?",
        )
        .bind("adopted-rate-0")
        .fetch_one(reopened.usage_pool().as_ref())
        .await
        .expect("read request and observed model separately");
        assert_eq!(
            actual_identity,
            ("gpt-6-sol".to_string(), "gpt-6.1-sol".to_string())
        );

        let provider_credit_detail = sqlx::query_as::<_, (Option<f64>, Option<f64>, Option<String>)>(
            "SELECT rate_card_estimated_total_credits, estimated_total_credits, credit_source FROM usage_provider_call_credit_estimates WHERE provider_call_id = ?",
        )
        .bind("provider-credits-call")
        .fetch_one(reopened.usage_pool().as_ref())
        .await
        .expect("read provider-credit precedence after reopen");
        assert_eq!(
            provider_credit_detail,
            (
                Some(610.0),
                Some(42.25),
                Some("provider_reported".to_string())
            )
        );

        let summary = sqlx::query_as::<_, (i64, i64, i64, i64)>(
            "SELECT provider_call_count, priced_call_count, unpriced_call_count, partial FROM usage_thread_credit_summary WHERE thread_id = ?",
        )
        .bind("adopted-rate-thread")
        .fetch_one(reopened.usage_pool().as_ref())
        .await
        .expect("read reopened thread credit summary");
        assert_eq!(summary, (15, 12, 3, 1));

        reopened.close().await;
        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn operator_standard_rate_scenario_uses_observed_model_and_fails_closed() {
        let codex_home = unique_temp_dir();
        let sqlite = SqliteConfig::new_for_testing(codex_home.as_path().abs());
        let runtime = StateRuntime::init(sqlite.clone(), "openai".to_string())
            .await
            .expect("initialize isolated state runtime");

        let mut priced = completed_record(
            "scenario-priced",
            "scenario-thread",
            "response-scenario-priced",
            "ok",
            Some((800, 200, 0, 25, 1_025)),
        );
        priced.requested_model = "gpt-6-luna".to_string();
        priced.actual_model_used = Some("GPT-6.1-SOL".to_string());
        priced.actual_service_tier = None;
        priced.actual_service_tier_source = None;
        priced.fast_mode_used = None;
        priced.billing_surface = None;
        priced.account_plan = None;
        priced.started_at = "2026-09-29T23:52:00Z".to_string();
        assert_eq!(
            runtime
                .record_provider_call_usage(&priced)
                .await
                .expect("persist observed response for standard-rate scenario"),
            ProviderCallUsageWriteOutcome::Inserted
        );

        let mut replay = priced.clone();
        replay.provider_call_id = "scenario-priced-replay".to_string();
        assert_eq!(
            runtime
                .record_provider_call_usage(&replay)
                .await
                .expect("same response replay must not create another estimate row"),
            ProviderCallUsageWriteOutcome::Duplicate
        );
        let mut conflict = replay.clone();
        conflict.provider_call_id = "scenario-priced-conflict".to_string();
        conflict.input_tokens_uncached = Some(801);
        assert!(runtime.record_provider_call_usage(&conflict).await.is_err());

        let mut before_adoption = completed_record(
            "scenario-before-adoption",
            "scenario-thread",
            "response-before-adoption",
            "ok",
            Some((800, 200, 0, 25, 1_025)),
        );
        before_adoption.actual_model_used = Some("gpt-6.1-sol".to_string());
        before_adoption.actual_service_tier = None;
        before_adoption.actual_service_tier_source = None;
        before_adoption.fast_mode_used = None;
        before_adoption.billing_surface = None;
        before_adoption.account_plan = None;
        before_adoption.started_at = "2026-09-29T23:51:59Z".to_string();

        let mut unknown_model = completed_record(
            "scenario-unknown-model",
            "scenario-thread",
            "response-unknown-model",
            "ok",
            Some((800, 200, 0, 25, 1_025)),
        );
        unknown_model.actual_model_used = Some("gpt-unlisted-fixture".to_string());
        unknown_model.actual_service_tier = None;
        unknown_model.actual_service_tier_source = None;
        unknown_model.fast_mode_used = None;
        unknown_model.billing_surface = None;
        unknown_model.account_plan = None;

        let mut requested_only = completed_record(
            "scenario-requested-only",
            "scenario-thread",
            "response-requested-only",
            "ok",
            Some((800, 200, 0, 25, 1_025)),
        );
        requested_only.requested_model = "gpt-6.1-sol".to_string();
        requested_only.actual_model_used = None;
        requested_only.actual_service_tier = None;
        requested_only.actual_service_tier_source = None;
        requested_only.fast_mode_used = None;
        requested_only.billing_surface = None;
        requested_only.account_plan = None;

        let mut missing_usage = completed_record(
            "scenario-missing-usage",
            "scenario-thread",
            "response-missing-usage",
            "provider_usage_missing",
            /*usage*/ None,
        );
        missing_usage.actual_model_used = Some("gpt-6.1-sol".to_string());
        missing_usage.actual_service_tier = None;
        missing_usage.actual_service_tier_source = None;
        missing_usage.fast_mode_used = None;
        missing_usage.billing_surface = None;
        missing_usage.account_plan = None;

        let mut cache_write = completed_record(
            "scenario-cache-write",
            "scenario-thread",
            "response-cache-write",
            "ok",
            Some((800, 200, 1, 25, 1_026)),
        );
        cache_write.actual_model_used = Some("gpt-6.1-sol".to_string());
        cache_write.actual_service_tier = None;
        cache_write.actual_service_tier_source = None;
        cache_write.fast_mode_used = None;
        cache_write.billing_surface = None;
        cache_write.account_plan = None;

        let mut ambiguous = completed_record(
            "scenario-ambiguous-rate",
            "scenario-thread",
            "response-ambiguous-rate",
            "ok",
            Some((800, 200, 0, 25, 1_025)),
        );
        ambiguous.actual_model_used = Some("gpt-6.1-sol".to_string());
        ambiguous.actual_service_tier = None;
        ambiguous.actual_service_tier_source = None;
        ambiguous.fast_mode_used = None;
        ambiguous.billing_surface = None;
        ambiguous.account_plan = None;
        ambiguous.started_at = "2026-10-01T00:00:00Z".to_string();

        for record in [
            before_adoption,
            unknown_model,
            requested_only,
            missing_usage,
            cache_write,
            ambiguous,
        ] {
            runtime
                .record_provider_call_usage(&record)
                .await
                .expect("persist scenario negative fixture");
        }

        // Differently cased legacy rate data can bypass the historical
        // case-sensitive overlap trigger while matching the scenario's
        // case-insensitive model comparison. The estimate must fail closed.
        sqlx::query(
            r#"
            INSERT INTO usage_codex_credit_rates (
                rate_id, provider, model, service_tier, speed_mode,
                rate_card_kind, credits_per_1m_uncached_input,
                credits_per_1m_cached_input, credits_per_1m_output,
                effective_from, effective_to, source_url, source_observed_at
            )
            SELECT
                'fixture-casefold-ambiguous', provider, upper(model),
                service_tier, speed_mode, rate_card_kind,
                credits_per_1m_uncached_input, credits_per_1m_cached_input,
                credits_per_1m_output, '2026-10-01T00:00:00Z',
                '2026-10-02T00:00:00Z', source_url, source_observed_at
            FROM usage_codex_credit_rates
            WHERE rate_id = 'openai-gpt-6.1-sol-standard-20260930'
            "#,
        )
        .execute(runtime.usage_pool().as_ref())
        .await
        .expect("seed a case-normalized ambiguous rate negative");

        let strict = sqlx::query_as::<_, (String, Option<f64>)>(
            "SELECT pricing_status, estimated_total_credits FROM usage_provider_call_credit_estimates WHERE provider_call_id = ?",
        )
        .bind("scenario-priced")
        .fetch_one(runtime.usage_pool().as_ref())
        .await
        .expect("read strict billing-context view independently");
        assert_eq!(strict, ("actual_tier_missing".to_string(), None));

        runtime.close().await;
        let reopened = StateRuntime::init(sqlite, "openai".to_string())
            .await
            .expect("reopen isolated state runtime");
        type StandardScenarioRow = (
            String,
            Option<f64>,
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            String,
            Option<String>,
            String,
            String,
        );
        let priced = sqlx::query_as::<_, StandardScenarioRow>(
            r#"
            SELECT scenario_status, estimated_total_credits,
                   estimate_scenario, assumption_source,
                   assumed_rate_provider, assumed_rate_card_kind,
                   estimate_unit, rate_source_observed_at, model_evidence,
                   requested_model, observed_model, assumed_service_tier,
                   assumed_speed_mode
            FROM usage_provider_call_standard_rate_estimates
            WHERE provider_call_id = ?
            "#,
        )
        .bind("scenario-priced")
        .fetch_one(reopened.usage_pool().as_ref())
        .await
        .expect("read separate standard-rate scenario after reopen");
        assert_eq!(priced.0, "priced_scenario_estimate");
        assert!((priced.1.expect("priced total") - 0.04675).abs() < 1e-12);
        assert_eq!(
            (
                priced.2.as_str(),
                priced.3.as_str(),
                priced.4.as_str(),
                priced.5.as_str(),
                priced.6.as_str(),
                priced.7.as_deref(),
                priced.8.as_deref(),
                priced.9.as_str(),
                priced.10.as_deref(),
                priced.11.as_str(),
                priced.12.as_str(),
            ),
            (
                "operator_supplied_standard_rate_card_scenario",
                "operator_supplied_credit_rate_guide",
                "openai",
                "codex_token_based",
                "credits",
                Some("2026-09-29T23:52:00Z"),
                Some("actual_model_used"),
                "gpt-6-luna",
                Some("GPT-6.1-SOL"),
                "default",
                "standard",
            )
        );

        for (provider_call_id, expected_status) in [
            ("scenario-before-adoption", "rate_not_effective"),
            ("scenario-unknown-model", "model_rate_missing"),
            ("scenario-requested-only", "actual_model_missing"),
            ("scenario-missing-usage", "provider_usage_missing"),
            ("scenario-cache-write", "cache_write_unsupported"),
            ("scenario-ambiguous-rate", "ambiguous_rate"),
        ] {
            let actual = sqlx::query_as::<_, (String, Option<f64>)>(
                "SELECT scenario_status, estimated_total_credits FROM usage_provider_call_standard_rate_estimates WHERE provider_call_id = ?",
            )
            .bind(provider_call_id)
            .fetch_one(reopened.usage_pool().as_ref())
            .await
            .expect("read explicit scenario partial/unpriced reason");
            assert_eq!(actual, (expected_status.to_string(), None));
        }

        let count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM usage_provider_calls WHERE thread_id = ?",
        )
        .bind("scenario-thread")
        .fetch_one(reopened.usage_pool().as_ref())
        .await
        .expect("count durable response rows after replay and conflict");
        assert_eq!(count, 7);
        assert_eq!(
            reopened
                .record_provider_call_usage(&replay)
                .await
                .expect("replay after reopen stays idempotent"),
            ProviderCallUsageWriteOutcome::Duplicate
        );
        let reopened_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM usage_provider_calls WHERE thread_id = ?",
        )
        .bind("scenario-thread")
        .fetch_one(reopened.usage_pool().as_ref())
        .await
        .expect("count remains stable after replay");
        assert_eq!(reopened_count, 7);

        reopened.close().await;
        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }
}
