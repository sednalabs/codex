use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::SkillsService;
use crate::agent::AgentControl;
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
use crate::guardian::GuardianRejectionCircuitBreaker;
use crate::mcp::McpManager;
use crate::tools::code_mode::CodeModeService;
use crate::tools::handlers::ToolSearchHandlerCache;
use crate::tools::network_approval::NetworkApprovalService;
use crate::tools::sandboxing::ApprovalStore;
use crate::unified_exec::UnifiedExecProcessManager;
use arc_swap::ArcSwap;
use arc_swap::ArcSwapOption;
use codex_analytics::AnalyticsEventsClient;
use codex_core_plugins::PluginsManager;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionDataInit;
use codex_extension_api::ExtensionRegistry;
use codex_hooks::Hooks;
use codex_login::AuthManager;
use codex_mcp::McpRuntime;
use codex_models_manager::manager::SharedModelsManager;
use codex_otel::SessionTelemetry;
use codex_protocol::ThreadId;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::protocol::Event;
use codex_rollout::state_db::StateDbHandle;
use codex_rollout_trace::ThreadTraceContext;
use codex_state::UsageLogger;
use codex_thread_store::LiveThread;
use codex_thread_store::ThreadStore;
use tokio::runtime::Handle;
use tokio::sync::Mutex;

pub(crate) struct SessionServices {
    /// The single owner of live MCP connections for this thread.
    pub(crate) mcp_runtime: Arc<McpRuntime>,
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
    pub(crate) models_manager: SharedModelsManager,
    pub(crate) session_telemetry: SessionTelemetry,
    pub(crate) tool_approvals: Mutex<ApprovalStore>,
    pub(crate) guardian_rejection_circuit_breaker: Mutex<GuardianRejectionCircuitBreaker>,
    pub(crate) runtime_handle: Handle,
    pub(crate) skills_service: Arc<SkillsService>,
    pub(crate) agents_md_manager: Arc<AgentsMdManager>,
    pub(crate) plugins_manager: Arc<PluginsManager>,
    pub(crate) mcp_manager: Arc<McpManager>,
    pub(crate) extensions: Arc<ExtensionRegistry<crate::config::Config>>,
    pub(crate) session_extension_data: ExtensionData,
    pub(crate) thread_extension_data: ExtensionData,
    pub(crate) supports_openai_form_elicitation: AtomicBool,
    /// Raw capability selections for this thread. Each model step resolves them against its
    /// current executor environments before using them.
    pub(crate) selected_capability_roots: Vec<SelectedCapabilityRoot>,
    pub(crate) mcp_thread_init: ExtensionDataInit,
    pub(crate) agent_control: AgentControl,
    pub(crate) network_proxy: ArcSwapOption<StartedNetworkProxy>,
    pub(crate) network_proxy_audit_metadata: NetworkProxyAuditMetadata,
    pub(crate) managed_network_requirements_configured: bool,
    pub(crate) network_approval: Arc<NetworkApprovalService>,
    pub(crate) state_db: Option<StateDbHandle>,
    pub(crate) live_thread: Option<LiveThread>,
    pub(crate) thread_store: Arc<dyn ThreadStore>,
    pub(crate) attestation_provider: Option<Arc<dyn AttestationProvider>>,
    pub(crate) time_provider: Arc<dyn TimeProvider>,
    /// Session-scoped model client shared across turns.
    pub(crate) model_client: ModelClient,
    pub(crate) code_mode_service: CodeModeService,
    pub(crate) usage_logger: Option<Mutex<UsageLogger>>,
    pub(crate) tool_search_handler_cache: ToolSearchHandlerCache,
    pub(crate) turn_environments: Arc<ThreadEnvironments>,
    /// Server-authenticated principals for submissions whose events are still in flight. The
    /// app-server supplies these through the internal thread bridge; they are never client data.
    pub(crate) automatic_turn_principals: Mutex<HashMap<String, AutomaticTurnPrincipal>>,
}

#[derive(Clone, Debug)]
pub(crate) struct AutomaticTurnPrincipal {
    pub(crate) principal: String,
    pub(crate) client_user_message_id: Option<String>,
    pub(crate) provider_authority: Option<ProviderAuthority>,
}

impl SessionServices {
    #[allow(
        clippy::await_holding_invalid_type,
        reason = "usage logger event handling mutates ordered in-memory snapshots around async ledger writes"
    )]
    pub(crate) async fn log_usage_event(&self, thread_id: ThreadId, event: &Event) {
        let relevant_to_automatic_turns = matches!(
            &event.msg,
            codex_protocol::protocol::EventMsg::Error(_)
                | codex_protocol::protocol::EventMsg::ItemCompleted(_)
                | codex_protocol::protocol::EventMsg::TurnComplete(_)
                | codex_protocol::protocol::EventMsg::TurnAborted(_)
        );
        if relevant_to_automatic_turns {
            if let Some(state_db) = &self.state_db {
                let operation = self
                    .automatic_turn_principals
                    .lock()
                    .await
                    .get(&event.id)
                    .cloned();
                state_db
                    .record_automatic_turn_event_with_principal_and_client_user_message_id(
                        thread_id,
                        event,
                        operation
                            .as_ref()
                            .map(|operation| operation.principal.as_str()),
                        operation
                            .as_ref()
                            .and_then(|operation| operation.client_user_message_id.as_deref()),
                    )
                    .await;
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

        let Some(usage_logger) = &self.usage_logger else {
            return;
        };

        usage_logger.lock().await.record_event(event).await;
    }

    pub(crate) async fn register_automatic_turn_principal(
        &self,
        event_occurrence_id: impl Into<String>,
        principal: impl Into<String>,
        client_user_message_id: Option<&str>,
    ) {
        let principal = principal.into();
        let client_user_message_id = client_user_message_id.map(str::to_owned);
        self.automatic_turn_principals
            .lock()
            .await
            .entry(event_occurrence_id.into())
            .and_modify(|operation| {
                // A repeated same-turn steer is a new admitted attempt. Replace the complete
                // identity whenever it carries a client message id; retaining the old id would
                // let a later abort terminalize the wrong attempt.
                if client_user_message_id.is_some() {
                    operation.principal = principal.clone();
                    operation.client_user_message_id = client_user_message_id.clone();
                }
            })
            .or_insert_with(|| AutomaticTurnPrincipal {
                principal,
                client_user_message_id,
                provider_authority: None,
            });
    }

    pub(crate) async fn set_automatic_turn_provider_authority(
        &self,
        event_occurrence_id: &str,
        provider_authority: ProviderAuthority,
    ) {
        if let Some(operation) = self
            .automatic_turn_principals
            .lock()
            .await
            .get_mut(event_occurrence_id)
        {
            operation.provider_authority = Some(provider_authority);
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

    pub(crate) async fn remove_automatic_turn_principal_if_matches(
        &self,
        event_occurrence_id: &str,
        principal: &str,
        client_user_message_id: Option<&str>,
    ) {
        let mut principals = self.automatic_turn_principals.lock().await;
        let matches = principals
            .get(event_occurrence_id)
            .is_some_and(|operation| {
                operation.principal == principal
                    && operation.client_user_message_id.as_deref() == client_user_message_id
            });
        if matches {
            principals.remove(event_occurrence_id);
        }
    }
}
