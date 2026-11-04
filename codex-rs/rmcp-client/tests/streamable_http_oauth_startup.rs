mod streamable_http_test_support;

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_exec_server::Environment;
use codex_rmcp_client::McpAuthState;
use codex_rmcp_client::McpLoginRequirement;
use codex_rmcp_client::RmcpClient;
use codex_rmcp_client::StoredOAuthTokens;
use codex_rmcp_client::WrappedOAuthTokenResponse;
use codex_rmcp_client::determine_streamable_http_auth_status;
use codex_rmcp_client::is_authentication_required_error;
use codex_rmcp_client::save_oauth_tokens;
use oauth2::AccessToken;
use oauth2::RefreshToken;
use oauth2::Scope;
use oauth2::basic::BasicTokenType;
use pretty_assertions::assert_eq;
use rmcp::transport::auth::OAuthTokenResponse;
use rmcp::transport::auth::VendorExtraTokenFields;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;
use tokio::process::Command;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::ResponseTemplate;
use wiremock::matchers::body_string_contains;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

use streamable_http_test_support::initialize_client;

const SERVER_NAME: &str = "test-streamable-http-oauth-startup";
const EXPIRED_ACCESS_TOKEN: &str = "expired-access-token";
const REFRESH_TOKEN: &str = "valid-refresh-token";
const REFRESHED_ACCESS_TOKEN: &str = "refreshed-access-token";
const CHILD_SERVER_URL_ENV: &str = "MCP_TEST_OAUTH_STARTUP_SERVER_URL";
const UNREFRESHABLE_SERVER_URL: &str = "https://unrefreshable.example/mcp";
const UNEXPIRED_SERVER_URL: &str = "https://unexpired.example/mcp";
const REFRESHABLE_SERVER_URL: &str = "https://refreshable.example/mcp";
const INVALID_TOKEN_SERVER_NAME: &str = "test-streamable-http-oauth-invalid-token";
const INVALID_TOKEN_ACCESS_TOKEN: &str = "initial-access-token";
const INVALID_TOKEN_REFRESH_TOKEN: &str = "invalid-token-test-refresh";

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn refreshes_expired_persisted_token_before_initialize() -> anyhow::Result<()> {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server/mcp"))
        .and(header(
            "user-agent",
            concat!("codex-mcp-client/", env!("CARGO_PKG_VERSION")),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": format!("{}/mcp", server.uri()),
            "authorization_endpoint": format!("{}/oauth/authorize", server.uri()),
            "token_endpoint": format!("{}/oauth/token", server.uri()),
            "scopes_supported": ["profile", "offline_access"],
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(header(
            "user-agent",
            concat!("codex-mcp-client/", env!("CARGO_PKG_VERSION")),
        ))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains(format!(
            "refresh_token={REFRESH_TOKEN}"
        )))
        .respond_with(|request: &Request| {
            let body = String::from_utf8_lossy(&request.body);
            assert!(
                !body.contains("scope="),
                "refresh must not broaden explicit persisted scopes from AS metadata: {body}"
            );
            ResponseTemplate::new(200).set_body_json(json!({
                "access_token": REFRESHED_ACCESS_TOKEN,
                "token_type": "Bearer",
                "expires_in": 7200,
                "refresh_token": REFRESH_TOKEN,
            }))
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(header(
            "user-agent",
            concat!("codex-mcp-client/", env!("CARGO_PKG_VERSION")),
        ))
        .and(header(
            "authorization",
            format!("Bearer {REFRESHED_ACCESS_TOKEN}"),
        ))
        .respond_with(|request: &Request| {
            let body: Value = request.body_json().expect("valid JSON-RPC request");
            match body.get("method").and_then(Value::as_str) {
                Some("initialize") => ResponseTemplate::new(200).set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": body.get("id").cloned().unwrap_or(Value::Null),
                    "result": {
                        "protocolVersion": body
                            .pointer("/params/protocolVersion")
                            .cloned()
                            .unwrap_or_else(|| json!("2025-06-18")),
                        "capabilities": {},
                        "serverInfo": {
                            "name": "oauth-startup-test",
                            "version": "0.0.0-test",
                        },
                    },
                })),
                Some("notifications/initialized") => ResponseTemplate::new(202),
                method => ResponseTemplate::new(400)
                    .set_body_string(format!("unexpected JSON-RPC method: {method:?}")),
            }
        })
        .expect(2)
        .mount(&server)
        .await;

    let codex_home = TempDir::new()?;
    let server_url = format!("{}/mcp", server.uri());

    // Credential storage resolves CODEX_HOME from the process environment.
    // Run the client half of the test in an ignored helper test so it can use
    // an isolated home without mutating the parent test runner's environment.
    let status = Command::new(std::env::current_exe()?)
        .args(["oauth_startup_child", "--exact", "--ignored", "--nocapture"])
        .env("CODEX_HOME", codex_home.path())
        .env(CHILD_SERVER_URL_ENV, server_url)
        .status()
        .await?;
    assert!(status.success(), "OAuth startup child failed: {status}");
    server.verify().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn invalid_token_refresh_never_replays_tool_calls() -> anyhow::Result<()> {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": format!("{}/mcp", server.uri()),
            "authorization_endpoint": format!("{}/oauth/authorize", server.uri()),
            "token_endpoint": format!("{}/oauth/token", server.uri()),
            "scopes_supported": ["profile", "ops:read"],
        })))
        .mount(&server)
        .await;

    let refresh_attempts = Arc::new(AtomicUsize::new(0));
    let refresh_attempts_for_response = Arc::clone(&refresh_attempts);
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains(format!(
            "refresh_token={INVALID_TOKEN_REFRESH_TOKEN}"
        )))
        .respond_with(move |_request: &Request| {
            let attempt = refresh_attempts_for_response.fetch_add(1, Ordering::SeqCst);
            let access_token = match attempt {
                0 => "refreshed-read-only-token",
                _ => "refreshed-future-token",
            };
            ResponseTemplate::new(200).set_body_json(json!({
                "access_token": access_token,
                "token_type": "Bearer",
                "expires_in": 7200,
                "refresh_token": INVALID_TOKEN_REFRESH_TOKEN,
            }))
        })
        .expect(2)
        .mount(&server)
        .await;

    let read_only_attempts = Arc::new(AtomicUsize::new(0));
    let mutating_attempts = Arc::new(AtomicUsize::new(0));
    let metadata_challenge_attempts = Arc::new(AtomicUsize::new(0));
    let read_only_attempts_for_response = Arc::clone(&read_only_attempts);
    let mutating_attempts_for_response = Arc::clone(&mutating_attempts);
    let metadata_challenge_attempts_for_response = Arc::clone(&metadata_challenge_attempts);
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(move |request: &Request| {
            let body: Value = request.body_json().expect("valid JSON-RPC request");
            let id = body.get("id").cloned().unwrap_or(Value::Null);
            match body.get("method").and_then(Value::as_str) {
                Some("initialize") => ResponseTemplate::new(200).set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": body.pointer("/params/protocolVersion")
                            .cloned().unwrap_or_else(|| json!("2025-06-18")),
                        "capabilities": {},
                        "serverInfo": {"name": "invalid-token-test", "version": "0.0.0-test"},
                    },
                })),
                Some("notifications/initialized") => ResponseTemplate::new(202),
                Some("tools/call") => {
                    let name = body.pointer("/params/name").and_then(Value::as_str);
                    let attempt = match name {
                        Some("read_only") => {
                            read_only_attempts_for_response.fetch_add(1, Ordering::SeqCst)
                        }
                        Some("mutating") => {
                            mutating_attempts_for_response.fetch_add(1, Ordering::SeqCst)
                        }
                        Some("result_challenge") => {
                            metadata_challenge_attempts_for_response.fetch_add(1, Ordering::SeqCst);
                            return ResponseTemplate::new(200).set_body_json(json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "content": [{"type": "text", "text": "denied"}],
                                    "isError": true,
                                    "_meta": {"mcp/www_authenticate": "Bearer error=invalid_token"},
                                },
                            }));
                        }
                        _ => return ResponseTemplate::new(400),
                    };
                    if attempt == 0 {
                        ResponseTemplate::new(401)
                            .insert_header("WWW-Authenticate", r#"Bearer error="invalid_token""#)
                    } else {
                        ResponseTemplate::new(200).set_body_json(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {"content": [{"type": "text", "text": "ok"}]},
                        }))
                    }
                }
                _ => ResponseTemplate::new(400),
            }
        })
        .expect(5)
        .mount(&server)
        .await;

    let codex_home = TempDir::new()?;
    let server_url = format!("{}/mcp", server.uri());
    let status = Command::new(std::env::current_exe()?)
        .args([
            "invalid_token_recovery_child",
            "--exact",
            "--ignored",
            "--nocapture",
        ])
        .env("CODEX_HOME", codex_home.path())
        .env(CHILD_SERVER_URL_ENV, server_url)
        .status()
        .await?;
    assert!(
        status.success(),
        "OAuth invalid_token child failed: {status}"
    );
    assert_eq!(refresh_attempts.load(Ordering::SeqCst), 2);
    assert_eq!(read_only_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(mutating_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(metadata_challenge_attempts.load(Ordering::SeqCst), 1);
    server.verify().await;
    Ok(())
}

#[tokio::test]
#[ignore = "spawned by invalid_token_refresh_never_replays_tool_calls"]
async fn invalid_token_recovery_child() -> anyhow::Result<()> {
    let server_url = std::env::var(CHILD_SERVER_URL_ENV)?;
    let mut token_response = OAuthTokenResponse::new(
        AccessToken::new(INVALID_TOKEN_ACCESS_TOKEN.to_string()),
        BasicTokenType::Bearer,
        VendorExtraTokenFields::default(),
    );
    token_response.set_refresh_token(Some(RefreshToken::new(
        INVALID_TOKEN_REFRESH_TOKEN.to_string(),
    )));
    token_response.set_scopes(Some(vec![Scope::new("ops:read".to_string())]));
    let expires_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis() as u64
        + 120_000;
    let tokens = StoredOAuthTokens {
        server_name: INVALID_TOKEN_SERVER_NAME.to_string(),
        url: server_url.clone(),
        client_id: "test-client-id".to_string(),
        token_response: WrappedOAuthTokenResponse(token_response),
        expires_at: Some(expires_at),
    };
    save_oauth_tokens(
        INVALID_TOKEN_SERVER_NAME,
        &tokens,
        OAuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;

    let client = RmcpClient::new_streamable_http_client(
        INVALID_TOKEN_SERVER_NAME,
        &server_url,
        /*bearer_token*/ None,
        /*http_headers*/ None,
        /*env_http_headers*/ None,
        OAuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
        Environment::default_for_tests().get_http_client(),
        /*auth_provider*/ None,
    )
    .await?;
    initialize_client(&client).await?;

    let read_only_error = client
        .call_tool(
            "read_only".to_string(),
            None,
            None,
            Some(Duration::from_secs(5)),
        )
        .await
        .expect_err("read-only annotation cannot authorize automatic replay");
    assert!(
        read_only_error
            .to_string()
            .contains("code=mcp_oauth_invalid_token_refreshed_no_replay")
    );
    assert!(read_only_error.to_string().contains("outcome=unknown"));
    assert!(read_only_error.to_string().contains("retry=read_only_only"));
    assert!(
        read_only_error
            .to_string()
            .contains("Do not repeat a mutation")
    );

    let mutating = client
        .call_tool(
            "mutating".to_string(),
            None,
            None,
            Some(Duration::from_secs(5)),
        )
        .await;
    let mutating_error = mutating.expect_err("mutating tool call must not be replayed");
    assert!(mutating_error.to_string().contains("outcome=unknown"));

    let result_challenge = client
        .call_tool(
            "result_challenge".to_string(),
            None,
            None,
            Some(Duration::from_secs(5)),
        )
        .await?;
    assert!(result_challenge.is_error.unwrap_or(false));
    client.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn reports_auth_status_for_persisted_credentials() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;

    let status = Command::new(std::env::current_exe()?)
        .args([
            "persisted_credentials_auth_status_child",
            "--exact",
            "--ignored",
            "--nocapture",
        ])
        .env("CODEX_HOME", codex_home.path())
        .status()
        .await?;

    assert!(
        status.success(),
        "persisted credentials auth status child failed: {status}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn identifies_expired_unrefreshable_token_startup_error() -> anyhow::Result<()> {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": format!("{}/mcp", server.uri()),
            "authorization_endpoint": format!("{}/oauth/authorize", server.uri()),
            "token_endpoint": format!("{}/oauth/token", server.uri()),
        })))
        .expect(1)
        .mount(&server)
        .await;

    let codex_home = TempDir::new()?;
    let status = Command::new(std::env::current_exe()?)
        .args([
            "expired_unrefreshable_startup_child",
            "--exact",
            "--ignored",
            "--nocapture",
        ])
        .env("CODEX_HOME", codex_home.path())
        .env(CHILD_SERVER_URL_ENV, format!("{}/mcp", server.uri()))
        .status()
        .await?;

    assert!(
        status.success(),
        "expired OAuth startup child failed: {status}"
    );
    server.verify().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "spawned by reports_auth_status_for_persisted_credentials"]
async fn persisted_credentials_auth_status_child() -> anyhow::Result<()> {
    let first_login_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": format!("{}/mcp", first_login_server.uri()),
            "authorization_endpoint": format!("{}/oauth/authorize", first_login_server.uri()),
            "token_endpoint": format!("{}/oauth/token", first_login_server.uri()),
        })))
        .expect(1)
        .mount(&first_login_server)
        .await;

    let status = auth_status(&format!("{}/mcp", first_login_server.uri())).await?;
    assert_eq!(status, McpAuthState::LoggedOut(McpLoginRequirement::Login));
    first_login_server.verify().await;

    let response = OAuthTokenResponse::new(
        AccessToken::new(EXPIRED_ACCESS_TOKEN.to_string()),
        BasicTokenType::Bearer,
        VendorExtraTokenFields::default(),
    );
    let tokens = StoredOAuthTokens {
        server_name: SERVER_NAME.to_string(),
        url: UNREFRESHABLE_SERVER_URL.to_string(),
        client_id: "test-client-id".to_string(),
        token_response: WrappedOAuthTokenResponse(response),
        expires_at: Some(0),
    };
    save_oauth_tokens(
        SERVER_NAME,
        &tokens,
        OAuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;

    let status = auth_status(UNREFRESHABLE_SERVER_URL).await?;
    assert_eq!(
        status,
        McpAuthState::LoggedOut(McpLoginRequirement::Reauthentication)
    );

    let response = OAuthTokenResponse::new(
        AccessToken::new("unexpired-access-token".to_string()),
        BasicTokenType::Bearer,
        VendorExtraTokenFields::default(),
    );
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis() as u64;
    let tokens = StoredOAuthTokens {
        server_name: SERVER_NAME.to_string(),
        url: UNEXPIRED_SERVER_URL.to_string(),
        client_id: "test-client-id".to_string(),
        token_response: WrappedOAuthTokenResponse(response),
        expires_at: Some(now.saturating_add(/*rhs*/ 120_000)),
    };
    save_oauth_tokens(
        SERVER_NAME,
        &tokens,
        OAuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;

    let status = auth_status(UNEXPIRED_SERVER_URL).await?;
    assert_eq!(status, McpAuthState::OAuth);

    let mut response = OAuthTokenResponse::new(
        AccessToken::new(EXPIRED_ACCESS_TOKEN.to_string()),
        BasicTokenType::Bearer,
        VendorExtraTokenFields::default(),
    );
    response.set_refresh_token(Some(RefreshToken::new(REFRESH_TOKEN.to_string())));
    let tokens = StoredOAuthTokens {
        server_name: SERVER_NAME.to_string(),
        url: REFRESHABLE_SERVER_URL.to_string(),
        client_id: "test-client-id".to_string(),
        token_response: WrappedOAuthTokenResponse(response),
        expires_at: Some(0),
    };
    save_oauth_tokens(
        SERVER_NAME,
        &tokens,
        OAuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;

    let status = auth_status(REFRESHABLE_SERVER_URL).await?;
    assert_eq!(status, McpAuthState::OAuth);
    Ok(())
}

async fn auth_status(server_url: &str) -> anyhow::Result<McpAuthState> {
    determine_streamable_http_auth_status(
        SERVER_NAME,
        server_url,
        /*bearer_token_env_var*/ None,
        /*http_headers*/ None,
        /*env_http_headers*/ None,
        OAuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "spawned by refreshes_expired_persisted_token_before_initialize"]
async fn oauth_startup_child() -> anyhow::Result<()> {
    let server_url = std::env::var(CHILD_SERVER_URL_ENV)?;

    // Save an expired access token with a valid refresh token so startup must
    // refresh before sending the initialize request.
    let mut response = OAuthTokenResponse::new(
        AccessToken::new(EXPIRED_ACCESS_TOKEN.to_string()),
        BasicTokenType::Bearer,
        VendorExtraTokenFields::default(),
    );
    response.set_refresh_token(Some(RefreshToken::new(REFRESH_TOKEN.to_string())));
    response.set_scopes(Some(vec![
        Scope::new("profile".to_string()),
        Scope::new("ops:read".to_string()),
    ]));
    response.set_expires_in(Some(&Duration::from_secs(7200)));
    let tokens = StoredOAuthTokens {
        server_name: SERVER_NAME.to_string(),
        url: server_url.clone(),
        client_id: "test-client-id".to_string(),
        token_response: WrappedOAuthTokenResponse(response),
        expires_at: Some(0),
    };
    save_oauth_tokens(
        SERVER_NAME,
        &tokens,
        OAuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;

    // This mirrors create_client's transport and initialization setup, except
    // it omits the direct bearer token. Supplying that token would bypass the
    // persisted OAuth credentials and the startup refresh under test.
    let client = RmcpClient::new_streamable_http_client(
        SERVER_NAME,
        &server_url,
        /*bearer_token*/ None,
        /*http_headers*/ None,
        /*env_http_headers*/ None,
        OAuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
        Environment::default_for_tests().get_http_client(),
        /*auth_provider*/ None,
    )
    .await?;

    initialize_client(&client).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "spawned by identifies_expired_unrefreshable_token_startup_error"]
async fn expired_unrefreshable_startup_child() -> anyhow::Result<()> {
    let server_url = std::env::var(CHILD_SERVER_URL_ENV)?;
    let response = OAuthTokenResponse::new(
        AccessToken::new(EXPIRED_ACCESS_TOKEN.to_string()),
        BasicTokenType::Bearer,
        VendorExtraTokenFields::default(),
    );
    let tokens = StoredOAuthTokens {
        server_name: SERVER_NAME.to_string(),
        url: server_url.clone(),
        client_id: "test-client-id".to_string(),
        token_response: WrappedOAuthTokenResponse(response),
        expires_at: Some(0),
    };
    save_oauth_tokens(
        SERVER_NAME,
        &tokens,
        OAuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;

    let client = RmcpClient::new_streamable_http_client(
        SERVER_NAME,
        &server_url,
        /*bearer_token*/ None,
        /*http_headers*/ None,
        /*env_http_headers*/ None,
        OAuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
        Environment::default_for_tests().get_http_client(),
        /*auth_provider*/ None,
    )
    .await?;

    let error = initialize_client(&client)
        .await
        .expect_err("expired token without a refresh token should fail startup");
    assert!(is_authentication_required_error(&error));
    Ok(())
}
