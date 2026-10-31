//! Verify bounded chat discovery, account eligibility, and partial estimate handling.

use super::super::client::tests::live;
use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[tokio::test]
async fn incomplete_chat_batches_retain_available_estimates_without_inventing_zeroes() {
    let server = MockServer::start().await;
    let (_home, live) = live(&server, "enterprise_cbp_usage_based").await;
    Mock::given(method("POST")).and(path("/backend-api/wham/usage/thread_usage/query"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = request.body_json().unwrap();
            let ids = body["thread_ids"].as_array().unwrap();
            if ids.len() > 1 || ids[0] == "incomplete" {
                ResponseTemplate::new(/*s*/ 503).set_body_json(json!({"detail": "Thread usage is temporarily unavailable."}))
            } else {
                ResponseTemplate::new(/*s*/ 200).set_body_json(json!({"threads": [{
                    "thread_id": ids[0], "estimated_usage_credits_micros": if ids[0] == "missing" { None } else { Some(1250000) },
                    "estimated_usage_usd_micros": null, "groups": null,
                }]}))
            }
        }).expect(/*r*/ 5).mount(&server).await;
    let session = live.session().await.unwrap();
    assert_eq!(
        estimates(session, &["available", "incomplete", "missing"])
            .await
            .unwrap(),
        vec![ThreadUsage {
            thread_id: "available".into(),
            estimated_usage_credits_micros: 1250000,
            estimated_usage_usd_micros: None,
            native_usage_usd_micros: None,
            groups: Vec::new(),
        }]
    );
}

#[tokio::test]
async fn bounded_repairing_chat_listing_keeps_estimate_budget_and_marks_partial_ranking() {
    use codex_app_server_client::RemoteAppServerClient;
    use codex_app_server_client::RemoteAppServerConnectArgs;
    use codex_app_server_client::RemoteAppServerEndpoint;
    use futures::SinkExt;
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    let server = MockServer::start().await;
    let (_home, live) = live(&server, "enterprise_cbp_usage_based").await;
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
                    json!({"userAgent": "analytics-test", "codexHome": "/unused"})
                }
                Some("thread/list") => {
                    let params = &request["params"];
                    pages.push(json!([
                        params["cursor"],
                        params["limit"],
                        params["useStateDbOnly"].as_bool().unwrap_or_default(),
                        params["modelProviders"],
                        params["sortKey"]
                    ]));
                    if pages.len() == 1 {
                        // A stale root on the newest-created page must not stop cursor continuation.
                        tokio::time::pause();
                        tokio::time::advance(std::time::Duration::from_secs(/*secs*/ 26)).await;
                        tokio::time::resume();
                    }
                    let range = if pages.len() == 1 { 0..100 } else { 100..200 };
                    let cutoff = chrono::Utc::now().timestamp() - 31 * 24 * 60 * 60;
                    let data = range
                        .map(|index| {
                            let child_only_page = pages.len() == 1 && index != 0;
                            json!({
                                "id": format!("chat-{index}"), "sessionId": format!("chat-{index}"),
                                "preview": "", "name": format!("Chat {index}"), "ephemeral": false,
                                "parentThreadId": if child_only_page { Some("root-parent") } else { None },
                                "modelProvider": "openai", "createdAt": if index < 100 { 300 - index } else { 200 - (index - 100) },
                                "updatedAt": if index == 0 { cutoff } else { chrono::Utc::now().timestamp() }, "status": {"type": "idle"},
                                "cwd": std::env::temp_dir(), "cliVersion": "test", "source": "cli", "turns": [],
                            })
                        })
                        .collect::<Vec<_>>();
                    json!({"data": data, "nextCursor": if pages.len() == 1 { Some("next") } else { None }})
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
        required_human_approval: true,
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
    Mock::given(method("POST")).and(path("/backend-api/wham/usage/thread_usage/query"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = request.body_json().unwrap();
            let ids = body["thread_ids"].as_array().unwrap();
            assert!(ids.len() <= 100);
            if ids.iter().any(|id| id == "chat-135") {
                return ResponseTemplate::new(/*s*/ 503)
                    .set_body_json(json!({"detail": "Thread usage is temporarily unavailable."}));
            }
            let threads = ids.iter().map(|id| json!({
                "thread_id": id,
                "estimated_usage_credits_micros": id.as_str().unwrap().strip_prefix("chat-").unwrap().parse::<i64>().unwrap(),
            })).collect::<Vec<_>>();
            ResponseTemplate::new(/*s*/ 200).set_body_json(json!({"threads": threads}))
        }).expect(/*r*/ 2..=16).mount(&server).await;
    let handle = AppServerRequestHandle::Remote(remote.request_handle());
    let live = Arc::new(live);
    let chats = read(handle.clone(), Arc::clone(&live), None)
        .await
        .unwrap()
        .unwrap();
    assert!(chats.rows.is_empty());
    assert!(chats.truncated);
    let next_cursor = chats.next_cursor.clone().unwrap();
    let older_chats = read(handle, live, Some(next_cursor.clone()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(older_chats.rows.len(), 100);
    assert_eq!(older_chats.rows.first().unwrap().title, "Chat 199");
    assert_eq!(older_chats.rows.last().unwrap().title, "Chat 135");
    assert!(older_chats.rows.last().unwrap().usage.is_none());
    assert!(older_chats.truncated);
    assert!(older_chats.next_cursor.is_none());
    assert_eq!(
        rpc.await.unwrap(),
        vec![
            json!([null, 100, false, [], "created_at"]),
            json!([next_cursor, 100, false, [], "created_at"])
        ]
    );
}

#[tokio::test]
async fn unavailable_chat_batches_bound_requests_during_an_outage() {
    let server = MockServer::start().await;
    let (_home, live) = live(&server, "enterprise_cbp_usage_based").await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(/*s*/ 503))
        .expect(/*r*/ 15)
        .mount(&server)
        .await;
    let ids = (0..100).map(|i| format!("chat-{i}")).collect::<Vec<_>>();
    let ids = ids.iter().map(String::as_str).collect::<Vec<_>>();
    assert_eq!(
        estimates(live.session().await.unwrap(), &ids)
            .await
            .unwrap(),
        Vec::<ThreadUsage>::new()
    );
}
