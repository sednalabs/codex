use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use arc_swap::ArcSwap;
use codex_rmcp_client::InProcessTransportFactory;
use codex_rmcp_client::RmcpClient;
use codex_rmcp_client::SendElicitation;
use futures::future::BoxFuture;
use pretty_assertions::assert_eq;
use rmcp::ErrorData as McpError;
use rmcp::RoleServer;
use rmcp::ServerHandler;
use rmcp::ServiceExt;
use rmcp::model::JsonObject;
use rmcp::model::ListToolsResult;
use rmcp::model::PaginatedRequestParams;
use rmcp::model::ServerCapabilities;
use rmcp::model::ServerInfo;
use rmcp::model::Tool;
use rmcp::service::NotificationContext;
use tokio::sync::Notify;

use super::*;

#[derive(Clone)]
struct ChangingPaginatedServer {
    generation: Arc<AtomicUsize>,
    list_calls: Arc<AtomicUsize>,
    change: Arc<Notify>,
}

impl ChangingPaginatedServer {
    fn new() -> Self {
        Self {
            generation: Arc::new(AtomicUsize::new(0)),
            list_calls: Arc::new(AtomicUsize::new(0)),
            change: Arc::new(Notify::new()),
        }
    }

    fn tool(name: &str) -> Tool {
        Tool::new(
            name.to_string(),
            "listChanged test tool".to_string(),
            Arc::new(JsonObject::new()),
        )
    }
}

impl ServerHandler for ChangingPaginatedServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
    }

    fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        self.list_calls.fetch_add(1, Ordering::AcqRel);
        let generation = self.generation.load(Ordering::Acquire);
        async move {
            if generation == 1 {
                return Err(McpError::internal_error("transient list failure", None));
            }
            let page = request.as_ref().and_then(|params| params.cursor.as_deref());
            let prefix = if generation == 0 { "old" } else { "new" };
            match page {
                None => Ok(ListToolsResult {
                    tools: vec![Self::tool(&format!("{prefix}_first"))],
                    next_cursor: Some(String::new()),
                    meta: None,
                }),
                Some("") => Ok(ListToolsResult {
                    tools: vec![Self::tool(&format!("{prefix}_later"))],
                    next_cursor: None,
                    meta: None,
                }),
                Some(cursor) => Err(McpError::invalid_params(
                    ["unexpected cursor ", cursor].concat(),
                    None,
                )),
            }
        }
    }

    fn on_initialized(
        &self,
        context: NotificationContext<RoleServer>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let generation = Arc::clone(&self.generation);
        let change = Arc::clone(&self.change);
        let peer = context.peer.clone();
        async move {
            tokio::spawn(async move {
                for next_generation in 1..=2 {
                    change.notified().await;
                    generation.store(next_generation, Ordering::Release);
                    peer.notify_tool_list_changed()
                        .await
                        .expect("send tools/list_changed");
                }
            });
        }
    }
}

struct ChangingServerFactory(ChangingPaginatedServer);

impl InProcessTransportFactory for ChangingServerFactory {
    fn open(&self) -> BoxFuture<'static, std::io::Result<tokio::io::DuplexStream>> {
        let server = self.0.clone();
        Box::pin(async move {
            let (client_transport, server_transport) = tokio::io::duplex(4096);
            tokio::spawn(async move {
                let running = server
                    .serve(server_transport)
                    .await
                    .expect("start changing MCP server");
                running.waiting().await.expect("run changing MCP server");
            });
            Ok(client_transport)
        })
    }
}

#[tokio::test]
async fn list_changed_failure_is_attempted_once_and_next_change_replaces_snapshot() {
    let server = ChangingPaginatedServer::new();
    let change = Arc::clone(&server.change);
    let list_calls = Arc::clone(&server.list_calls);
    let client = Arc::new(
        RmcpClient::new_in_process_client(Arc::new(ChangingServerFactory(server)))
            .await
            .expect("create in-process client"),
    );
    let send_elicitation: SendElicitation =
        Box::new(|_, _| Box::pin(async { Err(anyhow!("unexpected elicitation")) }));
    let initialize_result = client
        .initialize(
            mcp_initialize_request_params(ElicitationCapability::default(), false),
            Some(Duration::from_secs(5)),
            send_elicitation,
        )
        .await
        .expect("initialize changing server");
    let initial = list_tools_for_client_uncached(
        "changing",
        false,
        &client,
        Some(Duration::from_secs(5)),
        initialize_result.instructions.as_deref(),
    )
    .await
    .expect("list initial tools");
    let server_info = mcp_server_info_from_implementation(initialize_result.server_info);
    let managed = ManagedClient {
        client: Arc::clone(&client),
        server_info,
        tool_catalogue: Arc::new(ArcSwap::from_pointee(ToolCatalogueSnapshot {
            observed_generation: initial.generation,
            tools: initial.tools,
        })),
        tool_refresh_lock: Arc::new(Semaphore::new(1)),
        server_name: "changing".to_string(),
        is_codex_apps_mcp_server: false,
        tool_filter: ToolFilter::default(),
        tool_timeout: Some(Duration::from_secs(5)),
        server_instructions: initialize_result.instructions,
        server_supports_sandbox_state_meta_capability: false,
        codex_apps_tools_cache_context: None,
    };
    assert_eq!(
        tool_names(managed.listed_tools().await),
        ["old_first", "old_later"]
    );
    assert_eq!(list_calls.load(Ordering::Acquire), 2);

    change.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        while client.tool_list_generation() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("receive tools/list_changed");

    assert_eq!(
        tool_names(managed.listed_tools().await),
        ["old_first", "old_later"]
    );
    let calls_after_failed_refresh = list_calls.load(Ordering::Acquire);
    assert_eq!(calls_after_failed_refresh, 3);
    assert_eq!(
        tool_names(managed.listed_tools().await),
        ["old_first", "old_later"]
    );
    assert_eq!(
        list_calls.load(Ordering::Acquire),
        calls_after_failed_refresh
    );

    change.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        while client.tool_list_generation() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("receive next tools/list_changed");
    assert_eq!(
        tool_names(managed.listed_tools().await),
        ["new_first", "new_later"]
    );
    assert_eq!(list_calls.load(Ordering::Acquire), 5);
    client.shutdown().await;
}

fn tool_names(tools: Vec<ToolInfo>) -> Vec<String> {
    tools.into_iter().map(|tool| tool.callable_name).collect()
}
