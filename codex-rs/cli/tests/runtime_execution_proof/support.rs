use anyhow::Context;
use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::Signer;
use ed25519_dalek::SigningKey;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use std::fs;
use std::fs::File;
use std::io::Cursor;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const RUN_UID: u32 = 65_534;
const RUN_GID: u32 = 65_534;
const BOOTSTRAP_FD: i32 = 200;
const AUTH_FD: i32 = 4;
const WORK_ITEM_REF: &str = "w900001";
pub const OPS_BEARER_TOKEN: &str = "runtime-proof-fixture-mcp-token-v1";
pub const CHATGPT_ACCOUNT_ID: &str = "00000000-0000-4000-8000-000000000001";
pub const CHATGPT_PLAN_TYPE: &str = "free";
const ARGUMENTS: &str = r#"{"work_item_ref":"w900001","note":"protected fixture claim","claim_precondition":{"expected_generation":3,"expected_updated_at":"2026-10-04T05:00:00Z","request_id":"8f14e45f-ea42-4d73-9ca9-9a8b35d1c05a"}}"#;

#[derive(Clone, Copy)]
pub enum AuthFault {
    WrongContext,
    ConfigDigest,
    Expired,
    ProviderRecipient,
    CredentialClass,
    MutatedToken,
    ExtraTokenSegment,
    ExpireAfterFirstClaim,
}

#[derive(Clone)]
struct ModelResponder {
    requests: Arc<Mutex<Vec<Value>>>,
    request_paths: Arc<Mutex<Vec<String>>>,
    authorization_headers: Arc<Mutex<Vec<Option<String>>>>,
    account_headers: Arc<Mutex<Vec<Option<String>>>>,
    root_calls: Arc<Mutex<usize>>,
    delegate_calls: Arc<Mutex<usize>>,
    wait_targets: Arc<Mutex<Vec<String>>>,
    wait_call_ids: Arc<Mutex<Vec<String>>>,
    accepted_wait_results: Arc<Mutex<Vec<(bool, bool)>>>,
    spawn_child: bool,
}

impl Respond for ModelResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let Some(body) = decode_model_request_body(request) else {
            return ResponseTemplate::new(400);
        };
        if let Ok(mut requests) = self.requests.lock() {
            requests.push(body.clone());
        }
        if let Ok(mut paths) = self.request_paths.lock() {
            paths.push(request.url.path().to_string());
        }
        if let Ok(mut headers) = self.authorization_headers.lock() {
            headers.push(
                request
                    .headers
                    .get("authorization")
                    .and_then(|header| header.to_str().ok())
                    .map(str::to_string),
            );
        }
        if let Ok(mut headers) = self.account_headers.lock() {
            headers.push(
                request
                    .headers
                    .get("chatgpt-account-id")
                    .and_then(|header| header.to_str().ok())
                    .map(str::to_string),
            );
        }
        let (namespace, name, arguments, call_id): (&str, &str, String, String) =
            if request_has_user_text(&body, "delegate-claim") {
                let mut calls = self
                    .delegate_calls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if *calls == 0 {
                    *calls += 1;
                    (
                        "mcp__ops",
                        "work_item_claim",
                        ARGUMENTS.to_string(),
                        "delegate-claim-call".to_string(),
                    )
                } else {
                    ("", "", String::new(), "delegate-final".to_string())
                }
            } else if request_has_user_text(&body, "root-claim")
                || request_has_user_text(&body, "role-provider-control")
            {
                let mut calls = self
                    .root_calls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if *calls == 0 {
                    *calls += 1;
                    (
                        "mcp__ops",
                        "work_item_claim",
                        ARGUMENTS.to_string(),
                        "root-claim-call".to_string(),
                    )
                } else if *calls == 1 {
                    *calls += 1;
                    if self.spawn_child {
                        (
                            "multi_agent_v1",
                            "spawn_agent",
                            if request_has_user_text(&body, "role-provider-control") {
                                r#"{"message":"delegate-claim","task_name":"proof-child","agent_type":"proof-child"}"#.to_string()
                            } else {
                                r#"{"message":"delegate-claim","task_name":"proof-child","agent_type":"worker"}"#.to_string()
                            },
                            "root-spawn-call".to_string(),
                        )
                    } else {
                        ("", "", String::new(), "root-final".to_string())
                    }
                } else {
                    let target = self
                        .wait_targets
                        .lock()
                        .ok()
                        .and_then(|targets| targets.first().cloned())
                        .or_else(|| find_spawned_agent_id(&body))
                        .unwrap_or_default();
                    let previous_wait = self
                        .wait_call_ids
                        .lock()
                        .ok()
                        .and_then(|calls| calls.last().cloned());
                    let expects_provider_rejection =
                        request_has_user_text(&body, "role-provider-control");
                    let accepted = previous_wait
                    .as_deref()
                    .and_then(|call_id| function_output(&body, call_id))
                    .and_then(|output| serde_json::from_str::<Value>(output).ok())
                    .and_then(|result| wait_result_for_target(&result, &target))
                    .map(|status| {
                        let accepted = if expects_provider_rejection {
                            status
                                .get("errored")
                                .and_then(Value::as_str)
                                .is_some_and(|error| {
                                    error.contains(
                                        "protected model provider differs from pinned built-in configuration",
                                    )
                                })
                        } else {
                            status.get("completed").is_some()
                        };
                        if accepted {
                            self.accepted_wait_results
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .push((!expects_provider_rejection, expects_provider_rejection));
                        }
                        accepted
                    })
                    .unwrap_or(false);
                    if accepted {
                        ("", "", String::new(), "root-final".to_string())
                    } else {
                        let wait_number = self
                            .wait_call_ids
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .len();
                        let call_id = format!("root-wait-call-{wait_number}");
                        self.wait_targets
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(target.clone());
                        self.wait_call_ids
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(call_id.clone());
                        (
                            "multi_agent_v1",
                            "wait_agent",
                            serde_json::json!({
                                "targets": [target],
                                "timeout_ms": 80000
                            })
                            .to_string(),
                            call_id,
                        )
                    }
                }
            } else {
                ("", "", String::new(), "final".to_string())
            };
        let response_id = format!("response-{call_id}");
        let mut events =
            vec![serde_json::json!({"type":"response.created","response":{"id":response_id}})];
        if namespace.is_empty() {
            events.push(serde_json::json!({
                "type":"response.output_item.done",
                "item":{"type":"message","id":format!("message-{call_id}"),"role":"assistant","content":[{"type":"output_text","text":"fixture complete"}]}
            }));
        } else {
            events.push(serde_json::json!({
                "type":"response.output_item.done",
                "item":{"type":"function_call","call_id":call_id,"namespace":namespace,"name":name,"arguments":arguments}
            }));
        }
        events.push(serde_json::json!({"type":"response.completed","response":{"id":response_id,"usage":{"input_tokens":0,"input_tokens_details":null,"output_tokens":0,"output_tokens_details":null,"total_tokens":0}}}));
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(
                events
                    .into_iter()
                    .map(|event| {
                        format!(
                            "event: {}\ndata: {}\n\n",
                            event["type"].as_str().unwrap_or(""),
                            event
                        )
                    })
                    .collect::<String>(),
            )
    }
}

fn decode_model_request_body(request: &wiremock::Request) -> Option<Value> {
    let mut content_encodings = request.headers.get_all("content-encoding").iter();
    let content_encoding = match (content_encodings.next(), content_encodings.next()) {
        (None, None) => None,
        (Some(value), None) => Some(value.to_str().ok()?),
        _ => return None,
    };
    decode_model_request_body_bytes(content_encoding, &request.body)
}

fn decode_model_request_body_bytes(content_encoding: Option<&str>, body: &[u8]) -> Option<Value> {
    let body = if content_encoding.is_some_and(|value| value.trim().eq_ignore_ascii_case("zstd")) {
        zstd::stream::decode_all(Cursor::new(body)).ok()?
    } else if content_encoding.is_none() {
        body.to_vec()
    } else {
        return None;
    };
    serde_json::from_slice(&body).ok()
}

#[cfg(test)]
mod tests {
    use super::decode_model_request_body;
    use super::decode_model_request_body_bytes;
    use http::HeaderMap;
    use http::HeaderName;
    use http::HeaderValue;
    use http::Method;
    use std::io::Cursor;
    use wiremock::Request;

    fn request_with_headers(headers: HeaderMap, body: Vec<u8>) -> Request {
        Request {
            url: "http://localhost".parse().expect("fixture URL"),
            method: Method::POST,
            headers,
            body,
        }
    }

    #[test]
    fn decodes_zstd_model_requests_and_rejects_malformed_bodies() {
        let expected = serde_json::json!({"input": [{"role": "user", "content": "root-claim"}]});
        let json = serde_json::to_vec(&expected).expect("serialize synthetic request");
        let encoded = zstd::stream::encode_all(Cursor::new(json.as_slice()), 0)
            .expect("encode synthetic request");

        assert_eq!(
            decode_model_request_body_bytes(Some("zstd"), &encoded),
            Some(expected.clone())
        );
        assert_eq!(
            decode_model_request_body_bytes(Some("zstd"), b"invalid"),
            None
        );
        assert_eq!(decode_model_request_body_bytes(Some("br"), &json), None);
        assert_eq!(
            decode_model_request_body_bytes(Some("zstd, br"), &encoded),
            None
        );
        assert_eq!(decode_model_request_body_bytes(Some(""), &json), None);
        assert_eq!(
            decode_model_request_body_bytes(/*content_encoding*/ None, b"not-json"),
            None
        );

        let mut zstd_encoding = HeaderMap::new();
        zstd_encoding.append(
            HeaderName::from_static("content-encoding"),
            HeaderValue::from_static("zstd"),
        );
        assert_eq!(
            decode_model_request_body(&request_with_headers(zstd_encoding, encoded.clone(),)),
            Some(expected)
        );

        let mut duplicate_encodings = HeaderMap::new();
        duplicate_encodings.append(
            HeaderName::from_static("content-encoding"),
            HeaderValue::from_static("zstd"),
        );
        duplicate_encodings.append(
            HeaderName::from_static("content-encoding"),
            HeaderValue::from_static("br"),
        );
        assert_eq!(
            decode_model_request_body(&request_with_headers(duplicate_encodings, encoded.clone(),)),
            None
        );

        let mut invalid_encoding = HeaderMap::new();
        invalid_encoding.append(
            HeaderName::from_static("content-encoding"),
            HeaderValue::from_bytes(&[0xff]).expect("valid opaque header byte"),
        );
        assert_eq!(
            decode_model_request_body(&request_with_headers(invalid_encoding, encoded)),
            None
        );
    }
}

fn request_has_user_text(request: &Value, expected: &str) -> bool {
    request
        .get("input")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some("message")
                    && item.get("role").and_then(Value::as_str) == Some("user")
                    && item
                        .get("content")
                        .and_then(Value::as_array)
                        .is_some_and(|content| {
                            content.iter().any(|part| {
                                part.get("text")
                                    .and_then(Value::as_str)
                                    .is_some_and(|text| text.contains(expected))
                            })
                        })
            })
        })
}

fn find_spawned_agent_id(value: &Value) -> Option<String> {
    match value {
        Value::Object(object) => object
            .get("agent_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .or_else(|| object.values().find_map(find_spawned_agent_id)),
        Value::Array(values) => values.iter().find_map(find_spawned_agent_id),
        Value::String(text) => serde_json::from_str::<Value>(text)
            .ok()
            .and_then(|parsed| find_spawned_agent_id(&parsed)),
        Value::Null | Value::Bool(_) | Value::Number(_) => None,
    }
}

fn function_output<'a>(body: &'a Value, call_id: &str) -> Option<&'a str> {
    body.get("input")
        .and_then(Value::as_array)?
        .iter()
        .find(|item| {
            item.get("type").and_then(Value::as_str) == Some("function_call_output")
                && item.get("call_id").and_then(Value::as_str) == Some(call_id)
        })?
        .get("output")?
        .as_str()
}

fn wait_result_for_target(result: &Value, target: &str) -> Option<Value> {
    if target.is_empty() || result.get("timed_out")?.as_bool()? {
        return None;
    }
    result.get("status")?.get(target).cloned()
}

#[derive(Clone)]
struct McpResponder {
    calls: Arc<Mutex<Vec<Value>>>,
    authorization_headers: Arc<Mutex<Vec<Option<String>>>>,
    request_records: Arc<Mutex<Vec<McpRequestRecord>>>,
    redirect_claims: bool,
    delay_first_claim: bool,
    error_claims: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpRequestRecord {
    pub http_method: String,
    pub path: String,
    pub phase: String,
    pub authorization: Option<String>,
}

impl Respond for McpResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body = serde_json::from_slice::<Value>(&request.body).unwrap_or(Value::Null);
        let id = body.get("id").cloned().unwrap_or(Value::Null);
        let method = body
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let authorization = request
            .headers
            .get("authorization")
            .and_then(|header| header.to_str().ok())
            .map(str::to_string);
        if let Ok(mut records) = self.request_records.lock() {
            records.push(McpRequestRecord {
                http_method: request.method.as_str().to_string(),
                path: request.url.path().to_string(),
                phase: method.to_string(),
                authorization: authorization.clone(),
            });
        }
        if method == "tools/call"
            && let Ok(mut headers) = self.authorization_headers.lock()
        {
            headers.push(authorization);
        }
        let response = match method {
            "initialize" => serde_json::json!({
                "jsonrpc":"2.0","id":id,"result":{
                    "protocolVersion":"2025-03-26","capabilities":{"tools":{}},
                    "serverInfo":{"name":"fixture","version":"1"}
                }
            }),
            "tools/list" => serde_json::json!({
                "jsonrpc":"2.0","id":id,"result":{"tools":[{
                    "name":"work_item_claim","description":"synthetic claim fixture",
                    "inputSchema":{"type":"object","properties":{
                        "work_item_ref":{"type":"string"},"note":{"type":"string"},
                        "claim_precondition":{"type":"object","properties":{
                            "expected_generation":{"type":"integer"},"expected_updated_at":{"type":"string"},
                            "request_id":{"type":"string"}
                        },"required":["expected_generation","expected_updated_at","request_id"],"additionalProperties":false}
                    },"required":["work_item_ref","claim_precondition"],"additionalProperties":false}
                }]}
            }),
            "tools/call" => {
                let first_claim = self
                    .calls
                    .lock()
                    .map(|calls| calls.is_empty())
                    .unwrap_or(false);
                if let Ok(mut calls) = self.calls.lock() {
                    calls.push(body.clone());
                }
                if self.delay_first_claim && first_claim {
                    std::thread::sleep(Duration::from_secs(22));
                }
                if self.redirect_claims {
                    return ResponseTemplate::new(307)
                        .insert_header("location", "/redirect-target");
                }
                let proof = body
                    .pointer("/params/_meta/runtime~1execution-proof")
                    .cloned()
                    .unwrap_or(Value::Null);
                if self.error_claims {
                    return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "jsonrpc":"2.0","id":id,"error":{
                            "code":-32000,
                            "message":format!("{proof} {OPS_BEARER_TOKEN}")
                        }
                    }));
                }
                let echo = format!("{proof} {OPS_BEARER_TOKEN}");
                serde_json::json!({
                    "jsonrpc":"2.0","id":id,"result":{
                        "content":[{"type":"text","text":echo}],
                        "structuredContent":proof,"_meta":proof,"isError":false
                    }
                })
            }
            _ if method.starts_with("notifications/") => return ResponseTemplate::new(202),
            _ => {
                serde_json::json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"fixture method not found"}})
            }
        };
        ResponseTemplate::new(200).set_body_json(response)
    }
}

pub struct ProtectedRuntimeFixture {
    _temp: TempDir,
    executable_path: PathBuf,
    home: PathBuf,
    state: PathBuf,
    work: PathBuf,
    output_path: PathBuf,
    errors_path: PathBuf,
    artifact_sha256: String,
    provider_recipient: String,
    provider_expires_at: i64,
    provider_token: String,
    issuer_key: SigningKey,
    _model: MockServer,
    mcp: MockServer,
    model_requests: Arc<Mutex<Vec<Value>>>,
    model_request_paths: Arc<Mutex<Vec<String>>>,
    model_authorization_headers: Arc<Mutex<Vec<Option<String>>>>,
    model_account_headers: Arc<Mutex<Vec<Option<String>>>>,
    claim_calls: Arc<Mutex<Vec<Value>>>,
    mcp_authorization_headers: Arc<Mutex<Vec<Option<String>>>>,
    mcp_request_records: Arc<Mutex<Vec<McpRequestRecord>>>,
    followed_redirects: Arc<AtomicUsize>,
    wait_targets: Arc<Mutex<Vec<String>>>,
    accepted_wait_results: Arc<Mutex<Vec<(bool, bool)>>>,
    delay_first_claim: bool,
    expiring_provider_token: Arc<Mutex<Option<String>>>,
}

impl ProtectedRuntimeFixture {
    pub async fn start() -> Result<Self> {
        Self::start_config(
            /*redirect_claims*/ false, /*delay_first_claim*/ false,
            /*error_claims*/ false, /*spawn_child*/ true,
        )
        .await
    }

    pub async fn start_with_redirect(redirect_claims: bool) -> Result<Self> {
        Self::start_config(
            redirect_claims,
            /*delay_first_claim*/ false,
            /*error_claims*/ false,
            /*spawn_child*/ true,
        )
        .await
    }

    pub async fn start_delayed_response_expiry() -> Result<Self> {
        Self::start_config(
            /*redirect_claims*/ false, /*delay_first_claim*/ true,
            /*error_claims*/ false, /*spawn_child*/ false,
        )
        .await
    }

    pub async fn start_with_mcp_error() -> Result<Self> {
        Self::start_config(
            /*redirect_claims*/ false, /*delay_first_claim*/ false,
            /*error_claims*/ true, /*spawn_child*/ true,
        )
        .await
    }

    async fn start_config(
        redirect_claims: bool,
        delay_first_claim: bool,
        error_claims: bool,
        spawn_child: bool,
    ) -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        let home = root.join("immutable-home");
        let state = root.join("state");
        let work = root.join("work");
        fs::create_dir_all(home.join("sessions"))?;
        fs::create_dir_all(home.join("tmp/arg0"))?;
        fs::create_dir_all(home.join("agents"))?;
        fs::create_dir_all(&state)?;
        fs::create_dir_all(&work)?;
        fs::set_permissions(home.join("sessions"), fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(&work, fs::Permissions::from_mode(0o700))?;
        let output_path = root.join("stdout.jsonl");
        let errors_path = root.join("stderr.log");

        let model = MockServer::start().await;
        let mcp = MockServer::start().await;
        let model_requests = Arc::new(Mutex::new(Vec::new()));
        let model_request_paths = Arc::new(Mutex::new(Vec::new()));
        let model_authorization_headers = Arc::new(Mutex::new(Vec::new()));
        let model_account_headers = Arc::new(Mutex::new(Vec::new()));
        let claim_calls = Arc::new(Mutex::new(Vec::new()));
        let mcp_authorization_headers = Arc::new(Mutex::new(Vec::new()));
        let mcp_request_records = Arc::new(Mutex::new(Vec::new()));
        let followed_redirects = Arc::new(AtomicUsize::new(0));
        let wait_targets = Arc::new(Mutex::new(Vec::new()));
        let wait_call_ids = Arc::new(Mutex::new(Vec::new()));
        let accepted_wait_results = Arc::new(Mutex::new(Vec::new()));
        let expiring_provider_token = Arc::new(Mutex::new(None));
        let model_responder = ModelResponder {
            requests: model_requests.clone(),
            request_paths: model_request_paths.clone(),
            authorization_headers: model_authorization_headers.clone(),
            account_headers: model_account_headers.clone(),
            root_calls: Arc::new(Mutex::new(0)),
            delegate_calls: Arc::new(Mutex::new(0)),
            wait_targets: wait_targets.clone(),
            wait_call_ids: wait_call_ids.clone(),
            accepted_wait_results: accepted_wait_results.clone(),
            spawn_child,
        };
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(model_responder.clone())
            .mount(&model)
            .await;
        Mock::given(method("POST"))
            .and(path("/off-path/responses"))
            .respond_with(model_responder)
            .mount(&model)
            .await;
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(McpResponder {
                calls: claim_calls.clone(),
                authorization_headers: mcp_authorization_headers.clone(),
                request_records: mcp_request_records.clone(),
                redirect_claims,
                delay_first_claim,
                error_claims,
            })
            .mount(&mcp)
            .await;
        let redirect_counter = followed_redirects.clone();
        Mock::given(method("POST"))
            .and(path("/redirect-target"))
            .respond_with(move |_request: &wiremock::Request| {
                redirect_counter.fetch_add(1, Ordering::AcqRel);
                ResponseTemplate::new(200).set_body_json(serde_json::json!({}))
            })
            .mount(&mcp)
            .await;

        let executable = codex_utils_cargo_bin::cargo_bin("codex")?;
        let artifact_sha256 = hash_file(&executable)?.sha256;
        // The protected child drops to RUN_UID, so its executable must live under the
        // fixture root instead of behind the runner-owned Cargo target directories.
        let executable_path = root.join("codex-fixture");
        fs::copy(&executable, &executable_path)
            .context("stage protected CLI fixture executable")?;
        fs::set_permissions(&executable_path, fs::Permissions::from_mode(0o755))?;
        anyhow::ensure!(
            hash_file(&executable_path)?.sha256 == artifact_sha256,
            "staged protected CLI fixture executable changed during copy"
        );
        let issuer_key = SigningKey::from_bytes(&[23_u8; 32]);
        let recipient = format!("{}/mcp", mcp.uri());
        let provider_recipient = format!("{}/v1", model.uri());
        let overridden_provider = format!("{}/off-path", model.uri());
        let provider_expires_at =
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64 + 800;
        let provider_token = synthetic_access_token(provider_expires_at)?;
        fs::write(
            home.join("agents/proof-child.toml"),
            format!("model_provider = \"openai\"\nopenai_base_url = {overridden_provider:?}\n"),
        )?;
        fs::write(
            home.join("config.toml"),
            format!(
                "model = \"gpt-5.2\"\nmodel_provider = \"openai\"\nopenai_base_url = {:?}\ncli_auth_credentials_store = \"ephemeral\"\napproval_policy = \"never\"\nlog_dir = {:?}\n\n[features]\ncollab = true\n\n[agents.\"proof-child\"]\ndescription = \"synthetic provider guard fixture\"\nconfig_file = {:?}\n\n[mcp_servers.ops]\nurl = {:?}\n\n[mcp_servers.ops.tools.work_item_claim]\napproval_mode = \"approve\"\n",
                provider_recipient,
                root.join("logs").display().to_string(),
                home.join("agents/proof-child.toml").display().to_string(),
                recipient,
            ),
        )?;
        // The child app server updates this state while the configuration home stays immutable.
        let installation_id_path = home.join("installation_id");
        fs::write(
            &installation_id_path,
            "11111111-1111-4111-8111-111111111111",
        )?;
        chown_to_runner(&installation_id_path)?;
        fs::set_permissions(&installation_id_path, fs::Permissions::from_mode(0o644))?;
        fs::set_permissions(home.join("config.toml"), fs::Permissions::from_mode(0o444))?;
        fs::set_permissions(home.join("agents"), fs::Permissions::from_mode(0o555))?;
        fs::set_permissions(&home, fs::Permissions::from_mode(0o555))?;
        fs::create_dir_all(root.join("logs"))?;
        fs::set_permissions(root.join("logs"), fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(root, fs::Permissions::from_mode(0o711))?;
        chown_to_runner(&home.join("sessions"))?;
        chown_to_runner(&home.join("tmp/arg0"))?;
        chown_to_runner(&state)?;
        chown_to_runner(&work)?;
        chown_to_runner(&root.join("logs"))?;

        Ok(Self {
            _temp: temp,
            executable_path,
            home,
            state,
            work,
            output_path,
            errors_path,
            artifact_sha256,
            provider_recipient,
            provider_expires_at,
            provider_token,
            issuer_key,
            _model: model,
            mcp,
            model_requests,
            model_request_paths,
            model_authorization_headers,
            model_account_headers,
            claim_calls,
            mcp_authorization_headers,
            mcp_request_records,
            followed_redirects,
            wait_targets,
            accepted_wait_results,
            delay_first_claim,
            expiring_provider_token,
        })
    }

    pub async fn run_cli(&self) -> Result<Output> {
        self.run_cli_with_prompt_and_auth_fault("root-claim", /*fault*/ None)
            .await
    }

    pub async fn run_cli_with_auth_fault(&self, fault: Option<AuthFault>) -> Result<Output> {
        self.run_cli_with_prompt_and_auth_fault("root-claim", fault)
            .await
    }

    pub async fn run_cli_with_prompt(&self, prompt: &str) -> Result<Output> {
        self.run_cli_with_prompt_and_auth_fault(prompt, /*fault*/ None)
            .await
    }

    pub fn model_request_paths(&self) -> Vec<String> {
        self.model_request_paths
            .lock()
            .map(|paths| paths.clone())
            .unwrap_or_default()
    }

    async fn run_cli_with_prompt_and_auth_fault(
        &self,
        prompt: &str,
        fault: Option<AuthFault>,
    ) -> Result<Output> {
        self.run_cli_with_prompt_fault_and_log_filter(prompt, fault, /*log_filter*/ None)
            .await
    }

    pub async fn run_cli_with_log_filter(&self, prompt: &str, log_filter: &str) -> Result<Output> {
        self.run_cli_with_prompt_fault_and_log_filter(prompt, /*fault*/ None, Some(log_filter))
            .await
    }

    async fn run_cli_with_prompt_fault_and_log_filter(
        &self,
        prompt: &str,
        fault: Option<AuthFault>,
        log_filter: Option<&str>,
    ) -> Result<Output> {
        eprintln!("runtime-proof-root-stage:cli_launch_begin");
        let image_hash = hash_file(&self.executable_path)?;
        eprintln!(
            "runtime-proof-root-cli-hash:bytes={};elapsed_ms={}",
            image_hash.bytes_read, image_hash.elapsed_ms
        );
        anyhow::ensure!(
            image_hash.sha256 == self.artifact_sha256,
            "staged protected CLI fixture executable changed before invocation"
        );
        eprintln!("runtime-proof-root-stage:cli_executable_ready");
        let stdout = File::create(&self.output_path)?;
        let stderr = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.errors_path)?;
        let mut stderr_reader = stderr.try_clone()?;
        let (mut launcher, child_socket) = seqpacket_pair()?;
        let (auth_launcher, auth_child_socket) = seqpacket_pair()?;
        eprintln!("runtime-proof-root-stage:cli_channels_ready");
        let child_fd = child_socket.as_raw_fd();
        let auth_child_fd = auth_child_socket.as_raw_fd();
        eprintln!(
            "runtime-proof-root-fd-layout:{}",
            child_fd_layout_category(child_fd, auth_child_fd)
        );
        let mut command = Command::new(&self.executable_path);
        command
            .env_clear()
            .env("HOME", self.work.as_path())
            .env("CODEX_HOME", &self.home)
            .env("CODEX_SQLITE_HOME", &self.state)
            .env("OPS_RUNTIME_BOOTSTRAP_FD", BOOTSTRAP_FD.to_string())
            .env("OPS_RUNTIME_AUTH_FD", AUTH_FD.to_string())
            .env("PATH", "/usr/bin:/bin")
            .current_dir(&self.work)
            .args(["exec", "--json", "--skip-git-repo-check", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        if let Some(log_filter) = log_filter {
            command.env("RUST_LOG", log_filter);
        }
        unsafe {
            command.pre_exec(move || {
                remap_child_channel_fds(child_fd, auth_child_fd, BOOTSTRAP_FD, AUTH_FD)?;
                if libc::setgroups(0, std::ptr::null()) != 0
                    || libc::setresgid(RUN_GID, RUN_GID, RUN_GID) != 0
                    || libc::setresuid(RUN_UID, RUN_UID, RUN_UID) != 0
                    || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().map_err(|error| {
            let category = match error.kind() {
                std::io::ErrorKind::NotFound => "not_found",
                std::io::ErrorKind::PermissionDenied => "permission_denied",
                std::io::ErrorKind::InvalidInput => "invalid_input",
                _ => "other",
            };
            match error.raw_os_error() {
                Some(errno) => anyhow::anyhow!(
                    "launch protected codex fixture failed: {category} (errno {errno})"
                ),
                None => {
                    anyhow::anyhow!("launch protected codex fixture failed: {category} (no errno)")
                }
            }
        })?;
        eprintln!("runtime-proof-root-stage:cli_child_spawned");
        child
            .stdin
            .take()
            .context("open fixed protected CLI stdin")?
            .write_all(prompt.as_bytes())?;
        eprintln!("runtime-proof-root-stage:cli_prompt_written");
        let certificate = create_certificate(
            &self.issuer_key,
            &[17_u8; 32],
            &self.artifact_sha256,
            &format!("{}/mcp", self.mcp.uri()),
        )?;
        let mut frame = Vec::with_capacity(4 + certificate.len() + 32);
        frame.extend_from_slice(&(certificate.len() as u32).to_be_bytes());
        frame.extend_from_slice(certificate.as_bytes());
        frame.extend_from_slice(&[17_u8; 32]);
        send_packet(&launcher, &frame)?;
        frame.fill(0);
        eprintln!("runtime-proof-root-stage:cli_bootstrap_sent");
        let mut auth_packet = serde_json::json!({
            "version": 1,
            "certificate_sha256": format!("sha256:{}", sha256_hex(certificate.as_bytes())),
            "runtime_incarnation": "11111111-1111-4111-8111-111111111111",
            "principal": "22222222-2222-4222-8222-222222222222",
            "project_id": 7,
            "codex_home": self.home,
            "config_sha256": sha256_hex(&fs::read(self.home.join("config.toml"))?),
            "mcp_server": "ops",
            "recipient": format!("{}/mcp", self.mcp.uri()),
            "provider_recipient": self.provider_recipient,
            "expires_at": self.provider_expires_at,
            "provider": {
                "kind": "chatgpt_access_token",
                "credential_class": "synthetic_fixture",
                "access_token": self.provider_token,
                "account_id": CHATGPT_ACCOUNT_ID,
                "plan_type": CHATGPT_PLAN_TYPE
            },
            "mcp_bearer": OPS_BEARER_TOKEN
        });
        match fault {
            Some(AuthFault::WrongContext) => {
                auth_packet["principal"] = serde_json::json!("33333333-3333-4333-8333-333333333333")
            }
            Some(AuthFault::ConfigDigest) => {
                auth_packet["config_sha256"] = serde_json::json!("00".repeat(32))
            }
            Some(AuthFault::Expired) => {
                auth_packet["expires_at"] = serde_json::json!(
                    SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64 - 1
                )
            }
            Some(AuthFault::ExpireAfterFirstClaim) => {
                anyhow::ensure!(
                    self.delay_first_claim,
                    "expiring auth control requires the delayed MCP fixture"
                );
                let expires_at =
                    SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64 + 15;
                auth_packet["expires_at"] = serde_json::json!(expires_at);
                let access_token = synthetic_access_token(expires_at)?;
                *self
                    .expiring_provider_token
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(access_token.clone());
                auth_packet["provider"]["access_token"] = serde_json::json!(access_token);
            }
            Some(AuthFault::ProviderRecipient) => {
                auth_packet["provider_recipient"] = serde_json::json!("http://127.0.0.1:1/v1")
            }
            Some(AuthFault::CredentialClass) => {
                auth_packet["provider"]["credential_class"] = serde_json::json!("live")
            }
            Some(AuthFault::MutatedToken) => {
                let token = auth_packet["provider"]["access_token"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                auth_packet["provider"]["access_token"] = serde_json::json!(format!("{token}x"));
            }
            Some(AuthFault::ExtraTokenSegment) => {
                let token = auth_packet["provider"]["access_token"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                auth_packet["provider"]["access_token"] =
                    serde_json::json!(format!("{token}.extra"));
            }
            None => {}
        }
        let mut auth_frame = serde_json::to_vec(&auth_packet)?;
        send_packet(&auth_launcher, &auth_frame)?;
        auth_frame.fill(0);
        drop(auth_launcher);
        eprintln!("runtime-proof-root-stage:cli_auth_sent");
        set_read_timeout(&launcher, Duration::from_secs(60))?;
        eprintln!("runtime-proof-root-stage:cli_waiting_for_ack");
        let mut ack = [0_u8; 4];
        if let Err(error) = launcher.read_exact(&mut ack) {
            eprintln!(
                "runtime-proof-root-channel-pending:bootstrap={};auth={}",
                pending_packet_category(&child_socket),
                pending_packet_category(&auth_child_socket)
            );
            drop(child_socket);
            drop(auth_child_socket);
            let (child_state, child_syscall) = stop_child_after_ack_failure(&mut child);
            eprintln!("runtime-proof-root-child-state-before-kill:{child_state}");
            eprintln!("runtime-proof-root-child-syscall-before-kill:{child_syscall}");
            eprintln!(
                "runtime-proof-root-ack-failure:{}",
                ack_error_category(error.kind())
            );
            eprintln!(
                "runtime-proof-root-startup-failure:{}",
                startup_error_category(&mut stderr_reader)
            );
            return Err(error).context("read protected CLI bootstrap acknowledgement");
        }
        if &ack != b"ACK1" {
            eprintln!(
                "runtime-proof-root-channel-pending:bootstrap={};auth={}",
                pending_packet_category(&child_socket),
                pending_packet_category(&auth_child_socket)
            );
            drop(child_socket);
            drop(auth_child_socket);
            let (child_state, child_syscall) = stop_child_after_ack_failure(&mut child);
            eprintln!("runtime-proof-root-child-state-before-kill:{child_state}");
            eprintln!("runtime-proof-root-child-syscall-before-kill:{child_syscall}");
            eprintln!("runtime-proof-root-ack-failure:invalid_packet");
            eprintln!(
                "runtime-proof-root-startup-failure:{}",
                startup_error_category(&mut stderr_reader)
            );
            anyhow::bail!("CLI bootstrap acknowledgement did not match");
        }
        eprintln!("runtime-proof-root-stage:cli_ack_validated");
        drop(child_socket);
        drop(auth_child_socket);
        drop(launcher);

        let deadline = std::time::Instant::now() + Duration::from_secs(100);
        loop {
            if let Some(status) = child.try_wait()? {
                eprintln!("runtime-proof-root-stage:cli_process_exited");
                if !status.success() {
                    let exit = status
                        .code()
                        .filter(|code| (1..=255).contains(code))
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "signal".to_owned());
                    eprintln!(
                        "runtime-proof-root-cli-failure:exit={exit};stderr={}",
                        startup_error_category(&mut stderr_reader)
                    );
                }
                let stdout = fs::read(&self.output_path)?;
                let stderr = fs::read(&self.errors_path)?;
                eprintln!("runtime-proof-root-stage:cli_output_captured");
                return Ok(Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            if std::time::Instant::now() >= deadline {
                eprintln!("runtime-proof-root-stage:cli_timed_out");
                child.kill()?;
                let _ = child.wait()?;
                anyhow::bail!("protected CLI fixture timed out");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    pub fn claim_calls(&self) -> Vec<Value> {
        self.claim_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn model_requests(&self) -> Vec<Value> {
        self.model_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn model_authorization_headers(&self) -> Vec<Option<String>> {
        self.model_authorization_headers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn model_account_headers(&self) -> Vec<Option<String>> {
        self.model_account_headers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn provider_token(&self) -> &str {
        &self.provider_token
    }

    pub fn expiring_provider_token(&self) -> Option<String> {
        self.expiring_provider_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn mcp_authorization_headers(&self) -> Vec<Option<String>> {
        self.mcp_authorization_headers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn mcp_request_records(&self) -> Vec<McpRequestRecord> {
        self.mcp_request_records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn mcp_phase_counts(&self) -> [u8; 5] {
        let records = self
            .mcp_request_records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut counts = [0_u8; 5];
        for record in records.iter() {
            let index = match record.phase.as_str() {
                "initialize" => 0,
                "notifications/initialized" => 1,
                "tools/list" => 2,
                "tools/call" => 3,
                _ => 4,
            };
            counts[index] = counts[index].saturating_add(1).min(64);
        }
        counts
    }

    pub fn followed_redirects(&self) -> usize {
        self.followed_redirects.load(Ordering::Acquire)
    }

    pub fn wait_targets(&self) -> Vec<String> {
        self.wait_targets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn accepted_wait_results(&self) -> Vec<(bool, bool)> {
        self.accepted_wait_results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn issuer_verifying_key(&self) -> ed25519_dalek::VerifyingKey {
        self.issuer_key.verifying_key()
    }

    pub fn artifact_sha256(&self) -> &str {
        &self.artifact_sha256
    }

    pub fn mcp_url(&self) -> String {
        format!("{}/mcp", self.mcp.uri())
    }

    pub fn work_item_ref(&self) -> &str {
        WORK_ITEM_REF
    }

    pub fn fixture_root(&self) -> PathBuf {
        self.home.parent().unwrap_or(&self.home).to_path_buf()
    }

    pub fn captured_output(&self) -> Result<String> {
        Ok(format!(
            "{}{}",
            String::from_utf8_lossy(&fs::read(&self.output_path)?),
            String::from_utf8_lossy(&fs::read(&self.errors_path)?)
        ))
    }
}

fn send_packet(socket: &std::os::unix::net::UnixStream, bytes: &[u8]) -> Result<()> {
    let sent = unsafe {
        libc::send(
            socket.as_raw_fd(),
            bytes.as_ptr().cast(),
            bytes.len(),
            libc::MSG_NOSIGNAL,
        )
    };
    anyhow::ensure!(
        sent == bytes.len() as isize,
        "send bootstrap packet: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex(&digest)
}

fn synthetic_access_token(expires_at: i64) -> Result<String> {
    let header = serde_json::json!({
        "alg": "HS256",
        "kid": "runtime-proof-fixture-only",
        "typ": "JWT",
    });
    let claims = serde_json::json!({
        "aud": "runtime-proof-fixture-only",
        "exp": expires_at,
        "https://api.openai.com/auth": {
            "chatgpt_account_id": CHATGPT_ACCOUNT_ID,
            "chatgpt_plan_type": CHATGPT_PLAN_TYPE,
            "chatgpt_user_id": "runtime-proof-fixture-user-v1",
        },
        "iss": "https://runtime-proof-fixture.invalid",
        "sub": "runtime-proof-fixture-user-v1",
    });
    let mut header_bytes = Vec::new();
    serde_json_canonicalizer::to_writer(&header, &mut header_bytes)?;
    let mut claims_bytes = Vec::new();
    serde_json_canonicalizer::to_writer(&claims, &mut claims_bytes)?;
    Ok(format!(
        "{}.{}.{}",
        URL_SAFE_NO_PAD.encode(header_bytes),
        URL_SAFE_NO_PAD.encode(claims_bytes),
        URL_SAFE_NO_PAD.encode("not-a-real-signature-runtime-proof-fixture-v1")
    ))
}

struct FileHash {
    sha256: String,
    bytes_read: u64,
    elapsed_ms: u64,
}

fn hash_file(path: &std::path::Path) -> Result<FileHash> {
    let started = Instant::now();
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    let mut bytes_read = 0_u64;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes_read = bytes_read.saturating_add(
            u64::try_from(count).context("runtime fixture hash byte count overflow")?,
        );
        hash.update(&buffer[..count]);
    }
    let digest = hash.finalize();
    Ok(FileHash {
        sha256: hex(&digest),
        bytes_read,
        elapsed_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
    })
}

fn ack_error_category(kind: std::io::ErrorKind) -> &'static str {
    match kind {
        std::io::ErrorKind::TimedOut => "timeout",
        std::io::ErrorKind::UnexpectedEof => "eof",
        std::io::ErrorKind::WouldBlock => "would_block",
        std::io::ErrorKind::ConnectionReset => "connection_reset",
        std::io::ErrorKind::BrokenPipe => "broken_pipe",
        _ => "other",
    }
}

fn pending_packet_category(socket: &std::os::unix::net::UnixStream) -> &'static str {
    let mut pending_bytes: libc::c_int = 0;
    if unsafe { libc::ioctl(socket.as_raw_fd(), libc::FIONREAD, &mut pending_bytes) } != 0 {
        "unavailable"
    } else if pending_bytes > 0 {
        "pending"
    } else {
        "empty"
    }
}

fn child_fd_layout_category(bootstrap_source: i32, auth_source: i32) -> &'static str {
    match (bootstrap_source == AUTH_FD, auth_source == BOOTSTRAP_FD) {
        (true, true) => "crossed_targets",
        (false, true) => "auth_source_at_bootstrap_target",
        (true, false) => "bootstrap_source_at_auth_target",
        (false, false) => "no_cross_target_alias",
    }
}

fn remap_child_channel_fds(
    bootstrap_source: i32,
    auth_source: i32,
    bootstrap_target: i32,
    auth_target: i32,
) -> std::io::Result<()> {
    if bootstrap_target == auth_target {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "child channel descriptors must have distinct targets",
        ));
    }

    if bootstrap_source == auth_target && auth_source == bootstrap_target {
        let preserved_bootstrap =
            unsafe { libc::fcntl(bootstrap_source, libc::F_DUPFD_CLOEXEC, 3) };
        if preserved_bootstrap < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let result = dup_to_child_target(auth_source, auth_target)
            .and_then(|()| dup_to_child_target(preserved_bootstrap, bootstrap_target));
        unsafe {
            libc::close(preserved_bootstrap);
        }
        return result;
    }

    if auth_source == bootstrap_target {
        dup_to_child_target(auth_source, auth_target)?;
        dup_to_child_target(bootstrap_source, bootstrap_target)
    } else {
        dup_to_child_target(bootstrap_source, bootstrap_target)?;
        dup_to_child_target(auth_source, auth_target)
    }
}

fn dup_to_child_target(source: i32, target: i32) -> std::io::Result<()> {
    if unsafe { libc::dup2(source, target) } < 0
        || unsafe { libc::fcntl(target, libc::F_SETFD, 0) } < 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn stop_child_after_ack_failure(child: &mut Child) -> (&'static str, &'static str) {
    match child.try_wait() {
        Ok(Some(status)) if status.success() => ("exited_success", "not_running"),
        Ok(Some(_)) => ("exited_failure", "not_running"),
        Ok(None) => {
            let syscall_category = child_syscall_category(child);
            let _ = child.kill();
            let _ = child.wait();
            ("running_before_kill", syscall_category)
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            ("status_unavailable", "unavailable")
        }
    }
}

fn child_syscall_category(child: &Child) -> &'static str {
    let path = format!("/proc/{}/syscall", child.id());
    let Ok(mut syscall_file) = File::open(path) else {
        return "unavailable";
    };
    let mut syscall_number = [0_u8; 20];
    let mut length = 0;
    loop {
        let mut byte = [0_u8; 1];
        match syscall_file.read(&mut byte) {
            Ok(1) if byte[0].is_ascii_whitespace() => break,
            Ok(1) if length < syscall_number.len() => {
                syscall_number[length] = byte[0];
                length += 1;
            }
            Ok(1) => return "unavailable",
            Ok(0) => break,
            _ => return "unavailable",
        }
    }
    let Ok(syscall_number) = std::str::from_utf8(&syscall_number[..length]) else {
        return "unavailable";
    };
    if syscall_number == "running" {
        return "running";
    }
    if syscall_number == "-1" {
        return "blocked_outside_syscall";
    }
    let Ok(syscall_number) = syscall_number.parse::<libc::c_long>() else {
        return "unavailable";
    };
    if syscall_number == libc::SYS_recvmsg || syscall_number == libc::SYS_recvfrom {
        "socket_receive_syscall"
    } else if syscall_number == libc::SYS_futex {
        "futex_syscall"
    } else if syscall_number == libc::SYS_read || syscall_number == libc::SYS_pread64 {
        "read_syscall"
    } else if syscall_number < 0 {
        "unavailable"
    } else {
        "other_syscall"
    }
}

fn startup_error_category(stderr: &mut File) -> &'static str {
    let mut captured_stderr = Vec::new();
    if stderr.seek(SeekFrom::Start(0)).is_err() {
        return "unavailable";
    }
    if stderr
        .take(64 * 1024)
        .read_to_end(&mut captured_stderr)
        .is_err()
    {
        return "unavailable";
    }
    let stderr = String::from_utf8_lossy(&captured_stderr);
    if stderr.trim().is_empty() {
        "empty"
    } else if stderr.contains("runtime auth") {
        "runtime_auth"
    } else if stderr.contains("runtime proof") {
        "runtime_proof"
    } else if stderr.contains("provider") {
        "provider_auth"
    } else if stderr.contains("config.toml") || stderr.contains("CODEX_HOME") {
        "runtime_config"
    } else if stderr.contains("runtime must") || stderr.contains("capability") {
        "runtime_protection"
    } else {
        "unclassified"
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        let byte = *byte;
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn create_certificate(
    issuer: &SigningKey,
    runtime_seed: &[u8; 32],
    artifact_sha256: &str,
    recipient: &str,
) -> Result<String> {
    let runtime_key = SigningKey::from_bytes(runtime_seed);
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let header = serde_json::json!({"alg":"EdDSA","typ":"runtime-certificate+jwt","kid":"test-issuer-key-1"});
    let claims = serde_json::json!({
        "version":1,
        "runtime_public_key":URL_SAFE_NO_PAD.encode(runtime_key.verifying_key().as_bytes()),
        "runtime_incarnation":"11111111-1111-4111-8111-111111111111",
        "provider":"codex-native-runtime",
        "artifact_sha256":artifact_sha256,
        "principal":"22222222-2222-4222-8222-222222222222",
        "project_id":7,
        "execution_lease_seconds":900,
        "work_item_ref":WORK_ITEM_REF,
        "recipient":recipient,
        "mcp_server":"ops",
        "method":"tools/call",
        "tool":"work_item_claim",
        "purpose":"work-claim",
        "iat":now-1,
        "exp":now+900,
    });
    let header = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header)?);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims)?);
    let signing_input = format!("{header}.{payload}");
    let signature = URL_SAFE_NO_PAD.encode(issuer.sign(signing_input.as_bytes()).to_bytes());
    Ok(format!("{signing_input}.{signature}"))
}

fn seqpacket_pair() -> Result<(
    std::os::unix::net::UnixStream,
    std::os::unix::net::UnixStream,
)> {
    let mut sockets = [-1; 2];
    let status = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            sockets.as_mut_ptr(),
        )
    };
    anyhow::ensure!(
        status == 0,
        "create bootstrap socketpair: {}",
        std::io::Error::last_os_error()
    );
    unsafe {
        Ok((
            std::os::unix::net::UnixStream::from_raw_fd(sockets[0]),
            std::os::unix::net::UnixStream::from_raw_fd(sockets[1]),
        ))
    }
}

fn set_read_timeout(socket: &std::os::unix::net::UnixStream, timeout: Duration) -> Result<()> {
    let value = libc::timeval {
        tv_sec: timeout.as_secs() as libc::time_t,
        tv_usec: timeout.subsec_micros() as libc::suseconds_t,
    };
    let status = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&value as *const libc::timeval).cast(),
            std::mem::size_of_val(&value) as libc::socklen_t,
        )
    };
    anyhow::ensure!(
        status == 0,
        "set bootstrap acknowledgement timeout: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

fn chown_to_runner(path: &std::path::Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    anyhow::ensure!(
        unsafe { libc::chown(path.as_ptr(), RUN_UID, RUN_GID) } == 0,
        "assign fixture output ownership: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

use std::os::fd::FromRawFd;

#[cfg(test)]
mod fd_remapping_tests {
    use super::*;
    use std::os::fd::IntoRawFd;
    use std::os::unix::net::UnixStream;

    fn assert_channel_identity(
        bootstrap_source: i32,
        auth_source: i32,
        bootstrap_target: i32,
        auth_target: i32,
        bootstrap_sender: &UnixStream,
        auth_sender: &UnixStream,
    ) -> Result<()> {
        remap_child_channel_fds(bootstrap_source, auth_source, bootstrap_target, auth_target)?;
        for source in [bootstrap_source, auth_source] {
            if source != bootstrap_target && source != auth_target {
                unsafe {
                    libc::close(source);
                }
            }
        }
        let mut bootstrap_receiver = unsafe { UnixStream::from_raw_fd(bootstrap_target) };
        let mut auth_receiver = unsafe { UnixStream::from_raw_fd(auth_target) };
        send_packet(bootstrap_sender, b"BOOT")?;
        send_packet(auth_sender, b"AUTH")?;
        let mut bootstrap_packet = [0; 4];
        let mut auth_packet = [0; 4];
        bootstrap_receiver.read_exact(&mut bootstrap_packet)?;
        auth_receiver.read_exact(&mut auth_packet)?;
        anyhow::ensure!(bootstrap_packet == *b"BOOT");
        anyhow::ensure!(auth_packet == *b"AUTH");
        Ok(())
    }

    #[test]
    fn auth_source_at_bootstrap_target_is_preserved_before_replacement() {
        let (bootstrap_sender, bootstrap_child) = seqpacket_pair().unwrap();
        let (auth_sender, auth_child) = seqpacket_pair().unwrap();
        let bootstrap_source = bootstrap_child.into_raw_fd();
        let auth_source = auth_child.into_raw_fd();
        let auth_target =
            unsafe { libc::fcntl(bootstrap_sender.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        assert!(auth_target >= 0);

        assert_channel_identity(
            bootstrap_source,
            auth_source,
            auth_source,
            auth_target,
            &bootstrap_sender,
            &auth_sender,
        )
        .unwrap();
    }

    #[test]
    fn crossed_source_targets_keep_both_channels_separate() {
        let (bootstrap_sender, bootstrap_child) = seqpacket_pair().unwrap();
        let (auth_sender, auth_child) = seqpacket_pair().unwrap();
        let bootstrap_source = bootstrap_child.into_raw_fd();
        let auth_source = auth_child.into_raw_fd();

        assert_channel_identity(
            bootstrap_source,
            auth_source,
            auth_source,
            bootstrap_source,
            &bootstrap_sender,
            &auth_sender,
        )
        .unwrap();
    }

    #[test]
    fn bootstrap_source_at_auth_target_is_preserved_before_replacement() {
        let (bootstrap_sender, bootstrap_child) = seqpacket_pair().unwrap();
        let (auth_sender, auth_child) = seqpacket_pair().unwrap();
        let bootstrap_source = bootstrap_child.into_raw_fd();
        let auth_source = auth_child.into_raw_fd();
        let bootstrap_target =
            unsafe { libc::fcntl(bootstrap_sender.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 100) };
        assert!(bootstrap_target >= 0);

        assert_channel_identity(
            bootstrap_source,
            auth_source,
            bootstrap_target,
            bootstrap_source,
            &bootstrap_sender,
            &auth_sender,
        )
        .unwrap();
    }
}
