use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use crate::agent::api::AgentControl;
use crate::agent::control::LocalAgentRuntime;
use crate::agents_md_manager::AgentsMdManager;
use crate::attestation::AttestationProvider;
use crate::client::ModelClient;
use crate::client::ProviderAuthority;
use crate::config::NetworkProxyAuditMetadata;
use crate::config::StartedNetworkProxy;
use crate::current_time::TimeProvider;
use crate::elicitation::ElicitationService;
use crate::environment_selection::ThreadEnvironments;
use crate::exec_policy::ExecPolicyManager;
use crate::mcp::McpManager;
use crate::mcp_tool_exposure::McpHandlerCache;
use crate::tools::ExecutedToolCalls;
use crate::tools::code_mode::CodeModeService;
use crate::tools::handlers::ToolSearchHandlerCache;
use crate::tools::network_approval::NetworkApprovalService;
use crate::tools::sandboxing::ApprovalStore;
use crate::unified_exec::UnifiedExecProcessManager;
use arc_swap::ArcSwap;
use arc_swap::ArcSwapOption;
use codex_analytics::AnalyticsEventsClient;
use codex_attachment_store::AttachmentStore;
use codex_core_plugins::PluginsManager;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionDataInit;
use codex_extension_api::ExtensionRegistry;
use codex_hooks::Hooks;
use codex_http_client::RouteAwareClientPool;
use codex_login::AuthManager;
use codex_mcp::McpRuntime;
use codex_models_manager::manager::SharedModelsManager;
use codex_otel::SessionTelemetry;
use codex_protocol::ThreadId;
use codex_protocol::automatic_turn::AutomaticTurnContinuationRef;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_rollout::state_db::StateDbHandle;
use codex_rollout_trace::ThreadTraceContext;
use codex_sandboxing::policy_transforms::merge_permission_profiles;
use codex_skills_extension::HostSkillsService;
use codex_thread_store::LiveThread;
use codex_thread_store::ThreadStore;
use codex_utils_git_discovery::GitRootDiscovery;
use tokio::runtime::Handle;
use tokio::sync::Mutex;

pub(crate) struct SessionServices {
    /// The single owner of live MCP connections for this thread.
    pub(crate) mcp_runtime: Arc<McpRuntime>,
    /// Immutable MCP handlers scoped to this thread's current binding.
    pub(crate) mcp_handler_cache: McpHandlerCache,
    pub(crate) unified_exec_manager: UnifiedExecProcessManager,
    pub(crate) elicitations: ElicitationService,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) shell_zsh_path: Option<PathBuf>,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) main_execve_wrapper_exe: Option<PathBuf>,
    pub(crate) analytics_events_client: AnalyticsEventsClient,
    pub(crate) hooks: ArcSwap<Hooks>,
    pub(crate) rollout_thread_trace: ThreadTraceContext,
    pub(crate) user_shell: Arc<crate::shell::Shell>,
    pub(crate) show_raw_agent_reasoning: bool,
    pub(crate) exec_policy: Arc<ExecPolicyManager>,
    pub(crate) auth_manager: Arc<AuthManager>,
    /// Upload-only clients shared across turns without logging signed blob URLs.
    pub(crate) openai_file_upload_client_pool: RouteAwareClientPool,
    pub(crate) models_manager: SharedModelsManager,
    pub(crate) git_root_discovery: Arc<GitRootDiscovery>,
    pub(crate) session_telemetry: SessionTelemetry,
    pub(crate) tool_approvals: Mutex<ApprovalStore>,
    /// Shared with captured steps so later calls observe newly approved permissions.
    pub(crate) granted_permissions_by_environment_id:
        Arc<StdMutex<HashMap<String, AdditionalPermissionProfile>>>,
    pub(crate) runtime_handle: Handle,
    pub(crate) skills_service: Arc<HostSkillsService>,
    pub(crate) agents_md_manager: Arc<AgentsMdManager>,
    pub(crate) plugins_manager: Arc<PluginsManager>,
    pub(crate) mcp_manager: Arc<McpManager>,
    pub(crate) extensions: Arc<ExtensionRegistry<crate::config::Config>>,
    pub(crate) session_extension_data: ExtensionData,
    pub(crate) thread_extension_data: ExtensionData,
    /// MCP extensions fixed when this session is created.
    pub(crate) client_mcp_extensions: ClientMcpExtensions,
    /// Roots supplied when the thread started or loaded from saved history.
    /// Keep roots for unselected environments so selecting them again restores their capabilities.
    pub(crate) selected_capability_roots: Vec<SelectedCapabilityRoot>,
    pub(crate) mcp_thread_init: ExtensionDataInit,
    pub(crate) agent_control: Arc<dyn AgentControl>,
    pub(crate) local_agent_runtime: LocalAgentRuntime,
    pub(crate) network_proxy: ArcSwapOption<StartedNetworkProxy>,
    pub(crate) network_proxy_audit_metadata: NetworkProxyAuditMetadata,
    pub(crate) managed_network_requirements_configured: bool,
    pub(crate) network_approval: Arc<NetworkApprovalService>,
    pub(crate) state_db: Option<StateDbHandle>,
    pub(crate) live_thread: Option<LiveThread>,
    pub(crate) image_store: Arc<dyn AttachmentStore>,
    pub(crate) thread_store: Arc<dyn ThreadStore>,
    pub(crate) attestation_provider: Option<Arc<dyn AttestationProvider>>,
    pub(crate) time_provider: Arc<dyn TimeProvider>,
    /// Session-scoped model client shared across turns.
    pub(crate) model_client: ModelClient,
    pub(crate) executed_tool_calls: ExecutedToolCalls,
    pub(crate) code_mode_service: CodeModeService,
    pub(crate) tool_search_handler_cache: ToolSearchHandlerCache,
    pub(crate) turn_environments: Arc<ThreadEnvironments>,
    /// Server-authenticated provenance for in-flight events. The key is Core's immutable
    /// submission/turn occurrence id; neither the map nor its values are protocol or history data.
    pub(crate) automatic_turn_principals: Mutex<HashMap<String, AutomaticTurnPrincipal>>,
}

#[derive(Clone, Debug)]
pub(crate) struct AutomaticTurnPrincipal {
    pub(crate) principal_identity: Option<String>,
    pub(crate) client_user_message_id: Option<String>,
    pub(crate) provider_authority: Option<ProviderAuthority>,
    pub(crate) expected_epochs: Option<(i64, i64)>,
    pub(crate) continuation_ref: Option<AutomaticTurnContinuationRef>,
}

impl SessionServices {
    pub(crate) async fn log_automatic_turn_event(
        &self,
        thread_id: ThreadId,
        event: &codex_protocol::protocol::Event,
    ) {
        let relevant = matches!(
            &event.msg,
            codex_protocol::protocol::EventMsg::Error(_)
                | codex_protocol::protocol::EventMsg::ItemCompleted(_)
                | codex_protocol::protocol::EventMsg::TurnComplete(_)
                | codex_protocol::protocol::EventMsg::TurnAborted(_)
        );
        if !relevant {
            return;
        }

        if let Some(state_db) = &self.state_db {
            let operation = self
                .automatic_turn_principals
                .lock()
                .await
                .get(&event.id)
                .cloned();
            let continuation_ref = state_db
                .record_automatic_turn_event_with_submission_context(
                    thread_id,
                    event,
                    operation
                        .as_ref()
                        .and_then(|operation| operation.principal_identity.as_deref()),
                    operation
                        .as_ref()
                        .and_then(|operation| operation.client_user_message_id.as_deref()),
                    operation
                        .as_ref()
                        .and_then(|operation| operation.continuation_ref.as_ref())
                        .map(AutomaticTurnContinuationRef::as_str),
                    operation
                        .as_ref()
                        .and_then(|operation| operation.expected_epochs),
                )
                .await;
            if let Some(reference) = continuation_ref.and_then(AutomaticTurnContinuationRef::new) {
                self.automatic_turn_principals
                    .lock()
                    .await
                    .entry(event.id.clone())
                    .and_modify(|operation| operation.continuation_ref = Some(reference.clone()));
            }
        }

        if matches!(
            &event.msg,
            codex_protocol::protocol::EventMsg::TurnComplete(_)
                | codex_protocol::protocol::EventMsg::TurnAborted(_)
        ) {
            self.automatic_turn_principals
                .lock()
                .await
                .remove(&event.id);
        }
    }

    pub(crate) async fn register_automatic_turn_principal(
        &self,
        event_occurrence_id: impl Into<String>,
        principal_identity: Option<String>,
        client_user_message_id: Option<&str>,
        provider_authority: Option<ProviderAuthority>,
        expected_epochs: Option<(i64, i64)>,
        continuation_ref: Option<AutomaticTurnContinuationRef>,
    ) {
        let client_user_message_id = client_user_message_id.map(str::to_owned);
        self.automatic_turn_principals
            .lock()
            .await
            .entry(event_occurrence_id.into())
            .and_modify(|operation| {
                if client_user_message_id.is_some() {
                    operation.principal_identity = principal_identity.clone();
                    operation.client_user_message_id = client_user_message_id.clone();
                    operation.provider_authority = provider_authority;
                    operation.expected_epochs = expected_epochs;
                    operation.continuation_ref = continuation_ref.clone();
                }
            })
            .or_insert(AutomaticTurnPrincipal {
                principal_identity,
                client_user_message_id,
                provider_authority,
                expected_epochs,
                continuation_ref,
            });
    }

    pub(crate) async fn remove_automatic_turn_principal_if_matches(
        &self,
        event_occurrence_id: &str,
        principal_identity: Option<&str>,
        client_user_message_id: Option<&str>,
    ) {
        let mut principals = self.automatic_turn_principals.lock().await;
        if principals
            .get(event_occurrence_id)
            .is_some_and(|operation| {
                operation.principal_identity.as_deref() == principal_identity
                    && operation.client_user_message_id.as_deref() == client_user_message_id
            })
        {
            principals.remove(event_occurrence_id);
        }
    }

    pub(crate) async fn automatic_turn_provider_authority(
        &self,
        event_occurrence_id: &str,
    ) -> Option<ProviderAuthority> {
        self.automatic_turn_principals
            .lock()
            .await
            .get(event_occurrence_id)
            .and_then(|operation| operation.provider_authority)
    }

    pub(crate) async fn automatic_turn_continuation_details(
        &self,
        event_occurrence_id: &str,
    ) -> Option<String> {
        self.automatic_turn_principals
            .lock()
            .await
            .get(event_occurrence_id)
            .and_then(|operation| operation.continuation_ref.as_ref())
            .map(AutomaticTurnContinuationRef::to_error_details)
    }

    #[expect(
        clippy::expect_used,
        reason = "poisoned grant state must not authorize further operations"
    )]
    pub(crate) fn record_granted_permissions(
        &self,
        environment_id: &str,
        permissions: AdditionalPermissionProfile,
    ) {
        let mut grants = self
            .granted_permissions_by_environment_id
            .lock()
            .expect("session permission grants lock poisoned");
        let granted_permissions =
            merge_permission_profiles(grants.get(environment_id), Some(&permissions));
        if let Some(granted_permissions) = granted_permissions {
            grants.insert(environment_id.to_string(), granted_permissions);
        }
    }
}
