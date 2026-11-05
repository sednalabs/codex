use codex_exec_server::ExecServerError;
use codex_exec_server::HttpClient;
use codex_exec_server::HttpRedirectPolicy;
use codex_exec_server::HttpRequestParams;
use codex_exec_server::HttpRequestResponse;
use codex_exec_server::HttpResponseBodyStream;
use futures::future::BoxFuture;

/// Sends protected Ops MCP HTTP through the launcher-held authority broker.
pub(crate) struct ProtectedMcpHttpClient {
    server: String,
    recipient: String,
}

impl ProtectedMcpHttpClient {
    pub(crate) fn new(server: &str, recipient: &str) -> Self {
        Self {
            server: server.to_string(),
            recipient: recipient.to_string(),
        }
    }

    pub(crate) fn prepare(
        &self,
        mut params: HttpRequestParams,
    ) -> Result<HttpRequestParams, ExecServerError> {
        if params.headers.iter().any(|header| {
            header.name.eq_ignore_ascii_case("authorization")
                || header.name.eq_ignore_ascii_case("proxy-authorization")
        }) {
            return Err(protected_http_error());
        }
        if params.url != self.recipient
            || !matches!(params.method.as_str(), "POST" | "GET" | "DELETE")
            || !codex_runtime_proof::is_protected_mcp_target(&self.server, &self.recipient)
                .map_err(|_| protected_http_error())?
        {
            return Err(protected_http_error());
        }
        params.redirect_policy = HttpRedirectPolicy::Stop;
        Ok(params)
    }

    fn recheck_before_send(&self) -> Result<(), ExecServerError> {
        if !matches!(
            codex_runtime_proof::is_protected_mcp_target(&self.server, &self.recipient),
            Ok(true)
        ) {
            return Err(protected_http_error());
        }
        Ok(())
    }
}

impl HttpClient for ProtectedMcpHttpClient {
    fn http_request(
        &self,
        params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<HttpRequestResponse, ExecServerError>> {
        Box::pin(async move {
            let params = self.prepare(params)?;
            self.recheck_before_send()?;
            codex_runtime_proof::broker_protected_mcp_http_request(params)
                .await
                .map_err(|_| protected_http_error())
        })
    }

    fn http_request_stream(
        &self,
        params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<(HttpRequestResponse, HttpResponseBodyStream), ExecServerError>> {
        Box::pin(async move {
            let params = self.prepare(params)?;
            self.recheck_before_send()?;
            let mut response = codex_runtime_proof::broker_protected_mcp_http_request(params)
                .await
                .map_err(|_| protected_http_error())?;
            let body = std::mem::replace(&mut response.body, Vec::new().into()).into_inner();
            Ok((response, HttpResponseBodyStream::from_bytes(body)))
        })
    }
}

fn protected_http_error() -> ExecServerError {
    ExecServerError::HttpRequest("protected MCP request rejected".to_string())
}
