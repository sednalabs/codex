mod streamable_http_test_support;

use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_exec_server::Environment;
use codex_rmcp_client::McpProtocolMode;
use codex_rmcp_client::RmcpClient;
use rmcp::model::DiscoverResult;
use rmcp::model::Implementation;
use rmcp::model::ProtocolVersion;
use rmcp::model::ServerCapabilities;
use serde_json::Value;
use serde_json::json;
use streamable_http_test_support::initialize_client;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn current_protocol_mode_discovers_and_sends_stateless_http_requests() -> anyhow::Result<()> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(|request: &Request| {
            let body: Value = request.body_json().expect("valid JSON-RPC request");
            let method = body["method"].as_str().expect("request method");
            assert!(!request.headers.contains_key("mcp-session-id"));
            let result = match method {
                "server/discover" => {
                    assert_eq!(
                        request
                            .headers
                            .get("mcp-protocol-version")
                            .and_then(|value| value.to_str().ok()),
                        Some("2026-07-28")
                    );
                    serde_json::to_value(
                        DiscoverResult::new(
                            vec![ProtocolVersion::V_2026_07_28],
                            ServerCapabilities::default(),
                        )
                        .with_server_info(Implementation::new("protocol-mode-test", "0.0.0-test")),
                    )
                    .expect("serialize discovery result")
                }
                "tools/list" => {
                    assert_eq!(
                        request
                            .headers
                            .get("mcp-protocol-version")
                            .and_then(|value| value.to_str().ok()),
                        Some("2026-07-28")
                    );
                    json!({ "tools": [] })
                }
                other => panic!("unexpected MCP request: {other}"),
            };
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": body["id"],
                "result": result,
            }))
        })
        .expect(2)
        .mount(&server)
        .await;

    let client = RmcpClient::new_streamable_http_client_with_protocol_mode(
        "protocol-mode-test",
        &format!("{}/mcp", server.uri()),
        Some("test-bearer".to_string()),
        None,
        None,
        OAuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
        Environment::default_for_tests().get_http_client(),
        None,
        McpProtocolMode::V20260728,
    )
    .await?;

    initialize_client(&client).await?;
    let result = client
        .list_tools(None, Some(std::time::Duration::from_secs(5)))
        .await?;
    assert!(result.tools.is_empty());
    client.shutdown().await;
    server.verify().await;

    Ok(())
}
