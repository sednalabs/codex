/*
This module implements the remote app-server client transport.

It owns the remote connection lifecycle, including the initialize/initialized
handshake, JSON-RPC request/response routing, server-request resolution, and
notification streaming. Remote connections always carry WebSocket frames, over
either TCP WebSocket URLs or local Unix sockets. Both transports retain the
same legacy `AppServerEvent` surface and offer the additive
`TaggedAppServerEvent` stream for callers such as TUI that need listener
lifecycle identity.
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
use crate::TaggedAppServerEvent;
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
// After initialization, retain up to two ordinary events and reserve the final slot for an
// explicit terminal event. The fixed bound keeps a stalled consumer from turning the remote
// transport into an unbounded event buffer. Initialization uses the same total budget while its
// event receiver does not yet exist; an overflow rejects initialization rather than accumulating
// indefinitely.
const REMOTE_PENDING_EVENT_CAPACITY: usize = 3;
const REMOTE_PENDING_NON_TERMINAL_EVENT_CAPACITY: usize = REMOTE_PENDING_EVENT_CAPACITY - 1;
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
    event_rx: mpsc::Receiver<TaggedAppServerEvent>,
    pending_events: VecDeque<TaggedAppServerEvent>,
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
        let (event_tx, event_rx) = mpsc::channel::<TaggedAppServerEvent>(channel_capacity);
        let worker_handle = tokio::spawn(async move {
            let mut pending_requests =
                HashMap::<RequestId, oneshot::Sender<IoResult<RequestResult>>>::new();
            let mut worker_exit_error: Option<(ErrorKind, String)> = None;
            let mut skipped_events = 0usize;
            let mut pending_remote_events = VecDeque::with_capacity(REMOTE_PENDING_EVENT_CAPACITY);
            let mut exit_after_pending_events = false;
            let mut event_consumer_closed = false;
            loop {
                if !event_consumer_closed && event_tx.is_closed() {
                    if !pending_remote_events.is_empty() {
                        warn!("remote app-server event consumer closed while required event was pending");
                        pending_remote_events.clear();
                    }
                    // `RemoteAppServerClient::shutdown` drops its receiver before sending
                    // `Shutdown`. Keep the worker alive long enough to close the transport and
                    // acknowledge that command instead of waiting for its timeout.
                    event_consumer_closed = true;
                }
                if exit_after_pending_events {
                    let (err_kind, err_message) =
                        terminal_remote_transport_error(&worker_exit_error);
                    fail_pending_remote_requests(
                        &mut pending_requests,
                        err_kind,
                        &err_message,
                    );
                }
                if exit_after_pending_events
                    && pending_remote_events.is_empty()
                    && !event_consumer_closed
                {
                    break;
                }
                tokio::select! {
                    permit = event_tx.reserve(),
                    if !event_consumer_closed && !pending_remote_events.is_empty() => {
                        match permit {
                            Ok(permit) => {
                                let event = pending_remote_events
                                    .pop_front()
                                    .expect("pending remote event should exist");
                                permit.send(event);
                            }
                            Err(_) => {
                                warn!("remote app-server event consumer closed while required event was pending");
                                pending_remote_events.clear();
                                event_consumer_closed = true;
                            }
                        }
                    }
                    command = command_rx.recv() => {
                        let Some(command) = command else {
                            let _ = stream.close(None).await;
                            break;
                        };
                        match command {
                            RemoteClientCommand::Request { request, response_tx } => {
                                if exit_after_pending_events {
                                    let (error_kind, message) =
                                        terminal_remote_transport_error(&worker_exit_error);
                                    let _ = response_tx.send(Err(IoError::new(
                                        error_kind,
                                        message,
                                    )));
                                    continue;
                                }
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
                                    if let Err(err) = queue_remote_event(
                                        &event_tx,
                                        &mut skipped_events,
                                        &mut pending_remote_events,
                                        TaggedAppServerEvent::Disconnected {
                                            message: message.clone(),
                                        },
                                        /*exit_after_delivery*/ true,
                                        &endpoint,
                                        &mut worker_exit_error,
                                        &mut exit_after_pending_events,
                                    ) {
                                        warn!(%err, "failed to deliver remote app-server disconnect event");
                                    }
                                    worker_exit_error = Some((ErrorKind::BrokenPipe, message));
                                    if !exit_after_pending_events {
                                        break;
                                    }
                                }
                            }
                            RemoteClientCommand::Notify { response_tx, .. }
                            | RemoteClientCommand::ResolveServerRequest { response_tx, .. }
                            | RemoteClientCommand::RejectServerRequest { response_tx, .. }
                                if exit_after_pending_events => {
                                let (error_kind, message) =
                                    terminal_remote_transport_error(&worker_exit_error);
                                let _ = response_tx.send(Err(IoError::new(error_kind, message)));
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
                    message = stream.next(), if !exit_after_pending_events && !event_consumer_closed => {
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
                                            if let Err(err) = queue_remote_event(
                                                &event_tx,
                                                &mut skipped_events,
                                                &mut pending_remote_events,
                                                event,
                                                /*exit_after_delivery*/ false,
                                                &endpoint,
                                                &mut worker_exit_error,
                                                &mut exit_after_pending_events,
                                            ) {
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
                                                        TaggedAppServerEvent::ThreadServerRequest {
                                                            thread_subscription_id,
                                                            request,
                                                        }
                                                    }
                                                    None => {
                                                        TaggedAppServerEvent::ServerRequest(request)
                                                    }
                                                };
                                                // A terminal overload replaces this request with a
                                                // disconnect event, so answer it before starting drain.
                                                let required_event_count =
                                                    if skipped_events > 0 { 2 } else { 1 };
                                                if !pending_remote_events.is_empty()
                                                    && pending_remote_events.len()
                                                        + required_event_count
                                                        > REMOTE_PENDING_NON_TERMINAL_EVENT_CAPACITY
                                                {
                                                    let message =
                                                        remote_event_backlog_overflow_message(
                                                            &endpoint,
                                                            skipped_events,
                                                        );
                                                    if let Err(reject_err) = write_jsonrpc_message(
                                                        &mut stream,
                                                        JSONRPCMessage::Error(JSONRPCError {
                                                            error: JSONRPCErrorError {
                                                                code: -32000,
                                                                message,
                                                                data: None,
                                                            },
                                                            id: request_id,
                                                        }),
                                                        &endpoint,
                                                    )
                                                    .await
                                                    {
                                                        warn!(
                                                            %reject_err,
                                                            "failed to reject remote app-server server request after event backlog overflow"
                                                        );
                                                    }
                                                }
                                                if let Err(err) = queue_remote_event(
                                                    &event_tx,
                                                    &mut skipped_events,
                                                    &mut pending_remote_events,
                                                    event,
                                                    /*exit_after_delivery*/ false,
                                                    &endpoint,
                                                    &mut worker_exit_error,
                                                    &mut exit_after_pending_events,
                                                ) {
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
                                                    if let Err(err) = queue_remote_event(
                                                        &event_tx,
                                                        &mut skipped_events,
                                                        &mut pending_remote_events,
                                                        TaggedAppServerEvent::Disconnected {
                                                            message: message.clone(),
                                                        },
                                                        /*exit_after_delivery*/ true,
                                                        &endpoint,
                                                        &mut worker_exit_error,
                                                        &mut exit_after_pending_events,
                                                    ) {
                                                        warn!(%err, "failed to deliver remote app-server disconnect event");
                                                    }
                                                    worker_exit_error =
                                                        Some((ErrorKind::BrokenPipe, message));
                                                    if !exit_after_pending_events {
                                                        break;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    },
                                    Err(err) => {
                                        let message = format!(
                                            "remote app server at `{endpoint}` sent invalid JSON-RPC: {err}"
                                        );
                                        if let Err(deliver_err) = queue_remote_event(
                                            &event_tx,
                                            &mut skipped_events,
                                            &mut pending_remote_events,
                                            TaggedAppServerEvent::Disconnected {
                                                message: message.clone(),
                                            },
                                            /*exit_after_delivery*/ true,
                                            &endpoint,
                                            &mut worker_exit_error,
                                            &mut exit_after_pending_events,
                                        ) {
                                            warn!(%deliver_err, "failed to deliver remote app-server disconnect event");
                                        }
                                        worker_exit_error =
                                            Some((ErrorKind::InvalidData, message));
                                        if !exit_after_pending_events {
                                            break;
                                        }
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
                                if let Err(err) = queue_remote_event(
                                    &event_tx,
                                    &mut skipped_events,
                                    &mut pending_remote_events,
                                    TaggedAppServerEvent::Disconnected {
                                        message: message.clone(),
                                    },
                                    /*exit_after_delivery*/ true,
                                    &endpoint,
                                    &mut worker_exit_error,
                                    &mut exit_after_pending_events,
                                ) {
                                    warn!(%err, "failed to deliver remote app-server disconnect event");
                                }
                                worker_exit_error = Some((
                                    ErrorKind::ConnectionAborted,
                                    message,
                                ));
                                if !exit_after_pending_events {
                                    break;
                                }
                            }
                            Some(Ok(Message::Binary(_)))
                            | Some(Ok(Message::Ping(_)))
                            | Some(Ok(Message::Pong(_)))
                            | Some(Ok(Message::Frame(_))) => {}
                            Some(Err(err)) => {
                                let message = format!(
                                    "remote app server at `{endpoint}` transport failed: {err}"
                                );
                                if let Err(deliver_err) = queue_remote_event(
                                    &event_tx,
                                    &mut skipped_events,
                                    &mut pending_remote_events,
                                    TaggedAppServerEvent::Disconnected {
                                        message: message.clone(),
                                    },
                                    /*exit_after_delivery*/ true,
                                    &endpoint,
                                    &mut worker_exit_error,
                                    &mut exit_after_pending_events,
                                ) {
                                    warn!(%deliver_err, "failed to deliver remote app-server disconnect event");
                                }
                                worker_exit_error = Some((ErrorKind::InvalidData, message));
                                if !exit_after_pending_events {
                                    break;
                                }
                            }
                            None => {
                                let message = format!(
                                    "remote app server at `{endpoint}` closed the connection"
                                );
                                if let Err(err) = queue_remote_event(
                                    &event_tx,
                                    &mut skipped_events,
                                    &mut pending_remote_events,
                                    TaggedAppServerEvent::Disconnected {
                                        message: message.clone(),
                                    },
                                    /*exit_after_delivery*/ true,
                                    &endpoint,
                                    &mut worker_exit_error,
                                    &mut exit_after_pending_events,
                                ) {
                                    warn!(%err, "failed to deliver remote app-server disconnect event");
                                }
                                worker_exit_error = Some((ErrorKind::UnexpectedEof, message));
                                if !exit_after_pending_events {
                                    break;
                                }
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
            fail_pending_remote_requests(&mut pending_requests, err_kind, &err_message);
        });

        Ok(Self {
            command_tx,
            event_rx,
            pending_events,
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

    /// Returns the next legacy event. Thread subscription identity is retained
    /// only on [`next_tagged_event`](Self::next_tagged_event).
    pub async fn next_event(&mut self) -> Option<AppServerEvent> {
        self.next_tagged_event().await.map(Into::into)
    }

    /// Returns the next event with immutable thread subscription identity.
    pub async fn next_tagged_event(&mut self) -> Option<TaggedAppServerEvent> {
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
) -> IoResult<(
    VecDeque<TaggedAppServerEvent>,
    Option<String>,
    Option<String>,
)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let initialize_request_id = RequestId::String("initialize".to_string());
    let mut pending_events = VecDeque::with_capacity(REMOTE_PENDING_EVENT_CAPACITY);
    let mut skipped_events = 0usize;
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
                            if skipped_events > 0 {
                                pending_events.push_back(TaggedAppServerEvent::Lagged {
                                    skipped: skipped_events,
                                });
                            }
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
                                if let Err(err) = queue_initialize_event(
                                    &mut pending_events,
                                    &mut skipped_events,
                                    event,
                                    endpoint,
                                ) {
                                    let message = err.to_string();
                                    reject_pending_initialize_server_requests(
                                        stream,
                                        endpoint,
                                        &pending_events,
                                        &message,
                                    )
                                    .await;
                                    return Err(err);
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
                                            TaggedAppServerEvent::ThreadServerRequest {
                                                thread_subscription_id,
                                                request,
                                            }
                                        }
                                        None => TaggedAppServerEvent::ServerRequest(request),
                                    };
                                    if let Err(err) = queue_initialize_event(
                                        &mut pending_events,
                                        &mut skipped_events,
                                        event,
                                        endpoint,
                                    ) {
                                        let message = err.to_string();
                                        reject_pending_initialize_server_requests(
                                            stream,
                                            endpoint,
                                            &pending_events,
                                            &message,
                                        )
                                        .await;
                                        if let Err(reject_err) = write_jsonrpc_message(
                                            stream,
                                            JSONRPCMessage::Error(JSONRPCError {
                                                error: JSONRPCErrorError {
                                                    code: -32000,
                                                    message,
                                                    data: None,
                                                },
                                                id: request_id,
                                            }),
                                            endpoint,
                                        )
                                        .await
                                        {
                                            warn!(
                                                %reject_err,
                                                "failed to reject remote app-server request after initialize backlog overflow"
                                            );
                                        }
                                        return Err(err);
                                    }
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

/// Retains recognized events that arrive before initialization completes without allowing a
/// slow initialize response to turn the handshake into an unbounded event buffer. This preserves
/// the post-initialize split: best-effort events can become a later `Lagged` marker, but required
/// state and server requests fail the connection explicitly rather than being silently dropped.
fn queue_initialize_event(
    pending_events: &mut VecDeque<TaggedAppServerEvent>,
    skipped_events: &mut usize,
    event: TaggedAppServerEvent,
    endpoint: &str,
) -> IoResult<()> {
    let event_requires_delivery = remote_event_requires_delivery(&event);
    let queued_event_count = if event_requires_delivery && *skipped_events > 0 {
        2
    } else {
        1
    };
    if pending_events.len() + queued_event_count
        <= REMOTE_PENDING_NON_TERMINAL_EVENT_CAPACITY
    {
        if event_requires_delivery && *skipped_events > 0 {
            pending_events.push_back(TaggedAppServerEvent::Lagged {
                skipped: *skipped_events,
            });
            *skipped_events = 0;
        }
        pending_events.push_back(event);
        return Ok(());
    }

    if !event_requires_delivery {
        *skipped_events = skipped_events.saturating_add(1);
        warn!("dropping remote app-server event because the initialize FIFO is full");
        return Ok(());
    }

    Err(IoError::new(
        ErrorKind::WouldBlock,
        format!(
            "remote app server at `{endpoint}` exceeded the bounded initialize event backlog"
        ),
    ))
}

async fn reject_pending_initialize_server_requests<S>(
    stream: &mut WebSocketStream<S>,
    endpoint: &str,
    pending_events: &VecDeque<TaggedAppServerEvent>,
    message: &str,
)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    for event in pending_events {
        let request_id = match event {
            TaggedAppServerEvent::ServerRequest(request)
            | TaggedAppServerEvent::ThreadServerRequest { request, .. } => request.id().clone(),
            TaggedAppServerEvent::ServerNotification(_)
            | TaggedAppServerEvent::ThreadServerNotification { .. }
            | TaggedAppServerEvent::Lagged { .. }
            | TaggedAppServerEvent::Disconnected { .. } => continue,
        };
        if let Err(err) = write_jsonrpc_message(
            stream,
            JSONRPCMessage::Error(JSONRPCError {
                error: JSONRPCErrorError {
                    code: -32000,
                    message: message.to_string(),
                    data: None,
                },
                id: request_id,
            }),
            endpoint,
        )
        .await
        {
            warn!(
                %err,
                "failed to reject pending remote app-server request after initialize backlog overflow"
            );
        }
    }
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
) -> Option<TaggedAppServerEvent> {
    match ServerNotification::try_from(notification) {
        Ok(notification) => Some(match thread_subscription_id {
            Some(thread_subscription_id) => TaggedAppServerEvent::ThreadServerNotification {
                thread_subscription_id,
                notification,
            },
            None => TaggedAppServerEvent::ServerNotification(notification),
        }),
        Err(_) => None,
    }
}

enum RemoteEventDelivery {
    Forwarded,
    Pending(VecDeque<TaggedAppServerEvent>),
}

fn remote_event_backlog_overflow_message(endpoint: &str, skipped_events: usize) -> String {
    let mut message =
        format!("remote app server at `{endpoint}` exceeded the bounded remote event backlog");
    if skipped_events > 0 {
        message.push_str(&format!(
            "; {skipped_events} best-effort remote event(s) were dropped before disconnect"
        ));
    }
    message
}

/// Queues one remote event without allowing a later event to bypass the bounded pending FIFO.
/// The FIFO reserves one slot for a terminal disconnect so a transport error remains visible after
/// previously queued events. If a further required event would exhaust that reserve, the worker
/// reports an explicit overload disconnect rather than retaining an unbounded backlog.
/// If that final slot is the only remaining space, its disconnect message carries any unreported
/// best-effort loss count so backpressure accounting remains visible without exceeding the bound.
fn queue_remote_event(
    event_tx: &mpsc::Sender<TaggedAppServerEvent>,
    skipped_events: &mut usize,
    pending_remote_events: &mut VecDeque<TaggedAppServerEvent>,
    mut event: TaggedAppServerEvent,
    exit_after_delivery: bool,
    endpoint: &str,
    worker_exit_error: &mut Option<(ErrorKind, String)>,
    exit_after_pending_events: &mut bool,
) -> IoResult<()> {
    if !pending_remote_events.is_empty() {
        if exit_after_delivery {
            if *skipped_events > 0
                && pending_remote_events.len() < REMOTE_PENDING_NON_TERMINAL_EVENT_CAPACITY
            {
                pending_remote_events.push_back(TaggedAppServerEvent::Lagged {
                    skipped: *skipped_events,
                });
            } else if *skipped_events > 0 {
                let skipped = *skipped_events;
                let TaggedAppServerEvent::Disconnected { message } = &mut event else {
                    unreachable!("terminal remote event delivery must use a disconnect event");
                };
                message.push_str(&format!(
                    "; {skipped} best-effort remote event(s) were dropped before disconnect"
                ));
            }
            *skipped_events = 0;
            if pending_remote_events.len() >= REMOTE_PENDING_EVENT_CAPACITY {
                let message = format!(
                    "remote app server at `{endpoint}` exhausted the bounded terminal-event backlog"
                );
                warn!(
                    %message,
                    "closing remote app-server transport without another terminal event"
                );
                *worker_exit_error = Some((ErrorKind::WouldBlock, message));
                *exit_after_pending_events = true;
                return Ok(());
            }
            pending_remote_events.push_back(event);
            *exit_after_pending_events = true;
            return Ok(());
        }

        let queued_event_count = if *skipped_events > 0 { 2 } else { 1 };
        if pending_remote_events.len() + queued_event_count
            <= REMOTE_PENDING_NON_TERMINAL_EVENT_CAPACITY
        {
            if *skipped_events > 0 {
                pending_remote_events.push_back(TaggedAppServerEvent::Lagged {
                    skipped: *skipped_events,
                });
                *skipped_events = 0;
            }
            pending_remote_events.push_back(event);
            return Ok(());
        }

        if !remote_event_requires_delivery(&event) {
            *skipped_events = skipped_events.saturating_add(1);
            warn!("dropping remote app-server event because the pending FIFO is full");
            return Ok(());
        }

        let message = remote_event_backlog_overflow_message(endpoint, *skipped_events);
        warn!(
            %message,
            "closing remote app-server transport after retained required events drain"
        );
        debug_assert!(pending_remote_events.len() < REMOTE_PENDING_EVENT_CAPACITY);
        pending_remote_events.push_back(TaggedAppServerEvent::Disconnected {
            message: message.clone(),
        });
        *worker_exit_error = Some((ErrorKind::WouldBlock, message));
        *skipped_events = 0;
        *exit_after_pending_events = true;
        return Ok(());
    }

    match try_deliver_event(event_tx, skipped_events, event)? {
        RemoteEventDelivery::Forwarded => {}
        RemoteEventDelivery::Pending(events) => {
            debug_assert!(events.len() <= REMOTE_PENDING_NON_TERMINAL_EVENT_CAPACITY);
            *pending_remote_events = events;
        }
    }
    if exit_after_delivery {
        *exit_after_pending_events = true;
    }
    Ok(())
}

/// Makes one bounded, non-blocking forwarding attempt. Required events remain in the returned
/// fixed-size pending queue when the consumer is full; the transport worker keeps command and
/// shutdown handling selectable while that queue waits for capacity.
fn try_deliver_event(
    event_tx: &mpsc::Sender<TaggedAppServerEvent>,
    skipped_events: &mut usize,
    event: TaggedAppServerEvent,
) -> IoResult<RemoteEventDelivery> {
    let mut required_events = VecDeque::new();
    if *skipped_events > 0 {
        if remote_event_requires_delivery(&event) {
            required_events.push_back(TaggedAppServerEvent::Lagged {
                skipped: *skipped_events,
            });
            *skipped_events = 0;
        } else {
            match event_tx.try_send(TaggedAppServerEvent::Lagged {
                skipped: *skipped_events,
            }) {
                Ok(()) => {
                    *skipped_events = 0;
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    *skipped_events = skipped_events.saturating_add(1);
                    warn!("dropping remote app-server event because consumer queue is full");
                    return Ok(RemoteEventDelivery::Forwarded);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => return Err(event_consumer_closed()),
            }
        }
    }

    if remote_event_requires_delivery(&event) {
        required_events.push_back(event);
        while let Some(required_event) = required_events.pop_front() {
            match event_tx.try_send(required_event) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(required_event)) => {
                    required_events.push_front(required_event);
                    return Ok(RemoteEventDelivery::Pending(required_events));
                }
                Err(mpsc::error::TrySendError::Closed(_)) => return Err(event_consumer_closed()),
            }
        }
        Ok(RemoteEventDelivery::Forwarded)
    } else {
        match event_tx.try_send(event) {
            Ok(()) => Ok(RemoteEventDelivery::Forwarded),
            Err(mpsc::error::TrySendError::Full(_)) => {
                *skipped_events = skipped_events.saturating_add(1);
                warn!("dropping remote app-server event because consumer queue is full");
                Ok(RemoteEventDelivery::Forwarded)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(event_consumer_closed()),
        }
    }
}

fn remote_event_requires_delivery(event: &TaggedAppServerEvent) -> bool {
    match event {
        TaggedAppServerEvent::Lagged { .. } => false,
        TaggedAppServerEvent::ServerNotification(notification)
        | TaggedAppServerEvent::ThreadServerNotification { notification, .. } => {
            server_notification_requires_delivery(notification)
        }
        TaggedAppServerEvent::ServerRequest(_)
        | TaggedAppServerEvent::ThreadServerRequest { .. }
        | TaggedAppServerEvent::Disconnected { .. } => true,
    }
}

/// Fails every outstanding outbound RPC as soon as the transport has a terminal state. Taking
/// the map makes the cleanup exactly-once while retained events continue draining independently.
fn fail_pending_remote_requests(
    pending_requests: &mut HashMap<RequestId, oneshot::Sender<IoResult<RequestResult>>>,
    err_kind: ErrorKind,
    err_message: &str,
) {
    for (_, response_tx) in std::mem::take(pending_requests) {
        let _ = response_tx.send(Err(IoError::new(err_kind, err_message.to_string())));
    }
}

fn terminal_remote_transport_error(
    worker_exit_error: &Option<(ErrorKind, String)>,
) -> (ErrorKind, String) {
    worker_exit_error.as_ref().map_or_else(
        || {
            (
                ErrorKind::BrokenPipe,
                "remote app-server transport is closing after required events drain".to_string(),
            )
        },
        |(kind, message)| (*kind, message.clone()),
    )
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
    use pretty_assertions::assert_eq;
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

    fn thread_goal_updated_notification() -> ServerNotification {
        ServerNotification::ThreadGoalUpdated(
            codex_app_server_protocol::ThreadGoalUpdatedNotification {
                thread_id: "thread".to_string(),
                turn_id: Some("turn".to_string()),
                goal: codex_app_server_protocol::ThreadGoal {
                    thread_id: "thread".to_string(),
                    objective: "finish the task".to_string(),
                    status: codex_app_server_protocol::ThreadGoalStatus::Active,
                    token_budget: Some(100),
                    tokens_used: 25,
                    time_used_seconds: 1,
                    created_at: 0,
                    updated_at: 0,
                },
            },
        )
    }

    fn thread_token_usage_updated_notification() -> ServerNotification {
        let usage = codex_app_server_protocol::TokenUsageBreakdown {
            total_tokens: 25,
            input_tokens: 20,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 5,
            reasoning_output_tokens: 0,
        };
        ServerNotification::ThreadTokenUsageUpdated(
            codex_app_server_protocol::ThreadTokenUsageUpdatedNotification {
                thread_id: "thread".to_string(),
                turn_id: "turn".to_string(),
                token_usage: codex_app_server_protocol::ThreadTokenUsage {
                    total: usage.clone(),
                    last: usage,
                    model_context_window: Some(100),
                },
            },
        )
    }

    fn thread_name_updated_notification() -> ServerNotification {
        ServerNotification::ThreadNameUpdated(
            codex_app_server_protocol::ThreadNameUpdatedNotification {
                thread_id: "thread".to_string(),
                thread_name: Some("renamed thread".to_string()),
            },
        )
    }

    fn thread_status_changed_notification(status_revision: u64) -> ServerNotification {
        ServerNotification::ThreadStatusChanged(
            codex_app_server_protocol::ThreadStatusChangedNotification {
                thread_id: "thread".to_string(),
                status: codex_app_server_protocol::ThreadStatus::Active {
                    active_flags: Vec::new(),
                },
                status_revision: Some(status_revision),
            },
        )
    }

    fn command_execution_output_delta_notification(delta: &str) -> ServerNotification {
        ServerNotification::CommandExecutionOutputDelta(
            codex_app_server_protocol::CommandExecutionOutputDeltaNotification {
                thread_id: "thread".to_string(),
                turn_id: "turn".to_string(),
                item_id: "item".to_string(),
                delta: delta.to_string(),
            },
        )
    }

    #[tokio::test]
    async fn transport_worker_preserves_server_subscription_identity_and_response_liveness() {
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
            /*channel_capacity*/ 1,
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
            tokio::time::timeout(Duration::from_secs(1), client.next_tagged_event())
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
        let delayed_request =
            tokio::time::timeout(Duration::from_secs(1), client.next_tagged_event())
                .await
            .expect("thread request should reach the client queue")
            .expect("client event stream should stay open");

        match delayed_notification {
            TaggedAppServerEvent::ThreadServerNotification {
                thread_subscription_id,
                notification: ServerNotification::ThreadClosed(notification),
            } => {
                assert_eq!(thread_subscription_id, old_subscription_id);
                assert_eq!(notification.thread_id, thread_id.to_string());
            }
            event => panic!("expected transport-stamped thread close, got {event:?}"),
        }
        match delayed_request {
            TaggedAppServerEvent::ThreadServerRequest {
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
        match tokio::time::timeout(Duration::from_secs(1), client.next_tagged_event())
            .await
            .expect("replacement thread notification should reach the client queue")
            .expect("client event stream should stay open")
        {
            TaggedAppServerEvent::ThreadServerNotification {
                thread_subscription_id,
                ..
            } => {
                assert_eq!(thread_subscription_id, "new-subscription");
            }
            event => panic!("expected current transport-stamped thread close, got {event:?}"),
        }

        // Fill the one-slot consumer queue with required lifecycle state, then send another
        // required lifecycle event. The worker must retain the second event without awaiting the
        // full queue, while continuing to read a JSON-RPC response that follows it.
        server_event_tx
            .send(thread_scoped_server_message(
                thread_status_changed_notification(1),
                "backpressure-subscription",
            ))
            .expect("test server channel should be open");
        timeout(Duration::from_secs(1), async {
            while client.event_rx.len() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("required status should fill the one-slot consumer queue");
        server_event_tx
            .send(thread_scoped_server_message(
                thread_status_changed_notification(2),
                "backpressure-subscription",
            ))
            .expect("test server channel should be open");
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        let expected_request = JSONRPCRequest {
            id: RequestId::Integer(8),
            method: "test/echo".to_string(),
            params: Some(serde_json::json!({"value": "backpressure"})),
            trace: None,
        };
        let request_handle = client.request_handle();
        let request_task = tokio::spawn({
            let request = expected_request.clone();
            async move { request_handle.request_json_rpc(request).await }
        });

        let JSONRPCMessage::Request(request) =
            timeout(Duration::from_secs(1), server_message_rx.recv())
                .await
                .expect("remote request should reach the server while events are backpressured")
                .expect("test server should stay open")
        else {
            panic!("expected request while required events are pending");
        };
        assert_eq!(request, expected_request);
        server_event_tx
            .send(
                serde_json::to_value(JSONRPCMessage::Response(JSONRPCResponse {
                    id: RequestId::Integer(8),
                    result: serde_json::json!({"accepted": true}),
                }))
                .expect("response should serialize"),
            )
            .expect("test server channel should be open");

        let response = timeout(Duration::from_secs(1), request_task)
            .await
            .expect("required-event backpressure must not block a JSON-RPC response")
            .expect("request task should not panic")
            .expect("remote request should complete")
            .expect("server should return a successful response");
        assert_eq!(response, serde_json::json!({"accepted": true}));
        assert_eq!(
            client.event_rx.len(),
            1,
            "the one-slot event receiver must remain full until the client drains it"
        );

        match timeout(Duration::from_secs(1), client.next_tagged_event())
            .await
            .expect("queued status should remain available")
            .expect("client event stream should stay open")
        {
            TaggedAppServerEvent::ThreadServerNotification {
                thread_subscription_id,
                notification: ServerNotification::ThreadStatusChanged(notification),
            } => {
                assert_eq!(thread_subscription_id, "backpressure-subscription");
                assert_eq!(notification.status_revision, Some(1));
            }
            event => panic!("expected retained required status, got {event:?}"),
        }
        match timeout(Duration::from_secs(1), client.next_tagged_event())
            .await
            .expect("queued second status should drain after the first status")
            .expect("client event stream should stay open")
        {
            TaggedAppServerEvent::ThreadServerNotification {
                thread_subscription_id,
                notification: ServerNotification::ThreadStatusChanged(notification),
            } => {
                assert_eq!(thread_subscription_id, "backpressure-subscription");
                assert_eq!(notification.status_revision, Some(2));
            }
            event => panic!("expected retained status after the first status, got {event:?}"),
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
    async fn terminal_disconnect_fails_outbound_commands_before_event_fifo_drains() {
        let (client_io, server_io) = duplex(64 * 1024);
        let client_stream = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
        let mut server_stream =
            WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
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
            let initialize_response = JSONRPCMessage::Response(JSONRPCResponse {
                id: initialize.id,
                result: serde_json::json!({"userAgent": "test-server/1.0"}),
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

            let request = server_stream
                .next()
                .await
                .expect("client should send a request")
                .expect("request frame should succeed");
            let Message::Text(text) = request else {
                panic!("expected request text frame");
            };
            let JSONRPCMessage::Request(request) =
                serde_json::from_str(&text).expect("request frame should contain JSON-RPC")
            else {
                panic!("expected client request");
            };
            assert_eq!(request.id, RequestId::Integer(42));
            assert_eq!(request.method, "test/pending");

            let server_request = thread_scoped_server_message(
                ServerRequest::ToolRequestUserInput {
                    request_id: RequestId::Integer(43),
                    params: ToolRequestUserInputParams {
                        thread_id: "thread".to_string(),
                        turn_id: "turn".to_string(),
                        item_id: "item".to_string(),
                        questions: Vec::new(),
                        auto_resolution_ms: None,
                    },
                },
                "terminal-subscription",
            );
            server_stream
                .send(Message::Text(
                    serde_json::to_string(&server_request)
                        .expect("server request should serialize")
                        .into(),
                ))
                .await
                .expect("server should send a request before closing the transport");
            for status_revision in [1, 2] {
                let event = thread_scoped_server_message(
                    thread_status_changed_notification(status_revision),
                    "terminal-subscription",
                );
                server_stream
                    .send(Message::Text(
                        serde_json::to_string(&event)
                            .expect("server event should serialize")
                            .into(),
                    ))
                    .await
                    .expect("server should send required event");
            }
            server_stream
                .send(Message::Close(None))
                .await
                .expect("server should close the test transport");
        });

        let mut client = RemoteAppServerClient::connect_with_stream(
            /*channel_capacity*/ 1,
            "in-memory terminal RPC transport".to_string(),
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

        let request_task = tokio::spawn({
            let request_handle = client.request_handle();
            async move {
                request_handle
                    .request_json_rpc(JSONRPCRequest {
                        id: RequestId::Integer(42),
                        method: "test/pending".to_string(),
                        params: None,
                        trace: None,
                    })
                    .await
            }
        });
        let request_error = timeout(Duration::from_secs(1), request_task)
            .await
            .expect("terminal transport state should fail the pending RPC promptly")
            .expect("request task should not panic")
            .err()
            .expect("terminal transport state should fail the pending RPC");
        assert_eq!(request_error.kind(), ErrorKind::ConnectionAborted);
        assert!(request_error.to_string().contains("disconnected"));
        assert_eq!(
            client.event_rx.len(),
            1,
            "the one-slot event receiver must still be full when the RPC fails"
        );

        for command_error in [
            timeout(Duration::from_secs(1), client.notify(ClientNotification::Initialized))
                .await
                .expect(
                    "terminal transport should fail notifications without waiting for events",
                )
                .err()
                .expect("terminal transport should not write a notification"),
            timeout(
                Duration::from_secs(1),
                client.resolve_server_request(RequestId::Integer(43), serde_json::json!({})),
            )
            .await
            .expect(
                "terminal transport should fail server-request resolution without waiting for events",
            )
            .err()
            .expect("terminal transport should not write a server-request resolution"),
            timeout(
                Duration::from_secs(1),
                client.reject_server_request(
                    RequestId::Integer(43),
                    JSONRPCErrorError {
                        code: -32000,
                        message: "transport was already closed".to_string(),
                        data: None,
                    },
                ),
            )
            .await
            .expect(
                "terminal transport should fail server-request rejection without waiting for events",
            )
            .err()
            .expect("terminal transport should not write a server-request rejection"),
        ] {
            assert_eq!(command_error.kind(), ErrorKind::ConnectionAborted);
            assert!(command_error.to_string().contains("disconnected"));
        }

        match client.next_tagged_event().await {
            Some(TaggedAppServerEvent::ThreadServerRequest {
                thread_subscription_id,
                request: ServerRequest::ToolRequestUserInput { request_id, .. },
            }) => {
                assert_eq!(thread_subscription_id, "terminal-subscription");
                assert_eq!(request_id, RequestId::Integer(43));
            }
            event => panic!("expected retained terminal-path server request, got {event:?}"),
        }
        for status_revision in [1, 2] {
            match client.next_tagged_event().await {
                Some(TaggedAppServerEvent::ThreadServerNotification {
                    thread_subscription_id,
                    notification: ServerNotification::ThreadStatusChanged(notification),
                }) => {
                    assert_eq!(thread_subscription_id, "terminal-subscription");
                    assert_eq!(notification.status_revision, Some(status_revision));
                }
                event => panic!("expected retained terminal-path status, got {event:?}"),
            }
        }
        match client.next_tagged_event().await {
            Some(TaggedAppServerEvent::Disconnected { message }) => {
                assert!(message.contains("disconnected"));
            }
            event => panic!("expected retained terminal disconnect, got {event:?}"),
        }
        assert!(
            timeout(Duration::from_secs(1), client.next_tagged_event())
                .await
                .expect("worker should exit after the retained terminal FIFO drains")
                .is_none()
        );
        server_task
            .await
            .expect("test server task should not panic");
    }

    #[tokio::test]
    async fn postinitialize_server_request_overflow_is_rejected_before_terminal_drain() {
        let (client_io, server_io) = duplex(64 * 1024);
        let client_stream = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
        let mut server_stream =
            WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
        let (first_event_sent_tx, first_event_sent_rx) = oneshot::channel();
        let (send_overflow_tx, send_overflow_rx) = oneshot::channel();
        let (rejection_observed_tx, rejection_observed_rx) = oneshot::channel();
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
            let initialize_response = JSONRPCMessage::Response(JSONRPCResponse {
                id: initialize.id,
                result: serde_json::json!({"userAgent": "test-server/1.0"}),
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

            let first_event = thread_scoped_server_message(
                thread_status_changed_notification(1),
                "overflow-subscription",
            );
            server_stream
                .send(Message::Text(
                    serde_json::to_string(&first_event)
                        .expect("first required event should serialize")
                        .into(),
                ))
                .await
                .expect("server should send first required event");
            first_event_sent_tx
                .send(())
                .expect("test client should wait for the first required event");
            send_overflow_rx
                .await
                .expect("test client should hold the first required event in its full queue");

            for status_revision in [2, 3] {
                let event = thread_scoped_server_message(
                    thread_status_changed_notification(status_revision),
                    "overflow-subscription",
                );
                server_stream
                    .send(Message::Text(
                        serde_json::to_string(&event)
                            .expect("required event should serialize")
                            .into(),
                    ))
                    .await
                    .expect("server should send required event");
            }

            let overflow_request = thread_scoped_server_message(
                ServerRequest::ToolRequestUserInput {
                    request_id: RequestId::Integer(99),
                    params: ToolRequestUserInputParams {
                        thread_id: "thread".to_string(),
                        turn_id: "turn".to_string(),
                        item_id: "item".to_string(),
                        questions: Vec::new(),
                        auto_resolution_ms: None,
                    },
                },
                "overflow-subscription",
            );
            server_stream
                .send(Message::Text(
                    serde_json::to_string(&overflow_request)
                        .expect("overflow request should serialize")
                        .into(),
                ))
                .await
                .expect("server should send overflowing request");

            let rejection = timeout(Duration::from_secs(1), server_stream.next())
                .await
                .expect("client should promptly reject the overflowing server request")
                .expect("client should send an overflowing server request rejection")
                .expect("overflow rejection frame should succeed");
            let Message::Text(text) = rejection else {
                panic!("expected overflowing server request error text frame");
            };
            let JSONRPCMessage::Error(rejection) =
                serde_json::from_str(&text).expect("overflow rejection should contain JSON-RPC")
            else {
                panic!("expected overflowing server request rejection");
            };
            assert_eq!(rejection.id, RequestId::Integer(99));
            assert_eq!(rejection.error.code, -32000);
            assert!(
                rejection
                    .error
                    .message
                    .contains("exceeded the bounded remote event backlog")
            );
            rejection_observed_tx
                .send(())
                .expect("test client should wait for the server-request rejection");
        });

        let mut client = RemoteAppServerClient::connect_with_stream(
            /*channel_capacity*/ 1,
            "in-memory post-initialize overflow transport".to_string(),
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

        first_event_sent_rx
            .await
            .expect("server should send the first required event after initialize");
        timeout(Duration::from_secs(1), async {
            while client.event_rx.len() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the first required event should fill the one-slot consumer queue");
        send_overflow_tx
            .send(())
            .expect("test server should still accept the overflow sequence");
        timeout(Duration::from_secs(1), rejection_observed_rx)
            .await
            .expect("overflowing server request should be rejected before event drain")
            .expect("server should observe the overflowing request rejection");

        for status_revision in [1, 2, 3] {
            match timeout(Duration::from_secs(1), client.next_tagged_event())
                .await
                .expect("retained required event should drain promptly")
            {
                Some(TaggedAppServerEvent::ThreadServerNotification {
                    thread_subscription_id,
                    notification: ServerNotification::ThreadStatusChanged(notification),
                }) => {
                    assert_eq!(thread_subscription_id, "overflow-subscription");
                    assert_eq!(notification.status_revision, Some(status_revision));
                }
                event => panic!("expected retained required event, got {event:?}"),
            }
        }
        match timeout(Duration::from_secs(1), client.next_tagged_event())
            .await
            .expect("overflow disconnect should remain observable")
        {
            Some(TaggedAppServerEvent::Disconnected { message }) => {
                assert!(message.contains("exceeded the bounded remote event backlog"));
            }
            event => panic!("expected terminal overload disconnect, got {event:?}"),
        }
        assert!(
            timeout(Duration::from_secs(1), client.next_tagged_event())
                .await
                .expect("worker should exit after the retained terminal FIFO drains")
                .is_none()
        );
        server_task
            .await
            .expect("test server task should not panic");
    }

    #[tokio::test]
    async fn initialize_queues_preinit_events_and_keeps_server_requests_replyable() {
        let (client_io, server_io) = duplex(64 * 1024);
        let client_stream = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
        let mut server_stream =
            WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
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

            let preinit_notification = thread_scoped_server_message(
                thread_status_changed_notification(1),
                "preinit-subscription",
            );
            server_stream
                .send(Message::Text(
                    serde_json::to_string(&preinit_notification)
                        .expect("pre-initialize notification should serialize")
                        .into(),
                ))
                .await
                .expect("server should send pre-initialize notification");

            let preinit_request = thread_scoped_server_message(
                ServerRequest::ToolRequestUserInput {
                    request_id: RequestId::Integer(17),
                    params: ToolRequestUserInputParams {
                        thread_id: "thread".to_string(),
                        turn_id: "turn".to_string(),
                        item_id: "item".to_string(),
                        questions: Vec::new(),
                        auto_resolution_ms: None,
                    },
                },
                "preinit-subscription",
            );
            server_stream
                .send(Message::Text(
                    serde_json::to_string(&preinit_request)
                        .expect("pre-initialize request should serialize")
                        .into(),
                ))
                .await
                .expect("server should send pre-initialize request");

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

            let rejection = server_stream
                .next()
                .await
                .expect("client should answer the pre-initialize request")
                .expect("server request answer should succeed");
            let Message::Text(text) = rejection else {
                panic!("expected server request error text frame");
            };
            let JSONRPCMessage::Error(rejection) =
                serde_json::from_str(&text).expect("server request answer should contain JSON-RPC")
            else {
                panic!("expected server request rejection");
            };
            assert_eq!(rejection.id, RequestId::Integer(17));
            assert_eq!(rejection.error.code, -32000);
            assert_eq!(rejection.error.message, "pre-initialize request was rejected");

            match server_stream.next().await {
                Some(Ok(Message::Close(_))) | None => {}
                Some(Ok(message)) => panic!("expected client close, got {message:?}"),
                Some(Err(err)) => panic!("test transport should not fail: {err}"),
            }
        });

        let mut client = RemoteAppServerClient::connect_with_stream(
            /*channel_capacity*/ 1,
            "in-memory initialize event transport".to_string(),
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
        .expect("remote client should initialize after pre-initialize events");

        assert_eq!(client.server_version(), Some("1.0"));
        assert_eq!(
            client.codex_home(),
            Some("/tmp/codex-app-server-client-test")
        );
        match client.next_tagged_event().await {
            Some(TaggedAppServerEvent::ThreadServerNotification {
                thread_subscription_id,
                notification: ServerNotification::ThreadStatusChanged(notification),
            }) => {
                assert_eq!(thread_subscription_id, "preinit-subscription");
                assert_eq!(notification.status_revision, Some(1));
            }
            event => panic!("expected pre-initialize thread status, got {event:?}"),
        }
        match client.next_tagged_event().await {
            Some(TaggedAppServerEvent::ThreadServerRequest {
                thread_subscription_id,
                request: ServerRequest::ToolRequestUserInput { request_id, params },
            }) => {
                assert_eq!(thread_subscription_id, "preinit-subscription");
                assert_eq!(request_id, RequestId::Integer(17));
                assert_eq!(params.thread_id, "thread");
                assert_eq!(params.turn_id, "turn");
                assert_eq!(params.item_id, "item");
                assert!(params.questions.is_empty());
                assert_eq!(params.auto_resolution_ms, None);
            }
            event => panic!("expected pre-initialize thread request, got {event:?}"),
        }

        client
            .reject_server_request(
                RequestId::Integer(17),
                JSONRPCErrorError {
                    code: -32000,
                    message: "pre-initialize request was rejected".to_string(),
                    data: None,
                },
            )
            .await
            .expect("pre-initialize request should retain a rejectable JSON-RPC id");
        client
            .shutdown()
            .await
            .expect("client shutdown should complete");
        server_task
            .await
            .expect("test server task should not panic");
    }

    #[tokio::test]
    async fn delayed_initialize_event_flood_fails_with_a_bounded_backlog_error() {
        let (client_io, server_io) = duplex(64 * 1024);
        let client_stream = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
        let mut server_stream =
            WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
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

            for status_revision in 0..REMOTE_PENDING_NON_TERMINAL_EVENT_CAPACITY {
                let notification = thread_scoped_server_message(
                    thread_status_changed_notification(status_revision as u64),
                    "flood-subscription",
                );
                server_stream
                    .send(Message::Text(
                        serde_json::to_string(&notification)
                            .expect("pre-initialize notification should serialize")
                            .into(),
                    ))
                    .await
                    .expect("server should send pre-initialize notification");
            }

            let overflow_request = thread_scoped_server_message(
                ServerRequest::ToolRequestUserInput {
                    request_id: RequestId::Integer(99),
                    params: ToolRequestUserInputParams {
                        thread_id: "thread".to_string(),
                        turn_id: "turn".to_string(),
                        item_id: "item".to_string(),
                        questions: Vec::new(),
                        auto_resolution_ms: None,
                    },
                },
                "flood-subscription",
            );
            server_stream
                .send(Message::Text(
                    serde_json::to_string(&overflow_request)
                        .expect("overflow request should serialize")
                        .into(),
                ))
                .await
                .expect("server should send overflowing pre-initialize request");

            let rejection = timeout(Duration::from_secs(1), server_stream.next())
                .await
                .expect("client should promptly reject the overflowing server request")
                .expect("client should send an overflowing server request rejection")
                .expect("overflow rejection frame should succeed");
            let Message::Text(text) = rejection else {
                panic!("expected overflowing server request error text frame");
            };
            let JSONRPCMessage::Error(rejection) =
                serde_json::from_str(&text).expect("overflow rejection should contain JSON-RPC")
            else {
                panic!("expected overflowing server request rejection");
            };
            assert_eq!(rejection.id, RequestId::Integer(99));
            assert_eq!(rejection.error.code, -32000);
            assert!(
                rejection
                    .error
                    .message
                    .contains("bounded initialize event backlog")
            );
        });

        let err = timeout(
            Duration::from_secs(1),
            RemoteAppServerClient::connect_with_stream(
                /*channel_capacity*/ 1,
                "in-memory delayed initialize flood transport".to_string(),
                client_stream,
                InitializeParams {
                    client_info: ClientInfo {
                        name: "test-client".to_string(),
                        title: None,
                        version: "1.0".to_string(),
                    },
                    capabilities: None,
                },
            ),
        )
        .await
        .expect("bounded initialization should fail promptly")
        .err()
        .expect("pre-initialize flood should reject the remote connection");
        assert_eq!(err.kind(), ErrorKind::WouldBlock);
        assert!(
            err.to_string()
                .contains("exceeded the bounded initialize event backlog")
        );
        server_task
            .await
            .expect("test server task should not panic");
    }

    #[tokio::test]
    async fn pending_remote_fifo_keeps_best_effort_events_behind_required_events() {
        let (event_tx, mut event_rx) = mpsc::channel(1);
        event_tx
            .send(TaggedAppServerEvent::ServerNotification(
                thread_status_changed_notification(0),
            ))
            .await
            .expect("initial required event should fill the consumer queue");

        let mut skipped_events = 0usize;
        let mut pending_remote_events = VecDeque::with_capacity(REMOTE_PENDING_EVENT_CAPACITY);
        let mut worker_exit_error = None;
        let mut exit_after_pending_events = false;
        queue_remote_event(
            &event_tx,
            &mut skipped_events,
            &mut pending_remote_events,
            TaggedAppServerEvent::ServerNotification(thread_status_changed_notification(1)),
            /*exit_after_delivery*/ false,
            "test transport",
            &mut worker_exit_error,
            &mut exit_after_pending_events,
        )
        .expect("required event should be retained when the consumer queue is full");
        assert_eq!(pending_remote_events.len(), 1);

        match event_rx.recv().await {
            Some(TaggedAppServerEvent::ServerNotification(
                ServerNotification::ThreadStatusChanged(notification),
            )) => {
                assert_eq!(notification.status_revision, Some(0));
            }
            event => panic!("expected initial required status, got {event:?}"),
        }

        queue_remote_event(
            &event_tx,
            &mut skipped_events,
            &mut pending_remote_events,
            TaggedAppServerEvent::ServerNotification(command_execution_output_delta_notification(
                "progress",
            )),
            /*exit_after_delivery*/ false,
            "test transport",
            &mut worker_exit_error,
            &mut exit_after_pending_events,
        )
        .expect("best-effort event should join the pending FIFO");
        assert_eq!(pending_remote_events.len(), 2);
        assert!(matches!(
            event_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));

        event_tx
            .try_send(
                pending_remote_events
                    .pop_front()
                    .expect("retained required event should be first"),
            )
            .expect("required event should enqueue once capacity returns");
        match event_rx.recv().await {
            Some(TaggedAppServerEvent::ServerNotification(
                ServerNotification::ThreadStatusChanged(notification),
            )) => {
                assert_eq!(notification.status_revision, Some(1));
            }
            event => panic!("expected retained required status before progress, got {event:?}"),
        }

        event_tx
            .try_send(
                pending_remote_events
                    .pop_front()
                    .expect("queued best-effort event should follow the required event"),
            )
            .expect("best-effort event should enqueue after the required event");
        match event_rx.recv().await {
            Some(TaggedAppServerEvent::ServerNotification(
                ServerNotification::CommandExecutionOutputDelta(notification),
            )) => {
                assert_eq!(notification.delta, "progress");
            }
            event => panic!("expected best-effort progress after required status, got {event:?}"),
        }

        assert!(pending_remote_events.is_empty());
        assert_eq!(skipped_events, 0);
        assert!(worker_exit_error.is_none());
        assert!(!exit_after_pending_events);
    }

    #[tokio::test]
    async fn pending_remote_fifo_delivers_disconnected_after_retained_event() {
        let (event_tx, mut event_rx) = mpsc::channel(1);
        event_tx
            .send(TaggedAppServerEvent::ServerNotification(
                thread_status_changed_notification(0),
            ))
            .await
            .expect("initial required event should fill the consumer queue");

        let mut skipped_events = 0usize;
        let mut pending_remote_events = VecDeque::with_capacity(REMOTE_PENDING_EVENT_CAPACITY);
        let mut worker_exit_error = None;
        let mut exit_after_pending_events = false;
        queue_remote_event(
            &event_tx,
            &mut skipped_events,
            &mut pending_remote_events,
            TaggedAppServerEvent::ServerNotification(thread_status_changed_notification(1)),
            /*exit_after_delivery*/ false,
            "test transport",
            &mut worker_exit_error,
            &mut exit_after_pending_events,
        )
        .expect("required event should be retained when the consumer queue is full");
        match event_rx.recv().await {
            Some(TaggedAppServerEvent::ServerNotification(
                ServerNotification::ThreadStatusChanged(notification),
            )) => {
                assert_eq!(notification.status_revision, Some(0));
            }
            event => panic!("expected initial required status, got {event:?}"),
        }

        queue_remote_event(
            &event_tx,
            &mut skipped_events,
            &mut pending_remote_events,
            TaggedAppServerEvent::Disconnected {
                message: "remote transport failed".to_string(),
            },
            /*exit_after_delivery*/ true,
            "test transport",
            &mut worker_exit_error,
            &mut exit_after_pending_events,
        )
        .expect("terminal transport error should follow the retained event");
        assert_eq!(pending_remote_events.len(), 2);
        assert!(exit_after_pending_events);
        assert!(matches!(
            event_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));

        event_tx
            .try_send(
                pending_remote_events
                    .pop_front()
                    .expect("retained required event should precede disconnect"),
            )
            .expect("retained required event should enqueue once capacity returns");
        match event_rx.recv().await {
            Some(TaggedAppServerEvent::ServerNotification(
                ServerNotification::ThreadStatusChanged(notification),
            )) => {
                assert_eq!(notification.status_revision, Some(1));
            }
            event => panic!("expected retained event before disconnect, got {event:?}"),
        }

        event_tx
            .try_send(
                pending_remote_events
                    .pop_front()
                    .expect("disconnect should remain queued after the retained event"),
            )
            .expect("disconnect should enqueue after retained event");
        match event_rx.recv().await {
            Some(TaggedAppServerEvent::Disconnected { message }) => {
                assert_eq!(message, "remote transport failed");
            }
            event => panic!("expected explicit remote transport error, got {event:?}"),
        }

        assert!(pending_remote_events.is_empty());
        assert_eq!(skipped_events, 0);
        assert!(worker_exit_error.is_none());
    }

    #[tokio::test]
    async fn pending_remote_fifo_delivers_lag_before_disconnect() {
        let (event_tx, mut event_rx) = mpsc::channel(1);
        event_tx
            .send(TaggedAppServerEvent::ServerNotification(
                thread_status_changed_notification(0),
            ))
            .await
            .expect("initial required event should fill the consumer queue");

        let mut skipped_events = 0usize;
        let mut pending_remote_events = VecDeque::with_capacity(REMOTE_PENDING_EVENT_CAPACITY);
        let mut worker_exit_error = None;
        let mut exit_after_pending_events = false;
        queue_remote_event(
            &event_tx,
            &mut skipped_events,
            &mut pending_remote_events,
            TaggedAppServerEvent::ServerNotification(thread_status_changed_notification(1)),
            /*exit_after_delivery*/ false,
            "test transport",
            &mut worker_exit_error,
            &mut exit_after_pending_events,
        )
        .expect("required event should be retained when the consumer queue is full");
        assert!(matches!(
            event_rx.recv().await,
            Some(TaggedAppServerEvent::ServerNotification(
                ServerNotification::ThreadStatusChanged(_)
            ))
        ));

        skipped_events = 2;
        queue_remote_event(
            &event_tx,
            &mut skipped_events,
            &mut pending_remote_events,
            TaggedAppServerEvent::Disconnected {
                message: "remote transport failed".to_string(),
            },
            /*exit_after_delivery*/ true,
            "test transport",
            &mut worker_exit_error,
            &mut exit_after_pending_events,
        )
        .expect("terminal transport error should preserve skipped-event accounting");

        assert_eq!(pending_remote_events.len(), REMOTE_PENDING_EVENT_CAPACITY);
        assert_eq!(skipped_events, 0);
        assert!(exit_after_pending_events);
        assert!(matches!(
            pending_remote_events.pop_front(),
            Some(TaggedAppServerEvent::ServerNotification(
                ServerNotification::ThreadStatusChanged(notification)
            )) if notification.status_revision == Some(1)
        ));
        assert!(matches!(
            pending_remote_events.pop_front(),
            Some(TaggedAppServerEvent::Lagged { skipped: 2 })
        ));
        match pending_remote_events
            .pop_front()
            .expect("disconnect should follow the lag marker")
        {
            TaggedAppServerEvent::Disconnected { message } => {
                assert_eq!(message, "remote transport failed");
            }
            event => panic!("expected disconnect after lag marker, got {event:?}"),
        }
        assert!(pending_remote_events.is_empty());
    }

    #[tokio::test]
    async fn full_pending_remote_fifo_reports_skipped_events_in_disconnect() {
        let (event_tx, _event_rx) = mpsc::channel(1);
        event_tx
            .send(TaggedAppServerEvent::ServerNotification(
                thread_status_changed_notification(0),
            ))
            .await
            .expect("initial required event should fill the consumer queue");

        let mut skipped_events = 0usize;
        let mut pending_remote_events = VecDeque::with_capacity(REMOTE_PENDING_EVENT_CAPACITY);
        let mut worker_exit_error = None;
        let mut exit_after_pending_events = false;
        for status_revision in [1, 2] {
            queue_remote_event(
                &event_tx,
                &mut skipped_events,
                &mut pending_remote_events,
                TaggedAppServerEvent::ServerNotification(thread_status_changed_notification(
                    status_revision,
                )),
                /*exit_after_delivery*/ false,
                "test transport",
                &mut worker_exit_error,
                &mut exit_after_pending_events,
            )
            .expect("required event should remain in the bounded pending FIFO");
        }

        skipped_events = 3;
        queue_remote_event(
            &event_tx,
            &mut skipped_events,
            &mut pending_remote_events,
            TaggedAppServerEvent::Disconnected {
                message: "remote transport failed".to_string(),
            },
            /*exit_after_delivery*/ true,
            "test transport",
            &mut worker_exit_error,
            &mut exit_after_pending_events,
        )
        .expect("terminal transport error should preserve combined skipped-event accounting");

        assert_eq!(pending_remote_events.len(), REMOTE_PENDING_EVENT_CAPACITY);
        assert_eq!(skipped_events, 0);
        assert!(exit_after_pending_events);
        for status_revision in [1, 2] {
            assert!(matches!(
                pending_remote_events.pop_front(),
                Some(TaggedAppServerEvent::ServerNotification(
                    ServerNotification::ThreadStatusChanged(notification)
                )) if notification.status_revision == Some(status_revision)
            ));
        }
        match pending_remote_events
            .pop_front()
            .expect("combined terminal accounting should remain visible")
        {
            TaggedAppServerEvent::Disconnected { message } => {
                assert!(message.contains("3 best-effort remote event(s) were dropped"));
            }
            event => panic!("expected disconnect with combined accounting, got {event:?}"),
        }
        assert!(pending_remote_events.is_empty());
    }

    #[tokio::test]
    async fn required_event_overflow_reports_unreported_best_effort_loss() {
        let (event_tx, _event_rx) = mpsc::channel(1);
        event_tx
            .send(TaggedAppServerEvent::ServerNotification(
                thread_status_changed_notification(0),
            ))
            .await
            .expect("initial required event should fill the consumer queue");

        let mut skipped_events = 0usize;
        let mut pending_remote_events = VecDeque::with_capacity(REMOTE_PENDING_EVENT_CAPACITY);
        let mut worker_exit_error = None;
        let mut exit_after_pending_events = false;
        queue_remote_event(
            &event_tx,
            &mut skipped_events,
            &mut pending_remote_events,
            TaggedAppServerEvent::ServerNotification(thread_status_changed_notification(1)),
            /*exit_after_delivery*/ false,
            "test transport",
            &mut worker_exit_error,
            &mut exit_after_pending_events,
        )
        .expect("first required event should remain in the bounded pending FIFO");
        queue_remote_event(
            &event_tx,
            &mut skipped_events,
            &mut pending_remote_events,
            TaggedAppServerEvent::ServerNotification(command_execution_output_delta_notification(
                "queued progress",
            )),
            /*exit_after_delivery*/ false,
            "test transport",
            &mut worker_exit_error,
            &mut exit_after_pending_events,
        )
        .expect("first best-effort event should remain behind the retained required event");
        queue_remote_event(
            &event_tx,
            &mut skipped_events,
            &mut pending_remote_events,
            TaggedAppServerEvent::ServerNotification(command_execution_output_delta_notification(
                "dropped progress",
            )),
            /*exit_after_delivery*/ false,
            "test transport",
            &mut worker_exit_error,
            &mut exit_after_pending_events,
        )
        .expect("second best-effort event should be accounted for without growing the FIFO");
        assert_eq!(skipped_events, 1);

        queue_remote_event(
            &event_tx,
            &mut skipped_events,
            &mut pending_remote_events,
            TaggedAppServerEvent::ServerNotification(thread_status_changed_notification(2)),
            /*exit_after_delivery*/ false,
            "test transport",
            &mut worker_exit_error,
            &mut exit_after_pending_events,
        )
        .expect("required overflow should close through the reserved terminal slot");

        assert_eq!(pending_remote_events.len(), REMOTE_PENDING_EVENT_CAPACITY);
        assert_eq!(skipped_events, 0);
        assert!(exit_after_pending_events);
        assert!(matches!(
            pending_remote_events.pop_front(),
            Some(TaggedAppServerEvent::ServerNotification(
                ServerNotification::ThreadStatusChanged(notification)
            )) if notification.status_revision == Some(1)
        ));
        assert!(matches!(
            pending_remote_events.pop_front(),
            Some(TaggedAppServerEvent::ServerNotification(
                ServerNotification::CommandExecutionOutputDelta(notification)
            )) if notification.delta == "queued progress"
        ));
        match pending_remote_events
            .pop_front()
            .expect("terminal overload must preserve the accumulated loss count")
        {
            TaggedAppServerEvent::Disconnected { message } => {
                assert!(message.contains("1 best-effort remote event(s) were dropped"));
            }
            event => panic!("expected overload disconnect with loss accounting, got {event:?}"),
        }
        match worker_exit_error {
            Some((ErrorKind::WouldBlock, message)) => {
                assert!(message.contains("1 best-effort remote event(s) were dropped"));
            }
            error => panic!("expected overload error with loss accounting, got {error:?}"),
        }
    }

    #[tokio::test]
    async fn required_event_overflow_appends_an_explicit_disconnect() {
        let (event_tx, _event_rx) = mpsc::channel(1);
        event_tx
            .send(TaggedAppServerEvent::ServerNotification(
                thread_status_changed_notification(0),
            ))
            .await
            .expect("initial required event should fill the consumer queue");

        let mut skipped_events = 0usize;
        let mut pending_remote_events = VecDeque::with_capacity(REMOTE_PENDING_EVENT_CAPACITY);
        let mut worker_exit_error = None;
        let mut exit_after_pending_events = false;
        for status_revision in [1, 2, 3] {
            queue_remote_event(
                &event_tx,
                &mut skipped_events,
                &mut pending_remote_events,
                TaggedAppServerEvent::ServerNotification(thread_status_changed_notification(
                    status_revision,
                )),
                /*exit_after_delivery*/ false,
                "test transport",
                &mut worker_exit_error,
                &mut exit_after_pending_events,
            )
            .expect("bounded queue processing should not fail");
        }

        assert_eq!(pending_remote_events.len(), REMOTE_PENDING_EVENT_CAPACITY);
        assert!(exit_after_pending_events);
        match pending_remote_events
            .pop_front()
            .expect("first required event should remain queued")
        {
            TaggedAppServerEvent::ServerNotification(
                ServerNotification::ThreadStatusChanged(notification),
            ) => {
                assert_eq!(notification.status_revision, Some(1));
            }
            event => panic!("expected first retained required event, got {event:?}"),
        }
        match pending_remote_events
            .pop_front()
            .expect("second required event should remain queued")
        {
            TaggedAppServerEvent::ServerNotification(
                ServerNotification::ThreadStatusChanged(notification),
            ) => {
                assert_eq!(notification.status_revision, Some(2));
            }
            event => panic!("expected second retained required event, got {event:?}"),
        }
        match pending_remote_events
            .pop_front()
            .expect("bounded overload should append a terminal event")
        {
            TaggedAppServerEvent::Disconnected { message } => {
                assert!(message.contains("exceeded the bounded remote event backlog"));
            }
            event => panic!("expected explicit overload disconnect, got {event:?}"),
        }
        match worker_exit_error {
            Some((ErrorKind::WouldBlock, message)) => {
                assert!(message.contains("exceeded the bounded remote event backlog"));
            }
            error => panic!("expected explicit overload error, got {error:?}"),
        }
    }

    #[tokio::test]
    async fn terminal_thread_transition_stays_in_bounded_pending_queue_under_backpressure() {
        let (event_tx, mut event_rx) = mpsc::channel(1);
        event_tx
            .send(TaggedAppServerEvent::Lagged { skipped: 1 })
            .await
            .expect("initial event should enqueue");

        let mut skipped_events = 0usize;
        let RemoteEventDelivery::Pending(mut pending_events) = try_deliver_event(
            &event_tx,
            &mut skipped_events,
            TaggedAppServerEvent::ThreadServerNotification {
                thread_subscription_id: "thread-subscription".to_string(),
                notification: ServerNotification::ThreadArchived(
                    codex_app_server_protocol::ThreadArchivedNotification {
                        thread_id: "thread".to_string(),
                    },
                ),
            },
        )
        .expect("full consumer queue should retain the terminal transition")
        else {
            panic!("terminal thread transition must remain pending rather than be dropped");
        };
        assert_eq!(pending_events.len(), 1);
        assert!(matches!(
            event_rx.recv().await,
            Some(TaggedAppServerEvent::Lagged { skipped: 1 })
        ));
        event_tx
            .try_send(
                pending_events
                    .pop_front()
                    .expect("one retained terminal transition"),
            )
            .expect("retained terminal transition should enqueue once capacity returns");
        assert!(matches!(
            event_rx.recv().await,
            Some(TaggedAppServerEvent::ThreadServerNotification {
                thread_subscription_id,
                notification: ServerNotification::ThreadArchived(_),
            }) if thread_subscription_id == "thread-subscription"
        ));
        assert_eq!(skipped_events, 0);
    }

    #[tokio::test]
    async fn remote_queue_retains_goal_usage_and_name_state_under_backpressure() {
        for (notification, expected_kind) in [
            (thread_goal_updated_notification(), "goal"),
            (thread_token_usage_updated_notification(), "usage"),
            (thread_name_updated_notification(), "name"),
        ] {
            let (event_tx, mut event_rx) = mpsc::channel(1);
            event_tx
                .send(TaggedAppServerEvent::Lagged { skipped: 1 })
                .await
                .expect("initial event should enqueue");

            let mut skipped_events = 0usize;
            let RemoteEventDelivery::Pending(mut pending_events) = try_deliver_event(
                &event_tx,
                &mut skipped_events,
                TaggedAppServerEvent::ThreadServerNotification {
                    thread_subscription_id: "thread-subscription".to_string(),
                    notification,
                },
            )
            .expect("full consumer queue should retain required remote state")
            else {
                panic!("{expected_kind} state must remain pending rather than be dropped");
            };
            assert_eq!(pending_events.len(), 1);
            assert!(matches!(
                event_rx.recv().await,
                Some(TaggedAppServerEvent::Lagged { skipped: 1 })
            ));
            event_tx
                .try_send(
                    pending_events
                        .pop_front()
                        .expect("one retained required remote state"),
                )
                .expect("retained required remote state should enqueue once capacity returns");
            match event_rx.recv().await {
                Some(TaggedAppServerEvent::ThreadServerNotification {
                    thread_subscription_id,
                    notification: ServerNotification::ThreadGoalUpdated(_),
                }) if expected_kind == "goal" => {
                    assert_eq!(thread_subscription_id, "thread-subscription");
                }
                Some(TaggedAppServerEvent::ThreadServerNotification {
                    thread_subscription_id,
                    notification: ServerNotification::ThreadTokenUsageUpdated(_),
                }) if expected_kind == "usage" => {
                    assert_eq!(thread_subscription_id, "thread-subscription");
                }
                Some(TaggedAppServerEvent::ThreadServerNotification {
                    thread_subscription_id,
                    notification: ServerNotification::ThreadNameUpdated(_),
                }) if expected_kind == "name" => {
                    assert_eq!(thread_subscription_id, "thread-subscription");
                }
                event => panic!("expected retained {expected_kind} state, got {event:?}"),
            }
            assert_eq!(skipped_events, 0);
        }
    }

    #[tokio::test]
    async fn shutdown_command_acknowledges_after_event_receiver_closes_with_pending_fifo() {
        let (client_io, server_io) = duplex(64 * 1024);
        let client_stream = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
        let mut server_stream =
            WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
        let (events_sent_tx, events_sent_rx) = oneshot::channel();
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
            let initialize_response = JSONRPCMessage::Response(JSONRPCResponse {
                id: initialize.id,
                result: serde_json::json!({"userAgent": "test-server/1.0"}),
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

            for status_revision in [1, 2, 3] {
                let event = thread_scoped_server_message(
                    thread_status_changed_notification(status_revision),
                    "shutdown-subscription",
                );
                server_stream
                    .send(Message::Text(
                        serde_json::to_string(&event)
                            .expect("status notification should serialize")
                            .into(),
                    ))
                    .await
                    .expect("server should send required status");
            }
            let _ = events_sent_tx.send(());

            let close = timeout(Duration::from_secs(1), server_stream.next())
                .await
                .expect("client shutdown should promptly close the transport")
                .expect("client shutdown should send a close frame")
                .expect("close frame should succeed");
            assert!(matches!(close, Message::Close(_)));
        });

        let client = RemoteAppServerClient::connect_with_stream(
            /*channel_capacity*/ 1,
            "in-memory shutdown pending FIFO transport".to_string(),
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
        events_sent_rx
            .await
            .expect("server should send the retained required events");
        timeout(Duration::from_secs(1), async {
            while client.event_rx.len() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first required event should fill the one-slot consumer queue");
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        let RemoteAppServerClient {
            command_tx,
            event_rx,
            pending_events: _,
            server_version: _,
            codex_home: _,
            worker_handle,
        } = client;
        drop(event_rx);
        let (response_tx, response_rx) = oneshot::channel();
        command_tx
            .send(RemoteClientCommand::Shutdown { response_tx })
            .await
            .expect("shutdown command should reach the remote worker");
        timeout(Duration::from_secs(1), response_rx)
            .await
            .expect("closed event receiver must not delay shutdown acknowledgement")
            .expect("remote worker must acknowledge the queued shutdown command")
            .expect("remote close should succeed");
        timeout(Duration::from_secs(1), worker_handle)
            .await
            .expect("worker should exit after acknowledging shutdown")
            .expect("worker should not panic during shutdown");
        server_task
            .await
            .expect("test server task should not panic");
    }

    #[tokio::test]
    async fn shutdown_tolerates_worker_exit_after_command_is_queued() {
        let (command_tx, mut command_rx) = mpsc::channel(1);
        let (_event_tx, event_rx) = mpsc::channel::<TaggedAppServerEvent>(1);
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
