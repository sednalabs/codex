use super::session::Session;
use super::turn_context::TurnContext;
use crate::client_common::Prompt;
use crate::config::Config;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::openai_models::ModelInfo;

#[derive(Debug)]
pub(crate) struct ContextWindowTokenStatus {
    // Full active context usage, independent of the configured auto-compact scope.
    pub(crate) active_context_tokens: i64,
    // Usage counted against `model_auto_compact_token_limit` for the current scope.
    pub(crate) auto_compact_scope_tokens: i64,
    pub(crate) auto_compact_scope_limit: Option<i64>,
    pub(crate) full_context_window_limit: Option<i64>,
    pub(crate) base_window_tokens_remaining: Option<i64>,
    pub(crate) auto_compact_window_prefill_tokens: Option<i64>,
    pub(crate) full_context_window_limit_reached: bool,
    pub(crate) token_limit_reached: bool,
    pub(crate) turn_end_compaction_threshold_reached: bool,
}

fn tokens_remaining(limit: Option<i64>, used: i64) -> Option<i64> {
    limit.map(|limit| limit.saturating_sub(used).max(0))
}

pub(crate) async fn context_window_token_status(
    sess: &Session,
    turn_context: &TurnContext,
) -> ContextWindowTokenStatus {
    context_window_token_status_with_config(
        sess,
        turn_context.config.as_ref(),
        turn_context.model_info().as_ref(),
    )
    .await
}

pub(crate) async fn context_window_token_status_for_model(
    sess: &Session,
    config: &Config,
    turn_context: &TurnContext,
    model_info: &ModelInfo,
) -> ContextWindowTokenStatus {
    let mut config = config.clone();
    config.token_budget = super::token_budget::resolve_token_budget(
        turn_context.configured_token_budget.as_ref(),
        turn_context.use_model_token_budget_defaults,
        model_info,
    );
    context_window_token_status_with_config(sess, &config, model_info).await
}

async fn context_window_token_status_with_config(
    sess: &Session,
    config: &Config,
    model_info: &ModelInfo,
) -> ContextWindowTokenStatus {
    let active_context_tokens = sess.get_total_token_usage().await;
    let prefill_input_tokens = sess
        .auto_compact_window_snapshot()
        .await
        .prefill_input_tokens;
    context_window_token_status_for_usage(
        config,
        model_info,
        active_context_tokens,
        prefill_input_tokens,
    )
}

/// Sampling and direct WebSocket prewarm must charge the same transformed request view.
/// Do not add the estimate to server usage: previous responses may already include overlays.
/// Taking the larger value preserves measured usage without double-charging that history.
pub(crate) async fn check_instruction_overlay_request(
    sess: &Session,
    config: &Config,
    model_info: &ModelInfo,
    prompt: &Prompt,
) -> Result<()> {
    let Some(estimated_tokens) = sess
        .services
        .model_client
        .estimate_instruction_overlay_request(prompt, model_info)?
    else {
        return Ok(());
    };
    let active_context_tokens = sess.get_total_token_usage().await.max(estimated_tokens);
    let prefill_input_tokens = sess
        .auto_compact_window_snapshot()
        .await
        .prefill_input_tokens;
    let status = context_window_token_status_for_usage(
        config,
        model_info,
        active_context_tokens,
        prefill_input_tokens,
    );
    if status.token_limit_reached {
        return Err(CodexErr::ContextWindowExceeded);
    }
    Ok(())
}

fn context_window_token_status_for_usage(
    config: &Config,
    model_info: &ModelInfo,
    active_context_tokens: i64,
    prefill_input_tokens: Option<i64>,
) -> ContextWindowTokenStatus {
    // Count either the full active context or only the tokens added after the initial prefix.
    let (auto_compact_scope_tokens, auto_compact_scope_limit, auto_compact_window_prefill_tokens) =
        match config.model_auto_compact_token_limit_scope {
            AutoCompactTokenLimitScope::Total => (
                active_context_tokens,
                model_info.auto_compact_token_limit(),
                None,
            ),
            AutoCompactTokenLimitScope::BodyAfterPrefix => {
                let baseline = prefill_input_tokens.unwrap_or(active_context_tokens);

                let scope_limit = config
                    .model_auto_compact_token_limit
                    .or_else(|| model_info.auto_compact_token_limit());
                (
                    active_context_tokens.saturating_sub(baseline),
                    scope_limit,
                    prefill_input_tokens,
                )
            }
        };

    // The model's full context window is a hard cap, independent of the auto-compaction scope.
    let full_context_window_limit = model_info.resolved_context_window().map(|context_window| {
        context_window.saturating_mul(model_info.effective_context_window_percent) / 100
    });

    // Report remaining tokens against the base (unbuffered) window, capped by the full context.
    let base_window_tokens_remaining = [
        tokens_remaining(auto_compact_scope_limit, auto_compact_scope_tokens),
        tokens_remaining(full_context_window_limit, active_context_tokens),
    ]
    .into_iter()
    .flatten()
    .min();

    // Only reserve the fallback buffer when there is a fallback prompt to use it.
    let auto_compact_fallback_buffer_tokens = config
        .token_budget
        .as_ref()
        .map_or(0, crate::config::TokenBudgetConfig::fallback_buffer_tokens);
    let buffered_auto_compact_limit = auto_compact_scope_limit
        .map(|limit| limit.saturating_add(auto_compact_fallback_buffer_tokens));

    // Force compaction once the buffered window or the model's full context window is reached.
    let full_context_window_limit_reached =
        full_context_window_limit.is_some_and(|limit| active_context_tokens >= limit);
    let token_limit_reached = buffered_auto_compact_limit
        .is_some_and(|limit| auto_compact_scope_tokens >= limit)
        || full_context_window_limit_reached;
    let post_turn_percent = config.model_post_turn_compact_threshold_percent;
    let turn_end_compaction_threshold_reached = post_turn_percent > 0
        && (token_limit_reached
            || full_context_window_limit.is_some_and(|limit| {
                i128::from(active_context_tokens) * 100
                    >= i128::from(limit) * i128::from(post_turn_percent)
            }));

    ContextWindowTokenStatus {
        active_context_tokens,
        auto_compact_scope_tokens,
        auto_compact_scope_limit,
        full_context_window_limit,
        base_window_tokens_remaining,
        auto_compact_window_prefill_tokens,
        full_context_window_limit_reached,
        token_limit_reached,
        turn_end_compaction_threshold_reached,
    }
}

#[cfg(test)]
mod instruction_overlay_budget_tests {
    use super::*;
    use crate::config::ConfigBuilder;

    #[tokio::test]
    async fn effective_request_usage_drives_total_body_and_full_window_arithmetic() {
        let home = tempfile::tempdir().unwrap();
        let mut config = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(home.path().to_path_buf())
            .fallback_cwd(/*fallback_cwd*/ Some(home.path().to_path_buf()))
            .build()
            .await
            .unwrap();
        config.token_budget = None;
        let mut model = codex_models_manager::model_info::model_info_from_slug("gpt-5.4");
        model.context_window = Some(1_000);
        model.effective_context_window_percent = 100;
        model.auto_compact_token_limit = Some(800);
        config.model_auto_compact_token_limit_scope = AutoCompactTokenLimitScope::Total;
        let total = context_window_token_status_for_usage(
            &config,
            &model,
            /*active_context_tokens*/ 900,
            /*prefill_input_tokens*/ Some(600),
        );
        assert_eq!(total.active_context_tokens, 900);
        assert_eq!(total.auto_compact_scope_tokens, 900);
        assert!(total.token_limit_reached);
        assert_eq!(total.base_window_tokens_remaining, Some(0));
        config.model_auto_compact_token_limit_scope = AutoCompactTokenLimitScope::BodyAfterPrefix;
        config.model_auto_compact_token_limit = Some(400);
        let body = context_window_token_status_for_usage(
            &config,
            &model,
            /*active_context_tokens*/ 900,
            /*prefill_input_tokens*/ Some(600),
        );
        assert_eq!(body.auto_compact_scope_tokens, 300);
        assert_eq!(body.base_window_tokens_remaining, Some(100));
        assert!(!body.token_limit_reached);
        let expanded = context_window_token_status_for_usage(
            &config,
            &model,
            /*active_context_tokens*/ 1_500,
            /*prefill_input_tokens*/ Some(1_400),
        );
        assert_eq!(expanded.auto_compact_scope_tokens, 100);
        assert!(expanded.full_context_window_limit_reached);
        assert!(expanded.token_limit_reached);
        assert_eq!(expanded.base_window_tokens_remaining, Some(0));
    }
}
