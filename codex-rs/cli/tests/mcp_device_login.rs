use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use tokio::io::copy_bidirectional;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls;
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
        let upstream = url::Url::parse(&server.uri())?;
        let upstream_address = format!(
            "{}:{}",
            upstream.host_str().context("wiremock host is missing")?,
            upstream.port().context("wiremock port is missing")?
        );
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let tls_address = listener.local_addr()?;
        let base = format!("https://localhost:{}", tls_address.port());
        let certificate = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])?;
        let home = TempDir::new()?;
        let certificate_path = home.path().join("device-auth-ca.pem");
        std::fs::write(&certificate_path, certificate.cert.pem())?;
        let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(
            vec![certificate.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(certificate.signing_key.serialize_der()).into(),
        )?;
        let mut tls_tasks = spawn_tls_forwarder(
            listener,
            TlsAcceptor::from(Arc::new(tls)),
            upstream_address,
        );

        Mock::given(method("GET"))
            .and(path("/.well-known/oauth-protected-resource/mcp"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "resource": format!("{base}/mcp"),
                "authorization_servers": [base]
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/.well-known/oauth-authorization-server"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issuer": base,
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
            .expect(1)
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
            .env("CODEX_CA_CERTIFICATE", &certificate_path)
            .env("NO_PROXY", "localhost,127.0.0.1")
            .env("no_proxy", "localhost,127.0.0.1")
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
        tls_tasks.abort_all();
    }
    Ok(())
}

fn spawn_tls_forwarder(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    upstream: String,
) -> JoinSet<Result<()>> {
    let mut tasks = JoinSet::new();
    tasks.spawn(async move {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    let acceptor = acceptor.clone();
                    let upstream = upstream.clone();
                    connections.spawn(async move {
                        forward_tls_request(stream, acceptor, upstream).await
                    });
                }
                Some(result) = connections.join_next(), if !connections.is_empty() => {
                    result??;
                }
            }
        }
    });
    tasks
}

async fn forward_tls_request(
    stream: TcpStream,
    acceptor: TlsAcceptor,
    upstream: String,
) -> Result<()> {
    let mut client = acceptor.accept(stream).await?;
    let mut backend = TcpStream::connect(upstream).await?;
    copy_bidirectional(&mut client, &mut backend).await?;
    Ok(())
}
