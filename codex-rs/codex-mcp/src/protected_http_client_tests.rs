use codex_exec_server::HttpRedirectPolicy;
use codex_exec_server::HttpRequestParams;

use crate::protected_http_client::ProtectedMcpHttpClient;

const SERVER: &str = "ops";
const RECIPIENT: &str = "https://ops.example/mcp";

fn request(url: &str, headers: Vec<codex_exec_server::HttpHeader>) -> HttpRequestParams {
    HttpRequestParams {
        method: "POST".to_string(),
        url: url.to_string(),
        headers,
        body: None,
        timeout_ms: Some(1000),
        redirect_policy: HttpRedirectPolicy::Follow,
        request_id: "protected-http-test".to_string(),
        stream_response: false,
    }
}

#[test]
fn protected_transport_rejects_child_authorization_headers() {
    let client = ProtectedMcpHttpClient::new(SERVER, RECIPIENT);
    let request = request(
        RECIPIENT,
        vec![codex_exec_server::HttpHeader {
            name: "authorization".to_string(),
            value: "Bearer child-controlled".to_string(),
        }],
    );
    assert!(client.prepare(request).is_err());
}

#[test]
fn protected_transport_rejects_a_changed_recipient() {
    let client = ProtectedMcpHttpClient::new(SERVER, RECIPIENT);
    assert!(
        client
            .prepare(request("https://ops.example/other", Vec::new()))
            .is_err()
    );
}
