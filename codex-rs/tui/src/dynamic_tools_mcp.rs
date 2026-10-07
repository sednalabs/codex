//! Approval-gated MCP transport for task tools owned by a local-daemon TUI.

use crate::app_event_sender::AppEventSender;
use crate::dynamic_tools;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::Request;
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::middleware;
use axum::middleware::Next;
use axum::response::Response;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::DynamicToolCallParams;
use codex_app_server_protocol::DynamicToolNamespaceTool;
use codex_app_server_protocol::DynamicToolSpec;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStatusChangedNotification;
use codex_config::McpServerConfig;
use codex_config::McpServerRequirement;
use codex_config::RawMcpServerConfig;
use codex_features::Features;
use rmcp::ErrorData as McpError;
use rmcp::handler::server::ServerHandler;
use rmcp::model::CallToolRequestParams;
use rmcp::model::CallToolResult;
use rmcp::model::ContentBlock;
use rmcp::model::JsonObject;
use rmcp::model::ListToolsResult;
use rmcp::model::PaginatedRequestParams;
use rmcp::model::ServerCapabilities;
use rmcp::model::ServerInfo;
use rmcp::model::Tool;
use rmcp::model::ToolAnnotations;
use rmcp::service::RequestContext;
use rmcp::service::RoleServer;
use rmcp::transport::StreamableHttpServerConfig;
use rmcp::transport::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use serde_json::Value;
use serde_json::json;
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::PoisonError;
use std::sync::RwLock;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use uuid::Uuid;

#[derive(Clone)]
pub(crate) enum ThreadToolTransport {
    Disabled,
    Dynamic,
    Mcp(Arc<DynamicToolMcpServer>),
}

impl ThreadToolTransport {
    pub(crate) fn validate_config(
        &self,
        config: &crate::legacy_core::config::Config,
    ) -> std::io::Result<()> {
        if !crate::android_computer_use_provider::specs_for_codex_home(config.codex_home.as_path())
            .is_empty()
            && config
                .mcp_servers
                .get()
                .contains_key(crate::android_computer_use_provider::NAMESPACE)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!(
                    "MCP server key `{}` is reserved by Android dynamic tools; rename that server before starting this thread",
                    crate::android_computer_use_provider::NAMESPACE
                ),
            ));
        }
        if let Self::Mcp(server) = self
            && config.mcp_servers.get().contains_key(server.namespace)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "A configured MCP server owns the TUI tools namespace; keep the current task and rename that server before changing projects",
            ));
        }
        Ok(())
    }

    pub(crate) fn configure(
        &self,
        params: &mut ThreadStartParams,
        codex_home: &Path,
    ) -> Result<(), String> {
        crate::browser_dynamic_tools::validate_no_reserved_name_conflict(params)?;
        crate::android_computer_use_provider::validate_no_reserved_name_conflict(params)?;
        let mut specs = match self {
            Self::Disabled => Vec::new(),
            Self::Dynamic => dynamic_tools::non_delegation_tool_specs(),
            Self::Mcp(_) => {
                self.configure_mcp(&mut params.config);
                Vec::new()
            }
        };
        specs.extend(crate::browser_dynamic_tools::specs_for_codex_home(
            codex_home,
        ));
        specs.extend(crate::android_computer_use_provider::specs_for_codex_home(
            codex_home,
        ));
        params.dynamic_tools = (!specs.is_empty()).then_some(specs);
        Ok(())
    }

    pub(crate) fn configure_mcp(&self, config: &mut Option<HashMap<String, Value>>) {
        if let Self::Mcp(server) = self {
            config.get_or_insert_default().insert(
                format!("mcp_servers.{}", server.namespace),
                server.config.clone(),
            );
        }
    }
}

pub(crate) fn has_task_tools(params: &ThreadStartParams) -> bool {
    params.dynamic_tools.as_ref().is_some_and(|specs| {
        specs.iter().any(
            |spec| matches!(spec, DynamicToolSpec::Namespace(namespace) if namespace.name == dynamic_tools::NAMESPACE),
        )
    }) || params
        .config
        .as_ref()
        .is_some_and(|config| config.contains_key("mcp_servers.codex_tui"))
}

#[derive(Clone)]
pub(crate) struct ToolServices {
    pub(crate) task_tools: bool,
    pub(crate) worktrees: Option<crate::managed_worktree_tools::ManagedWorktreeTools>,
}

impl ToolServices {
    pub(crate) fn namespace(&self) -> &'static str {
        if self.task_tools {
            dynamic_tools::NAMESPACE
        } else {
            "codex_worktrees"
        }
    }
}

type ToolConnection = Arc<RwLock<Option<(AppServerRequestHandle, AppEventSender)>>>;

pub(crate) struct DynamicToolMcpServer {
    connection: ToolConnection,
    config: Value,
    namespace: &'static str,
    task: JoinHandle<()>,
}

impl DynamicToolMcpServer {
    pub(crate) fn suspend(&self) {
        *self
            .connection
            .write()
            .unwrap_or_else(PoisonError::into_inner) = None;
    }

    pub(crate) fn reconnect(&self, handle: AppServerRequestHandle, events: AppEventSender) {
        *self
            .connection
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some((handle, events));
    }

    pub(crate) async fn start(
        request_handle: AppServerRequestHandle,
        mut thread_start_params: ThreadStartParams,
        features: Features,
        app_event_tx: AppEventSender,
        status_updates: broadcast::Sender<ThreadStatusChangedNotification>,
        managed_requirement: Option<&McpServerRequirement>,
        services: ToolServices,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let authorization = Arc::new(format!("Bearer {}", Uuid::new_v4()));
        let server_config = json!({
            "url": format!("http://{address}/mcp"),
            "http_headers": {"Authorization": authorization.as_str()},
            "default_tools_approval_mode": "approve",
            "tools": {
                "create_thread": {"approval_mode": "prompt"},
                "send_message_to_thread": {"approval_mode": "prompt"},
                "fork_thread": {"approval_mode": "prompt"},
                "create_worktree": {"approval_mode": "approve"}
            }
        });
        if let Some(requirement) = managed_requirement {
            let raw_config = serde_json::from_value::<RawMcpServerConfig>(server_config.clone())
                .map_err(|error| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
                })?;
            let configured_server = McpServerConfig::try_from(raw_config)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
            if !configured_server.matches_requirement(requirement) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "managed MCP requirements do not permit the TUI task-tools server",
                ));
            }
        }
        if let Some(overrides) = thread_start_params.config.as_mut() {
            overrides.remove("web_search");
        }
        let connection = Arc::new(RwLock::new(Some((request_handle, app_event_tx))));
        let namespace = services.namespace();
        let handler = DynamicToolMcpHandler {
            connection: Arc::clone(&connection),
            thread_start_params,
            features,
            status_updates,
            server_config: server_config.clone(),
            services,
        };
        let service = StreamableHttpService::new(
            move || Ok(handler.clone()),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default(),
        );
        let router =
            Router::new()
                .nest_service("/mcp", service)
                .layer(middleware::from_fn_with_state(
                    authorization,
                    require_authorization,
                ));
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, router).await {
                tracing::warn!(%error, "TUI task-tools MCP server stopped");
            }
        });
        Ok(Self {
            connection,
            config: server_config,
            namespace,
            task,
        })
    }
}

impl Drop for DynamicToolMcpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn require_authorization(
    State(expected): State<Arc<String>>,
    request: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    if request
        .headers()
        .get(AUTHORIZATION)
        .is_some_and(|value| value.as_bytes() == expected.as_bytes())
    {
        Ok(next.run(request).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

#[derive(Clone)]
struct DynamicToolMcpHandler {
    connection: ToolConnection,
    thread_start_params: ThreadStartParams,
    features: Features,
    status_updates: broadcast::Sender<ThreadStatusChangedNotification>,
    server_config: Value,
    services: ToolServices,
}

impl ServerHandler for DynamicToolMcpHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let mut tools = Vec::new();
        let mut specs = if self.services.task_tools {
            dynamic_tools::tool_specs()
        } else {
            Vec::new()
        };
        if self.services.worktrees.is_some() {
            specs.extend(crate::managed_worktree_tool_specs::specs());
        }
        for spec in specs {
            let functions = match spec {
                DynamicToolSpec::Function(function) => vec![function],
                DynamicToolSpec::Namespace(namespace) => namespace
                    .tools
                    .into_iter()
                    .map(|tool| match tool {
                        DynamicToolNamespaceTool::Function(function) => function,
                    })
                    .collect(),
            };
            for function in functions {
                let schema = serde_json::from_value::<JsonObject>(function.input_schema)
                    .map_err(|error| McpError::internal_error(error.to_string(), None))?;
                let mut tool = Tool::new(
                    Cow::Owned(function.name),
                    Cow::Owned(function.description),
                    Arc::new(schema),
                );
                tool.annotations = Some(ToolAnnotations::new().read_only(matches!(
                    tool.name.as_ref(),
                    "list_threads"
                        | "list_archived_threads"
                        | "read_thread"
                        | "wait_threads"
                        | "list_worktrees"
                        | "get_worktree_creation_status"
                )));
                tools.push(tool);
            }
        }
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, McpError> {
        let metadata = &context.meta.0.0;
        let turn_metadata = metadata
            .get("x-codex-turn-metadata")
            .and_then(|value| match value {
                Value::Object(_) => Some(value.clone()),
                Value::String(value) => serde_json::from_str(value).ok(),
                _ => None,
            });
        let thread_id = metadata
            .get("threadId")
            .and_then(Value::as_str)
            .or_else(|| turn_metadata.as_ref()?.get("thread_id")?.as_str())
            .filter(|thread_id| !thread_id.is_empty())
            .ok_or_else(|| McpError::invalid_params("missing task metadata", None))?;
        let turn_id = metadata
            .get("turnId")
            .and_then(Value::as_str)
            .or_else(|| turn_metadata.as_ref()?.get("turn_id")?.as_str())
            .map_or_else(|| format!("mcp-turn-{}", Uuid::new_v4()), str::to_string);
        let call_id = metadata
            .get("callId")
            .and_then(Value::as_str)
            .map_or_else(|| format!("mcp-call-{}", Uuid::new_v4()), str::to_string);
        let params = DynamicToolCallParams {
            thread_id: thread_id.to_string(),
            turn_id,
            call_id,
            namespace: Some(dynamic_tools::NAMESPACE.to_string()),
            tool: request.name.into_owned(),
            arguments: Value::Object(request.arguments.unwrap_or_default()),
        };
        let mut thread_start_params = self.thread_start_params.clone();
        thread_start_params.config.get_or_insert_default().insert(
            format!("mcp_servers.{}", self.services.namespace()),
            self.server_config.clone(),
        );
        // Snapshot once: an in-flight call must never switch connections or replay a mutation.
        let (request_handle, app_event_tx) = self
            .connection
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| {
                McpError::internal_error("TUI is reconnecting; tool was not sent", None)
            })?;
        let response = if matches!(
            params.tool.as_str(),
            "create_worktree" | "get_worktree_creation_status" | "list_worktrees"
        ) {
            let service = self.services.worktrees.as_ref().ok_or_else(|| {
                McpError::invalid_params("Worktree tools are unavailable on this connection", None)
            })?;
            service
                .execute(
                    request_handle,
                    params.thread_id,
                    &params.tool,
                    params.arguments,
                )
                .await
        } else {
            if !self.services.task_tools {
                return Err(McpError::invalid_params(
                    "Task tools are unavailable on this connection",
                    None,
                ));
            }
            dynamic_tools::execute(
                request_handle,
                params,
                thread_start_params,
                self.features.clone(),
                self.status_updates.subscribe(),
                Some(&app_event_tx),
            )
            .await
        };
        let content = response
            .content_items
            .into_iter()
            .map(|item| match item {
                DynamicToolCallOutputContentItem::InputText { text } => ContentBlock::text(text),
                DynamicToolCallOutputContentItem::InputImage { image_url } => {
                    ContentBlock::text(image_url)
                }
                DynamicToolCallOutputContentItem::InputAudio { audio_url } => {
                    ContentBlock::text(audio_url)
                }
            })
            .collect();
        Ok(if response.success {
            CallToolResult::success(content)
        } else {
            CallToolResult::error(content)
        }
        .into())
    }
}

#[cfg(test)]
mod native_computer_use_registration_tests {
    use super::*;
    use crate::legacy_core::config::ConfigBuilder;
    use codex_app_server_protocol::DynamicToolSpec;
    use serde_json::json;

    fn configured_native_computer_use_home() -> tempfile::TempDir {
        let home = tempfile::tempdir().expect("temporary computer-use home");
        std::fs::write(
            home.path().join("browser-computer-use.json"),
            r#"{"provider":"playwright"}"#,
        )
        .expect("write browser config");
        std::fs::write(
            home.path().join("android-computer-use.json"),
            r#"{"mcp_url":"http://127.0.0.1:1/mcp"}"#,
        )
        .expect("write Android config");
        home
    }

    fn has_browser_namespace(params: &ThreadStartParams) -> bool {
        params.dynamic_tools.as_ref().is_some_and(|specs| {
            specs.iter().any(
                |spec| matches!(spec, DynamicToolSpec::Namespace(namespace) if namespace.name == crate::browser_dynamic_tools::NAMESPACE),
            )
        })
    }

    fn namespace_tool_names(params: &ThreadStartParams, name: &str) -> Vec<String> {
        params
            .dynamic_tools
            .as_ref()
            .into_iter()
            .flatten()
            .filter_map(|spec| match spec {
                DynamicToolSpec::Namespace(namespace) if namespace.name == name => Some(namespace),
                _ => None,
            })
            .flat_map(|namespace| namespace.tools.iter())
            .filter_map(|tool| match tool {
                codex_app_server_protocol::DynamicToolNamespaceTool::Function(function) => {
                    Some(function.name.clone())
                }
            })
            .collect()
    }

    #[tokio::test]
    async fn configured_native_provider_specs_are_registered_for_each_transport() {
        let home = configured_native_computer_use_home();

        let mut disabled = ThreadStartParams::default();
        ThreadToolTransport::Disabled
            .configure(&mut disabled, home.path())
            .expect("disabled task transport must retain native providers");
        assert!(has_browser_namespace(&disabled));
        assert_eq!(
            namespace_tool_names(&disabled, crate::android_computer_use_provider::NAMESPACE),
            vec![
                "android_observe".to_string(),
                "android_step".to_string(),
                "android_install_build_from_run".to_string()
            ]
        );
        assert!(!has_task_tools(&disabled));

        let mut dynamic = ThreadStartParams::default();
        ThreadToolTransport::Dynamic
            .configure(&mut dynamic, home.path())
            .expect("dynamic task transport must include native providers");
        assert!(dynamic.dynamic_tools.as_ref().is_some_and(|specs| {
            specs.iter().any(
                |spec| matches!(spec, DynamicToolSpec::Namespace(namespace) if namespace.name == dynamic_tools::NAMESPACE),
            )
        }));
        assert!(has_browser_namespace(&dynamic));
        assert_eq!(
            namespace_tool_names(&dynamic, crate::android_computer_use_provider::NAMESPACE),
            vec![
                "android_observe".to_string(),
                "android_step".to_string(),
                "android_install_build_from_run".to_string()
            ]
        );
        assert!(has_task_tools(&dynamic));

        let server = DynamicToolMcpServer {
            connection: Arc::new(RwLock::new(None)),
            config: json!({"url":"http://127.0.0.1/mcp"}),
            namespace: "codex_tui",
            task: tokio::spawn(async {}),
        };
        let mut mcp = ThreadStartParams::default();
        ThreadToolTransport::Mcp(Arc::new(server))
            .configure(&mut mcp, home.path())
            .expect("MCP task transport must include native providers");
        assert!(has_browser_namespace(&mcp));
        assert_eq!(
            namespace_tool_names(&mcp, crate::android_computer_use_provider::NAMESPACE),
            vec![
                "android_observe".to_string(),
                "android_step".to_string(),
                "android_install_build_from_run".to_string()
            ]
        );
        assert!(has_task_tools(&mcp));
        assert!(
            mcp.config
                .as_ref()
                .is_some_and(|config| config.contains_key("mcp_servers.codex_tui"))
        );
    }

    #[tokio::test]
    async fn persisted_android_server_collision_is_rejected_only_when_provider_is_configured() {
        let home = tempfile::tempdir().expect("temporary Codex home");
        std::fs::write(
            home.path().join("config.toml"),
            "[mcp_servers.codex_android]\nurl = 'http://127.0.0.1:1/mcp'\n",
        )
        .expect("write persisted MCP server config");
        std::fs::write(
            home.path().join("android-computer-use.json"),
            r#"{"mcp_url":"http://127.0.0.1:1/android-mcp"}"#,
        )
        .expect("write Android provider config");

        let configured = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .build()
            .await
            .expect("load config with both namespace claimants");
        let error = ThreadToolTransport::Disabled
            .validate_config(&configured)
            .expect_err("persisted MCP claimant must not mask the Android provider");
        assert!(error.to_string().contains("codex_android"));

        std::fs::remove_file(home.path().join("android-computer-use.json"))
            .expect("remove Android provider config");
        let unconfigured = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .build()
            .await
            .expect("load config with only the MCP server");
        ThreadToolTransport::Disabled
            .validate_config(&unconfigured)
            .expect("unclaimed Android namespace must not reject an unrelated MCP server");
    }
}
