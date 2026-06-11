//! Thread settings sync between TUI-local state and app-server thread state.

use super::App;
use crate::app_command::AppCommand;
use crate::app_server_session::AppServerSession;
use crate::permission_compat::legacy_compatible_permission_profile;
use crate::session_state::ThreadSessionState;
use codex_app_server_protocol::ApprovalsReviewer as AppServerApprovalsReviewer;
use codex_app_server_protocol::SandboxPolicy as AppServerSandboxPolicy;
use codex_app_server_protocol::ThreadSettings;
use codex_app_server_protocol::ThreadSettingsUpdateParams;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ModeKind;
use codex_protocol::models::ActivePermissionProfile;
use codex_protocol::models::PermissionProfile;
use std::path::Path;

impl App {
    pub(super) async fn sync_active_thread_model_setting(
        &mut self,
        app_server: &mut AppServerSession,
        model: String,
    ) {
        let Some(params) = self.active_thread_model_setting_update_params(model) else {
            return;
        };
        self.send_thread_settings_update(app_server, params).await;
    }

    pub(super) fn active_thread_model_setting_update_params(
        &self,
        model: String,
    ) -> Option<ThreadSettingsUpdateParams> {
        let thread_id = self.active_thread_id?;
        Some(ThreadSettingsUpdateParams {
            thread_id: thread_id.to_string(),
            model: Some(model),
            collaboration_mode: Some(self.chat_widget.effective_collaboration_mode()),
            ..ThreadSettingsUpdateParams::default()
        })
    }

    pub(super) async fn sync_active_thread_reasoning_setting(
        &mut self,
        app_server: &mut AppServerSession,
        effort: Option<codex_protocol::openai_models::ReasoningEffort>,
    ) {
        let Some(params) = self.active_thread_reasoning_setting_update_params(effort) else {
            return;
        };
        self.send_thread_settings_update(app_server, params).await;
    }

    pub(super) fn active_thread_reasoning_setting_update_params(
        &self,
        effort: Option<codex_protocol::openai_models::ReasoningEffort>,
    ) -> Option<ThreadSettingsUpdateParams> {
        let thread_id = self.active_thread_id?;
        Some(ThreadSettingsUpdateParams {
            thread_id: thread_id.to_string(),
            effort,
            collaboration_mode: Some(self.chat_widget.current_collaboration_mode().clone()),
            ..ThreadSettingsUpdateParams::default()
        })
    }

    pub(super) async fn sync_active_thread_plan_mode_reasoning_setting(
        &mut self,
        app_server: &mut AppServerSession,
    ) {
        let Some(thread_id) = self.active_thread_id else {
            return;
        };
        let params = ThreadSettingsUpdateParams {
            thread_id: thread_id.to_string(),
            collaboration_mode: Some(self.chat_widget.effective_collaboration_mode()),
            ..ThreadSettingsUpdateParams::default()
        };
        self.send_thread_settings_update(app_server, params).await;
    }

    pub(super) async fn sync_active_thread_personality_setting(
        &mut self,
        app_server: &mut AppServerSession,
        personality: codex_protocol::config_types::Personality,
    ) {
        let Some(thread_id) = self.active_thread_id else {
            return;
        };
        let params = ThreadSettingsUpdateParams {
            thread_id: thread_id.to_string(),
            personality: Some(personality),
            ..ThreadSettingsUpdateParams::default()
        };
        self.send_thread_settings_update(app_server, params).await;
    }

    pub(super) async fn sync_override_turn_context_settings(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        op: &AppCommand,
    ) {
        let AppCommand::OverrideTurnContext {
            cwd,
            approval_policy,
            approvals_reviewer,
            permission_profile,
            active_permission_profile,
            windows_sandbox_level: _,
            model,
            effort,
            summary,
            service_tier,
            collaboration_mode,
            personality,
        } = op
        else {
            return;
        };
        let config = self.chat_widget.config_ref();
        let permissions_cwd = cwd.as_deref().unwrap_or(config.cwd.as_path());
        let (sandbox_policy, permissions) = thread_settings_permission_overrides(
            permission_profile.as_ref(),
            active_permission_profile.as_ref(),
            permissions_cwd,
        );

        let params = ThreadSettingsUpdateParams {
            thread_id: thread_id.to_string(),
            cwd: cwd.clone(),
            approval_policy: *approval_policy,
            approvals_reviewer: approvals_reviewer.map(AppServerApprovalsReviewer::from),
            sandbox_policy,
            permissions,
            model: model.clone(),
            effort: effort.clone().unwrap_or_default(),
            summary: *summary,
            service_tier: service_tier.clone(),
            collaboration_mode: collaboration_mode.clone(),
            personality: *personality,
            ..ThreadSettingsUpdateParams::default()
        };
        self.send_thread_settings_update(app_server, params).await;
    }

    pub(super) async fn apply_thread_settings_to_cached_session(
        &mut self,
        thread_id: ThreadId,
        settings: &ThreadSettings,
    ) {
        if self.primary_thread_id == Some(thread_id)
            && let Some(session) = self.primary_session_configured.as_mut()
        {
            apply_thread_settings_to_session(session, settings);
        }

        if let Some(channel) = self.thread_event_channels.get(&thread_id) {
            let mut store = channel.store.lock().await;
            if let Some(session) = store.session.as_mut() {
                apply_thread_settings_to_session(session, settings);
            }
        }
    }

    async fn send_thread_settings_update(
        &mut self,
        app_server: &mut AppServerSession,
        params: ThreadSettingsUpdateParams,
    ) {
        if !thread_settings_update_has_changes(&params) {
            return;
        }
        if let Err(err) = app_server.thread_settings_update(params).await {
            tracing::warn!("failed to update app-server thread settings from TUI: {err}");
            self.chat_widget
                .add_error_message(format!("Failed to update thread settings: {err}"));
        }
    }
}

fn thread_settings_permission_overrides(
    permission_profile: Option<&PermissionProfile>,
    active_permission_profile: Option<&ActivePermissionProfile>,
    cwd: &Path,
) -> (Option<AppServerSandboxPolicy>, Option<String>) {
    if let Some(active_permission_profile) = active_permission_profile {
        return (None, Some(active_permission_profile.id.clone()));
    }

    let Some(permission_profile) = permission_profile else {
        return (None, None);
    };

    let legacy_profile = legacy_compatible_permission_profile(permission_profile, cwd);
    let sandbox_policy = legacy_profile
        .to_legacy_sandbox_policy(cwd)
        .unwrap_or_else(|err| {
            unreachable!("legacy-compatible permissions must project to legacy policy: {err}")
        });
    (Some(AppServerSandboxPolicy::from(sandbox_policy)), None)
}

fn apply_thread_settings_to_session(session: &mut ThreadSessionState, settings: &ThreadSettings) {
    if settings.collaboration_mode.mode == ModeKind::Default {
        session.model = settings.model.clone();
        session.reasoning_effort = settings.effort.clone();
    }
    session.model_provider_id = settings.model_provider.clone();
    session.service_tier = settings.service_tier.clone();
    session.approval_policy = settings.approval_policy;
    session.approvals_reviewer = settings.approvals_reviewer.to_core();
    session.permission_profile = PermissionProfile::from_legacy_sandbox_policy_for_cwd(
        &settings.sandbox_policy.to_core(),
        settings.cwd.as_path(),
    );
    session.active_permission_profile = settings.active_permission_profile.clone().map(Into::into);
    session.set_cwd_retargeting_implicit_runtime_workspace_root(settings.cwd.clone());
    session.personality = settings.personality;
    let mut collaboration_mode = settings.collaboration_mode.clone();
    collaboration_mode
        .settings
        .model
        .clone_from(&settings.model);
    collaboration_mode.settings.reasoning_effort = settings.effort.clone();
    session.collaboration_mode = Some(Box::new(collaboration_mode));
}

fn thread_settings_update_has_changes(params: &ThreadSettingsUpdateParams) -> bool {
    params.cwd.is_some()
        || params.approval_policy.is_some()
        || params.approvals_reviewer.is_some()
        || params.sandbox_policy.is_some()
        || params.permissions.is_some()
        || params.model.is_some()
        || params.service_tier.is_some()
        || params.effort.is_some()
        || params.summary.is_some()
        || params.collaboration_mode.is_some()
        || params.personality.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::SandboxPolicy as AppServerSandboxPolicy;
    use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_WORKSPACE;
    use std::path::Path;

    #[test]
    fn permission_overrides_project_disabled_profile_without_active_profile() {
        let (sandbox_policy, permissions) = thread_settings_permission_overrides(
            Some(&PermissionProfile::Disabled),
            /*active_permission_profile*/ None,
            Path::new("/workspace/project"),
        );

        assert_eq!(
            sandbox_policy,
            Some(AppServerSandboxPolicy::DangerFullAccess)
        );
        assert_eq!(permissions, None);
    }

    #[test]
    fn permission_overrides_prefer_active_profile_identity() {
        let active_profile = ActivePermissionProfile::new(BUILT_IN_PERMISSION_PROFILE_WORKSPACE);
        let (sandbox_policy, permissions) = thread_settings_permission_overrides(
            Some(&PermissionProfile::Disabled),
            Some(&active_profile),
            Path::new("/workspace/project"),
        );

        assert_eq!(sandbox_policy, None);
        assert_eq!(
            permissions.as_deref(),
            Some(BUILT_IN_PERMISSION_PROFILE_WORKSPACE)
        );
    }
}
