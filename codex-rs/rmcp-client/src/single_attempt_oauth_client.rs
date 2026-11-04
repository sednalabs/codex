use std::collections::HashMap;
use std::sync::Arc;

use futures::stream::BoxStream;
use http::HeaderName;
use http::HeaderValue;
use rmcp::model::ClientJsonRpcMessage;
use rmcp::model::ClientRequest;
use rmcp::model::JsonRpcMessage;
use rmcp::transport::auth::AuthClient;
use rmcp::transport::auth::AuthError;
use rmcp::transport::streamable_http_client::StreamableHttpClient;
use rmcp::transport::streamable_http_client::StreamableHttpError;
use rmcp::transport::streamable_http_client::StreamableHttpPostResponse;
use sse_stream::Error as SseError;
use sse_stream::Sse;

/// Keeps RMCP's challenge retry for bootstrap requests and safe GETs only.
///
/// For tool calls and other non-bootstrap POSTs, Codex must observe the
/// original authorization denial so its operation policy can decide whether
/// to retry. In particular, a tool call must never be replayed below that
/// policy boundary.
#[derive(Clone)]
pub(crate) struct SingleAttemptOAuthClient<C> {
    auth_client: AuthClient<C>,
}

impl<C> SingleAttemptOAuthClient<C> {
    pub(crate) fn new(auth_client: AuthClient<C>) -> Self {
        Self { auth_client }
    }
}

impl<C> SingleAttemptOAuthClient<C>
where
    C: StreamableHttpClient + Send + Sync,
{
    fn is_bootstrap_request(message: &ClientJsonRpcMessage) -> bool {
        matches!(
            message,
            JsonRpcMessage::Request(request)
                if matches!(
                    &request.request,
                    ClientRequest::InitializeRequest(_) | ClientRequest::DiscoverRequest(_)
                )
        )
    }

    async fn auth_header(
        &self,
        auth_header: Option<String>,
    ) -> Result<Option<String>, StreamableHttpError<C::Error>> {
        if auth_header.is_some() {
            return Ok(auth_header);
        }

        match self.auth_client.get_access_token().await {
            Ok(access_token) => Ok(Some(access_token)),
            Err(AuthError::AuthorizationRequired) => Ok(None),
            Err(error) => Err(StreamableHttpError::Auth(error)),
        }
    }
}

impl<C> StreamableHttpClient for SingleAttemptOAuthClient<C>
where
    C: StreamableHttpClient + Send + Sync,
{
    type Error = C::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        if Self::is_bootstrap_request(&message) {
            return self
                .auth_client
                .post_message(uri, message, session_id, auth_header, custom_headers)
                .await;
        }

        let auth_header = self.auth_header(auth_header).await?;
        self.auth_client
            .http_client
            .post_message(uri, message, session_id, auth_header, custom_headers)
            .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        if Self::is_bootstrap_request(&message) {
            return self
                .auth_client
                .post_message_with_max_sse_event_size(
                    uri,
                    message,
                    session_id,
                    auth_header,
                    custom_headers,
                    max_sse_event_size,
                )
                .await;
        }

        let auth_header = self.auth_header(auth_header).await?;
        self.auth_client
            .http_client
            .post_message_with_max_sse_event_size(
                uri,
                message,
                session_id,
                auth_header,
                custom_headers,
                max_sse_event_size,
            )
            .await
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        let auth_header = self.auth_header(auth_header).await?;
        self.auth_client
            .http_client
            .delete_session(uri, session_id, auth_header, custom_headers)
            .await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        self.auth_client
            .get_stream(uri, session_id, last_event_id, auth_header, custom_headers)
            .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        self.auth_client
            .get_stream_with_max_sse_event_size(
                uri,
                session_id,
                last_event_id,
                auth_header,
                custom_headers,
                max_sse_event_size,
            )
            .await
    }
}
