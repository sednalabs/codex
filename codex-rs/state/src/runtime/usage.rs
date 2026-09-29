use crate::StateRuntime;

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

impl StateRuntime {
    /// Persist one already-completed response without aggregating across turn
    /// state. Missing provider usage is represented with NULL token fields.
    pub async fn record_provider_call_usage(
        &self,
        record: &ProviderCallUsageRecord,
    ) -> anyhow::Result<()> {
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
        .execute(self.usage_pool.as_ref())
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::ProviderCallUsageRecord;
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
                None,
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
}
