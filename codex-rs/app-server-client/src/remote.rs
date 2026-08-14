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
const MAX_UNANSWERED_SERVER_REQUESTS: usize = 1024;
const MAX_UNANSWERED_SERVER_REQUEST_ID_BYTES: usize = 256 << 10;
const MAX_UNANSWERED_SERVER_REQUEST_BYTES: usize = 16 << 20;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerRequestDisposition {
    Pending,
    ResponseAttempted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerRequestRegistration {
    Registered,
    Duplicate,
    CapacityExceeded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ServerRequestLedgerEntry {
    disposition: ServerRequestDisposition,
    request_id_bytes: usize,
    request_bytes: usize,
}

#[derive(Debug, Default)]
struct ServerRequestLedger {
    entries: HashMap<RequestId, ServerRequestLedgerEntry>,
    order: VecDeque<RequestId>,
    request_id_bytes: usize,
    request_bytes: usize,
}

impl ServerRequestLedger {
    fn register(
        &mut self,
        request_id: RequestId,
        request_bytes: usize,
    ) -> ServerRequestRegistration {
        if self.entries.contains_key(&request_id) {
            return ServerRequestRegistration::Duplicate;
        }
        let request_id_bytes = match &request_id {
            RequestId::String(value) => value.len(),
            RequestId::Integer(_) => std::mem::size_of::<i64>(),
        };
        let Some(next_request_id_bytes) = self.request_id_bytes.checked_add(request_id_bytes)
        else {
            return ServerRequestRegistration::CapacityExceeded;
        };
        let Some(next_request_bytes) = self.request_bytes.checked_add(request_bytes) else {
            return ServerRequestRegistration::CapacityExceeded;
        };
        if self.entries.len() >= MAX_UNANSWERED_SERVER_REQUESTS
            || next_request_id_bytes > MAX_UNANSWERED_SERVER_REQUEST_ID_BYTES
            || next_request_bytes > MAX_UNANSWERED_SERVER_REQUEST_BYTES
        {
            return ServerRequestRegistration::CapacityExceeded;
        }
        self.entries.insert(
            request_id.clone(),
            ServerRequestLedgerEntry {
                disposition: ServerRequestDisposition::Pending,
                request_id_bytes,
                request_bytes,
            },
        );
        self.order.push_back(request_id);
        self.request_id_bytes = next_request_id_bytes;
        self.request_bytes = next_request_bytes;
        ServerRequestRegistration::Registered
    }

    fn begin_response(&mut self, request_id: &RequestId) -> bool {
        let Some(entry) = self.entries.get_mut(request_id) else {
            return false;
        };
        if entry.disposition == ServerRequestDisposition::ResponseAttempted {
            return false;
        }
        // A failed socket write might still have reached the peer. Retain the
        // attempted disposition so terminal cleanup never answers it twice.
        entry.disposition = ServerRequestDisposition::ResponseAttempted;
        true
    }

    fn complete_response(&mut self, request_id: &RequestId) {
        if let Some(entry) = self.entries.remove(request_id) {
            self.request_id_bytes -= entry.request_id_bytes;
            self.request_bytes -= entry.request_bytes;
            self.order
                .retain(|pending_request_id| pending_request_id != request_id);
        }
    }

    fn take_unanswered(&mut self) -> Vec<RequestId> {
        let mut unanswered = Vec::new();
        while let Some(request_id) = self.order.pop_front() {
            match self.entries.remove(&request_id) {
                Some(ServerRequestLedgerEntry {
                    disposition: ServerRequestDisposition::Pending,
                    ..
                }) => unanswered.push(request_id),
                Some(ServerRequestLedgerEntry {
                    disposition: ServerRequestDisposition::ResponseAttempted,
                    ..
                })
                | None => {}
            }
        }
        self.request_id_bytes = 0;
        self.request_bytes = 0;
        debug_assert!(self.entries.is_empty());
        unanswered
    }
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
        let mut server_requests = ServerRequestLedger::default();
        let (pending_events, server_version, codex_home) = initialize_remote_connection(
            &mut stream,
            &endpoint,
            initialize_params,
            INITIALIZE_TIMEOUT,
            &mut server_requests,
        )
        .await?;

        let (command_tx, mut command_rx) = mpsc::channel::<RemoteClientCommand>(channel_capacity);
        let (event_tx, event_rx) = mpsc::channel::<AppServerEvent>(channel_capacity);
        let worker_handle = tokio::spawn(async move {
            let mut pending_requests =
                HashMap::<RequestId, oneshot::Sender<IoResult<RequestResult>>>::new();
            let mut worker_exit_error: Option<(ErrorKind, String)> = None;
            let mut skipped_events = 0usize;
            let mut pending_required_events = VecDeque::new();
            loop {
                tokio::select! {
                    command = command_rx.recv() => {
                        let Some(command) = command else {
                            if let Err(err) = reject_unanswered_server_requests(
                                &mut stream,
                                &endpoint,
                                server_requests.take_unanswered(),
                            )
                            .await
                            {
                                warn!(%err, "failed to reject unanswered remote app-server server request");
                            }
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
                                        &mut pending_required_events,
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
                                if !server_requests.begin_response(&request_id) {
                                    let _ = response_tx.send(Err(IoError::new(
                                        ErrorKind::InvalidInput,
                                        format!("remote app-server server request id `{request_id}` is not pending"),
                                    )));
                                    continue;
                                }
                                let result = write_jsonrpc_message(
                                    &mut stream,
                                    JSONRPCMessage::Response(JSONRPCResponse {
                                        id: request_id.clone(),
                                        result,
                                    }),
                                    &endpoint,
                                )
                                .await;
                                if result.is_ok() {
                                    server_requests.complete_response(&request_id);
                                }
                                let _ = response_tx.send(result);
                            }
                            RemoteClientCommand::RejectServerRequest {
                                request_id,
                                error,
                                response_tx,
                            } => {
                                if !server_requests.begin_response(&request_id) {
                                    let _ = response_tx.send(Err(IoError::new(
                                        ErrorKind::InvalidInput,
                                        format!("remote app-server server request id `{request_id}` is not pending"),
                                    )));
                                    continue;
                                }
                                let result = write_jsonrpc_message(
                                    &mut stream,
                                    JSONRPCMessage::Error(JSONRPCError {
                                        error,
                                        id: request_id.clone(),
                                    }),
                                    &endpoint,
                                )
                                .await;
                                if result.is_ok() {
                                    server_requests.complete_response(&request_id);
                                }
                                let _ = response_tx.send(result);
                            }
                            RemoteClientCommand::Shutdown { response_tx } => {
                                let rejection_result = reject_unanswered_server_requests(
                                    &mut stream,
                                    &endpoint,
                                    server_requests.take_unanswered(),
                                )
                                .await;
                                let close_result = stream.close(None).await.or_else(|err| {
                                    if websocket_close_error_is_already_closed(&err) {
                                        Ok(())
                                    } else {
                                        Err(IoError::other(format!(
                                            "failed to close websocket app server `{endpoint}`: {err}"
                                        )))
                                    }
                                });
                                let _ = response_tx.send(rejection_result.and(close_result));
                                break;
                            }
                        }
                    }
                    permit = event_tx.reserve(), if !pending_required_events.is_empty() => {
                        match permit {
                            Ok(permit) => {
                                if let Some(event) = pending_required_events.pop_front() {
                                    permit.send(event);
                                }
                            }
                            Err(_) => {
                                pending_required_events.clear();
                            }
                        }
                    }
                    message = stream.next(), if pending_required_events.is_empty() => {
                        match message {
                            Some(Ok(Message::Text(text))) => {
                                let message_bytes = text.len();
                                match serde_json::from_str::<JSONRPCMessage>(&text) {
                                    Ok(JSONRPCMessage::Response(response)) => {
                                        if let Some(response_tx) = pending_requests.remove(&response.id) {
                                            let _ = response_tx.send(Ok(Ok(response.result)));
                                        }
                                    }
                                    Ok(JSONRPCMessage::Error(error)) => {
                                        if let Some(response_tx) = pending_requests.remove(&error.id) {
                                            let _ = response_tx.send(Ok(Err(error.error)));
                                        }
                                    }
                                    Ok(JSONRPCMessage::Notification(notification)) => {
                                        if let Some(event) =
                                            app_server_event_from_notification(notification)
                                            && let Err(err) = deliver_event(
                                                &event_tx,
                                                &mut skipped_events,
                                                &mut pending_required_events,
                                                event,
                                            )
                                            .await
                                            {
                                                warn!(%err, "failed to deliver remote app-server event");
                                                // The event receiver is dropped before shutdown
                                                // is queued. Keep servicing commands so shutdown
                                                // can close the socket and receive an explicit
                                                // acknowledgement.
                                                continue;
                                            }
                                    }
                                    Ok(JSONRPCMessage::Request(request)) => {
                                        let request_id = request.id.clone();
                                        let method = request.method.clone();
                                        match ServerRequest::try_from(request) {
                                            Ok(request) => {
                                                match server_requests.register(
                                                    request_id.clone(),
                                                    message_bytes,
                                                ) {
                                                    ServerRequestRegistration::Registered => {}
                                                    ServerRequestRegistration::Duplicate => {
                                                        warn!(%request_id, "ignoring duplicate remote app-server server request");
                                                        continue;
                                                    }
                                                    ServerRequestRegistration::CapacityExceeded => {
                                                        warn!(%request_id, "rejecting remote app-server server request because the unanswered-request limit was reached");
                                                        if let Err(err) = reject_server_request_overflow(
                                                            &mut stream,
                                                            &endpoint,
                                                            request_id,
                                                        )
                                                        .await
                                                        {
                                                            worker_exit_error = Some((ErrorKind::BrokenPipe, err.to_string()));
                                                            break;
                                                        }
                                                        continue;
                                                    }
                                                }
                                                if let Err(err) = deliver_event(
                                                    &event_tx,
                                                    &mut skipped_events,
                                                    &mut pending_required_events,
                                                    AppServerEvent::ServerRequest(request),
                                                )
                                                .await
                                                {
                                                    warn!(%err, "failed to deliver remote app-server server request");
                                                    // Shutdown drops the event receiver before
                                                    // queueing its command. Continue so the worker
                                                    // can reject retained requests, close the
                                                    // transport, and acknowledge that command.
                                                    continue;
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
                                                        &mut pending_required_events,
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
                                    Err(err) => {
                                        let message = format!(
                                            "remote app server at `{endpoint}` sent invalid JSON-RPC: {err}"
                                        );
                                        if let Err(deliver_err) = deliver_event(
                                            &event_tx,
                                            &mut skipped_events,
                                            &mut pending_required_events,
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
                                    &mut pending_required_events,
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
                                    &mut pending_required_events,
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
                                    &mut pending_required_events,
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

            if worker_exit_error.is_some() {
                while let Some(event) = pending_required_events.pop_front() {
                    if event_tx.send(event).await.is_err() {
                        break;
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
    server_requests: &mut ServerRequestLedger,
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
                    let message_bytes = text.len();
                    let message = serde_json::from_str::<JSONRPCMessage>(&text).map_err(|err| {
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
                            if let Some(event) = app_server_event_from_notification(notification) {
                                pending_events.push(event);
                            }
                        }
                        JSONRPCMessage::Request(request) => {
                            let request_id = request.id.clone();
                            let method = request.method.clone();
                            match ServerRequest::try_from(request) {
                                Ok(request) => {
                                    match server_requests.register(
                                        request_id.clone(),
                                        message_bytes,
                                    ) {
                                        ServerRequestRegistration::Registered => {
                                            pending_events
                                                .push(AppServerEvent::ServerRequest(request));
                                        }
                                        ServerRequestRegistration::Duplicate => {
                                            warn!(%request_id, "ignoring duplicate remote app-server server request during initialize");
                                        }
                                        ServerRequestRegistration::CapacityExceeded => {
                                            warn!(%request_id, "rejecting remote app-server server request during initialize because the unanswered-request limit was reached");
                                            reject_server_request_overflow(
                                                stream,
                                                endpoint,
                                                request_id,
                                            )
                                            .await?;
                                        }
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

fn app_server_event_from_notification(notification: JSONRPCNotification) -> Option<AppServerEvent> {
    match ServerNotification::try_from(notification) {
        Ok(notification) => Some(AppServerEvent::ServerNotification(notification)),
        Err(_) => None,
    }
}

async fn deliver_event(
    event_tx: &mpsc::Sender<AppServerEvent>,
    skipped_events: &mut usize,
    pending_required_events: &mut VecDeque<AppServerEvent>,
    event: AppServerEvent,
) -> IoResult<()> {
    if *skipped_events > 0 {
        if remote_event_requires_delivery(&event) {
            let lagged = AppServerEvent::Lagged {
                skipped: *skipped_events,
            };
            match event_tx.try_send(lagged) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(lagged)) => {
                    pending_required_events.push_back(lagged);
                    pending_required_events.push_back(event);
                    *skipped_events = 0;
                    return Ok(());
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    return Err(event_consumer_closed());
                }
            }
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
        match event_tx.try_send(event) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(event)) => {
                pending_required_events.push_back(event);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(event_consumer_closed()),
        }
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
        AppServerEvent::ServerNotification(notification) => {
            server_notification_requires_delivery(notification)
        }
        AppServerEvent::ServerRequest(_) | AppServerEvent::Disconnected { .. } => true,
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

async fn reject_unanswered_server_requests<S>(
    stream: &mut WebSocketStream<S>,
    endpoint: &str,
    request_ids: Vec<RequestId>,
) -> IoResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    for request_id in request_ids {
        write_jsonrpc_message(
            stream,
            JSONRPCMessage::Error(JSONRPCError {
                error: JSONRPCErrorError {
                    code: -32603,
                    message: "remote app-server client stopped before answering server request"
                        .to_string(),
                    data: None,
                },
                id: request_id,
            }),
            endpoint,
        )
        .await?;
    }
    Ok(())
}

async fn reject_server_request_overflow<S>(
    stream: &mut WebSocketStream<S>,
    endpoint: &str,
    request_id: RequestId,
) -> IoResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_jsonrpc_message(
        stream,
        JSONRPCMessage::Error(JSONRPCError {
            error: JSONRPCErrorError {
                code: -32603,
                message: "too many unanswered remote app-server server requests".to_string(),
                data: None,
            },
            id: request_id,
        }),
        endpoint,
    )
    .await
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

    fn server_request(request_id: i64) -> JSONRPCRequest {
        JSONRPCRequest {
            id: RequestId::Integer(request_id),
            method: "item/tool/requestUserInput".to_string(),
            params: Some(
                serde_json::to_value(codex_app_server_protocol::ToolRequestUserInputParams {
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    item_id: format!("call-{request_id}"),
                    questions: vec![codex_app_server_protocol::ToolRequestUserInputQuestion {
                        id: "question-1".to_string(),
                        header: "Mode".to_string(),
                        question: "Pick one".to_string(),
                        is_other: false,
                        is_secret: false,
                        options: Some(vec![]),
                    }],
                    is_blocking: true,
                    auto_resolution_ms: None,
                })
                .expect("request params should serialize"),
            ),
            trace: None,
        }
    }

    fn dynamic_tool_request(request_id: i64, argument_bytes: usize) -> JSONRPCRequest {
        JSONRPCRequest {
            id: RequestId::Integer(request_id),
            method: "item/tool/call".to_string(),
            params: Some(
                serde_json::to_value(codex_app_server_protocol::DynamicToolCallParams {
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    call_id: format!("call-{request_id}"),
                    namespace: None,
                    tool: "test-tool".to_string(),
                    arguments: serde_json::json!({"payload": "x".repeat(argument_bytes)}),
                })
                .expect("dynamic tool request params should serialize"),
            ),
            trace: None,
        }
    }

    fn test_initialize_params() -> InitializeParams {
        InitializeParams {
            client_info: ClientInfo {
                name: "test-client".to_string(),
                title: None,
                version: "1.0".to_string(),
            },
            capabilities: None,
        }
    }

    #[test]
    fn response_attempt_is_not_rejected_during_shutdown_cleanup() {
        let request_id = RequestId::Integer(1);
        let mut ledger = ServerRequestLedger::default();
        assert_eq!(
            ledger.register(request_id.clone(), 1),
            ServerRequestRegistration::Registered
        );
        assert!(ledger.begin_response(&request_id));
        assert!(!ledger.begin_response(&request_id));
        assert!(ledger.take_unanswered().is_empty());
    }

    #[test]
    fn unanswered_server_request_ledger_limits_count() {
        let mut ledger = ServerRequestLedger::default();
        for request_id in 0..MAX_UNANSWERED_SERVER_REQUESTS {
            assert_eq!(
                ledger.register(RequestId::Integer(request_id as i64), 1),
                ServerRequestRegistration::Registered
            );
        }
        assert_eq!(
            ledger.register(RequestId::Integer(MAX_UNANSWERED_SERVER_REQUESTS as i64), 1,),
            ServerRequestRegistration::CapacityExceeded
        );
        assert_eq!(
            ledger.take_unanswered().len(),
            MAX_UNANSWERED_SERVER_REQUESTS
        );
        assert_eq!(ledger.request_id_bytes, 0);
        assert_eq!(ledger.request_bytes, 0);
    }

    #[test]
    fn unanswered_server_request_ledger_limits_payload_bytes() {
        let mut ledger = ServerRequestLedger::default();
        let first_request_id = RequestId::Integer(1);
        assert_eq!(
            ledger.register(
                first_request_id.clone(),
                MAX_UNANSWERED_SERVER_REQUEST_BYTES,
            ),
            ServerRequestRegistration::Registered
        );
        assert_eq!(
            ledger.register(RequestId::Integer(2), 1),
            ServerRequestRegistration::CapacityExceeded
        );

        assert!(ledger.begin_response(&first_request_id));
        assert_eq!(ledger.request_bytes, 0);
        assert_eq!(
            ledger.register(RequestId::Integer(2), 1),
            ServerRequestRegistration::Registered
        );
    }

    #[test]
    fn unanswered_server_request_ledger_counts_string_bytes() {
        let mut ledger = ServerRequestLedger::default();
        let nearly_full = "a".repeat(MAX_UNANSWERED_SERVER_REQUEST_ID_BYTES - 1);
        assert_eq!(
            ledger.register(RequestId::String(nearly_full), 1),
            ServerRequestRegistration::Registered
        );
        assert_eq!(
            ledger.register(RequestId::String("é".to_string()), 1),
            ServerRequestRegistration::CapacityExceeded
        );
    }

    #[tokio::test]
    async fn initialize_rejects_server_requests_beyond_the_count_limit() {
        let (client_io, server_io) = tokio::io::duplex(4 << 20);
        let mut client_stream = WebSocketStream::from_raw_socket(
            client_io,
            tokio_tungstenite::tungstenite::protocol::Role::Client,
            None,
        )
        .await;
        let mut server_stream = WebSocketStream::from_raw_socket(
            server_io,
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        )
        .await;
        let server_task = tokio::spawn(async move {
            let Message::Text(initialize) = server_stream
                .next()
                .await
                .expect("client should initialize")
                .expect("initialize frame should succeed")
            else {
                panic!("expected initialize text frame");
            };
            let JSONRPCMessage::Request(initialize) = serde_json::from_str(&initialize)
                .expect("initialize frame should contain JSON-RPC")
            else {
                panic!("expected initialize request");
            };

            for request_id in 0..=MAX_UNANSWERED_SERVER_REQUESTS {
                server_stream
                    .send(Message::Text(
                        serde_json::to_string(&JSONRPCMessage::Request(server_request(
                            request_id as i64,
                        )))
                        .expect("server request should serialize")
                        .into(),
                    ))
                    .await
                    .expect("server request should send");
            }
            server_stream
                .send(Message::Text(
                    serde_json::to_string(&JSONRPCMessage::Response(JSONRPCResponse {
                        id: initialize.id,
                        result: serde_json::json!({"userAgent": "test-server/1.0"}),
                    }))
                    .expect("initialize response should serialize")
                    .into(),
                ))
                .await
                .expect("server should acknowledge initialize");

            let Message::Text(rejection) = server_stream
                .next()
                .await
                .expect("client should reject the overflow request")
                .expect("rejection frame should succeed")
            else {
                panic!("expected rejection text frame");
            };
            let JSONRPCMessage::Error(rejection) =
                serde_json::from_str(&rejection).expect("rejection frame should contain JSON-RPC")
            else {
                panic!("expected JSON-RPC error");
            };
            assert_eq!(
                (rejection.id, rejection.error.code),
                (
                    RequestId::Integer(MAX_UNANSWERED_SERVER_REQUESTS as i64),
                    -32603,
                )
            );
            let _initialized = server_stream
                .next()
                .await
                .expect("client should send initialized")
                .expect("initialized frame should succeed");
        });

        let mut ledger = ServerRequestLedger::default();
        let (pending_events, _, _) = initialize_remote_connection(
            &mut client_stream,
            "in-memory count-limit transport",
            test_initialize_params(),
            Duration::from_secs(2),
            &mut ledger,
        )
        .await
        .expect("remote client should initialize");
        assert_eq!(pending_events.len(), MAX_UNANSWERED_SERVER_REQUESTS);
        assert_eq!(ledger.entries.len(), MAX_UNANSWERED_SERVER_REQUESTS);
        server_task.await.expect("server task should not panic");
    }

    #[tokio::test]
    async fn initialize_rejects_oversized_dynamic_tool_request() {
        let (client_io, server_io) = tokio::io::duplex(32 << 20);
        let mut client_stream = WebSocketStream::from_raw_socket(
            client_io,
            tokio_tungstenite::tungstenite::protocol::Role::Client,
            None,
        )
        .await;
        let mut server_stream = WebSocketStream::from_raw_socket(
            server_io,
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        )
        .await;
        let server_task = tokio::spawn(async move {
            let Message::Text(initialize) = server_stream
                .next()
                .await
                .expect("client should initialize")
                .expect("initialize frame should succeed")
            else {
                panic!("expected initialize text frame");
            };
            let JSONRPCMessage::Request(initialize) = serde_json::from_str(&initialize)
                .expect("initialize frame should contain JSON-RPC")
            else {
                panic!("expected initialize request");
            };
            server_stream
                .send(Message::Text(
                    serde_json::to_string(&JSONRPCMessage::Request(dynamic_tool_request(
                        7,
                        MAX_UNANSWERED_SERVER_REQUEST_BYTES,
                    )))
                    .expect("dynamic tool request should serialize")
                    .into(),
                ))
                .await
                .expect("dynamic tool request should send");
            server_stream
                .send(Message::Text(
                    serde_json::to_string(&JSONRPCMessage::Response(JSONRPCResponse {
                        id: initialize.id,
                        result: serde_json::json!({"userAgent": "test-server/1.0"}),
                    }))
                    .expect("initialize response should serialize")
                    .into(),
                ))
                .await
                .expect("server should acknowledge initialize");

            let Message::Text(rejection) = server_stream
                .next()
                .await
                .expect("client should reject the oversized request")
                .expect("rejection frame should succeed")
            else {
                panic!("expected rejection text frame");
            };
            let JSONRPCMessage::Error(rejection) =
                serde_json::from_str(&rejection).expect("rejection frame should contain JSON-RPC")
            else {
                panic!("expected JSON-RPC error");
            };
            assert_eq!(
                (rejection.id, rejection.error.code),
                (RequestId::Integer(7), -32603)
            );
            let _initialized = server_stream
                .next()
                .await
                .expect("client should send initialized")
                .expect("initialized frame should succeed");
        });

        let mut ledger = ServerRequestLedger::default();
        let (pending_events, _, _) = initialize_remote_connection(
            &mut client_stream,
            "in-memory payload-limit transport",
            test_initialize_params(),
            Duration::from_secs(2),
            &mut ledger,
        )
        .await
        .expect("remote client should initialize");
        assert!(pending_events.is_empty());
        assert!(ledger.entries.is_empty());
        assert_eq!(ledger.request_bytes, 0);
        server_task.await.expect("server task should not panic");
    }

    #[tokio::test]
    async fn preinitialize_server_requests_resolve_and_reject_on_shutdown() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let client_stream = WebSocketStream::from_raw_socket(
            client_io,
            tokio_tungstenite::tungstenite::protocol::Role::Client,
            None,
        )
        .await;
        let mut server_stream = WebSocketStream::from_raw_socket(
            server_io,
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        )
        .await;
        let server_task = tokio::spawn(async move {
            let Message::Text(initialize) = server_stream
                .next()
                .await
                .expect("client should initialize")
                .expect("initialize frame should succeed")
            else {
                panic!("expected initialize text frame");
            };
            let JSONRPCMessage::Request(initialize) = serde_json::from_str(&initialize)
                .expect("initialize frame should contain JSON-RPC")
            else {
                panic!("expected initialize request");
            };
            for request_id in [1, 2] {
                server_stream
                    .send(Message::Text(
                        serde_json::to_string(&JSONRPCMessage::Request(server_request(request_id)))
                            .expect("server request should serialize")
                            .into(),
                    ))
                    .await
                    .expect("server request should send");
            }
            server_stream
                .send(Message::Text(
                    serde_json::to_string(&JSONRPCMessage::Response(JSONRPCResponse {
                        id: initialize.id,
                        result: serde_json::json!({"userAgent": "test-server/1.0"}),
                    }))
                    .expect("initialize response should serialize")
                    .into(),
                ))
                .await
                .expect("server should acknowledge initialize");
            let _initialized = server_stream
                .next()
                .await
                .expect("client should send initialized")
                .expect("initialized frame should succeed");

            let Message::Text(response) = server_stream
                .next()
                .await
                .expect("client should resolve the admitted request")
                .expect("response frame should succeed")
            else {
                panic!("expected response text frame");
            };
            let JSONRPCMessage::Response(response) =
                serde_json::from_str(&response).expect("response frame should contain JSON-RPC")
            else {
                panic!("expected JSON-RPC response");
            };
            assert_eq!(response.id, RequestId::Integer(1));

            let Message::Text(rejection) = server_stream
                .next()
                .await
                .expect("shutdown should reject the remaining request")
                .expect("rejection frame should succeed")
            else {
                panic!("expected rejection text frame");
            };
            let JSONRPCMessage::Error(rejection) =
                serde_json::from_str(&rejection).expect("rejection frame should contain JSON-RPC")
            else {
                panic!("expected JSON-RPC error");
            };
            assert_eq!(
                (rejection.id, rejection.error.code),
                (RequestId::Integer(2), -32603)
            );
        });

        let mut client = RemoteAppServerClient::connect_with_stream(
            /*channel_capacity*/ 1,
            "in-memory preinitialize lifecycle transport".to_string(),
            client_stream,
            test_initialize_params(),
        )
        .await
        .expect("remote client should initialize");
        for expected_index in 1..=2 {
            assert!(
                matches!(
                    client.next_event().await,
                    Some(AppServerEvent::ServerRequest(_))
                ),
                "expected preinitialize server request {expected_index}"
            );
        }
        client
            .resolve_server_request(RequestId::Integer(1), serde_json::json!({}))
            .await
            .expect("admitted preinitialize request should resolve");
        client
            .shutdown()
            .await
            .expect("shutdown should reject the unanswered request");
        server_task.await.expect("server task should not panic");
    }

    #[tokio::test]
    async fn control_commands_remain_live_while_required_event_queue_is_full() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let client_stream = WebSocketStream::from_raw_socket(
            client_io,
            tokio_tungstenite::tungstenite::protocol::Role::Client,
            None,
        )
        .await;
        let mut server_stream = WebSocketStream::from_raw_socket(
            server_io,
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        )
        .await;
        let (requests_sent_tx, requests_sent_rx) = oneshot::channel();
        let server_task = tokio::spawn(async move {
            let Message::Text(initialize) = server_stream
                .next()
                .await
                .expect("client should initialize")
                .expect("initialize frame should succeed")
            else {
                panic!("expected initialize text frame");
            };
            let JSONRPCMessage::Request(initialize) = serde_json::from_str(&initialize)
                .expect("initialize frame should contain JSON-RPC")
            else {
                panic!("expected initialize request");
            };
            let response = JSONRPCMessage::Response(JSONRPCResponse {
                id: initialize.id,
                result: serde_json::json!({"userAgent": "test-server/1.0"}),
            });
            server_stream
                .send(Message::Text(
                    serde_json::to_string(&response)
                        .expect("initialize response should serialize")
                        .into(),
                ))
                .await
                .expect("server should acknowledge initialize");
            let _initialized = server_stream
                .next()
                .await
                .expect("client should send initialized")
                .expect("initialized frame should succeed");
            for request_id in [1, 2] {
                server_stream
                    .send(Message::Text(
                        serde_json::to_string(&JSONRPCMessage::Request(server_request(request_id)))
                            .expect("server request should serialize")
                            .into(),
                    ))
                    .await
                    .expect("server request should send");
            }
            requests_sent_tx
                .send(())
                .expect("request send signal should remain open");

            let Message::Text(message) = server_stream
                .next()
                .await
                .expect("client should resolve the first request")
                .expect("response frame should succeed")
            else {
                panic!("expected response text frame");
            };
            let JSONRPCMessage::Response(response) =
                serde_json::from_str(&message).expect("response frame should contain JSON-RPC")
            else {
                panic!("expected JSON-RPC response");
            };
            assert_eq!(response.id, RequestId::Integer(1));

            let Message::Text(message) = server_stream
                .next()
                .await
                .expect("client should reject the remaining unanswered request")
                .expect("rejection frame should succeed")
            else {
                panic!("expected rejection text frame");
            };
            let JSONRPCMessage::Error(error) =
                serde_json::from_str(&message).expect("rejection frame should contain JSON-RPC")
            else {
                panic!("expected JSON-RPC error");
            };
            assert_eq!(
                (error.id, error.error.code),
                (RequestId::Integer(2), -32603)
            );
            assert!(matches!(
                server_stream.next().await,
                Some(Ok(Message::Close(_))) | None
            ));
        });

        let client = RemoteAppServerClient::connect_with_stream(
            /*channel_capacity*/ 1,
            "in-memory shutdown transport".to_string(),
            client_stream,
            test_initialize_params(),
        )
        .await
        .expect("remote client should initialize");
        requests_sent_rx
            .await
            .expect("server should send both required events");
        timeout(Duration::from_secs(2), async {
            while client.event_rx.len() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first required event should fill the consumer queue");
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        timeout(
            Duration::from_secs(2),
            client.resolve_server_request(RequestId::Integer(1), serde_json::json!({})),
        )
        .await
        .expect("resolve should not wait for required-event queue capacity")
        .expect("first request should resolve");

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
            .expect("shutdown command should reach the worker");
        timeout(Duration::from_secs(2), response_rx)
            .await
            .expect("shutdown should be acknowledged promptly")
            .expect("worker should acknowledge shutdown")
            .expect("remote close should succeed");
        timeout(Duration::from_secs(2), worker_handle)
            .await
            .expect("worker should exit after shutdown")
            .expect("worker should not panic");
        server_task.await.expect("server task should not panic");
    }

    #[tokio::test]
    async fn disconnect_is_delivered_after_a_full_required_event_queue_drains() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let client_stream = WebSocketStream::from_raw_socket(
            client_io,
            tokio_tungstenite::tungstenite::protocol::Role::Client,
            None,
        )
        .await;
        let mut server_stream = WebSocketStream::from_raw_socket(
            server_io,
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        )
        .await;
        let server_task = tokio::spawn(async move {
            let Message::Text(initialize) = server_stream
                .next()
                .await
                .expect("client should initialize")
                .expect("initialize frame should succeed")
            else {
                panic!("expected initialize text frame");
            };
            let JSONRPCMessage::Request(initialize) = serde_json::from_str(&initialize)
                .expect("initialize frame should contain JSON-RPC")
            else {
                panic!("expected initialize request");
            };
            server_stream
                .send(Message::Text(
                    serde_json::to_string(&JSONRPCMessage::Response(JSONRPCResponse {
                        id: initialize.id,
                        result: serde_json::json!({"userAgent": "test-server/1.0"}),
                    }))
                    .expect("initialize response should serialize")
                    .into(),
                ))
                .await
                .expect("server should acknowledge initialize");
            let _initialized = server_stream
                .next()
                .await
                .expect("client should send initialized")
                .expect("initialized frame should succeed");
            for request_id in [1, 2] {
                server_stream
                    .send(Message::Text(
                        serde_json::to_string(&JSONRPCMessage::Request(server_request(request_id)))
                            .expect("server request should serialize")
                            .into(),
                    ))
                    .await
                    .expect("server request should send");
            }
            server_stream
                .close(None)
                .await
                .expect("server close should send");
        });

        let mut client = RemoteAppServerClient::connect_with_stream(
            /*channel_capacity*/ 1,
            "in-memory disconnect transport".to_string(),
            client_stream,
            test_initialize_params(),
        )
        .await
        .expect("remote client should initialize");
        for expected_index in 1..=2 {
            assert!(
                matches!(
                    timeout(Duration::from_secs(2), client.next_event())
                        .await
                        .expect("required event should arrive"),
                    Some(AppServerEvent::ServerRequest(_))
                ),
                "expected server request {expected_index}"
            );
        }
        assert!(matches!(
            timeout(Duration::from_secs(2), client.next_event())
                .await
                .expect("disconnect event should arrive"),
            Some(AppServerEvent::Disconnected { .. })
        ));
        server_task.await.expect("server task should not panic");
        client
            .shutdown()
            .await
            .expect("shutdown after disconnect should complete");
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
