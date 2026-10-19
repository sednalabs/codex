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
/// `response_id` is stored in the existing `request_id` column; the separate
/// idempotency table provides its provider/account/thread-scoped durable key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderCallUsageRecord {
    pub provider_call_id: String,
    pub thread_id: String,
    pub turn_id: String,
    pub provider: String,
    /// Stable provider-account scope when authoritative; `None` stays unknown.
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageThreadRecord {
    pub thread_id: String,
    pub parent_thread_id: Option<String>,
    pub root_thread_id: Option<String>,
    pub fork_parent_thread_id: Option<String>,
    pub source: Option<String>,
}

impl StateRuntime {
    /// Persist one completed response without aggregating across turn state.
    /// Missing provider usage is represented with NULL token fields.
    pub async fn record_provider_call_usage(
        &self,
        record: &ProviderCallUsageRecord,
    ) -> anyhow::Result<ProviderCallUsageWriteOutcome> {
        ensure!(!record.provider_call_id.trim().is_empty(), "provider_call_id is required");
        ensure!(!record.provider.trim().is_empty(), "provider is required");
        ensure!(!record.thread_id.trim().is_empty(), "thread_id is required");
        ensure!(!record.turn_id.trim().is_empty(), "turn_id is required");
        ensure!(!record.response_id.trim().is_empty(), "response_id is required for idempotency");
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

        if record.provider_account_scope.is_none()
            && let Some(existing_call_id) = find_legacy_provider_call(&mut transaction, record).await?
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

    /// Persist or reconcile the exact lineage for a root, child, or resumed thread.
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
            ensure!(!parent.trim().is_empty(), "thread lineage IDs cannot be empty");
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

        let parent_thread_id = record.parent_thread_id.clone().or(existing_parent);
        let fork_parent_thread_id = record
            .fork_parent_thread_id
            .clone()
            .or(existing_fork_parent);
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
    let existing = sqlx::query_scalar::<_, String>(
        "SELECT p.provider_call_id FROM usage_provider_calls AS p WHERE p.provider = ? AND p.thread_id = ? AND p.request_id = ? AND NOT EXISTS (SELECT 1 FROM usage_response_idempotency AS idempotency WHERE idempotency.provider_call_id = p.provider_call_id) ORDER BY p.provider_call_id LIMIT 2",
    )
    .bind(&record.provider)
    .bind(&record.thread_id)
    .bind(&record.response_id)
    .fetch_all(&mut **transaction)
    .await?;
    ensure!(existing.len() <= 1, "ambiguous legacy rows for provider response identity");
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
    let optional_string_matches = |column: &str, incoming: &Option<String>| -> anyhow::Result<bool> {
        let stored: Option<String> = row.try_get(column)?;
        Ok(stored.as_deref() == incoming.as_deref())
    };
    let optional_bool_matches = |column: &str, incoming: Option<bool>| -> anyhow::Result<bool> {
        let stored: Option<i64> = row.try_get(column)?;
        Ok(stored == incoming.map(|value| if value { 1_i64 } else { 0_i64 }))
    };
    Ok(row.try_get::<String, _>("thread_id")? == record.thread_id
        && row.try_get::<Option<String>, _>("turn_id")?.as_deref() == Some(record.turn_id.as_str())
        && row.try_get::<Option<String>, _>("provider")?.as_deref() == Some(record.provider.as_str())
        && row.try_get::<Option<String>, _>("requested_model")?.as_deref()
            == Some(record.requested_model.as_str())
        && optional_string_matches("actual_model_used", &record.actual_model_used)?
        && row.try_get::<Option<String>, _>("request_id")?.as_deref()
            == Some(record.response_id.as_str())
        && optional_string_matches("requested_service_tier", &record.requested_service_tier)?
        && optional_string_matches("actual_service_tier", &record.actual_service_tier)?
        && optional_string_matches("actual_service_tier_source", &record.actual_service_tier_source)?
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
