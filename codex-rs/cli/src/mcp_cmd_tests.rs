//! Verifies MCP client-secret argument requirements and redacted CLI diagnostics.

use std::ffi::OsString;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

use clap::Parser;
use codex_config::types::McpServerConfig;
use codex_config::types::McpServerOAuthConfig;
use codex_config::types::McpServerTransportConfig;
use codex_exec_server::ExecServerError;
use codex_exec_server::HttpClient;
use codex_exec_server::HttpHeader;
use codex_exec_server::HttpRequestParams;
use codex_exec_server::HttpRequestResponse;
use codex_exec_server::HttpResponseBodyStream;
use futures::FutureExt;
use futures::future::BoxFuture;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::McpCli;
use super::McpOAuthClientRegistration;
use super::McpSubcommand;
use super::escape_terminal_text;
use super::run_device_auth_login;
use super::validate_device_auth_options;

#[test]
fn oauth_client_secret_is_redacted_in_parsed_command_debug() {
    let cli = McpCli::try_parse_from([
        "mcp",
        "add",
        "private",
        "--url",
        "https://example.com/mcp",
        "--oauth-client-id",
        "registered-client",
        "--oauth-client-secret",
        "cli-secret-marker",
    ])
    .expect("parse confidential client arguments");
    let debug = format!("{cli:?}");
    assert!(!debug.contains("cli-secret-marker"));
    assert!(debug.contains("oauth_client_secret: Some(<redacted>)"));
    let McpSubcommand::Add(add) = cli.subcommand else {
        panic!("expected MCP add");
    };
    let http = add.transport_args.streamable_http.expect("HTTP arguments");
    assert_eq!(
        http.oauth_client_secret
            .as_ref()
            .map(|secret| secret.as_str()),
        Some("cli-secret-marker")
    );
}

#[test]
fn oauth_client_secret_requires_url_and_client_id_without_disclosure() {
    for args in [
        vec!["--url", "https://example.com/mcp"],
        vec!["--oauth-client-id", "registered-client"],
    ] {
        let error = McpCli::try_parse_from(
            [
                "mcp",
                "add",
                "private",
                "--oauth-client-secret",
                "cli-secret-marker",
            ]
            .into_iter()
            .chain(args),
        )
        .expect_err("client secret requires both HTTP URL and client ID");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
        assert!(!error.to_string().contains("cli-secret-marker"));
    }
}

#[test]
fn mcp_device_auth_flag_parses_and_conflicts_with_no_browser() {
    let help = McpCli::try_parse_from(["mcp", "login", "--help"])
        .expect_err("login help exits without a server name");
    assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
    assert!(help.to_string().contains("--device-auth"));

    let cli = McpCli::try_parse_from(["mcp", "login", "synthetic", "--device-auth"])
        .expect("device authorization flag parses");
    let McpSubcommand::Login(args) = cli.subcommand else {
        panic!("expected MCP login");
    };
    assert!(args.device_auth);
    assert!(!args.no_browser);

    let error =
        McpCli::try_parse_from(["mcp", "login", "synthetic", "--device-auth", "--no-browser"])
            .expect_err("device auth and paste-callback modes are mutually exclusive");
    assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
}

#[test]
fn mcp_device_auth_rejects_unsupported_client_credentials_and_registration() {
    for registration in [
        McpOAuthClientRegistration::Auto,
        McpOAuthClientRegistration::Dcr,
        McpOAuthClientRegistration::Cimd,
    ] {
        let error = validate_device_auth_options(registration, true, true)
            .expect_err("device flow must not silently discard a configured client secret");
        assert!(
            error
                .to_string()
                .contains("does not support a configured client secret")
        );
    }

    let error = validate_device_auth_options(McpOAuthClientRegistration::Cimd, false, false)
        .expect_err("device flow must not silently turn CIMD into DCR");
    assert!(
        error
            .to_string()
            .contains("does not support CIMD registration")
    );

    validate_device_auth_options(McpOAuthClientRegistration::Cimd, true, false)
        .expect("an existing client ID makes registration strategy inapplicable");
    validate_device_auth_options(McpOAuthClientRegistration::Auto, false, false)
        .expect("auto may use advertised DCR for a device grant");
    validate_device_auth_options(McpOAuthClientRegistration::Dcr, false, false)
        .expect("explicit DCR is supported");
}

#[test]
fn mcp_device_prompt_escapes_terminal_controls() {
    assert_eq!(escape_terminal_text("ABCD\n\u{1b}[2J"), "ABCD\\n\\u{1b}[2J");
}

struct DeviceAuthTestHttpClient {
    base_url: String,
    requests: Mutex<Vec<(String, String)>>,
}

impl DeviceAuthTestHttpClient {
    fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.to_string(),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn response(status: u16, body: Vec<u8>) -> HttpRequestResponse {
        HttpRequestResponse {
            status,
            headers: vec![HttpHeader {
                name: "content-type".to_string(),
                value: "application/json".to_string(),
                value_env_var: None,
            }],
            body: body.into(),
        }
    }

    fn json_response(&self, status: u16, body: serde_json::Value) -> HttpRequestResponse {
        Self::response(
            status,
            serde_json::to_vec(&body).expect("encode synthetic OAuth response"),
        )
    }
}

impl HttpClient for DeviceAuthTestHttpClient {
    fn http_request(
        &self,
        params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<HttpRequestResponse, ExecServerError>> {
        self.requests
            .lock()
            .expect("record synthetic OAuth request")
            .push((params.method.clone(), params.url.clone()));
        let path = params.url.strip_prefix(&self.base_url).unwrap_or_default();
        let response = match (params.method.as_str(), path) {
            ("GET", "/mcp") => Self::response(405, Vec::new()),
            ("GET", path) if path.contains("oauth-authorization-server") => self.json_response(
                200,
                json!({
                    "issuer": format!("{}/mcp", self.base_url),
                    "authorization_endpoint": format!("{}/authorize", self.base_url),
                    "token_endpoint": format!("{}/token", self.base_url),
                    "device_authorization_endpoint": format!("{}/device", self.base_url),
                    "grant_types_supported": [
                        "urn:ietf:params:oauth:grant-type:device_code",
                        "refresh_token"
                    ],
                    "authorization_response_iss_parameter_supported": false
                }),
            ),
            ("POST", "/device") => self.json_response(
                200,
                json!({
                    "device_code": "synthetic-device-code",
                    "user_code": "SYNTHETIC-CODE",
                    "verification_uri": format!("{}/verify", self.base_url),
                    "expires_in": 60,
                    "interval": 1
                }),
            ),
            ("POST", "/token") => self.json_response(
                200,
                json!({
                    "access_token": "synthetic-access-token",
                    "refresh_token": "synthetic-refresh-token",
                    "token_type": "Bearer",
                    "expires_in": 3600
                }),
            ),
            ("GET", _) => Self::response(404, Vec::new()),
            _ => {
                return async {
                    Err(ExecServerError::HttpRequest(
                        "unexpected synthetic OAuth request".to_string(),
                    ))
                }
                .boxed();
            }
        };
        async move { Ok(response) }.boxed()
    }

    fn http_request_stream(
        &self,
        _params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<(HttpRequestResponse, HttpResponseBodyStream), ExecServerError>> {
        async {
            Err(ExecServerError::HttpRequest(
                "unexpected streaming synthetic OAuth request".to_string(),
            ))
        }
        .boxed()
    }
}

struct TempCodexHome {
    _guard: MutexGuard<'static, ()>,
    _directory: tempfile::TempDir,
    original: Option<OsString>,
}

impl TempCodexHome {
    fn new() -> Self {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let guard = LOCK
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let directory = tempfile::tempdir().expect("create temporary CODEX_HOME");
        let original = std::env::var_os("CODEX_HOME");
        unsafe {
            std::env::set_var("CODEX_HOME", directory.path());
        }
        Self {
            _guard: guard,
            _directory: directory,
            original,
        }
    }
}

impl Drop for TempCodexHome {
    fn drop(&mut self) {
        unsafe {
            if let Some(original) = &self.original {
                std::env::set_var("CODEX_HOME", original);
            } else {
                std::env::remove_var("CODEX_HOME");
            }
        }
    }
}

#[tokio::test]
async fn device_auth_login_discovers_prompts_and_saves_issuer_bound_tokens() -> anyhow::Result<()> {
    let _home = TempCodexHome::new();
    let base_url = "http://127.0.0.1:43219";
    let server_url = format!("{base_url}/mcp");
    let http_client = Arc::new(DeviceAuthTestHttpClient::new(base_url));
    let server = McpServerConfig {
        auth: Default::default(),
        transport: McpServerTransportConfig::StreamableHttp {
            url: server_url.clone(),
            bearer_token_env_var: None,
            http_headers: None,
            env_http_headers: None,
            http_headers_helper: None,
        },
        environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
        enabled: true,
        required: false,
        startup_readiness: Default::default(),
        supports_parallel_tool_calls: false,
        tool_input_schema_max_bytes: None,
        omit_tools_from: None,
        disabled_reason: None,
        startup_timeout_sec: None,
        tool_timeout_sec: None,
        default_tools_approval_mode: None,
        enabled_tools: None,
        disabled_tools: None,
        scopes: Some(vec!["ops:read".to_string()]),
        oauth: Some(McpServerOAuthConfig {
            client_id: Some("synthetic-client".to_string()),
            ..Default::default()
        }),
        oauth_resource: None,
        tools: Default::default(),
    };
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let captured_prompts = Arc::clone(&prompts);

    run_device_auth_login(
        "synthetic-server",
        &server,
        McpOAuthClientRegistration::Auto,
        None,
        http_client.clone(),
        codex_config::types::OAuthCredentialsStoreMode::File,
        codex_config::types::AuthKeyringBackendKind::Direct,
        move |prompt| {
            captured_prompts.lock().expect("capture CLI prompt").push((
                prompt.server_name().to_string(),
                prompt.verification_uri().to_string(),
                prompt.user_code().to_string(),
            ));
        },
    )
    .await?;

    assert_eq!(
        prompts.lock().expect("read CLI prompt").as_slice(),
        [(
            "synthetic-server".to_string(),
            format!("{base_url}/verify"),
            "SYNTHETIC-CODE".to_string(),
        )]
    );
    let issuer = server_url.clone();
    let saved = codex_rmcp_client::stored_oauth_credentials(
        "synthetic-server",
        &server_url,
        codex_config::types::OAuthCredentialsStoreMode::File,
        codex_config::types::AuthKeyringBackendKind::Direct,
    )?
    .expect("device flow saves usable credentials after token success");
    assert_eq!(saved.issuer.as_deref(), Some(issuer.as_str()));
    assert_eq!(saved.client_id, "synthetic-client");
    assert_eq!(
        serde_json::to_value(&saved.token_response)?["refresh_token"],
        "synthetic-refresh-token"
    );
    let requests = http_client
        .requests
        .lock()
        .expect("read synthetic OAuth requests");
    assert!(
        requests
            .iter()
            .any(|(method, url)| { method == "GET" && url.contains("oauth-authorization-server") })
    );
    assert!(
        requests
            .iter()
            .any(|(method, url)| method == "POST" && url == &format!("{base_url}/device"))
    );
    assert!(
        requests
            .iter()
            .any(|(method, url)| method == "POST" && url == &format!("{base_url}/token"))
    );
    Ok(())
}
