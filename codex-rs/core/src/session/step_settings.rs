//! Configured inputs used to derive effective step settings.

use crate::config::Constrained;
use crate::config::ConstraintError;
use crate::config::ConstraintResult;
use codex_config::ConfigRequirements;
use codex_models_manager::ModelsManagerConfig;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::Personality;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AskForApproval;

/// Model and execution settings selected for an individual model step within
/// a turn. A turn may contain several steps, each using its own captured settings.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StepSettings {
    pub(crate) collaboration_mode: CollaborationMode,
    pub(crate) reasoning_summary: Option<ReasoningSummary>,
    pub(crate) service_tier: Option<String>,
    pub(crate) personality: Option<Personality>,
    pub(crate) approval_policy: Constrained<AskForApproval>,
    pub(crate) approvals_reviewer: ApprovalsReviewer,
}

/// Explicit startup overrides applied to catalog-derived model metadata.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ModelInfoOverrides {
    pub(crate) context_window: Option<i64>,
    pub(crate) auto_compact_token_limit: Option<i64>,
    pub(crate) tool_output_token_limit: Option<usize>,
    pub(crate) base_instructions: Option<String>,
}

impl From<ModelsManagerConfig> for ModelInfoOverrides {
    fn from(config: ModelsManagerConfig) -> Self {
        Self {
            context_window: config.model_context_window,
            auto_compact_token_limit: config.model_auto_compact_token_limit,
            tool_output_token_limit: config.tool_output_token_limit,
            base_instructions: config.base_instructions,
        }
    }
}

impl ModelInfoOverrides {
    pub(crate) fn models_manager_config(
        &self,
        personality: Option<Personality>,
        personality_enabled: bool,
    ) -> ModelsManagerConfig {
        ModelsManagerConfig {
            model_context_window: self.context_window,
            model_auto_compact_token_limit: self.auto_compact_token_limit,
            tool_output_token_limit: self.tool_output_token_limit,
            base_instructions: self.base_instructions.clone(),
            personality,
            personality_enabled,
            // The models manager already owns its catalog.
            model_catalog: None,
        }
    }
}

/// Sparse edits applied independently to each settings owner.
///
/// Do not materialize a partial update against one settings owner and reuse
/// that full value for another. Merge the requested edits with each target.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct StepSettingsUpdate {
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<Option<ReasoningEffort>>,
    /// A complete collaboration mode takes precedence over model and effort edits.
    pub(crate) collaboration_mode: Option<CollaborationMode>,
    pub(crate) reasoning_summary: Option<ReasoningSummary>,
    pub(crate) service_tier: Option<Option<String>>,
    pub(crate) personality: Option<Personality>,
    pub(crate) approval_policy: Option<AskForApproval>,
    pub(crate) approvals_reviewer: Option<ApprovalsReviewer>,
}

/// Constraints used when applying and validating a candidate settings version.
pub(crate) struct StepSettingsConstraints<'a> {
    pub(crate) requirements: &'a ConfigRequirements,
    pub(crate) guardian_approval_enabled: bool,
    pub(crate) trusted_guardian_reviewer: bool,
    pub(crate) has_full_disk_write_access: bool,
}

impl StepSettings {
    /// Applies edits and validates the result against the supplied constraints.
    /// Callers must supply the constraints of the proposed target environment.
    pub(crate) fn apply(
        &self,
        update: &StepSettingsUpdate,
        constraints: &StepSettingsConstraints<'_>,
    ) -> ConstraintResult<Self> {
        let mut next = self.clone();
        next.collaboration_mode = update.collaboration_mode.clone().unwrap_or_else(|| {
            self.collaboration_mode.with_updates(
                update.model.clone(),
                update.effort.clone(),
                /*developer_instructions*/ None,
            )
        });
        if let Some(summary) = update.reasoning_summary {
            next.reasoning_summary = Some(summary);
        }
        if let Some(service_tier) = update.service_tier.clone() {
            // TODO(aibrahim): Remove once v2 clients no longer send the legacy
            // "fast" service tier value.
            next.service_tier = Some(match service_tier {
                Some(service_tier) => ServiceTier::from_request_value(&service_tier)
                    .map_or(service_tier, |service_tier| {
                        service_tier.request_value().to_string()
                    }),
                None => SERVICE_TIER_DEFAULT_REQUEST_VALUE.to_string(),
            });
        }
        if let Some(personality) = update.personality {
            next.personality = Some(personality);
        }
        if let Some(approval_policy) = update.approval_policy {
            next.approval_policy.set(approval_policy)?;
        }
        if let Some(approvals_reviewer) = update.approvals_reviewer {
            constraints
                .requirements
                .approvals_reviewer
                .can_set(&approvals_reviewer)?;
            next.approvals_reviewer = approvals_reviewer;
        }
        if !constraints.trusted_guardian_reviewer
            && self.collaboration_mode.model() != next.collaboration_mode.model()
            && constraints
                .requirements
                .auto_review_required_for_model(next.collaboration_mode.model())
            && update.approvals_reviewer.is_none()
        {
            constraints
                .requirements
                .approvals_reviewer
                .can_set(&ApprovalsReviewer::AutoReview)?;
            next.approvals_reviewer = ApprovalsReviewer::AutoReview;
        }
        next.validate(constraints)?;
        Ok(next)
    }

    /// Checks directly constructed settings or rechecks changed constraints.
    pub(super) fn validate(
        &self,
        constraints: &StepSettingsConstraints<'_>,
    ) -> ConstraintResult<()> {
        if constraints.trusted_guardian_reviewer {
            return Ok(());
        }

        let model = self.collaboration_mode.model();
        if !constraints
            .requirements
            .auto_review_required_for_model(model)
        {
            return Ok(());
        }

        if self.approvals_reviewer == ApprovalsReviewer::AutoReview
            && !constraints.has_full_disk_write_access
            && constraints.guardian_approval_enabled
        {
            return Ok(());
        }

        Err(ConstraintError::AutoReviewRequired {
            model: model.to_string(),
        })
    }
}

#[cfg(test)]
#[path = "step_settings_tests.rs"]
mod tests;
