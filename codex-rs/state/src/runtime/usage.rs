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
        assert_eq!(summary, (12, 9, 3, 1));

        reopened.close().await;
        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }
}
