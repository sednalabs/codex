use chrono::Utc;
use codex_extension_api::ExtensionData;
use codex_extension_api::LocalRequestIdentity;
use codex_extension_api::ProviderEvidenceAuthority;
use codex_extension_api::ProviderLimitEvidence;
use codex_extension_api::RateLimitDomain;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrKind;
use codex_protocol::error::CodexErrSource;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnAbortReason;
use std::time::Duration;

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
        self.emit_turn_error_lifecycle_with_domain(
            turn_context,
            error,
            /*error_kind*/ None,
            /*rate_limit_retry_after*/ None,
            self.fresh_unknown_rate_limit_domain(turn_context),
        )
        .await;
    }

    /// Emits lifecycle callbacks using only the exact error observed by Core.
    ///
    /// The rate-limit domain is a fresh per-callback diagnostic projection. It is never read
    /// from or written to the thread extension store, whose contributor-writable state is not
    /// authority for provider evidence.
    pub(crate) async fn emit_turn_error_lifecycle_for_error(
        &self,
        turn_context: &TurnContext,
        error: &CodexErr,
    ) {
        let rate_limit_retry_after = match error.details() {
            CodexErrorDetails::UsageLimitReached(details) => error.retry_delay().or_else(|| {
                details
                    .resets_at
                    .and_then(|reset_at| (reset_at - Utc::now()).to_std().ok())
            }),
            _ => None,
        };
        let rate_limit_domain = match error.details() {
            CodexErrorDetails::UsageLimitReached(details) => RateLimitDomain {
                local_request_identity: LocalRequestIdentity {
                    thread_id: self.thread_id(),
                    configured_provider_key: Some(turn_context.config.model_provider_id.clone()),
                    requested_model: turn_context.config.model.clone(),
                    resolved_model: Some(turn_context.model_info.slug.clone()),
                },
                provider_limit_evidence: ProviderLimitEvidence {
                    authority: match error.codex_source() {
                        Some(CodexErrSource::RecognizedHttpUsageLimit) => {
                            ProviderEvidenceAuthority::RecognizedHttpUsageLimit
                        }
                        None => ProviderEvidenceAuthority::UnknownLostProvenance,
                    },
                    snapshot: details.rate_limits.as_deref().cloned(),
                    reset_at: details.resets_at.map(|reset_at| reset_at.to_rfc3339()),
                    retry_after: rate_limit_retry_after,
                },
            },
            _ => self.fresh_unknown_rate_limit_domain(turn_context),
        };
        self.emit_turn_error_lifecycle_with_domain(
            turn_context,
            error.to_codex_protocol_error(),
            Some(error.kind()),
            rate_limit_retry_after,
            rate_limit_domain,
        )
        .await;
    }

    fn fresh_unknown_rate_limit_domain(&self, turn_context: &TurnContext) -> RateLimitDomain {
        RateLimitDomain {
            local_request_identity: LocalRequestIdentity {
                thread_id: self.thread_id(),
                configured_provider_key: Some(turn_context.config.model_provider_id.clone()),
                requested_model: turn_context.config.model.clone(),
                resolved_model: Some(turn_context.model_info.slug.clone()),
            },
            provider_limit_evidence: ProviderLimitEvidence {
                authority: ProviderEvidenceAuthority::UnknownUnsupportedTransport,
                snapshot: None,
                reset_at: None,
                retry_after: None,
            },
        }
    }

    async fn emit_turn_error_lifecycle_with_domain(
        &self,
        turn_context: &TurnContext,
        error: CodexErrorInfo,
        error_kind: Option<CodexErrKind>,
        rate_limit_retry_after: Option<Duration>,
        rate_limit_domain: RateLimitDomain,
    ) {
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
