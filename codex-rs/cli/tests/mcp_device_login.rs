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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn device_login_preserves_configured_and_discovered_scopes_on_rejection() -> Result<()> {
    let scopes = [
        "profile",
        "read",
        "write",
        "knowledge.read",
        "knowledge.write",
        "admin",
    ];
    for configured_scopes in [false, true] {
        let server = MockServer::start().await;
        let base = server.uri();
        // The combined discovery document permits the existing standard and
        // protected-resource discovery paths; every endpoint has one issuer.
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issuer": base,
                "resource": format!("{base}/mcp"),
                "authorization_servers": [base],
                "authorization_endpoint": format!("{base}/authorize"),
                "device_authorization_endpoint": format!("{base}/device"),
                "token_endpoint": format!("{base}/token"),
                "registration_endpoint": format!("{base}/register"),
                "response_types_supported": ["code"],
                "grant_types_supported": [
                    "urn:ietf:params:oauth:grant-type:device_code",
                    "refresh_token"
                ],
                "scopes_supported": scopes
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/device"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": "invalid_scope",
                "error_description": "the requested scopes were rejected"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let home = TempDir::new()?;
        let configured = if configured_scopes {
            format!("scopes = {}\n", serde_json::to_string(&scopes)?)
        } else {
            String::new()
        };
        std::fs::write(
            home.path().join("config.toml"),
            format!(
                "mcp_oauth_credentials_store = \"file\"\n[mcp_servers.fixture]\nurl = \"{base}/mcp\"\n{configured}[mcp_servers.fixture.oauth]\nclient_id = \"fixed-device-client\"\n"
            ),
        )?;
        let credentials = home.path().join(".credentials.json");
        let predecessor = serde_json::to_vec(&json!({
            "unrelated|synthetic": {
                "server_name": "unrelated",
                "server_url": "https://unrelated.test/mcp",
                "client_id": "predecessor-client",
                "access_token": "synthetic-predecessor-access",
                "refresh_token": "synthetic-predecessor-refresh",
                "expires_at": null,
                "scopes": ["read"]
            }
        }))?;
        std::fs::write(&credentials, &predecessor)?;

        let mut command = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?);
        command
            .env("CODEX_HOME", home.path())
            .args(["mcp", "login", "--device-auth", "fixture"])
            .timeout(Duration::from_secs(30));
        let output = command.output()?;
        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr)?;
        assert!(stderr.contains("invalid_scope"));
        assert_eq!(std::fs::read(&credentials)?, predecessor);

        let requests = server.received_requests().await.unwrap();
        let posts: Vec<_> = requests
            .iter()
            .filter(|request| request.method.as_str() == "POST")
            .collect();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].url.path(), "/device");
        let mut form: BTreeMap<String, String> = url::form_urlencoded::parse(&posts[0].body)
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        // The random PKCE challenge is the sole unstable request field.
        assert!(form.remove("code_challenge").is_some());
        assert_eq!(
            form,
            BTreeMap::from([
                ("client_id".to_string(), "fixed-device-client".to_string()),
                ("scope".to_string(), scopes.join(" ")),
                ("code_challenge_method".to_string(), "S256".to_string()),
            ])
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn device_login_persists_issuer_bound_tokens_in_the_existing_file_store() -> Result<()> {
    let server = MockServer::start().await;
    let base = server.uri();
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": base,
            "resource": format!("{base}/mcp"),
            "authorization_servers": [base],
            "authorization_endpoint": format!("{base}/authorize"),
            "device_authorization_endpoint": format!("{base}/device"),
            "token_endpoint": format!("{base}/token"),
            "grant_types_supported": ["urn:ietf:params:oauth:grant-type:device_code", "refresh_token"],
            "scopes_supported": ["read", "write"]
        })))
        .mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/device"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "synthetic-device-code",
            "user_code": "SYNTHETIC",
            "verification_uri": format!("{base}/verify"),
            "expires_in": 30,
            "interval": 1
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "synthetic-access",
            "refresh_token": "synthetic-refresh",
            "token_type": "Bearer",
            "scope": "read write"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let home = TempDir::new()?;
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "mcp_oauth_credentials_store = \"file\"\n[mcp_servers.fixture]\nurl = \"{base}/mcp\"\nscopes = [\"read\", \"write\"]\n[mcp_servers.fixture.oauth]\nclient_id = \"fixed-device-client\"\n"
        ),
    )?;
    let mut command = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?);
    command
        .env("CODEX_HOME", home.path())
        .args(["mcp", "login", "--device-auth", "fixture"])
        .timeout(Duration::from_secs(30));
    let output = command.output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let store: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.path().join(".credentials.json"))?)?;
    let entries = store.as_object().expect("credential map");
    assert_eq!(entries.len(), 1);
        "access_token": "synthetic-access",
        "refresh_token": "synthetic-refresh",
        "expires_at": null,
        "scopes": ["read", "write"]
    }));
    let posts: Vec<_> = server.received_requests().await.unwrap().into_iter()
        .filter(|request| request.method.as_str() == "POST").collect();
    assert_eq!(posts.iter().map(|request| request.url.path()).collect::<Vec<_>>(), vec!["/device", "/token"]);
    let mut device_form: BTreeMap<String, String> = url::form_urlencoded::parse(&posts[0].body)
        .map(|(key, value)| (key.into_owned(), value.into_owned())).collect();
    let challenge = device_form.remove("code_challenge").expect("PKCE challenge");
    assert!(!challenge.is_empty());
    assert_eq!(device_form, BTreeMap::from([
        ("client_id".to_string(), "fixed-device-client".to_string()),
        ("scope".to_string(), "read write".to_string()),
        ("code_challenge_method".to_string(), "S256".to_string())
    ]));
    let mut token_form: BTreeMap<String, String> = url::form_urlencoded::parse(&posts[1].body)
        .map(|(key, value)| (key.into_owned(), value.into_owned())).collect();
    assert!(!token_form.remove("code_verifier").expect("PKCE verifier").is_empty());
    assert_eq!(token_form, BTreeMap::from([
        ("client_id".to_string(), "fixed-device-client".to_string()),
        ("grant_type".to_string(), "urn:ietf:params:oauth:grant-type:device_code".to_string()),
        ("device_code".to_string(), "synthetic-device-code".to_string())
    ]));
    Ok(())
}
