/*
This module implements the remote app-server client transport.

It owns the remote connection lifecycle, including the initialize/initialized
handshake, JSON-RPC request/response routing, server-request resolution, and
notification streaming. Remote connections always carry WebSocket frames, over
either TCP WebSocket URLs or local Unix sockets. The rest of the crate uses the
same `AppServerEvent` surface for both in-process and remote transports, so
callers such as the TUI can switch between them without changing their
higher-level session logic.
*/

use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::io::Result as IoResult;
use std::time::Duration;

use crate::AppServerEvent;
use crate::RequestResult;
use crate::SHUTDOWN_TIMEOUT;
use crate::TypedRequestError;
use crate::server_notification_requires_delivery;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::ClientNotification;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InitializeCapabilities;
use codex_app_server_protocol::InitializeParams;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCRequest;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::Result as JsonRpcResult;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerRequest;
use codex_uds::UnixStream;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_rustls_provider::ensure_rustls_crypto_provider;
use futures::SinkExt;
use futures::StreamExt;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::timeout;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::client_async_with_config;
use tokio_tungstenite::connect_async_with_config;
use tokio_tungstenite::tungstenite::Error as TungsteniteError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tracing::warn;
use url::Url;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(10);
const REMOTE_APP_SERVER_MAX_WEBSOCKET_MESSAGE_SIZE: usize = 128 << 20;
// Tungstenite still needs an HTTP request URI for the WebSocket handshake;
// the bytes travel over the Unix socket, not TCP.
const UDS_WEBSOCKET_HANDSHAKE_URL: &str = "ws://localhost/rpc";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteAppServerEndpoint {
    WebSocket {
        websocket_url: String,
        auth_token: Option<String>,
    },
    UnixSocket {
        socket_path: AbsolutePathBuf,
    },
}

#[derive(Debug, Clone)]
pub struct RemoteAppServerConnectArgs {
    pub endpoint: RemoteAppServerEndpoint,
    pub client_name: String,
    pub client_version: String,
    pub experimental_api: bool,
    pub mcp_server_openai_form_elicitation: bool,
    pub opt_out_notification_methods: Vec<String>,
    pub channel_capacity: usize,
}
impl RemoteAppServerConnectArgs {
    pub(crate) fn initialize_params(&self) -> InitializeParams {
        let capabilities = InitializeCapabilities {
            experimental_api: self.experimental_api,
            request_attestation: false,
            opt_out_notification_methods: if self.opt_out_notification_methods.is_empty() {
                None
            } else {
                Some(self.opt_out_notification_methods.clone())
            },
            mcp_server_openai_form_elicitation: self.mcp_server_openai_form_elicitation,
        };

        InitializeParams {
            client_info: ClientInfo {
                name: self.client_name.clone(),
                title: None,
                version: self.client_version.clone(),
            },
            capabilities: Some(capabilities),
        }
    }
}

pub(crate) fn websocket_url_supports_auth_token(url: &Url) -> bool {
    match (url.scheme(), url.host()) {
        ("wss", Some(_)) => true,
        ("ws", Some(url::Host::Domain(domain))) => domain.eq_ignore_ascii_case("localhost"),
        ("ws", Some(url::Host::Ipv4(addr))) => addr.is_loopback(),
        ("ws", Some(url::Host::Ipv6(addr))) => addr.is_loopback(),
        _ => false,
    }
}

enum RemoteClientCommand {
    Request {
        request: Box<JSONRPCRequest>,
        response_tx: oneshot::Sender<IoResult<RequestResult>>,
    },
    Notify {
        notification: ClientNotification,
        response_tx: oneshot::Sender<IoResult<()>>,
    },
    ResolveServerRequest {
        request_id: RequestId,
        result: JsonRpcResult,
        response_tx: oneshot::Sender<IoResult<()>>,
    },
    RejectServerRequest {
        request_id: RequestId,
        error: JSONRPCErrorError,
        response_tx: oneshot::Sender<IoResult<()>>,
    },
    Shutdown {
        response_tx: oneshot::Sender<IoResult<()>>,
    },
}

pub struct RemoteAppServerClient {
    command_tx: mpsc::Sender<RemoteClientCommand>,
    event_rx: mpsc::Receiver<AppServerEvent>,
    pending_events: VecDeque<AppServerEvent>,
    server_version: Option<String>,
    codex_home: Option<String>,
    worker_handle: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
pub struct RemoteAppServerRequestHandle {
    command_tx: mpsc::Sender<RemoteClientCommand>,
}

impl RemoteAppServerClient {
    pub async fn connect(args: RemoteAppServerConnectArgs) -> IoResult<Self> {
        let channel_capacity = args.channel_capacity.max(1);
        let initialize_params = args.initialize_params();
        match args.endpoint {
            RemoteAppServerEndpoint::WebSocket {
                websocket_url,
                auth_token,
            } => {
                let (endpoint, stream) =
                    connect_websocket_endpoint(websocket_url, auth_token).await?;
                Self::connect_with_stream(channel_capacity, endpoint, stream, initialize_params)
                    .await
            }
            RemoteAppServerEndpoint::UnixSocket { socket_path } => {
                let (endpoint, stream) = connect_unix_socket_endpoint(socket_path).await?;
                Self::connect_with_stream(channel_capacity, endpoint, stream, initialize_params)
                    .await
            }
        }
    }

    pub fn server_version(&self) -> Option<&str> {
        self.server_version.as_deref()
    }

    pub fn codex_home(&self) -> Option<&str> {
        self.codex_home.as_deref()
    }

    async fn connect_with_stream<S>(
        channel_capacity: usize,
        endpoint: String,
        stream: WebSocketStream<S>,
        initialize_params: InitializeParams,
    ) -> IoResult<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let mut stream = stream;
        let (pending_events, server_version, codex_home) = initialize_remote_connection(
            &mut stream,
            &endpoint,
            initialize_params,
            INITIALIZE_TIMEOUT,
        )
        .await?;

        let (command_tx, mut command_rx) = mpsc::channel::<RemoteClientCommand>(channel_capacity);
        let (event_tx, event_rx) = mpsc::channel::<AppServerEvent>(channel_capacity);
        let worker_handle = tokio::spawn(async move {
            let mut pending_requests =
                HashMap::<RequestId, oneshot::Sender<IoResult<RequestResult>>>::new();
            let mut worker_exit_error: Option<(ErrorKind, String)> = None;
            let mut skipped_events = 0usize;
            loop {
                tokio::select! {
                    command = command_rx.recv() => {
                        let Some(command) = command else {
                            let _ = stream.close(None).await;
                            break;
                        };
                        match command {
                            RemoteClientCommand::Request { request, response_tx } => {
                                let request_id = request.id.clone();
                                if pending_requests.contains_key(&request_id) {
                                    let _ = response_tx.send(Err(IoError::new(
                                        ErrorKind::InvalidInput,
                                        format!("duplicate remote app-server request id `{request_id}`"),
                                    )));
                                    continue;
                                }
                                pending_requests.insert(request_id.clone(), response_tx);
                                if let Err(err) = write_jsonrpc_message(
                                    &mut stream,
                                    JSONRPCMessage::Request(*request),
                                    &endpoint,
                                )
                                .await
                                {
                                    let err_message = err.to_string();
                                    let message = format!(
                                        "remote app server at `{endpoint}` write failed: {err_message}"
                                    );
                                    if let Some(response_tx) = pending_requests.remove(&request_id) {
                                        let _ = response_tx.send(Err(err));
                                    }
                                    if let Err(err) = deliver_event(
                                        &event_tx,
                                        &mut skipped_events,
                                        AppServerEvent::Disconnected {
                                            message: message.clone(),
                                        },
                                    )
                                    .await
                                    {
                                        warn!(%err, "failed to deliver remote app-server disconnect event");
                                    }
                                    worker_exit_error = Some((ErrorKind::BrokenPipe, message));
                                    break;
                                }
                            }
                            RemoteClientCommand::Notify { notification, response_tx } => {
                                let result = write_jsonrpc_message(
                                    &mut stream,
                                    JSONRPCMessage::Notification(
                                        jsonrpc_notification_from_client_notification(notification),
                                    ),
                                    &endpoint,
                                )
                                .await;
                                let _ = response_tx.send(result);
                            }
                            RemoteClientCommand::ResolveServerRequest {
                                request_id,
                                result,
                                response_tx,
                            } => {
                                let result = write_jsonrpc_message(
                                    &mut stream,
                                    JSONRPCMessage::Response(JSONRPCResponse {
                                        id: request_id,
                                        result,
                                    }),
                                    &endpoint,
                                )
                                .await;
                                let _ = response_tx.send(result);
                            }
                            RemoteClientCommand::RejectServerRequest {
                                request_id,
                                error,
                                response_tx,
                            } => {
                                let result = write_jsonrpc_message(
                                    &mut stream,
                                    JSONRPCMessage::Error(JSONRPCError {
                                        error,
                                        id: request_id,
                                    }),
                                    &endpoint,
                                )
                                .await;
                                let _ = response_tx.send(result);
                            }
                            RemoteClientCommand::Shutdown { response_tx } => {
                                let close_result = stream.close(None).await.or_else(|err| {
                                    if websocket_close_error_is_already_closed(&err) {
                                        Ok(())
                                    } else {
                                        Err(IoError::other(format!(
                                            "failed to close websocket app server `{endpoint}`: {err}"
                                        )))
                                    }
                                });
                                let _ = response_tx.send(close_result);
                                break;
                            }
                        }
                    }
                    message = stream.next() => {
                        match message {
                            Some(Ok(Message::Text(text))) => {
                                match server_message_and_subscription_id(&text) {
                                    Ok((message, thread_subscription_id)) => match message {
                                    JSONRPCMessage::Response(response) => {
                                        if let Some(response_tx) = pending_requests.remove(&response.id) {
                                            let _ = response_tx.send(Ok(Ok(response.result)));
                                        }
                                    }
                                    JSONRPCMessage::Error(error) => {
                                        if let Some(response_tx) = pending_requests.remove(&error.id) {
                                            let _ = response_tx.send(Ok(Err(error.error)));
                                        }
                                    }
                                    JSONRPCMessage::Notification(notification) => {
                                        if let Some(event) = app_server_event_from_notification(
                                            notification,
                                            thread_subscription_id,
                                        )
                                        {
                                            if let Err(err) = deliver_event(
                                                &event_tx,
                                                &mut skipped_events,
                                                event,
                                            )
                                            .await
                                            {
                                                warn!(%err, "failed to deliver remote app-server event");
                                                break;
                                            }
                                        }
                                    }
                                    JSONRPCMessage::Request(request) => {
                                        let request_id = request.id.clone();
                                        let method = request.method.clone();
                                        match ServerRequest::try_from(request) {
                                            Ok(request) => {
                                                let event = match thread_subscription_id {
                                                    Some(thread_subscription_id) => {
                                                        AppServerEvent::ThreadServerRequest {
                                                            thread_subscription_id,
                                                            request,
                                                        }
                                                    }
                                                    None => AppServerEvent::ServerRequest(request),
                                                };
                                                if let Err(err) = deliver_event(
                                                    &event_tx,
                                                    &mut skipped_events,
                                                    event,
                                                )
                                                .await
                                                {
                                                    warn!(%err, "failed to deliver remote app-server server request");
                                                    break;
                                                }
                                            }
                                            Err(err) => {
                                                warn!(%err, method, "rejecting unknown remote app-server request");
                                                if let Err(reject_err) = write_jsonrpc_message(
                                                    &mut stream,
                                                    JSONRPCMessage::Error(JSONRPCError {
                                                        error: JSONRPCErrorError {
                                                            code: -32601,
                                                            message: format!(
                                                                "unsupported remote app-server request `{method}`"
                                                            ),
                                                            data: None,
                                                        },
                                                        id: request_id,
                                                    }),
                                                    &endpoint,
                                                )
                                                .await
                                                {
                                                    let err_message = reject_err.to_string();
                                                    let message = format!(
                                                        "remote app server at `{endpoint}` write failed: {err_message}"
                                                    );
                                                    if let Err(err) = deliver_event(
                                                        &event_tx,
                                                        &mut skipped_events,
                                                        AppServerEvent::Disconnected {
                                                            message: message.clone(),
                                                        },
                                                    )
                                                    .await
                                                    {
                                                        warn!(%err, "failed to deliver remote app-server disconnect event");
                                                    }
                                                    worker_exit_error =
                                                        Some((ErrorKind::BrokenPipe, message));
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                    },
                                    Err(err) => {
                                        let message = format!(
                                            "remote app server at `{endpoint}` sent invalid JSON-RPC: {err}"
                                        );
                                        if let Err(deliver_err) = deliver_event(
                                            &event_tx,
                                            &mut skipped_events,
                                            AppServerEvent::Disconnected {
                                                message: message.clone(),
                                            },
                                        )
                                        .await
                                        {
                                            warn!(%deliver_err, "failed to deliver remote app-server disconnect event");
                                        }
                                        worker_exit_error =
                                            Some((ErrorKind::InvalidData, message));
                                        break;
                                    }
                                }
                            }
                            Some(Ok(Message::Close(frame))) => {
                                let reason = frame
                                    .as_ref()
                                    .map(|frame| frame.reason.to_string())
                                    .filter(|reason| !reason.is_empty())
                                    .unwrap_or_else(|| "connection closed".to_string());
                                let message = format!(
                                    "remote app server at `{endpoint}` disconnected: {reason}"
                                );
                                if let Err(err) = deliver_event(
                                    &event_tx,
                                    &mut skipped_events,
                                    AppServerEvent::Disconnected {
                                        message: message.clone(),
                                    },
                                )
                                .await
                                {
                                    warn!(%err, "failed to deliver remote app-server disconnect event");
                                }
                                worker_exit_error = Some((
                                    ErrorKind::ConnectionAborted,
                                    message,
                                ));
                                break;
                            }
                            Some(Ok(Message::Binary(_)))
                            | Some(Ok(Message::Ping(_)))
                            | Some(Ok(Message::Pong(_)))
                            | Some(Ok(Message::Frame(_))) => {}
                            Some(Err(err)) => {
                                let message = format!(
                                    "remote app server at `{endpoint}` transport failed: {err}"
                                );
                                if let Err(deliver_err) = deliver_event(
                                    &event_tx,
                                    &mut skipped_events,
                                    AppServerEvent::Disconnected {
                                        message: message.clone(),
                                    },
                                )
                                .await
                                {
                                    warn!(%deliver_err, "failed to deliver remote app-server disconnect event");
                                }
                                worker_exit_error = Some((ErrorKind::InvalidData, message));
                                break;
                            }
                            None => {
                                let message = format!(
                                    "remote app server at `{endpoint}` closed the connection"
                                );
                                if let Err(err) = deliver_event(
                                    &event_tx,
                                    &mut skipped_events,
                                    AppServerEvent::Disconnected {
                                        message: message.clone(),
                                    },
                                )
                                .await
                                {
                                    warn!(%err, "failed to deliver remote app-server disconnect event");
                                }
                                worker_exit_error = Some((ErrorKind::UnexpectedEof, message));
                                break;
                            }
                        }
                    }
                }
            }

            let (err_kind, err_message) = worker_exit_error.unwrap_or_else(|| {
                (
                    ErrorKind::BrokenPipe,
                    "remote app-server worker channel is closed".to_string(),
                )
            });
            for (_, response_tx) in pending_requests {
                let _ = response_tx.send(Err(IoError::new(err_kind, err_message.clone())));
            }
        });

        Ok(Self {
            command_tx,
            event_rx,
            pending_events: pending_events.into(),
            server_version,
            codex_home,
            worker_handle,
        })
    }

    pub fn request_handle(&self) -> RemoteAppServerRequestHandle {
        RemoteAppServerRequestHandle {
            command_tx: self.command_tx.clone(),
        }
    }

    pub async fn request(&self, request: ClientRequest) -> IoResult<RequestResult> {
        self.request_handle().request(request).await
    }

    pub async fn request_typed<T>(&self, request: ClientRequest) -> Result<T, TypedRequestError>
    where
        T: DeserializeOwned,
    {
        let method = request.method_name();
        let response =
            self.request(request)
                .await
                .map_err(|source| TypedRequestError::Transport {
                    method: method.to_string(),
                    source,
                })?;
        let result = response.map_err(|source| TypedRequestError::Server {
            method: method.to_string(),
            source,
        })?;
        serde_json::from_value(result).map_err(|source| TypedRequestError::Deserialize {
            method: method.to_string(),
            source,
        })
    }

    pub async fn notify(&self, notification: ClientNotification) -> IoResult<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(RemoteClientCommand::Notify {
                notification,
                response_tx,
            })
            .await
            .map_err(|_| {
                IoError::new(
                    ErrorKind::BrokenPipe,
                    "remote app-server worker channel is closed",
                )
            })?;
        response_rx.await.map_err(|_| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "remote app-server notify channel is closed",
            )
        })?
    }

    pub async fn resolve_server_request(
        &self,
        request_id: RequestId,
        result: JsonRpcResult,
    ) -> IoResult<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(RemoteClientCommand::ResolveServerRequest {
                request_id,
                result,
                response_tx,
            })
            .await
            .map_err(|_| {
                IoError::new(
                    ErrorKind::BrokenPipe,
                    "remote app-server worker channel is closed",
                )
            })?;
        response_rx.await.map_err(|_| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "remote app-server resolve channel is closed",
            )
        })?
    }

    pub async fn reject_server_request(
        &self,
        request_id: RequestId,
        error: JSONRPCErrorError,
    ) -> IoResult<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(RemoteClientCommand::RejectServerRequest {
                request_id,
                error,
                response_tx,
            })
            .await
            .map_err(|_| {
                IoError::new(
                    ErrorKind::BrokenPipe,
                    "remote app-server worker channel is closed",
                )
            })?;
        response_rx.await.map_err(|_| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "remote app-server reject channel is closed",
            )
        })?
    }

    pub async fn next_event(&mut self) -> Option<AppServerEvent> {
        if let Some(event) = self.pending_events.pop_front() {
            return Some(event);
        }
        self.event_rx.recv().await
    }

    pub async fn shutdown(self) -> IoResult<()> {
        let Self {
            command_tx,
            event_rx,
            pending_events: _pending_events,
            server_version: _server_version,
            codex_home: _codex_home,
            worker_handle,
        } = self;
        let mut worker_handle = worker_handle;
        drop(event_rx);
        let (response_tx, response_rx) = oneshot::channel();
        if command_tx
            .send(RemoteClientCommand::Shutdown { response_tx })
            .await
            .is_ok()
            && let Ok(Ok(close_result)) = timeout(SHUTDOWN_TIMEOUT, response_rx).await
        {
            close_result?;
        }

        if let Err(_elapsed) = timeout(SHUTDOWN_TIMEOUT, &mut worker_handle).await {
            worker_handle.abort();
            let _ = worker_handle.await;
        }
        Ok(())
    }
}

impl RemoteAppServerRequestHandle {
    pub async fn request(&self, request: ClientRequest) -> IoResult<RequestResult> {
        self.request_json_rpc(jsonrpc_request_from_client_request(request))
            .await
    }

    pub async fn request_json_rpc(&self, request: JSONRPCRequest) -> IoResult<RequestResult> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(RemoteClientCommand::Request {
                request: Box::new(request),
                response_tx,
            })
            .await
            .map_err(|_| {
                IoError::new(
                    ErrorKind::BrokenPipe,
                    "remote app-server worker channel is closed",
                )
            })?;
        response_rx.await.map_err(|_| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "remote app-server request channel is closed",
            )
        })?
    }

    pub async fn request_typed<T>(&self, request: ClientRequest) -> Result<T, TypedRequestError>
    where
        T: DeserializeOwned,
    {
        let method = request.method_name();
        let response =
            self.request(request)
                .await
                .map_err(|source| TypedRequestError::Transport {
                    method: method.to_string(),
                    source,
                })?;
        let result = response.map_err(|source| TypedRequestError::Server {
            method: method.to_string(),
            source,
        })?;
        serde_json::from_value(result).map_err(|source| TypedRequestError::Deserialize {
            method: method.to_string(),
            source,
        })
    }
}

async fn connect_websocket_endpoint(
    websocket_url: String,
    auth_token: Option<String>,
) -> IoResult<(String, WebSocketStream<MaybeTlsStream<TcpStream>>)> {
    let url = Url::parse(&websocket_url).map_err(|err| {
        IoError::new(
            ErrorKind::InvalidInput,
            format!("invalid websocket URL `{websocket_url}`: {err}"),
        )
    })?;
    if auth_token.is_some() && !websocket_url_supports_auth_token(&url) {
        return Err(IoError::new(
            ErrorKind::InvalidInput,
            format!(
                "remote auth tokens require `wss://` or loopback `ws://` URLs; got `{websocket_url}`"
            ),
        ));
    }

    let mut request = url.as_str().into_client_request().map_err(|err| {
        IoError::new(
            ErrorKind::InvalidInput,
            format!("invalid websocket URL `{websocket_url}`: {err}"),
        )
    })?;
    if let Some(auth_token) = auth_token.as_deref() {
        let header_value =
            HeaderValue::from_str(&format!("Bearer {auth_token}")).map_err(|err| {
                IoError::new(
                    ErrorKind::InvalidInput,
                    format!("invalid remote authorization header value: {err}"),
                )
            })?;
        request.headers_mut().insert(AUTHORIZATION, header_value);
    }

    ensure_rustls_crypto_provider();
    let websocket_config = remote_websocket_config();
    let stream = timeout(
        CONNECT_TIMEOUT,
        connect_async_with_config(
            request,
            Some(websocket_config),
            /*disable_nagle*/ false,
        ),
    )
    .await
    .map_err(|_| {
        IoError::new(
            ErrorKind::TimedOut,
            format!("timed out connecting to remote app server at `{websocket_url}`"),
        )
    })?
    .map(|(stream, _response)| stream)
    .map_err(|err| {
        IoError::other(format!(
            "failed to connect to remote app server at `{websocket_url}`: {err}"
        ))
    })?;

    Ok((websocket_url, stream))
}

async fn connect_unix_socket_endpoint(
    socket_path: AbsolutePathBuf,
) -> IoResult<(String, WebSocketStream<UnixStream>)> {
    let endpoint = format!("unix://{}", socket_path.display());
    let request = UDS_WEBSOCKET_HANDSHAKE_URL
        .into_client_request()
        .map_err(|err| {
            IoError::new(
                ErrorKind::InvalidInput,
                format!("invalid UDS websocket handshake URL: {err}"),
            )
        })?;
    let stream = timeout(CONNECT_TIMEOUT, UnixStream::connect(socket_path.as_path()))
        .await
        .map_err(|_| {
            IoError::new(
                ErrorKind::TimedOut,
                format!("timed out connecting to remote app server at `{endpoint}`"),
            )
        })?
        .map_err(|err| {
            IoError::other(format!(
                "failed to connect to remote app server at `{endpoint}`: {err}"
            ))
        })?;
    let websocket_config = remote_websocket_config();
    let stream = timeout(
        CONNECT_TIMEOUT,
        client_async_with_config(request, stream, Some(websocket_config)),
    )
    .await
    .map_err(|_| {
        IoError::new(
            ErrorKind::TimedOut,
            format!("timed out upgrading remote app server at `{endpoint}`"),
        )
    })?
    .map(|(stream, _response)| stream)
    .map_err(|err| {
        IoError::other(format!(
            "failed to upgrade remote app server at `{endpoint}`: {err}"
        ))
    })?;

    Ok((endpoint, stream))
}

fn remote_websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_frame_size(Some(REMOTE_APP_SERVER_MAX_WEBSOCKET_MESSAGE_SIZE))
        .max_message_size(Some(REMOTE_APP_SERVER_MAX_WEBSOCKET_MESSAGE_SIZE))
}

async fn initialize_remote_connection<S>(
    stream: &mut WebSocketStream<S>,
    endpoint: &str,
    params: InitializeParams,
    initialize_timeout: Duration,
) -> IoResult<(Vec<AppServerEvent>, Option<String>, Option<String>)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let initialize_request_id = RequestId::String("initialize".to_string());
    let mut pending_events = Vec::new();
    let mut server_version = None;
    let mut codex_home = None;
    write_jsonrpc_message(
        stream,
        JSONRPCMessage::Request(jsonrpc_request_from_client_request(
            ClientRequest::Initialize {
                request_id: initialize_request_id.clone(),
                params,
            },
        )),
        endpoint,
    )
    .await?;

    timeout(initialize_timeout, async {
        loop {
            match stream.next().await {
                Some(Ok(Message::Text(text))) => {
                    let (message, thread_subscription_id) = server_message_and_subscription_id(&text).map_err(|err| {
                        IoError::other(format!(
                            "remote app server at `{endpoint}` sent invalid initialize response: {err}"
                        ))
                    })?;
                    match message {
                        JSONRPCMessage::Response(response) if response.id == initialize_request_id => {
                            server_version = response
                                .result
                                .get("userAgent")
                                .and_then(serde_json::Value::as_str)
                                .and_then(|user_agent| {
                                    let (_, rest) = user_agent.split_once('/')?;
                                    rest.split_whitespace().next().map(str::to_string)
                                });
                            codex_home = response
                                .result
                                .get("codexHome")
                                .and_then(serde_json::Value::as_str)
                                .filter(|codex_home| !codex_home.is_empty())
                                .map(str::to_string);
                            break Ok(());
                        }
                        JSONRPCMessage::Error(error) if error.id == initialize_request_id => {
                            break Err(IoError::other(format!(
                                "remote app server at `{endpoint}` rejected initialize: {}",
                                error.error.message
                            )));
                        }
                        JSONRPCMessage::Notification(notification) => {
                            if let Some(event) = app_server_event_from_notification(
                                notification,
                                thread_subscription_id,
                            ) {
                                pending_events.push(event);
                            }
                        }
                        JSONRPCMessage::Request(request) => {
                            let request_id = request.id.clone();
                            let method = request.method.clone();
                            match ServerRequest::try_from(request) {
                                Ok(request) => {
                                    pending_events.push(match thread_subscription_id {
                                        Some(thread_subscription_id) => {
                                            AppServerEvent::ThreadServerRequest {
                                                thread_subscription_id,
                                                request,
                                            }
                                        }
                                        None => AppServerEvent::ServerRequest(request),
                                    });
                                }
                                Err(err) => {
                                    warn!(%err, method, "rejecting unknown remote app-server request during initialize");
                                    write_jsonrpc_message(
                                        stream,
                                        JSONRPCMessage::Error(JSONRPCError {
                                            error: JSONRPCErrorError {
                                                code: -32601,
                                                message: format!(
                                                    "unsupported remote app-server request `{method}`"
                                                ),
                                                data: None,
                                            },
                                            id: request_id,
                                        }),
                                        endpoint,
                                    )
                                    .await?;
                                }
                            }
                        }
                        JSONRPCMessage::Response(_) | JSONRPCMessage::Error(_) => {}
                    }
                }
                Some(Ok(Message::Binary(_)))
                | Some(Ok(Message::Ping(_)))
                | Some(Ok(Message::Pong(_)))
                | Some(Ok(Message::Frame(_))) => {}
                Some(Ok(Message::Close(frame))) => {
                    let reason = frame
                        .as_ref()
                        .map(|frame| frame.reason.to_string())
                        .filter(|reason| !reason.is_empty())
                        .unwrap_or_else(|| "connection closed during initialize".to_string());
                    break Err(IoError::new(
                        ErrorKind::ConnectionAborted,
                        format!(
                            "remote app server at `{endpoint}` closed during initialize: {reason}"
                        ),
                    ));
                }
                Some(Err(err)) => {
                    break Err(IoError::other(format!(
                        "remote app server at `{endpoint}` transport failed during initialize: {err}"
                    )));
                }
                None => {
                    break Err(IoError::new(
                        ErrorKind::UnexpectedEof,
                        format!("remote app server at `{endpoint}` closed during initialize"),
                    ));
                }
            }
        }
    })
    .await
    .map_err(|_| {
        IoError::new(
            ErrorKind::TimedOut,
            format!("timed out waiting for initialize response from `{endpoint}`"),
        )
    })??;

    write_jsonrpc_message(
        stream,
        JSONRPCMessage::Notification(jsonrpc_notification_from_client_notification(
            ClientNotification::Initialized,
        )),
        endpoint,
    )
    .await?;

    Ok((pending_events, server_version, codex_home))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadSubscriptionExtension {
    thread_subscription_id: Option<String>,
}

fn server_message_and_subscription_id(
    text: &str,
) -> std::result::Result<(JSONRPCMessage, Option<String>), serde_json::Error> {
    let thread_subscription_id =
        serde_json::from_str::<ThreadSubscriptionExtension>(text)?.thread_subscription_id;
    let message = serde_json::from_str::<JSONRPCMessage>(text)?;
    Ok((message, thread_subscription_id))
}

fn app_server_event_from_notification(
    notification: JSONRPCNotification,
    thread_subscription_id: Option<String>,
) -> Option<AppServerEvent> {
    match ServerNotification::try_from(notification) {
        Ok(notification) => Some(match thread_subscription_id {
            Some(thread_subscription_id) => AppServerEvent::ThreadServerNotification {
                thread_subscription_id,
                notification,
            },
            None => AppServerEvent::ServerNotification(notification),
        }),
        Err(_) => None,
    }
}

async fn deliver_event(
    event_tx: &mpsc::Sender<AppServerEvent>,
    skipped_events: &mut usize,
    event: AppServerEvent,
) -> IoResult<()> {
    if *skipped_events > 0 {
        if remote_event_requires_delivery(&event) {
            event_tx
                .send(AppServerEvent::Lagged {
                    skipped: *skipped_events,
                })
                .await
                .map_err(|_| event_consumer_closed())?;
            *skipped_events = 0;
        } else {
            match event_tx.try_send(AppServerEvent::Lagged {
                skipped: *skipped_events,
            }) {
                Ok(()) => {
                    *skipped_events = 0;
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    *skipped_events = skipped_events.saturating_add(1);
                    warn!("dropping remote app-server event because consumer queue is full");
                    return Ok(());
                }
                Err(mpsc::error::TrySendError::Closed(_)) => return Err(event_consumer_closed()),
            }
        }
    }

    if remote_event_requires_delivery(&event) {
        event_tx
            .send(event)
            .await
            .map_err(|_| event_consumer_closed())
    } else {
        match event_tx.try_send(event) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                *skipped_events = skipped_events.saturating_add(1);
                warn!("dropping remote app-server event because consumer queue is full");
                Ok(())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(event_consumer_closed()),
        }
    }
}

fn remote_event_requires_delivery(event: &AppServerEvent) -> bool {
    match event {
        AppServerEvent::Lagged { .. } => false,
        AppServerEvent::ServerNotification(notification)
        | AppServerEvent::ThreadServerNotification { notification, .. } => {
            server_notification_requires_delivery(notification)
        }
        AppServerEvent::ServerRequest(_)
        | AppServerEvent::ThreadServerRequest { .. }
        | AppServerEvent::Disconnected { .. } => true,
    }
}

fn event_consumer_closed() -> IoError {
    IoError::new(
        ErrorKind::BrokenPipe,
        "remote app-server event consumer channel is closed",
    )
}

fn jsonrpc_request_from_client_request(request: ClientRequest) -> JSONRPCRequest {
    let value = match serde_json::to_value(request) {
        Ok(value) => value,
        Err(err) => panic!("client request should serialize: {err}"),
    };
    match serde_json::from_value(value) {
        Ok(request) => request,
        Err(err) => panic!("client request should encode as JSON-RPC request: {err}"),
    }
}

fn jsonrpc_notification_from_client_notification(
    notification: ClientNotification,
) -> JSONRPCNotification {
    let value = match serde_json::to_value(notification) {
        Ok(value) => value,
        Err(err) => panic!("client notification should serialize: {err}"),
    };
    match serde_json::from_value(value) {
        Ok(notification) => notification,
        Err(err) => panic!("client notification should encode as JSON-RPC notification: {err}"),
    }
}

async fn write_jsonrpc_message<S>(
    stream: &mut WebSocketStream<S>,
    message: JSONRPCMessage,
    endpoint: &str,
) -> IoResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let payload = serde_json::to_string(&message).map_err(IoError::other)?;
    stream
        .send(Message::Text(payload.into()))
        .await
        .map_err(|err| {
            IoError::other(format!(
                "failed to write websocket message to `{endpoint}`: {err}"
            ))
        })
}

fn websocket_close_error_is_already_closed(err: &TungsteniteError) -> bool {
    match err {
        TungsteniteError::ConnectionClosed | TungsteniteError::AlreadyClosed => true,
        TungsteniteError::Io(err) => matches!(
            err.kind(),
            ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::NotConnected
        ),
        _ => false,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::ThreadClosedNotification;
    use codex_app_server_protocol::ToolRequestUserInputParams;
    use codex_protocol::ThreadId;
    use futures::SinkExt;
    use futures::StreamExt;
    use std::time::Duration;
    use tokio::io::duplex;
    use tokio_tungstenite::tungstenite::protocol::Role;

    fn thread_scoped_server_message<T: serde::Serialize>(
        message: T,
        thread_subscription_id: &str,
    ) -> serde_json::Value {
        let mut message =
            serde_json::to_value(message).expect("server message should serialize as JSON-RPC");
        message
            .as_object_mut()
            .expect("server message should be a JSON-RPC object")
            .insert(
                "threadSubscriptionId".to_string(),
                serde_json::Value::String(thread_subscription_id.to_string()),
            );
        message
    }

    #[tokio::test]
    async fn transport_worker_preserves_server_subscription_identity() {
        let (client_io, server_io) = duplex(64 * 1024);
        let client_stream = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
        let mut server_stream =
            WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
        let (server_event_tx, mut server_event_rx) = mpsc::unbounded_channel::<serde_json::Value>();
        let (server_message_tx, mut server_message_rx) =
            mpsc::unbounded_channel::<JSONRPCMessage>();
        let (initialized_tx, initialized_rx) = oneshot::channel();
        let server_task = tokio::spawn(async move {
            let initialize = server_stream
                .next()
                .await
                .expect("client should initialize")
                .expect("initialize frame should succeed");
            let Message::Text(text) = initialize else {
                panic!("expected initialize text frame");
            };
            let JSONRPCMessage::Request(initialize) =
                serde_json::from_str(&text).expect("initialize frame should contain JSON-RPC")
            else {
                panic!("expected initialize request");
            };
            assert_eq!(initialize.method, "initialize");
            let initialize_response = JSONRPCMessage::Response(JSONRPCResponse {
                id: initialize.id,
                result: serde_json::json!({
                    "userAgent": "test-server/1.0",
                    "codexHome": "/tmp/codex-app-server-client-test",
                }),
            });
            server_stream
                .send(Message::Text(
                    serde_json::to_string(&initialize_response)
                        .expect("initialize response should serialize")
                        .into(),
                ))
                .await
                .expect("server should acknowledge initialize");

            let initialized = server_stream
                .next()
                .await
                .expect("client should send initialized")
                .expect("initialized frame should succeed");
            let Message::Text(text) = initialized else {
                panic!("expected initialized text frame");
            };
            let JSONRPCMessage::Notification(initialized) =
                serde_json::from_str(&text).expect("initialized frame should contain JSON-RPC")
            else {
                panic!("expected initialized notification");
            };
            assert_eq!(initialized.method, "initialized");
            let _ = initialized_tx.send(());

            loop {
                tokio::select! {
                    event = server_event_rx.recv() => {
                        let Some(event) = event else {
                            break;
                        };
                        server_stream
                            .send(Message::Text(
                                serde_json::to_string(&event)
                                    .expect("server event should serialize")
                                    .into(),
                            ))
                            .await
                            .expect("server should send event");
                    }
                    frame = server_stream.next() => {
                        match frame {
                            Some(Ok(Message::Text(text))) => {
                                let message = serde_json::from_str(&text)
                                    .expect("client frame should contain JSON-RPC");
                                let _ = server_message_tx.send(message);
                            }
                            Some(Ok(Message::Close(_))) | None => break,
                            Some(Ok(_)) => {}
                            Some(Err(err)) => panic!("test transport should not fail: {err}"),
                        }
                    }
                }
            }
        });

        let mut client = RemoteAppServerClient::connect_with_stream(
            /*channel_capacity*/ 4,
            "in-memory test transport".to_string(),
            client_stream,
            InitializeParams {
                client_info: ClientInfo {
                    name: "test-client".to_string(),
                    title: None,
                    version: "1.0".to_string(),
                },
                capabilities: None,
            },
        )
        .await
        .expect("remote client should initialize over the in-memory transport");
        initialized_rx
            .await
            .expect("server should observe the initialized notification");

        let thread_id = ThreadId::new();
        let old_subscription_id = "old-subscription";
        server_event_tx
            .send(thread_scoped_server_message(
                ServerNotification::ThreadClosed(ThreadClosedNotification {
                    thread_id: thread_id.to_string(),
                }),
                old_subscription_id,
            ))
            .expect("test server channel should be open");
        let delayed_notification =
            tokio::time::timeout(Duration::from_secs(1), client.next_event())
                .await
                .expect("thread notification should reach the client queue")
                .expect("client event stream should stay open");

        server_event_tx
            .send(thread_scoped_server_message(
                ServerRequest::ToolRequestUserInput {
                    request_id: RequestId::Integer(7),
                    params: ToolRequestUserInputParams {
                        thread_id: thread_id.to_string(),
                        turn_id: "turn-current".to_string(),
                        item_id: "item-current".to_string(),
                        questions: Vec::new(),
                        auto_resolution_ms: None,
                    },
                },
                old_subscription_id,
            ))
            .expect("test server channel should be open");
        let delayed_request = tokio::time::timeout(Duration::from_secs(1), client.next_event())
            .await
            .expect("thread request should reach the client queue")
            .expect("client event stream should stay open");

        match delayed_notification {
            AppServerEvent::ThreadServerNotification {
                thread_subscription_id,
                notification: ServerNotification::ThreadClosed(notification),
            } => {
                assert_eq!(thread_subscription_id, old_subscription_id);
                assert_eq!(notification.thread_id, thread_id.to_string());
            }
            event => panic!("expected transport-stamped thread close, got {event:?}"),
        }
        match delayed_request {
            AppServerEvent::ThreadServerRequest {
                thread_subscription_id,
                request: ServerRequest::ToolRequestUserInput { request_id, .. },
            } => {
                assert_eq!(thread_subscription_id, old_subscription_id);
                assert_eq!(request_id, RequestId::Integer(7));
            }
            event => panic!("expected transport-stamped thread request, got {event:?}"),
        }

        client
            .reject_server_request(
                RequestId::Integer(7),
                JSONRPCErrorError {
                    code: -32000,
                    message: "stale lifecycle rejected the captured request".to_string(),
                    data: None,
                },
            )
            .await
            .expect("captured server request should retain a rejectable JSON-RPC id");
        match tokio::time::timeout(Duration::from_secs(1), server_message_rx.recv())
            .await
            .expect("server should observe the rejection")
            .expect("test server should stay open")
        {
            JSONRPCMessage::Error(error) => {
                assert_eq!(error.id, RequestId::Integer(7));
                assert_eq!(error.error.code, -32000);
            }
            message => panic!("expected server-request rejection, got {message:?}"),
        }

        server_event_tx
            .send(thread_scoped_server_message(
                ServerNotification::ThreadClosed(ThreadClosedNotification {
                    thread_id: thread_id.to_string(),
                }),
                "new-subscription",
            ))
            .expect("test server channel should be open");
        match tokio::time::timeout(Duration::from_secs(1), client.next_event())
            .await
            .expect("replacement thread notification should reach the client queue")
            .expect("client event stream should stay open")
        {
            AppServerEvent::ThreadServerNotification {
                thread_subscription_id,
                ..
            } => {
                assert_eq!(thread_subscription_id, "new-subscription");
            }
            event => panic!("expected current transport-stamped thread close, got {event:?}"),
        }

        client
            .shutdown()
            .await
            .expect("client shutdown should complete");
        drop(server_event_tx);
        server_task
            .await
            .expect("test server task should not panic");
    }

    #[tokio::test]
    async fn shutdown_tolerates_worker_exit_after_command_is_queued() {
        let (command_tx, mut command_rx) = mpsc::channel(1);
        let (_event_tx, event_rx) = mpsc::channel::<AppServerEvent>(1);
        let worker_handle = tokio::spawn(async move {
            let _ = command_rx.recv().await;
        });
        let client = RemoteAppServerClient {
            command_tx,
            event_rx,
            pending_events: VecDeque::new(),
            server_version: None,
            codex_home: None,
            worker_handle,
        };

        client
            .shutdown()
            .await
            .expect("shutdown should complete when worker exits first");
    }
}
