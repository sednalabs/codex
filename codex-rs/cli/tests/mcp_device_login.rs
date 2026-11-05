use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

async fn mount_issuer_metadata(server: &MockServer, issuer: Option<&str>) {
    let base = server.uri();
    let mut metadata = json!({
        "resource": format!("{base}/mcp"),
        "authorization_servers": [base],
        "authorization_endpoint": format!("{base}/authorize"),
        "registration_endpoint": format!("{base}/register"),
        "device_authorization_endpoint": format!("{base}/device"),
        "token_endpoint": format!("{base}/token"),
        "response_types_supported": ["code"],
        "grant_types_supported": [
            "urn:ietf:params:oauth:grant-type:device_code",
            "refresh_token"
        ],
        "scopes_supported": ["read", "write"]
    });
    if let Some(issuer) = issuer {
        metadata["issuer"] = json!(issuer);
    }
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(metadata))
        .mount(server)
        .await;
}

fn fixture_config(home: &TempDir, server: &MockServer, client_id: Option<&str>) -> Result<()> {
    let base = server.uri();
    let oauth = client_id
        .map(|client_id| format!("[mcp_servers.fixture.oauth]\nclient_id = \"{client_id}\"\n"))
        .unwrap_or_default();
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "mcp_oauth_credentials_store = \"file\"\n[mcp_servers.fixture]\nurl = \"{base}/mcp\"\n{oauth}"
        ),
    )?;
    Ok(())
}

fn codex_cli() -> Result<assert_cmd::Command> {
    Ok(assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin(
        "codex",
    )?))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn device_login_stores_synthetic_credentials_bound_to_the_discovered_issuer() -> Result<()> {
    let server = MockServer::start().await;
    let base = server.uri();
    mount_issuer_metadata(&server, Some(&base)).await;
    Mock::given(method("POST"))
        .and(path("/device"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "synthetic-device-code",
            "user_code": "SYNTHETIC-CODE",
            "verification_uri": "https://issuer.test/device",
            "expires_in": 60,
            "interval": 1
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "synthetic-access-token",
            "refresh_token": "synthetic-refresh-token",
            "token_type": "Bearer",
            "expires_in": 3600
        })))
        .expect(1)
        .mount(&server)
        .await;

    let home = TempDir::new()?;
    fixture_config(&home, &server, Some("fixed-device-client"))?;
    let output = codex_cli()?
        .env("CODEX_HOME", home.path())
        .args(["mcp", "login", "--device-auth", "fixture"])
        .timeout(Duration::from_secs(30))
        .output()?;
    assert!(
        output.status.success(),
        "device login failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("SYNTHETIC-CODE"));

    let requests = server.received_requests().await.unwrap();
    let device_request = requests
        .iter()
        .find(|request| request.method.as_str() == "POST" && request.url.path() == "/device")
        .expect("device authorization request should be sent");
    let device_form: BTreeMap<String, String> = url::form_urlencoded::parse(&device_request.body)
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    assert_eq!(
        device_form.get("client_id").map(String::as_str),
        Some("fixed-device-client")
    );
    assert_eq!(
        device_form.get("scope").map(String::as_str),
        Some("read write")
    );
    assert!(
        device_form
            .get("code_challenge")
            .is_some_and(|challenge| !challenge.is_empty())
    );
    assert_eq!(
        device_form.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
    let token_request = requests
        .iter()
        .find(|request| request.method.as_str() == "POST" && request.url.path() == "/token")
        .expect("device token request should be sent");
    let token_form: BTreeMap<String, String> = url::form_urlencoded::parse(&token_request.body)
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    assert_eq!(
        token_form.get("grant_type").map(String::as_str),
        Some("urn:ietf:params:oauth:grant-type:device_code")
    );
    assert_eq!(
        token_form.get("device_code").map(String::as_str),
        Some("synthetic-device-code")
    );
    assert!(
        token_form
            .get("code_verifier")
            .is_some_and(|verifier| !verifier.is_empty())
    );

    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.path().join(".credentials.json"))?)?;
    let entries = stored
        .as_object()
        .expect("credential store should be an object");
    let credential = entries
        .values()
        .find(|entry| entry["server_name"] == "fixture")
        .expect("device credentials should be stored");
    assert_eq!(credential["issuer"], base);
    assert_eq!(credential["server_url"], format!("{base}/mcp"));
    assert_eq!(credential["client_id"], "fixed-device-client");
    assert_eq!(credential["access_token"], "synthetic-access-token");
    assert_eq!(credential["refresh_token"], "synthetic-refresh-token");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn device_login_rejects_missing_or_mismatched_issuer_before_grant() -> Result<()> {
    for issuer in [None, Some("https://different-issuer.example")] {
        let server = MockServer::start().await;
        mount_issuer_metadata(&server, issuer).await;
        Mock::given(method("POST"))
            .and(path("/device"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let home = TempDir::new()?;
        fixture_config(&home, &server, Some("fixed-device-client"))?;
        let output = codex_cli()?
            .env("CODEX_HOME", home.path())
            .args(["mcp", "login", "--device-auth", "fixture"])
            .timeout(Duration::from_secs(30))
            .output()?;
        assert!(!output.status.success());
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn device_login_registers_a_public_client_from_same_issuer_metadata() -> Result<()> {
    let server = MockServer::start().await;
    let base = server.uri();
    mount_issuer_metadata(&server, Some(&base)).await;
    Mock::given(method("POST"))
        .and(path("/register"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "client_id": "registered-device-client"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/device"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "synthetic-device-code",
            "user_code": "SYNTHETIC-CODE",
            "verification_uri": "https://issuer.test/device",
            "expires_in": 60,
            "interval": 1
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "synthetic-access-token",
            "refresh_token": "synthetic-refresh-token",
            "token_type": "Bearer",
            "expires_in": 3600
        })))
        .expect(1)
        .mount(&server)
        .await;

    let home = TempDir::new()?;
    fixture_config(&home, &server, /*client_id*/ None)?;
    let output = codex_cli()?
        .env("CODEX_HOME", home.path())
        .args(["mcp", "login", "--device-auth", "fixture"])
        .timeout(Duration::from_secs(30))
        .output()?;
    assert!(
        output.status.success(),
        "device login failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let requests = server.received_requests().await.unwrap();
    let registration = requests
        .iter()
        .find(|request| request.method.as_str() == "POST" && request.url.path() == "/register")
        .expect("client registration request should be sent");
    let registration_body: serde_json::Value = serde_json::from_slice(&registration.body)?;
    assert_eq!(registration_body["token_endpoint_auth_method"], "none");
    assert_eq!(
        registration_body["grant_types"][0],
        "urn:ietf:params:oauth:grant-type:device_code"
    );
    assert_eq!(registration_body["grant_types"][1], "refresh_token");
    assert_eq!(registration_body["scope"], "read write");
    let device_request = requests
        .iter()
        .find(|request| request.method.as_str() == "POST" && request.url.path() == "/device")
        .expect("device authorization request should be sent");
    let device_form: BTreeMap<String, String> = url::form_urlencoded::parse(&device_request.body)
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    assert_eq!(
        device_form.get("client_id").map(String::as_str),
        Some("registered-device-client")
    );
    Ok(())
}
