use std::sync::Arc;

use crate::AuthManager;
use crate::RolloutRecorder;
use crate::SkillsManager;
use crate::agent::AgentControl;
use crate::client::ModelClient;
use crate::config::StartedNetworkProxy;
use crate::exec_policy::ExecPolicyManager;
use crate::mcp::McpManager;
use crate::mcp_connection_manager::McpConnectionManager;
use crate::models_manager::manager::ModelsManager;
use crate::plugins::PluginsManager;
use crate::skills_watcher::SkillsWatcher;
use crate::state_db::StateDbHandle;
use crate::tools::code_mode::CodeModeService;
use crate::tools::network_approval::NetworkApprovalService;
use crate::tools::sandboxing::ApprovalStore;
use crate::unified_exec::UnifiedExecProcessManager;
use codex_analytics::AnalyticsEventsClient;
use codex_exec_server::Environment;
use codex_hooks::Hooks;
use codex_otel::SessionTelemetry;
use codex_protocol::protocol::Event;
use codex_state::UsageLogger;
use std::path::PathBuf;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::warn;

pub(crate) struct AsyncUsageLogger {
    tx: mpsc::UnboundedSender<UsageLoggerCmd>,
}

enum UsageLoggerCmd {
    RecordEvent(Event),
    UpdateTurnSnapshot {
        turn_id: String,
        requested_model: Option<String>,
        requested_provider: Option<String>,
    },
    Shutdown {
        ack: oneshot::Sender<()>,
    },
}

impl AsyncUsageLogger {
    pub(crate) fn new(mut logger: UsageLogger) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    UsageLoggerCmd::RecordEvent(event) => {
                        logger.record_event(&event).await;
                    }
                    UsageLoggerCmd::UpdateTurnSnapshot {
                        turn_id,
                        requested_model,
                        requested_provider,
                    } => {
                        logger.update_turn_snapshot(&turn_id, requested_model, requested_provider);
                    }
                    UsageLoggerCmd::Shutdown { ack } => {
                        let _ = ack.send(());
                        break;
                    }
                }
            }
        });
        Self { tx }
    }

    pub(crate) fn log_usage_event(&self, event: &Event) {
        if let Err(err) = self.tx.send(UsageLoggerCmd::RecordEvent(event.clone())) {
            warn!("failed to enqueue usage event: {err}");
        }
    }

    pub(crate) fn update_usage_turn_snapshot(
        &self,
        turn_id: &str,
        requested_model: Option<String>,
        requested_provider: Option<String>,
    ) {
        if let Err(err) = self.tx.send(UsageLoggerCmd::UpdateTurnSnapshot {
            turn_id: turn_id.to_string(),
            requested_model,
            requested_provider,
        }) {
            warn!("failed to enqueue usage turn snapshot update: {err}");
        }
    }

    pub(crate) async fn shutdown(&self) {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self
            .tx
            .send(UsageLoggerCmd::Shutdown { ack: ack_tx })
            .is_err()
        {
            return;
        }
        if ack_rx.await.is_err() {
            warn!("usage logger shutdown ack channel dropped before completion");
        }
    }
}

pub(crate) struct SessionServices {
    pub(crate) mcp_connection_manager: Arc<RwLock<McpConnectionManager>>,
    pub(crate) mcp_startup_cancellation_token: Mutex<CancellationToken>,
    pub(crate) unified_exec_manager: UnifiedExecProcessManager,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) shell_zsh_path: Option<PathBuf>,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) main_execve_wrapper_exe: Option<PathBuf>,
    pub(crate) analytics_events_client: AnalyticsEventsClient,
    pub(crate) hooks: Hooks,
    pub(crate) rollout: Mutex<Option<RolloutRecorder>>,
    pub(crate) user_shell: Arc<crate::shell::Shell>,
    pub(crate) shell_snapshot_tx: watch::Sender<Option<Arc<crate::shell_snapshot::ShellSnapshot>>>,
    pub(crate) show_raw_agent_reasoning: bool,
    pub(crate) exec_policy: Arc<ExecPolicyManager>,
    pub(crate) auth_manager: Arc<AuthManager>,
    pub(crate) models_manager: Arc<ModelsManager>,
    pub(crate) session_telemetry: SessionTelemetry,
    pub(crate) tool_approvals: Mutex<ApprovalStore>,
    pub(crate) skills_manager: Arc<SkillsManager>,
    pub(crate) plugins_manager: Arc<PluginsManager>,
    pub(crate) mcp_manager: Arc<McpManager>,
    pub(crate) skills_watcher: Arc<SkillsWatcher>,
    pub(crate) agent_control: AgentControl,
    pub(crate) network_proxy: Option<StartedNetworkProxy>,
    pub(crate) network_approval: Arc<NetworkApprovalService>,
    pub(crate) state_db: Option<StateDbHandle>,
    /// Session-scoped model client shared across turns.
    pub(crate) model_client: ModelClient,
    pub(crate) code_mode_service: CodeModeService,
    pub(crate) usage_logger: Option<AsyncUsageLogger>,
    pub(crate) environment: Arc<Environment>,
}

impl SessionServices {
    pub(crate) fn log_usage_event(&self, event: &Event) {
        if let Some(logger) = &self.usage_logger {
            logger.log_usage_event(event);
        }
    }

    pub(crate) fn update_usage_turn_snapshot(
        &self,
        turn_id: &str,
        requested_model: Option<String>,
        requested_provider: Option<String>,
    ) {
        if let Some(logger) = &self.usage_logger {
            logger.update_usage_turn_snapshot(turn_id, requested_model, requested_provider);
        }
    }

    pub(crate) async fn shutdown_usage_logger(&self) {
        if let Some(logger) = &self.usage_logger {
            logger.shutdown().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use codex_protocol::ThreadId;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::McpInvocation;
    use codex_protocol::protocol::McpToolCallBeginEvent;
    use codex_protocol::protocol::McpToolCallEndEvent;
    use codex_protocol::protocol::SessionSource;
    use codex_state::StateRuntime;
    use pretty_assertions::assert_eq;
    use sqlx::SqlitePool;
    use std::time::Duration;
    use tempfile::tempdir;

    #[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
    struct ToolCallRow {
        status: Option<String>,
        duration_ms: Option<i64>,
    }

    #[tokio::test]
    async fn async_usage_logger_shutdown_drains_queued_tool_events() -> Result<()> {
        let tmp_dir = tempdir()?;
        let runtime =
            StateRuntime::init(tmp_dir.path().to_path_buf(), "test-provider".to_string()).await?;
        let thread_id = ThreadId::new();
        let logger = UsageLogger::try_new(
            runtime.clone(),
            thread_id,
            SessionSource::Cli,
            /*forked_from_id*/ None,
            /*agent_nickname*/ None,
            /*agent_role*/ None,
        )
        .await?;
        let async_logger = AsyncUsageLogger::new(logger);
        let invocation = McpInvocation {
            server: "ops".to_string(),
            tool: "work_item_create".to_string(),
            arguments: None,
        };

        async_logger.log_usage_event(&Event {
            id: "turn-1".to_string(),
            msg: EventMsg::McpToolCallBegin(McpToolCallBeginEvent {
                call_id: "call-1".to_string(),
                invocation: invocation.clone(),
            }),
        });
        async_logger.log_usage_event(&Event {
            id: "turn-1".to_string(),
            msg: EventMsg::McpToolCallEnd(McpToolCallEndEvent {
                call_id: "call-1".to_string(),
                invocation,
                duration: Duration::from_millis(123),
                result: Err("boom".to_string()),
            }),
        });

        async_logger.shutdown().await;

        let pool_arc = runtime.usage_pool();
        let pool: &SqlitePool = pool_arc.as_ref();
        let row: ToolCallRow = sqlx::query_as(
            r#"
SELECT
  status,
  duration_ms
FROM usage_tool_calls
WHERE thread_id = ?
  AND tool_call_id = ?
"#,
        )
        .bind(thread_id.to_string())
        .bind("call-1")
        .fetch_one(pool)
        .await?;

        assert_eq!(
            row,
            ToolCallRow {
                status: Some("failed".to_string()),
                duration_ms: Some(123),
            }
        );

        Ok(())
    }
}
