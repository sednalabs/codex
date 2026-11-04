use super::*;
use anyhow::Context as AnyhowContext;
use codex_exec_server_protocol::ByteChunk;
use codex_exec_server_protocol::HttpHeader;
use codex_exec_server_protocol::HttpRedirectPolicy;
use codex_exec_server_protocol::HttpRequestParams;
use codex_exec_server_protocol::HttpRequestResponse;
use codex_http_client::HttpClient;
use codex_http_client::HttpClientBuilder;
use serde_json::Value;
use std::collections::HashSet;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_HEADERS: usize = 32;
const MAX_TIMEOUT: Duration = Duration::from_secs(300);

pub(super) fn run_worker(
    socket: File,
    expected_peer: crate::mcp_broker::PeerIdentity,
    mut context: Context,
    active: Arc<AtomicBool>,
    child_is_live: Arc<dyn Fn() -> bool + Send + Sync>,
    verify_boundary: Arc<dyn Fn() -> Result<()> + Send + Sync>,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("build protected MCP broker runtime")?;
    loop {
        if !active.load(Ordering::Acquire) || !child_is_live() {
            break;
        }
        if !wait_readable(socket.as_raw_fd(), Duration::from_millis(100))? {
            continue;
        }
        match crate::mcp_broker::serve_one(
            &socket,
            expected_peer,
            &active,
            &child_is_live,
            |bytes| {
                runtime.block_on(context.handle(bytes, &active, &child_is_live, &verify_boundary))
            },
        ) {
            Ok(true) => context.acknowledge_response()?,
            Ok(false) => break,
            Err(_error) if !active.load(Ordering::Acquire) || !child_is_live() => break,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn wait_readable(fd: std::os::fd::RawFd, timeout: Duration) -> Result<bool> {
    let timeout_ms = i32::try_from(timeout.as_millis())?;
    let mut descriptor = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if result == 0 {
            return Ok(false);
        }
        if result > 0 {
            if descriptor.revents & libc::POLLNVAL != 0 {
                bail!("protected MCP broker descriptor is invalid");
            }
            return Ok(descriptor.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error).context("wait for protected MCP request");
        }
    }
}

pub(super) struct Context {
    recipient: String,
    work_item_ref: String,
    project_id: u64,
    bearer: SecretString,
    authority: crate::signer::LauncherProofAuthority,
    task_deadline: Instant,
    lifecycle_invocation: Option<Uuid>,
    http_client: HttpClient,
    used_request_ids: HashSet<Uuid>,
    pending_mutation: Option<Uuid>,
    mutation_response_waiting_ack: Option<Uuid>,
}

impl Context {
    pub(super) fn new(
        recipient: String,
        work_item_ref: String,
        project_id: u64,
        bearer: SecretString,
        authority: crate::signer::LauncherProofAuthority,
        task_deadline: Instant,
        lifecycle_invocation: Option<Uuid>,
    ) -> Result<Self> {
        if authority.recipient() != recipient
            || authority.work_item_ref() != work_item_ref
            || authority.project_id() != project_id
        {
            bail!("protected MCP broker scope differs from the signed runtime certificate");
        }
        Ok(Self {
            recipient,
            work_item_ref,
            project_id,
            bearer,
            authority,
            task_deadline,
            lifecycle_invocation,
            http_client: HttpClientBuilder::new()
                .without_redirects()
                .without_retries()
                .without_request_logging()
                .connect_timeout(Duration::from_secs(3))
                .build_direct()
                .context("build fixed launcher-side MCP client")?,
            used_request_ids: HashSet::new(),
            pending_mutation: None,
            mutation_response_waiting_ack: None,
        })
    }

    pub(super) async fn handle(
        &mut self,
        bytes: Vec<u8>,
        active: &AtomicBool,
        child_is_live: &Arc<dyn Fn() -> bool + Send + Sync>,
        verify_boundary: &Arc<dyn Fn() -> Result<()> + Send + Sync>,
    ) -> Result<Vec<u8>> {
        ensure_active(active, child_is_live)?;
        verify_boundary()?;
        let request: HttpRequestParams =
            serde_json::from_slice(&bytes).context("decode protected MCP request")?;
        let (request, proof, mutation_request_id, mutation_rpc_id) = self.prepare(request)?;
        ensure_active(active, child_is_live)?;
        verify_boundary()?;
        if let Some(request_id) = mutation_request_id {
            // Write ahead before the request can have an external effect.
            self.pending_mutation = Some(request_id);
            if let Some(invocation) = self.lifecycle_invocation {
                crate::launcher::lifecycle::record_unresolved_mcp_request(invocation, request_id)?;
            }
        }
        let response = tokio::select! {
            biased;
            _ = wait_for_child_exit(active, child_is_live) => {
                bail!("protected MCP request ended with its child invocation");
            }
            response = self.send(request, proof.as_deref()) => response,
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => return Err(error),
        };
        ensure_active(active, child_is_live)?;
        verify_boundary()?;
        let mutation_response_is_complete = mutation_rpc_id.as_ref().is_some_and(|rpc_id| {
            response_confirms_mutation(&response, rpc_id)
        });
        let encoded = serde_json::to_vec(&response).context("encode protected MCP response")?;
        if let Some(request_id) = mutation_request_id
            && mutation_response_is_complete
        {
            self.mutation_response_waiting_ack = Some(request_id);
        }
        Ok(encoded)
    }

    fn acknowledge_response(&mut self) -> Result<()> {
        let Some(request_id) = self.mutation_response_waiting_ack.take() else {
            return Ok(());
        };
        if self.pending_mutation != Some(request_id) {
            bail!("protected MCP response acknowledgement has no matching request");
        }
        if let Some(invocation) = self.lifecycle_invocation {
            crate::launcher::lifecycle::resolve_unresolved_mcp_request(invocation, request_id)?;
        }
        self.pending_mutation = None;
        Ok(())
    }

    fn prepare(
        &mut self,
        mut request: HttpRequestParams,
    ) -> Result<(HttpRequestParams, Option<String>, Option<Uuid>, Option<Value>)> {
        if request.url != self.recipient
            || !matches!(request.method.as_str(), "POST" | "GET" | "DELETE")
            || (request.method != "POST" && request.body.is_some())
            || request.request_id.is_empty()
            || request.request_id.len() > 128
            || request.request_id.chars().any(char::is_control)
            || request.headers.len() > MAX_HEADERS
        {
            bail!("protected MCP request is outside its fixed transport contract");
        }
        for header in &request.headers {
            let name = header.name.to_ascii_lowercase();
            if name == "authorization"
                || name == "proxy-authorization"
                || !matches!(
                    name.as_str(),
                    "accept"
                        | "content-type"
                        | "last-event-id"
                        | "mcp-protocol-version"
                        | "mcp-session-id"
                        | "user-agent"
                )
                || header.value.len() > 4096
                || header.value.chars().any(char::is_control)
            {
                bail!("protected MCP request has an unadmitted header");
            }
        }
        request.redirect_policy = HttpRedirectPolicy::Stop;
        if request
            .timeout_ms
            .is_some_and(|timeout| timeout == 0 || timeout > 300_000)
        {
            bail!("protected MCP request timeout is outside its fixed bound");
        }
        let Some(body) = request.body.take() else {
            if request.method == "POST" {
                bail!("protected MCP POST request has no body");
            }
            return Ok((request, None, None, None));
        };
        if body.0.is_empty() || body.0.len() > MAX_REQUEST_BYTES {
            bail!("protected MCP request body exceeds its size limit");
        }
        let mut body: Value =
            serde_json::from_slice(&body.0).context("decode protected MCP JSON-RPC request")?;
        let object = body
            .as_object_mut()
            .context("protected MCP request must be one JSON-RPC object")?;
        let method = object
            .get("method")
            .and_then(Value::as_str)
            .context("protected MCP request has no method")?;
        let mut proof = None;
        let mut mutation_request_id = None;
        let mut mutation_rpc_id = None;
        match method {
            "initialize"
            | "notifications/initialized"
            | "notifications/cancelled"
            | "ping"
            | "tools/list" => {}
            "tools/call" => {
                let request_rpc_id = object.get("id").cloned();
                let (request_proof, mutation_id) = self.add_tool_proof(&mut body)?;
                proof = request_proof;
                mutation_request_id = mutation_id;
                if mutation_request_id.is_some() {
                    let request_rpc_id = request_rpc_id
                        .filter(is_valid_json_rpc_id)
                        .context("protected mutation has no valid JSON-RPC request id")?;
                    mutation_rpc_id = Some(request_rpc_id);
                }
            }
            _ => bail!("protected MCP method is outside the fixed tool surface"),
        }
        let body = serde_json::to_vec(&body)?;
        if body.len() > MAX_REQUEST_BYTES {
            bail!("protected MCP request body exceeds its size limit");
        }
        request.body = Some(ByteChunk::from(body));
        Ok((request, proof, mutation_request_id, mutation_rpc_id))
    }

    fn add_tool_proof(&mut self, body: &mut Value) -> Result<(Option<String>, Option<Uuid>)> {
        let params = body
            .get_mut("params")
            .and_then(Value::as_object_mut)
            .context("protected MCP tool call has no parameter object")?;
        let tool = params
            .get("name")
            .and_then(Value::as_str)
            .context("protected MCP tool call has no name")?
            .to_owned();
        if !matches!(
            tool.as_str(),
            "work_item_claim"
                | "work_item_current_work"
                | "work_item_execution_receipt_read"
                | "work_item_release"
        ) {
            bail!("protected MCP tool is outside the fixed Work-item tool set");
        }
        let arguments = params
            .get("arguments")
            .and_then(Value::as_object)
            .context("protected MCP tool arguments must be an object")?
            .clone();
        if arguments.get("work_item_ref").and_then(Value::as_str)
            != Some(self.work_item_ref.as_str())
        {
            bail!("protected MCP tool target differs from the signed Work item");
        }
        if arguments
            .get("project_id")
            .is_some_and(|project_id| project_id.as_u64() != Some(self.project_id))
        {
            bail!("protected MCP tool project differs from the signed project");
        }
        let meta = params
            .entry("_meta")
            .or_insert_with(|| Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .context("protected MCP tool metadata must be an object")?;
        if meta.contains_key(crate::wire::RESERVED_META_KEY) {
            bail!("child cannot supply protected MCP proof metadata");
        }
        if tool == "work_item_execution_receipt_read" {
            let request_id_text = arguments
                .get("request_id")
                .and_then(Value::as_str)
                .context("receipt read must name an invocation request id")?;
            let request_id = Uuid::parse_str(request_id_text)
                .context("receipt read must identify a request from this invocation")?;
            if request_id.to_string() != request_id_text
                || !self.used_request_ids.contains(&request_id)
                || self
                    .pending_mutation
                    .is_some_and(|pending| pending != request_id)
            {
                bail!("receipt read is outside this invocation's mutation set");
            }
            return Ok((None, None));
        }
        if tool == "work_item_claim" || tool == "work_item_release" {
            if self.pending_mutation.is_some() {
                bail!("a prior protected mutation has an unresolved effect");
            }
            let precondition = arguments
                .get("claim_precondition")
                .and_then(Value::as_object)
                .context("protected mutation requires its complete precondition")?;
            let request_id_text = precondition
                .get("request_id")
                .and_then(Value::as_str)
                .context("protected mutation request id is missing")?;
            let request_id = Uuid::parse_str(request_id_text)
                .context("protected mutation request id is invalid")?;
            if request_id.to_string() != request_id_text
                || !self.used_request_ids.insert(request_id)
            {
                bail!("protected mutation request id is noncanonical or already used");
            }
            let provider_thread_id = meta
                .get("threadId")
                .and_then(Value::as_str)
                .context("protected mutation lacks native thread binding")?;
            let execution_nonce = meta
                .get("runtime/execution-nonce")
                .and_then(Value::as_str)
                .and_then(|value| Uuid::parse_str(value).ok())
                .context("protected mutation lacks its session execution nonce")?;
            let arguments = Value::Object(arguments.clone());
            let proof =
                self.authority
                    .sign(&execution_nonce, provider_thread_id, &tool, &arguments)?;
            let proof_text = proof
                .get("proof")
                .and_then(Value::as_str)
                .context("launcher proof has no signed value")?
                .to_string();
            meta.insert(crate::wire::RESERVED_META_KEY.to_string(), proof);
            return Ok((Some(proof_text), Some(request_id)));
        }
        Ok((None, None))
    }

    async fn send(
        &self,
        request: HttpRequestParams,
        proof: Option<&str>,
    ) -> Result<HttpRequestResponse> {
        let remaining = self.task_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("protected MCP task expired before the request was sent");
        }
        let requested_timeout = request
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(MAX_TIMEOUT);
        let timeout = requested_timeout.min(MAX_TIMEOUT).min(remaining);
        let mut builder = match request.method.as_str() {
            "GET" => self.http_client.get(&request.url),
            "POST" => self.http_client.post(&request.url),
            "DELETE" => self.http_client.delete(&request.url),
            _ => bail!("protected MCP HTTP method is not admitted"),
        };
        for header in &request.headers {
            builder = builder.header(&header.name, &header.value);
        }
        let mut authorization = Zeroizing::new(String::with_capacity(self.bearer.0.len() + 7));
        authorization.push_str("Bearer ");
        authorization.push_str(&self.bearer.0);
        builder = builder.header("authorization", authorization.as_str());
        if let Some(body) = request.body {
            builder = builder.body(body.into_inner());
        }
        let mut response = builder
            .timeout(timeout)
            .send()
            .await
            .map_err(|_| anyhow!("protected MCP HTTP request failed"))?;
        let status = response.status().as_u16();
        let mut headers = Vec::new();
        for (name, value) in response.headers() {
            if matches!(
                name.as_str(),
                "cache-control"
                    | "content-length"
                    | "content-type"
                    | "last-event-id"
                    | "mcp-protocol-version"
                    | "mcp-session-id"
                    | "retry-after"
            ) && let Ok(value) = value.to_str()
            {
                if headers.len() >= MAX_HEADERS || value.len() > 4096 {
                    bail!("protected MCP response headers exceed their size limit");
                }
                headers.push(HttpHeader {
                    name: name.as_str().to_string(),
                    value: value.to_string(),
                });
            }
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .context("read protected MCP response")?
        {
            if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                bail!("protected MCP response exceeds its size limit");
            }
            body.extend_from_slice(&chunk);
        }
        redact_bytes(&mut body, &self.bearer.0);
        redact_bytes(&mut body, self.authority.certificate());
        if let Some(proof) = proof {
            redact_bytes(&mut body, proof);
        }
        Ok(HttpRequestResponse {
            status,
            headers,
            body: ByteChunk::from(body),
        })
    }
}

fn is_valid_json_rpc_id(value: &Value) -> bool {
    match value {
        Value::String(_) => true,
        Value::Number(value) => {
            value.as_i64().is_some()
                || value
                    .as_u64()
                    .is_some_and(|value| value <= i64::MAX as u64)
        }
        _ => false,
    }
}

fn response_confirms_mutation(response: &HttpRequestResponse, expected_rpc_id: &Value) -> bool {
    response.status == 200
        && response.headers.iter().any(|header| {
            header.name.eq_ignore_ascii_case("content-type")
                && header
                    .value
                    .split(';')
                    .next()
                    .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
        })
        && codex_rmcp_client::is_successful_tools_call_json_response(
            &response.body.0,
            expected_rpc_id,
        )
}

fn ensure_active(
    active: &AtomicBool,
    child_is_live: &Arc<dyn Fn() -> bool + Send + Sync>,
) -> Result<()> {
    if !active.load(Ordering::Acquire) || !child_is_live() {
        bail!("protected MCP child invocation has ended");
    }
    Ok(())
}

async fn wait_for_child_exit(
    active: &AtomicBool,
    child_is_live: &Arc<dyn Fn() -> bool + Send + Sync>,
) {
    loop {
        if !active.load(Ordering::Acquire) || !child_is_live() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn redact_bytes(body: &mut Vec<u8>, secret: &str) {
    if secret.is_empty() {
        return;
    }
    let secret = secret.as_bytes();
    let replacement = b"[runtime secret redacted]";
    let mut offset = 0;
    while let Some(position) = body[offset..]
        .windows(secret.len())
        .position(|window| window == secret)
    {
        let start = offset + position;
        let end = start + secret.len();
        body.splice(start..end, replacement.iter().copied());
        offset = start + replacement.len();
    }
}

#[cfg(test)]
#[path = "mcp_broker_tests.rs"]
mod tests;
