use super::*;
use crate::legacy_core::config::ConfigBuilder;
use base64::Engine;
use codex_config::LoaderOverrides;
use pretty_assertions::assert_eq;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

async fn session(server: &MockServer) -> (tempfile::TempDir, AnalyticsSession) {
    session_for_plan(server, "business").await
}

async fn session_for_plan(
    server: &MockServer,
    plan: &str,
) -> (tempfile::TempDir, AnalyticsSession) {
    let home = tempfile::tempdir().unwrap();
    let mut config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await
        .unwrap();
    config.chatgpt_base_url = format!("{}/backend-api", server.uri());
    config.cli_auth_credentials_store_mode = codex_login::AuthCredentialsStoreMode::File;
    let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        json!({
            "exp": 4102444800_i64,
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "account-a", "chatgpt_user_id": "user-a", "chatgpt_plan_type": plan
            }
        })
        .to_string(),
    );
    let token = format!("e30.{claims}.test");
    let auth = serde_json::from_value(json!({
        "auth_mode": "chatgpt",
        "tokens": {"id_token": token, "access_token": token, "refresh_token": "test-refresh", "account_id": "account-a"},
        "last_refresh": chrono::Utc::now(),
    }))
    .unwrap();
    codex_login::save_auth(
        home.path(),
        &auth,
        codex_login::AuthCredentialsStoreMode::File,
        codex_login::AuthKeyringBackendKind::default(),
    )
    .unwrap();
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/accounts/check"))
        .respond_with(ResponseTemplate::new(/*s*/ 200).set_body_json(json!({
            "accounts": [{"id": "account-a", "plan_type": plan}],
            "account_ordering": ["account-a"]
        })))
        .mount(server)
        .await;
    let session = AnalyticsSession::from_config(&config, config.http_client_factory())
        .await
        .unwrap();
    (home, session)
}

#[tokio::test]
async fn local_lineage_is_attached_only_to_account_verified_chats() {
    use codex_protocol::ThreadId;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::SubAgentSource;
    use codex_state::UsageLogger;
    use codex_utils_absolute_path::AbsolutePathBuf;

    let server = MockServer::start().await;
    let (_auth_home, session) = session(&server).await;
    let db_home = tempfile::tempdir().unwrap();
    let db = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(
            AbsolutePathBuf::from_absolute_path(db_home.path()).unwrap(),
        ),
        "test-provider".to_string(),
    )
    .await
    .unwrap();
    let root = ThreadId::new();
    let child = ThreadId::new();
    let other = ThreadId::new();
    let _root = UsageLogger::try_new(db.clone(), root, SessionSource::Cli, None, None, None)
        .await.unwrap();
    let _child = UsageLogger::try_new(
        db.clone(), child,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: root, depth: 1, agent_path: None,
            agent_nickname: None, agent_role: None,
        }), None, None, None,
    ).await.unwrap();
    let _other = UsageLogger::try_new(db.clone(), other, SessionSource::Cli, None, None, None)
        .await.unwrap();
    let mut page = AccountChats::Workspace(Chats {
        rows: vec![
            Chat {
                title: "verified".to_string(),
                usage: Some(ThreadUsage {
                    thread_id: root.to_string(), estimated_usage_credits_micros: 1,
                    estimated_usage_usd_micros: None, native_usage_usd_micros: None,
                    groups: Vec::new(),
                }),
                local_usage: None,
            },
            Chat { title: "unverified".to_string(), usage: None, local_usage: None },
        ],
        next_cursor: None, partial_history: true, partial_estimates: true,
    });
    page.attach_local_usage(Some(&db), &session).await.unwrap();
    let AccountChats::Workspace(page) = page else { panic!("workspace page") };
    let local = page.rows[0].local_usage.as_ref().unwrap();
    assert_eq!(local.threads.iter().map(|thread| thread.kind.as_str()).collect::<Vec<_>>(), vec!["Root", "Subagent"]);
    assert!(page.rows[1].local_usage.is_none());
    assert_eq!(page.rows[1].display_title(), "Chat usage unavailable");
}

#[tokio::test]
async fn unavailable_batches_keep_available_rows_and_do_not_invent_zeroes() {
    let server = MockServer::start().await;
    let (_home, session) = session(&server).await;
    Mock::given(method("POST"))
        .and(path("/backend-api/wham/usage/thread_usage/query"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = request.body_json().unwrap();
            let ids = body["thread_ids"].as_array().unwrap();
            if ids.len() > 1 || ids[0] == "unavailable" {
                ResponseTemplate::new(/*s*/ 503)
            } else {
                ResponseTemplate::new(/*s*/ 200).set_body_json(json!({"threads": [{
                    "thread_id": ids[0],
                    "estimated_usage_credits_micros": if ids[0] == "missing" { None } else { Some(1250000) },
                    "estimated_usage_usd_micros": null,
                    "groups": null
                }]}))
            }
        })
        .expect(/*r*/ 5)
        .mount(&server)
        .await;
    let rows = estimates(&session, &["available", "unavailable", "missing"])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].thread_id, "available");
    assert_eq!(rows[0].estimated_usage_credits_micros, 1250000);
}

#[tokio::test]
async fn repeated_outages_have_a_bounded_request_count() {
    let server = MockServer::start().await;
    let (_home, session) = session(&server).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(/*s*/ 503))
        .expect(/*r*/ 15)
        .mount(&server)
        .await;
    let ids = (0..100).map(|i| format!("chat-{i}")).collect::<Vec<_>>();
    let ids = ids.iter().map(String::as_str).collect::<Vec<_>>();
    assert!(estimates(&session, &ids).await.unwrap().is_empty());
}

#[tokio::test]
async fn indexed_root_listing_keeps_cursor_after_empty_filtered_page() {
    use codex_app_server_client::RemoteAppServerClient;
    use codex_app_server_client::RemoteAppServerConnectArgs;
    use codex_app_server_client::RemoteAppServerEndpoint;
    use futures::SinkExt;
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let websocket_url = format!("ws://{}", listener.local_addr().unwrap());
    let rpc = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let mut pages = Vec::new();
        while let Some(Ok(Message::Text(text))) = socket.next().await {
            let request: serde_json::Value = serde_json::from_str(&text).unwrap();
            let result = match request["method"].as_str() {
                Some("initialize") => {
                    json!({"userAgent": "analytics-test", "codexHome": "/unused", "platformFamily": "unix", "platformOs": "linux"})
                }
                Some("thread/list") => {
                    let params = &request["params"];
                    pages.push(json!([
                        params["cursor"],
                        params["limit"],
                        params["useStateDbOnly"],
                        params["sortKey"]
                    ]));
                    let old = chrono::Utc::now().timestamp() - 31 * 24 * 60 * 60;
                    let data = if pages.len() == 1 {
                        vec![json!({
                            "id": "old", "sessionId": "old", "preview": "", "ephemeral": false,
                            "modelProvider": "openai", "createdAt": old, "updatedAt": old,
                            "status": {"type": "idle"}, "cwd": std::env::temp_dir(), "cliVersion": "test", "source": "cli", "turns": []
                        })]
                    } else {
                        Vec::new()
                    };
                    json!({"data": data, "nextCursor": if pages.len() == 1 { Some("next") } else { None }, "backwardsCursor": null})
                }
                _ => continue,
            };
            socket
                .send(Message::Text(
                    json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            if pages.len() == 2 {
                break;
            }
        }
        pages
    });
    let remote = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
        endpoint: RemoteAppServerEndpoint::WebSocket {
            websocket_url,
            auth_token: None,
        },
        client_name: "analytics-test".into(),
        client_version: "test".into(),
        experimental_api: false,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 16,
    })
    .await
    .unwrap();
    let handle = AppServerRequestHandle::Remote(remote.request_handle());
    let (first, cursor) = roots(&handle, None).await.unwrap();
    assert!(first.is_empty());
    assert_eq!(cursor.as_deref(), Some("next"));
    let (second, next) = roots(&handle, cursor).await.unwrap();
    assert!(second.is_empty());
    assert_eq!(next, None);
    assert_eq!(
        rpc.await.unwrap(),
        vec![
            json!([null, 100, true, "created_at"]),
            json!(["next", 100, true, "created_at"])
        ]
    );
}

#[tokio::test]
async fn stalled_estimate_returns_partial_page_and_preserves_cursor() {
    use codex_app_server_client::RemoteAppServerClient;
    use codex_app_server_client::RemoteAppServerConnectArgs;
    use codex_app_server_client::RemoteAppServerEndpoint;
    use futures::SinkExt;
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    let server = MockServer::start().await;
    let (_home, session) = session(&server).await;
    Mock::given(method("POST"))
        .and(path("/backend-api/wham/usage/thread_usage/query"))
        .respond_with(ResponseTemplate::new(/*s*/ 503).set_delay(Duration::from_secs(/*secs*/ 8)))
        .expect(/*r*/ 1)
        .mount(&server)
        .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let websocket_url = format!("ws://{}", listener.local_addr().unwrap());
    let rpc = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        while let Some(Ok(Message::Text(text))) = socket.next().await {
            let request: serde_json::Value = serde_json::from_str(&text).unwrap();
            let result = match request["method"].as_str() {
                Some("initialize") => {
                    json!({"userAgent": "analytics-test", "codexHome": "/unused", "platformFamily": "unix", "platformOs": "linux"})
                }
                Some("thread/list") => json!({"data": [{
                    "id": "chat-1", "sessionId": "chat-1", "name": "Private title", "preview": "", "ephemeral": false,
                    "modelProvider": "openai", "createdAt": chrono::Utc::now().timestamp(), "updatedAt": chrono::Utc::now().timestamp(),
                    "status": {"type": "idle"}, "cwd": std::env::temp_dir(), "cliVersion": "test", "source": "cli", "turns": []
                }], "nextCursor": "older", "backwardsCursor": null}),
                _ => continue,
            };
            socket
                .send(Message::Text(
                    json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            if request["method"] == "thread/list" {
                break;
            }
        }
    });
    let remote = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
        endpoint: RemoteAppServerEndpoint::WebSocket {
            websocket_url,
            auth_token: None,
        },
        client_name: "analytics-test".into(),
        client_version: "test".into(),
        experimental_api: false,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 16,
    })
    .await
    .unwrap();
    let handle = AppServerRequestHandle::Remote(remote.request_handle());
    let page = tokio::time::timeout(
        Duration::from_secs(/*secs*/ 5),
        read(&handle, &session, None),
    )
    .await
    .expect("optional estimates must not block navigation")
    .unwrap();
    assert!(page.partial_history);
    assert!(page.partial_estimates);
    assert_eq!(page.next_cursor.as_deref(), Some("older"));
    assert_eq!(page.rows.len(), 1);
    assert!(page.rows[0].usage.is_none());
    assert_eq!(page.rows[0].display_title(), "Chat usage unavailable");
    rpc.await.unwrap();
}

#[tokio::test]
async fn descendant_lookup_rejects_unacknowledged_or_bounded_results() {
    use codex_app_server_client::RemoteAppServerClient;
    use codex_app_server_client::RemoteAppServerConnectArgs;
    use codex_app_server_client::RemoteAppServerEndpoint;
    use futures::SinkExt;
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let websocket_url = format!("ws://{}", listener.local_addr().unwrap());
    let rpc = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let mut lists = 0;
        while let Some(Ok(Message::Text(text))) = socket.next().await {
            let request: serde_json::Value = serde_json::from_str(&text).unwrap();
            let result = match request["method"].as_str() {
                Some("initialize") => {
                    json!({"userAgent": "analytics-test", "codexHome": "/unused", "platformFamily": "unix", "platformOs": "linux"})
                }
                Some("thread/list") => {
                    lists += 1;
                    if lists == 1 {
                        json!({"data": [], "nextCursor": null, "backwardsCursor": null})
                    } else if lists == 2 {
                        json!({"data": [], "nextCursor": null, "backwardsCursor": null, "ancestorFilterApplied": true, "relationLimitReached": true})
                    } else {
                        json!({"data": [], "nextCursor": null, "backwardsCursor": null, "ancestorFilterApplied": true})
                    }
                }
                _ => continue,
            };
            socket
                .send(Message::Text(
                    json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            if lists == 3 {
                break;
            }
        }
    });
    let remote = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
        endpoint: RemoteAppServerEndpoint::WebSocket {
            websocket_url,
            auth_token: None,
        },
        client_name: "analytics-test".into(),
        client_version: "test".into(),
        experimental_api: true,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 16,
    })
    .await
    .unwrap();
    let root = serde_json::from_value(json!({
        "id": "root", "sessionId": "root", "preview": "", "ephemeral": false,
        "modelProvider": "openai", "createdAt": 1, "updatedAt": 1,
        "status": {"type": "idle"}, "cwd": std::env::temp_dir(), "cliVersion": "test", "source": "cli", "turns": []
    })).unwrap();
    let handle = AppServerRequestHandle::Remote(remote.request_handle());
    assert_eq!(
        descendants(&handle, &root).await.unwrap_err(),
        "Task descendant coverage is unavailable."
    );
    assert_eq!(
        descendants(&handle, &root).await.unwrap_err(),
        "Task descendant coverage is unavailable."
    );
    assert_eq!(
        descendants(&handle, &root).await.unwrap_err(),
        "Task descendant coverage is unavailable."
    );
    rpc.await.unwrap();
}

#[tokio::test]
async fn consumer_usage_includes_verified_descendants_and_labels_index_partial() {
    use codex_app_server_client::RemoteAppServerClient;
    use codex_app_server_client::RemoteAppServerConnectArgs;
    use codex_app_server_client::RemoteAppServerEndpoint;
    use futures::SinkExt;
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    let server = MockServer::start().await;
    let (_home, session) = session_for_plan(&server, "plus").await;
    Mock::given(method("POST"))
        .and(path("/backend-api/wham/usage/thread_usage/query_v2"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = request.body_json().unwrap();
            assert_eq!(body["threads"][0]["thread_id"], "root");
            assert_eq!(body["threads"][0]["descendant_thread_ids"], json!(["child"]));
            ResponseTemplate::new(/*s*/ 200).set_body_json(json!({"data_as_of": null, "threads": [{
                "thread_id": "root", "data_status": "available", "usage_source": "plan_and_credits", "groups": []
            }]}))
        })
        .expect(/*r*/ 1)
        .mount(&server)
        .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let websocket_url = format!("ws://{}", listener.local_addr().unwrap());
    let rpc = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let mut lists = 0;
        while let Some(Ok(Message::Text(text))) = socket.next().await {
            let request: serde_json::Value = serde_json::from_str(&text).unwrap();
            let result = match request["method"].as_str() {
                Some("initialize") => {
                    json!({"userAgent": "analytics-test", "codexHome": "/unused", "platformFamily": "unix", "platformOs": "linux"})
                }
                Some("thread/list") => {
                    lists += 1;
                    let params = &request["params"];
                    let descendant = params["ancestorThreadId"].as_str().is_some();
                    let archived = params["archived"].as_bool().unwrap();
                    let id = if descendant { "child" } else { "root" };
                    let data = if descendant && archived {
                        Vec::new()
                    } else {
                        vec![json!({
                            "id": id, "sessionId": "root", "name": id, "preview": "", "ephemeral": false,
                            "parentThreadId": if descendant { Some("root") } else { None },
                            "modelProvider": "openai", "createdAt": chrono::Utc::now().timestamp(), "updatedAt": chrono::Utc::now().timestamp(),
                            "status": {"type": "idle"}, "cwd": std::env::temp_dir(), "cliVersion": "test", "source": "cli", "turns": []
                        })]
                    };
                    json!({"data": data, "nextCursor": null, "backwardsCursor": null, "ancestorFilterApplied": if descendant { Some(true) } else { None }, "relationLimitReached": false})
                }
                _ => continue,
            };
            socket
                .send(Message::Text(
                    json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            if lists == 3 {
                break;
            }
        }
    });
    let remote = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
        endpoint: RemoteAppServerEndpoint::WebSocket {
            websocket_url,
            auth_token: None,
        },
        client_name: "analytics-test".into(),
        client_version: "test".into(),
        experimental_api: true,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 16,
    })
    .await
    .unwrap();
    let handle = AppServerRequestHandle::Remote(remote.request_handle());
    let result = read_consumer(&handle, &session, None).await.unwrap();
    assert!(result.partial_history);
    assert!(!result.partial_usage);
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].display_title(), "root");
    rpc.await.unwrap();
}
