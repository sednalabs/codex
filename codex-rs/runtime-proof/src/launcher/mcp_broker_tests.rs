use super::redact_bytes;
use super::response_confirms_mutation;
use codex_exec_server_protocol::ByteChunk;
use codex_exec_server_protocol::HttpHeader;
use codex_exec_server_protocol::HttpRequestResponse;
use serde_json::json;

fn response(status: u16, content_type: &str, body: &str) -> HttpRequestResponse {
    HttpRequestResponse {
        status,
        headers: vec![HttpHeader {
            name: "content-type".to_string(),
            value: content_type.to_string(),
        }],
        body: ByteChunk::from(body.as_bytes().to_vec()),
    }
}

#[test]
fn mutation_response_requires_matching_successful_json_rpc_result() {
    let request_id = json!("call-1");
    let result = response(
        200,
        "application/json; charset=utf-8",
        r#"{"jsonrpc":"2.0","id":"call-1","result":{"content":[],"isError":false}}"#,
    );
    assert!(response_confirms_mutation(&result, &request_id));

    let mismatch = response(
        200,
        "application/json",
        r#"{"jsonrpc":"2.0","id":"call-2","result":{"content":[],"isError":false}}"#,
    );
    assert!(!response_confirms_mutation(&mismatch, &request_id));

    let tool_error = response(
        200,
        "application/json",
        r#"{"jsonrpc":"2.0","id":"call-1","result":{"content":[],"isError":true}}"#,
    );
    assert!(!response_confirms_mutation(&tool_error, &request_id));

    let json_rpc_error = response(
        200,
        "application/json",
        r#"{"jsonrpc":"2.0","id":"call-1","error":{"code":-32603,"message":"failed"}}"#,
    );
    assert!(!response_confirms_mutation(&json_rpc_error, &request_id));

    let accepted = response(202, "application/json", "");
    assert!(!response_confirms_mutation(&accepted, &request_id));
    let no_content = response(204, "application/json", "");
    assert!(!response_confirms_mutation(&no_content, &request_id));
    let sse = response(
        200,
        "text/event-stream",
        "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":\"call-1\",\"result\":{\"content\":[]}}\n\n",
    );
    assert!(!response_confirms_mutation(&sse, &request_id));
}

#[test]
fn response_body_redacts_launcher_credentials_and_proof() {
    let mut body = br#"Bearer test-bearer test-certificate test-proof test-bearer"#.to_vec();

    redact_bytes(&mut body, "test-bearer");
    redact_bytes(&mut body, "test-certificate");
    redact_bytes(&mut body, "test-proof");

    assert_eq!(
        String::from_utf8(body).expect("redacted response is UTF-8"),
        "Bearer [runtime secret redacted] [runtime secret redacted] [runtime secret redacted] [runtime secret redacted]"
    );
}
