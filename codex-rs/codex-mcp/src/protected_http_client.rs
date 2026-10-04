use std::sync::Arc;

use codex_exec_server::ExecServerError;
use codex_exec_server::HttpClient;
use codex_exec_server::HttpHeader;
use codex_exec_server::HttpRedirectPolicy;
use codex_exec_server::HttpRequestParams;
use codex_exec_server::HttpRequestResponse;
use codex_exec_server::HttpResponseBodyStream;
use futures::future::BoxFuture;

/// Adds the selected runtime bearer only at the final local HTTP send boundary.
pub(crate) struct ProtectedMcpHttpClient {
    inner: Arc<dyn HttpClient>,
    server: String,
    recipient: String,
}

impl ProtectedMcpHttpClient {
    pub(crate) fn new(inner: Arc<dyn HttpClient>, server: &str, recipient: &str) -> Self {
        Self {
            inner,
            server: server.to_string(),
            recipient: recipient.to_string(),
        }
    }

    fn prepare(&self, mut params: HttpRequestParams) -> Result<HttpRequestParams, ExecServerError> {
        if params.url != self.recipient
            || !matches!(params.method.as_str(), "POST" | "GET" | "DELETE")
            || !codex_runtime_proof::is_protected_mcp_target(&self.server, &self.recipient)
                .map_err(|_| protected_http_error())?
        {
            return Err(protected_http_error());
        }
        let bearer = codex_runtime_proof::bearer_for_mcp(&self.server, &self.recipient)
            .map_err(|_| protected_http_error())?
            .ok_or_else(protected_http_error)?;
        if params
            .headers
            .iter()
            .any(|header| header.name.eq_ignore_ascii_case("authorization"))
        {
            return Err(protected_http_error());
        }
        params.headers.push(HttpHeader {
            name: "authorization".to_string(),
            value: format!("Bearer {}", bearer.as_str()),
        });
        params.redirect_policy = HttpRedirectPolicy::Stop;
        Ok(params)
    }

    fn recheck_before_send(&self, params: &mut HttpRequestParams) -> Result<(), ExecServerError> {
        if !matches!(
            codex_runtime_proof::is_protected_mcp_target(&self.server, &self.recipient),
            Ok(true)
        ) {
            for header in &mut params.headers {
                if header.name.eq_ignore_ascii_case("authorization") {
                    codex_runtime_proof::erase_secret_string(&mut header.value);
                }
            }
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
            let mut params = self.prepare(params)?;
            self.recheck_before_send(&mut params)?;
            self.inner.http_request(params).await
        })
    }

    fn http_request_stream(
        &self,
        params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<(HttpRequestResponse, HttpResponseBodyStream), ExecServerError>> {
        Box::pin(async move {
            let mut params = self.prepare(params)?;
            self.recheck_before_send(&mut params)?;
            self.inner.http_request_stream(params).await
        })
    }
}

fn protected_http_error() -> ExecServerError {
    ExecServerError::HttpRequest("protected MCP request rejected".to_string())
}
