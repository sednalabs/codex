use codex_extension_api::ExtensionData;
use codex_extension_api::RateLimitDomain;
use codex_protocol::error::CodexErrKind;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnAbortReason;

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;

impl Session {
    pub(super) async fn emit_turn_start_lifecycle(
        &self,
        turn_context: &TurnContext,
        token_usage_at_turn_start: &TokenUsage,
    ) {
        let collaboration_mode = turn_context.collaboration_mode();
        for contributor in self.services.extensions.turn_lifecycle_contributors() {
            contributor
                .on_turn_start(codex_extension_api::TurnStartInput {
                    turn_id: turn_context.sub_id.as_str(),
                    collaboration_mode: &collaboration_mode,
                    token_usage_at_turn_start,
                    session_store: &self.services.session_extension_data,
                    thread_store: &self.services.thread_extension_data,
                    turn_store: turn_context.extension_data.as_ref(),
                })
                .await;
        }
    }

    pub(super) async fn emit_turn_stop_lifecycle(&self, turn_store: &ExtensionData) {
        self.services
            .thread_extension_data
            .remove::<codex_extension_api::GoalContinuationHealthCheck>();
        for contributor in self.services.extensions.turn_lifecycle_contributors() {
            contributor
                .on_turn_stop(codex_extension_api::TurnStopInput {
                    session_store: &self.services.session_extension_data,
                    thread_store: &self.services.thread_extension_data,
                    turn_store,
                })
                .await;
        }
    }

    pub(crate) async fn emit_thread_idle_lifecycle_if_idle(&self) {
        if self.active_turn.lock().await.is_some()
            || self.input_queue.has_trigger_turn_mailbox_items().await
        {
            return;
        }

        for contributor in self.services.extensions.thread_lifecycle_contributors() {
            contributor
                .on_thread_idle(codex_extension_api::ThreadIdleInput {
                    session_store: &self.services.session_extension_data,
                    thread_store: &self.services.thread_extension_data,
                })
                .await;
        }
    }

    pub(super) async fn emit_turn_abort_lifecycle(
        &self,
        reason: TurnAbortReason,
        turn_store: &ExtensionData,
    ) {
        self.services
            .thread_extension_data
            .remove::<codex_extension_api::GoalContinuationHealthCheck>();
        for contributor in self.services.extensions.turn_lifecycle_contributors() {
            contributor
                .on_turn_abort(codex_extension_api::TurnAbortInput {
                    reason: reason.clone(),
                    session_store: &self.services.session_extension_data,
                    thread_store: &self.services.thread_extension_data,
                    turn_store,
                })
                .await;
        }
    }

    pub(crate) async fn emit_turn_error_lifecycle(
        &self,
        turn_context: &TurnContext,
        error: CodexErrorInfo,
    ) {
        self.emit_turn_error_lifecycle_with_kind_and_rate_limit_delay(
            turn_context,
            error,
            /*error_kind*/ None,
            /*rate_limit_retry_after*/ None,
        )
        .await;
    }

    pub(crate) async fn emit_turn_error_lifecycle_with_rate_limit_delay(
        &self,
        turn_context: &TurnContext,
        error: CodexErrorInfo,
        rate_limit_retry_after: Option<std::time::Duration>,
    ) {
        self.emit_turn_error_lifecycle_with_kind_and_rate_limit_delay(
            turn_context,
            error,
            /*error_kind*/ None,
            rate_limit_retry_after,
        )
        .await;
    }

    pub(crate) async fn emit_turn_error_lifecycle_with_kind_and_rate_limit_delay(
        &self,
        turn_context: &TurnContext,
        error: CodexErrorInfo,
        error_kind: Option<CodexErrKind>,
        rate_limit_retry_after: Option<std::time::Duration>,
    ) {
        let rate_limit_domain = if matches!(&error, CodexErrorInfo::UsageLimitExceeded) {
            self.services
                .thread_extension_data
                .get::<RateLimitDomain>()
                .map(|domain| (*domain).clone())
        } else {
            None
        }
        .unwrap_or_else(|| RateLimitDomain {
            thread_id: self.thread_id(),
            provider_id: Some(turn_context.config.model_provider_id.clone()),
            requested_model: turn_context.config.model.clone(),
            effective_model: Some(turn_context.model_info.slug.clone()),
            // The host has no authoritative account/quota binding for this callback unless
            // the provider supplies one. Preserve unknown as None rather than inferring it
            // from parentage, process state, or a model name.
            account_context_key: None,
            shared_quota_key: None,
            snapshot: None,
            reset_at: None,
            retry_after: rate_limit_retry_after,
        });
        for contributor in self.services.extensions.turn_lifecycle_contributors() {
            contributor
                .on_turn_error(codex_extension_api::TurnErrorInput {
                    turn_id: turn_context.sub_id.as_str(),
                    error: error.clone(),
                    error_kind,
                    rate_limit_retry_after,
                    session_store: &self.services.session_extension_data,
                    thread_store: &self.services.thread_extension_data,
                    turn_store: turn_context.extension_data.as_ref(),
                    rate_limit_domain: rate_limit_domain.clone(),
                })
                .await;
        }
    }
}
