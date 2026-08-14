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
use std::sync::Arc;
use std::time::Duration;

#[cfg(test)]
use std::sync::Mutex;
#[cfg(test)]
use std::sync::OnceLock;

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
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
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

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub(super) enum RemoteWorkerTestEvent {
    ServerRequestQueued(RequestId),
    ServerRequestPublished(RequestId),
}

#[cfg(test)]
pub(super) struct RemoteWorkerTestHooks {
    endpoint: String,
    events: mpsc::UnboundedReceiver<RemoteWorkerTestEvent>,
}

#[cfg(test)]
impl RemoteWorkerTestHooks {
    pub(super) async fn next(&mut self) -> RemoteWorkerTestEvent {
        self.events
            .recv()
            .await
            .expect("remote worker test hook should stay connected")
    }
}

#[cfg(test)]
impl Drop for RemoteWorkerTestHooks {
    fn drop(&mut self) {
        let mut hooks = remote_worker_test_hooks()
            .lock()
            .expect("remote worker test hook mutex should not be poisoned");
        hooks.remove(&self.endpoint);
    }
}

#[cfg(test)]
type RemoteWorkerTestHookSenders = HashMap<String, mpsc::UnboundedSender<RemoteWorkerTestEvent>>;

#[cfg(test)]
static REMOTE_WORKER_TEST_HOOKS: OnceLock<Mutex<RemoteWorkerTestHookSenders>> = OnceLock::new();

#[cfg(test)]
fn remote_worker_test_hooks() -> &'static Mutex<RemoteWorkerTestHookSenders> {
    REMOTE_WORKER_TEST_HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub(super) fn install_remote_worker_test_hooks(endpoint: &str) -> RemoteWorkerTestHooks {
    let (event_tx, events) = mpsc::unbounded_channel();
    let mut hooks = remote_worker_test_hooks()
        .lock()
        .expect("remote worker test hook mutex should not be poisoned");
    assert!(
        hooks.insert(endpoint.to_string(), event_tx).is_none(),
        "remote worker test hooks should be unique per endpoint"
    );
    RemoteWorkerTestHooks {
        endpoint: endpoint.to_string(),
        events,
    }
}

#[cfg(test)]
fn record_remote_worker_test_event(endpoint: &str, event: RemoteWorkerTestEvent) {
    let event_tx = remote_worker_test_hooks()
        .lock()
        .expect("remote worker test hook mutex should not be poisoned")
        .get(endpoint)
        .cloned();
    if let Some(event_tx) = event_tx {
        let _ = event_tx.send(event);
    }
}

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
        _slot: OwnedSemaphorePermit,
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
}

pub struct RemoteAppServerClient {
    command_tx: mpsc::Sender<RemoteClientCommand>,
    request_slots: Arc<Semaphore>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    event_rx: mpsc::Receiver<AppServerEvent>,
    server_version: Option<String>,
    codex_home: Option<String>,
    worker_handle: tokio::task::JoinHandle<IoResult<()>>,
}

#[derive(Clone)]
pub struct RemoteAppServerRequestHandle {
    command_tx: mpsc::Sender<RemoteClientCommand>,
    request_slots: Arc<Semaphore>,
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
        let initialized = initialize_remote_connection(
            &mut stream,
            &endpoint,
            initialize_params,
            INITIALIZE_TIMEOUT,
            channel_capacity,
        )
        .await?;
        let InitializedRemoteConnection {
            backlog,
            terminal,
            server_version,
            codex_home,
        } = initialized;

        let (command_tx, command_rx) = mpsc::channel::<RemoteClientCommand>(channel_capacity);
        let (event_tx, event_rx) = mpsc::channel::<AppServerEvent>(channel_capacity);
        let request_slots = Arc::new(Semaphore::new(channel_capacity));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let worker_request_slots = Arc::clone(&request_slots);
        let worker_handle = tokio::spawn(async move {
            remote_worker(
                stream,
                endpoint,
                command_rx,
                event_tx,
                shutdown_rx,
                worker_request_slots,
                backlog,
                terminal,
            )
            .await
        });

        Ok(Self {
            command_tx,
            request_slots,
            shutdown_tx: Some(shutdown_tx),
            event_rx,
            server_version,
            codex_home,
            worker_handle,
        })
    }

    pub fn request_handle(&self) -> RemoteAppServerRequestHandle {
        RemoteAppServerRequestHandle {
            command_tx: self.command_tx.clone(),
            request_slots: Arc::clone(&self.request_slots),
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
        self.event_rx.recv().await
    }

    pub async fn shutdown(self) -> IoResult<()> {
        self.shutdown_with_timeout(SHUTDOWN_TIMEOUT).await
    }

    async fn shutdown_with_timeout(self, shutdown_timeout: Duration) -> IoResult<()> {
        let Self {
            command_tx,
            request_slots: _request_slots,
            shutdown_tx,
            event_rx,
            server_version: _server_version,
            codex_home: _codex_home,
            worker_handle,
        } = self;
        let mut worker_handle = worker_handle;
        let _ = shutdown_tx.map(|shutdown_tx| shutdown_tx.send(()));
        drop(event_rx);
        drop(command_tx);

        match timeout(shutdown_timeout, &mut worker_handle).await {
            Ok(Ok(result)) => result,
            Ok(Err(err)) => Err(IoError::other(format!(
                "remote app-server worker failed to join during shutdown: {err}"
            ))),
            Err(_) => {
                worker_handle.abort();
                let _ = worker_handle.await;
                Err(IoError::new(
                    ErrorKind::TimedOut,
                    "timed out waiting for remote app-server shutdown",
                ))
            }
        }
    }
}

impl RemoteAppServerRequestHandle {
    pub async fn request(&self, request: ClientRequest) -> IoResult<RequestResult> {
        self.request_json_rpc(jsonrpc_request_from_client_request(request))
            .await
    }

    pub async fn request_json_rpc(&self, request: JSONRPCRequest) -> IoResult<RequestResult> {
        let slot = Arc::clone(&self.request_slots)
            .acquire_owned()
            .await
            .map_err(|_| remote_worker_closed())?;
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(RemoteClientCommand::Request {
                request: Box::new(request),
                response_tx,
                _slot: slot,
            })
            .await
            .map_err(|_| remote_worker_closed())?;
        response_rx.await.map_err(|_| remote_worker_closed())?
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

/// The private event buffer has a count bound, not a byte bound: individual
/// JSON-RPC payloads can still be large. The public channel has the same
/// capacity, so a connection keeps at most `2 * capacity` ordinary events
/// between the worker and its caller. Server request ownership uses that same
/// bound across the private backlog and public channel until the client starts
/// a response attempt. The bounded ledger also preserves request order for
/// terminal cleanup. Terminal finalization may additionally synthesize one
/// `Lagged` marker and one `Disconnected` event.
struct RemoteEventBacklog {
    events: VecDeque<AppServerEvent>,
    skipped_events: usize,
    // The number of retained events that preceded the first skipped event.
    // The virtual Lagged event is emitted immediately after these entries.
    lagged_after: Option<usize>,
    capacity: usize,
    server_request_dispositions: HashMap<RequestId, ServerRequestResponseDisposition>,
    server_request_order: VecDeque<RequestId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerRequestResponseDisposition {
    Pending,
    ResponseAttempted,
}

#[derive(Debug)]
struct RequiredEventOverflow {
    server_request_id: Option<RequestId>,
}

impl RemoteEventBacklog {
    fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            events: VecDeque::with_capacity(capacity),
            skipped_events: 0,
            lagged_after: None,
            capacity,
            server_request_dispositions: HashMap::with_capacity(capacity.saturating_mul(2)),
            server_request_order: VecDeque::with_capacity(capacity.saturating_mul(2)),
        }
    }

    fn enqueue(&mut self, event: AppServerEvent) -> Result<(), RequiredEventOverflow> {
        let server_request_id = match &event {
            AppServerEvent::ServerRequest(request) => Some(request.id().clone()),
            _ => None,
        };
        if let Some(request_id) = &server_request_id {
            // A repeated peer request ID denotes the same in-flight request.
            // Do not publish a second prompt or generate a second response.
            if self.server_request_dispositions.contains_key(request_id) {
                warn!(%request_id, "ignoring duplicate remote app-server server request");
                return Ok(());
            }

            // Server requests occupy the same two bounded queues as ordinary
            // events, but they remain owned after crossing event_tx. Bound
            // that retained ownership so a fast event consumer that never
            // answers prompts cannot turn it into an unbounded side queue.
            if self.server_request_dispositions.len() >= self.capacity.saturating_mul(2) {
                return Err(RequiredEventOverflow {
                    server_request_id: Some(request_id.clone()),
                });
            }
        }

        if self.events.len() < self.capacity {
            if let Some(request_id) = server_request_id {
                self.server_request_dispositions.insert(
                    request_id.clone(),
                    ServerRequestResponseDisposition::Pending,
                );
                self.server_request_order.push_back(request_id);
            }
            self.events.push_back(event);
            return Ok(());
        }

        if remote_event_requires_delivery(&event) {
            return Err(RequiredEventOverflow {
                server_request_id: match event {
                    AppServerEvent::ServerRequest(request) => Some(request.id().clone()),
                    _ => None,
                },
            });
        }

        self.record_best_effort_skip();
        Ok(())
    }

    fn record_best_effort_skip(&mut self) {
        self.skipped_events = self.skipped_events.saturating_add(1);
        self.lagged_after.get_or_insert(self.events.len());
    }

    fn has_pending_public_event(&self) -> bool {
        !self.events.is_empty() || self.skipped_events > 0
    }

    fn pop_next_for_public(&mut self) -> Option<AppServerEvent> {
        if self.lagged_after == Some(0) {
            self.lagged_after = None;
            let skipped = std::mem::take(&mut self.skipped_events);
            return Some(AppServerEvent::Lagged { skipped });
        }

        let event = self.events.pop_front()?;
        if let Some(remaining) = &mut self.lagged_after {
            *remaining = remaining.saturating_sub(1);
        }
        Some(event)
    }

    fn begin_server_request_response(&mut self, request_id: &RequestId) -> bool {
        let Some(disposition) = self.server_request_dispositions.get_mut(request_id) else {
            return false;
        };
        if *disposition == ServerRequestResponseDisposition::ResponseAttempted {
            return false;
        }
        // A timed-out or failed socket write might still have reached the
        // peer. Keep this disposition through terminal cleanup so that ID is
        // never answered a second time.
        *disposition = ServerRequestResponseDisposition::ResponseAttempted;
        true
    }

    fn complete_server_request(&mut self, request_id: &RequestId) {
        if self
            .server_request_dispositions
            .remove(request_id)
            .is_some()
        {
            self.server_request_order
                .retain(|pending_request_id| pending_request_id != request_id);
        }
    }

    fn take_unanswered_server_request_ids(&mut self) -> Vec<RequestId> {
        let mut unanswered = Vec::new();
        while let Some(request_id) = self.server_request_order.pop_front() {
            match self.server_request_dispositions.remove(&request_id) {
                Some(ServerRequestResponseDisposition::Pending) => unanswered.push(request_id),
                Some(ServerRequestResponseDisposition::ResponseAttempted) | None => {}
            }
        }
        debug_assert!(self.server_request_dispositions.is_empty());
        unanswered
    }

    fn finalize(mut self, message: String) -> VecDeque<AppServerEvent> {
        let mut terminal_events = VecDeque::new();
        while let Some(event) = self.pop_next_for_public() {
            terminal_events.push_back(event);
        }
        terminal_events.push_back(AppServerEvent::Disconnected { message });
        terminal_events
    }
}

struct InitializedRemoteConnection {
    backlog: RemoteEventBacklog,
    terminal: Option<RemoteTerminal>,
    server_version: Option<String>,
    codex_home: Option<String>,
}

struct PendingRemoteRequest {
    response_tx: oneshot::Sender<IoResult<RequestResult>>,
    _slot: OwnedSemaphorePermit,
}

struct RemoteTerminal {
    error_kind: ErrorKind,
    message: String,
    server_request_error: JSONRPCErrorError,
    overflowed_server_request_ids: Vec<RequestId>,
}

impl RemoteTerminal {
    fn new(error_kind: ErrorKind, message: String) -> Self {
        let server_request_error = JSONRPCErrorError {
            code: -32603,
            message: format!(
                "remote app-server client stopped before answering server request: {message}"
            ),
            data: None,
        };
        Self {
            error_kind,
            message,
            server_request_error,
            overflowed_server_request_ids: Vec::new(),
        }
    }

    fn write_failed(endpoint: &str, err: IoError) -> Self {
        Self::new(
            ErrorKind::BrokenPipe,
            format!("remote app server at `{endpoint}` write failed: {err}"),
        )
    }

    fn required_event_overflow(endpoint: &str, server_request_id: Option<RequestId>) -> Self {
        let mut terminal = Self::new(
            ErrorKind::WouldBlock,
            format!(
                "remote app server at `{endpoint}` exceeded the bounded remote event backlog while delivering a required event"
            ),
        );
        if let Some(request_id) = server_request_id {
            terminal.overflowed_server_request_ids.push(request_id);
        }
        terminal.server_request_error = JSONRPCErrorError {
            code: -32001,
            message: "remote app-server event queue is full".to_string(),
            data: None,
        };
        terminal
    }

    fn io_error(&self) -> IoError {
        IoError::new(self.error_kind, self.message.clone())
    }
}

async fn remote_worker<S>(
    mut stream: WebSocketStream<S>,
    endpoint: String,
    mut command_rx: mpsc::Receiver<RemoteClientCommand>,
    event_tx: mpsc::Sender<AppServerEvent>,
    mut shutdown_rx: oneshot::Receiver<()>,
    request_slots: Arc<Semaphore>,
    mut backlog: RemoteEventBacklog,
    mut terminal: Option<RemoteTerminal>,
) -> IoResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut pending_requests = HashMap::<RequestId, PendingRemoteRequest>::new();

    while terminal.is_none() {
        // This intentionally uses Tokio's fair (non-biased) selection. Each
        // command branch handles exactly one command, so a command producer
        // cannot starve responses or websocket lifecycle messages.
        tokio::select! {
            _ = event_tx.closed() => {
                terminal = Some(RemoteTerminal::new(
                    ErrorKind::BrokenPipe,
                    "remote app-server event consumer channel is closed".to_string(),
                ));
            }
            _ = &mut shutdown_rx => {
                terminal = Some(RemoteTerminal::new(
                    ErrorKind::BrokenPipe,
                    format!("remote app server at `{endpoint}` is shutting down"),
                ));
            }
            command = command_rx.recv() => {
                terminal = match command {
                    Some(command) => {
                        handle_remote_command(
                            command,
                            &mut stream,
                            &endpoint,
                            &mut pending_requests,
                            &mut backlog,
                        )
                        .await
                    }
                    None => Some(RemoteTerminal::new(
                        ErrorKind::BrokenPipe,
                        "remote app-server worker command channel is closed".to_string(),
                    )),
                };
            }
            message = stream.next() => {
                terminal = handle_remote_message(
                    message,
                    &mut stream,
                    &endpoint,
                    &mut pending_requests,
                    &mut backlog,
                )
                .await;
            }
            permit = event_tx.reserve(), if backlog.has_pending_public_event() => {
                match permit {
                    Ok(permit) => {
                        let event = backlog
                            .pop_next_for_public()
                            .expect("backlog advertised a public event");
                        #[cfg(test)]
                        let server_request_id = match &event {
                            AppServerEvent::ServerRequest(request) => Some(request.id().clone()),
                            AppServerEvent::Lagged { .. }
                            | AppServerEvent::ServerNotification(_)
                            | AppServerEvent::Disconnected { .. } => None,
                        };
                        permit.send(event);
                        #[cfg(test)]
                        if let Some(request_id) = server_request_id {
                            record_remote_worker_test_event(
                                &endpoint,
                                RemoteWorkerTestEvent::ServerRequestPublished(request_id),
                            );
                        }
                    }
                    Err(_) => {
                        terminal = Some(RemoteTerminal::new(
                            ErrorKind::BrokenPipe,
                            "remote app-server event consumer channel is closed".to_string(),
                        ));
                    }
                }
            }
        }
    }

    let terminal = terminal.expect("terminal cause is set before finalization");

    // Close admission and request slots before an await. Command senders and
    // request-capacity waiters then wake instead of remaining blocked behind
    // remote cleanup. Commands already admitted to the channel are drained
    // below and receive this same terminal result exactly once.
    command_rx.close();
    request_slots.close();

    for (_, pending) in pending_requests {
        let _ = pending.response_tx.send(Err(terminal.io_error()));
    }
    fail_queued_commands(&mut command_rx, &terminal);

    let mut reject_request_ids = backlog.take_unanswered_server_request_ids();
    reject_request_ids.extend(terminal.overflowed_server_request_ids.iter().cloned());
    let terminal_events = backlog.finalize(terminal.message.clone());

    // Event publication is independent from socket cleanup. In particular, a
    // peer that stalls request rejections or the close handshake cannot delay
    // a consumer that is ready to observe the terminal event sequence.
    let (cleanup_result, ()) = tokio::join!(
        cleanup_terminal_socket(&mut stream, &endpoint, &terminal, reject_request_ids),
        publish_terminal_events(&event_tx, terminal_events),
    );
    cleanup_result
}

async fn handle_remote_command<S>(
    command: RemoteClientCommand,
    stream: &mut WebSocketStream<S>,
    endpoint: &str,
    pending_requests: &mut HashMap<RequestId, PendingRemoteRequest>,
    backlog: &mut RemoteEventBacklog,
) -> Option<RemoteTerminal>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match command {
        RemoteClientCommand::Request {
            request,
            response_tx,
            _slot,
        } => {
            let request_id = request.id.clone();
            if pending_requests.contains_key(&request_id) {
                let _ = response_tx.send(Err(IoError::new(
                    ErrorKind::InvalidInput,
                    format!("duplicate remote app-server request id `{request_id}`"),
                )));
                return None;
            }

            pending_requests.insert(request_id, PendingRemoteRequest { response_tx, _slot });
            write_jsonrpc_message(stream, JSONRPCMessage::Request(*request), endpoint)
                .await
                .err()
                .map(|err| RemoteTerminal::write_failed(endpoint, err))
        }
        RemoteClientCommand::Notify {
            notification,
            response_tx,
        } => {
            match write_jsonrpc_message(
                stream,
                JSONRPCMessage::Notification(jsonrpc_notification_from_client_notification(
                    notification,
                )),
                endpoint,
            )
            .await
            {
                Ok(()) => {
                    let _ = response_tx.send(Ok(()));
                    None
                }
                Err(err) => {
                    let terminal = RemoteTerminal::write_failed(endpoint, err);
                    let _ = response_tx.send(Err(terminal.io_error()));
                    Some(terminal)
                }
            }
        }
        RemoteClientCommand::ResolveServerRequest {
            request_id,
            result,
            response_tx,
        } => {
            if !backlog.begin_server_request_response(&request_id) {
                let _ = response_tx.send(Err(IoError::new(
                    ErrorKind::InvalidInput,
                    format!("remote app-server server request id `{request_id}` is not pending"),
                )));
                return None;
            }
            match write_jsonrpc_message(
                stream,
                JSONRPCMessage::Response(JSONRPCResponse {
                    id: request_id.clone(),
                    result,
                }),
                endpoint,
            )
            .await
            {
                Ok(()) => {
                    backlog.complete_server_request(&request_id);
                    let _ = response_tx.send(Ok(()));
                    None
                }
                Err(err) => {
                    let terminal = RemoteTerminal::write_failed(endpoint, err);
                    let _ = response_tx.send(Err(terminal.io_error()));
                    Some(terminal)
                }
            }
        }
        RemoteClientCommand::RejectServerRequest {
            request_id,
            error,
            response_tx,
        } => {
            if !backlog.begin_server_request_response(&request_id) {
                let _ = response_tx.send(Err(IoError::new(
                    ErrorKind::InvalidInput,
                    format!("remote app-server server request id `{request_id}` is not pending"),
                )));
                return None;
            }
            match write_jsonrpc_message(
                stream,
                JSONRPCMessage::Error(JSONRPCError {
                    error,
                    id: request_id.clone(),
                }),
                endpoint,
            )
            .await
            {
                Ok(()) => {
                    backlog.complete_server_request(&request_id);
                    let _ = response_tx.send(Ok(()));
                    None
                }
                Err(err) => {
                    let terminal = RemoteTerminal::write_failed(endpoint, err);
                    let _ = response_tx.send(Err(terminal.io_error()));
                    Some(terminal)
                }
            }
        }
    }
}

async fn handle_remote_message<S>(
    message: Option<Result<Message, TungsteniteError>>,
    stream: &mut WebSocketStream<S>,
    endpoint: &str,
    pending_requests: &mut HashMap<RequestId, PendingRemoteRequest>,
    backlog: &mut RemoteEventBacklog,
) -> Option<RemoteTerminal>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match message {
        Some(Ok(Message::Text(text))) => match serde_json::from_str::<JSONRPCMessage>(&text) {
            Ok(JSONRPCMessage::Response(response)) => {
                if let Some(pending) = pending_requests.remove(&response.id) {
                    let _ = pending.response_tx.send(Ok(Ok(response.result)));
                }
                None
            }
            Ok(JSONRPCMessage::Error(error)) => {
                if let Some(pending) = pending_requests.remove(&error.id) {
                    let _ = pending.response_tx.send(Ok(Err(error.error)));
                }
                None
            }
            Ok(JSONRPCMessage::Notification(notification)) => {
                app_server_event_from_notification(notification)
                    .and_then(|event| enqueue_remote_event(backlog, endpoint, event))
            }
            Ok(JSONRPCMessage::Request(request)) => {
                let request_id = request.id.clone();
                let method = request.method.clone();
                match ServerRequest::try_from(request) {
                    Ok(request) => enqueue_remote_event(
                        backlog,
                        endpoint,
                        AppServerEvent::ServerRequest(request),
                    ),
                    Err(err) => {
                        warn!(%err, method, "rejecting unknown remote app-server request");
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
                        .await
                        .err()
                        .map(|err| RemoteTerminal::write_failed(endpoint, err))
                    }
                }
            }
            Err(err) => Some(RemoteTerminal::new(
                ErrorKind::InvalidData,
                format!("remote app server at `{endpoint}` sent invalid JSON-RPC: {err}"),
            )),
        },
        Some(Ok(Message::Close(frame))) => {
            let reason = frame
                .as_ref()
                .map(|frame| frame.reason.to_string())
                .filter(|reason| !reason.is_empty())
                .unwrap_or_else(|| "connection closed".to_string());
            Some(RemoteTerminal::new(
                ErrorKind::ConnectionAborted,
                format!("remote app server at `{endpoint}` disconnected: {reason}"),
            ))
        }
        Some(Ok(Message::Binary(_)))
        | Some(Ok(Message::Ping(_)))
        | Some(Ok(Message::Pong(_)))
        | Some(Ok(Message::Frame(_))) => None,
        Some(Err(err)) => Some(RemoteTerminal::new(
            ErrorKind::InvalidData,
            format!("remote app server at `{endpoint}` transport failed: {err}"),
        )),
        None => Some(RemoteTerminal::new(
            ErrorKind::UnexpectedEof,
            format!("remote app server at `{endpoint}` closed the connection"),
        )),
    }
}

fn enqueue_remote_event(
    backlog: &mut RemoteEventBacklog,
    endpoint: &str,
    event: AppServerEvent,
) -> Option<RemoteTerminal> {
    #[cfg(test)]
    let server_request_id = match &event {
        AppServerEvent::ServerRequest(request) => Some(request.id().clone()),
        AppServerEvent::Lagged { .. }
        | AppServerEvent::ServerNotification(_)
        | AppServerEvent::Disconnected { .. } => None,
    };
    match backlog.enqueue(event) {
        Ok(()) => {
            #[cfg(test)]
            if let Some(request_id) = server_request_id {
                record_remote_worker_test_event(
                    endpoint,
                    RemoteWorkerTestEvent::ServerRequestQueued(request_id),
                );
            }
            None
        }
        Err(overflow) => Some(RemoteTerminal::required_event_overflow(
            endpoint,
            overflow.server_request_id,
        )),
    }
}

fn fail_queued_commands(
    command_rx: &mut mpsc::Receiver<RemoteClientCommand>,
    terminal: &RemoteTerminal,
) {
    while let Ok(command) = command_rx.try_recv() {
        match command {
            RemoteClientCommand::Request { response_tx, .. } => {
                let _ = response_tx.send(Err(terminal.io_error()));
            }
            RemoteClientCommand::Notify { response_tx, .. }
            | RemoteClientCommand::ResolveServerRequest { response_tx, .. }
            | RemoteClientCommand::RejectServerRequest { response_tx, .. } => {
                let _ = response_tx.send(Err(terminal.io_error()));
            }
        }
    }
}

async fn publish_terminal_events(
    event_tx: &mpsc::Sender<AppServerEvent>,
    mut terminal_events: VecDeque<AppServerEvent>,
) {
    while let Some(event) = terminal_events.pop_front() {
        if event_tx.send(event).await.is_err() {
            break;
        }
    }
}

async fn cleanup_terminal_socket<S>(
    stream: &mut WebSocketStream<S>,
    endpoint: &str,
    terminal: &RemoteTerminal,
    reject_request_ids: Vec<RequestId>,
) -> IoResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    timeout(SHUTDOWN_TIMEOUT, async {
        for request_id in reject_request_ids {
            if let Err(err) = write_jsonrpc_message_unbounded(
                stream,
                JSONRPCMessage::Error(JSONRPCError {
                    error: terminal.server_request_error.clone(),
                    id: request_id,
                }),
                endpoint,
            )
            .await
            {
                warn!(%err, "failed to reject unanswered remote app-server server request");
                break;
            }
        }
        close_remote_stream(stream, endpoint).await
    })
    .await
    .map_err(|_| {
        IoError::new(
            ErrorKind::TimedOut,
            format!("timed out cleaning up remote app server `{endpoint}`"),
        )
    })?
}

async fn close_remote_stream<S>(stream: &mut WebSocketStream<S>, endpoint: &str) -> IoResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream.close(None).await.or_else(|err| {
        if websocket_close_error_is_already_closed(&err) {
            Ok(())
        } else {
            Err(IoError::other(format!(
                "failed to close websocket app server `{endpoint}`: {err}"
            )))
        }
    })
}

fn remote_worker_closed() -> IoError {
    IoError::new(
        ErrorKind::BrokenPipe,
        "remote app-server worker channel is closed",
    )
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
    channel_capacity: usize,
) -> IoResult<InitializedRemoteConnection>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let initialize_request_id = RequestId::String("initialize".to_string());
    let mut backlog = RemoteEventBacklog::new(channel_capacity);
    let mut terminal = None;
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
                            if terminal.is_none()
                                && let Some(event) = app_server_event_from_notification(notification)
                            {
                                terminal = enqueue_remote_event(&mut backlog, endpoint, event);
                            }
                        }
                        JSONRPCMessage::Request(request) => {
                            let request_id = request.id.clone();
                            let method = request.method.clone();
                            match ServerRequest::try_from(request) {
                                Ok(request) => {
                                    terminal = enqueue_remote_event(
                                        &mut backlog,
                                        endpoint,
                                        AppServerEvent::ServerRequest(request),
                                    );
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
                    if terminal.is_some() {
                        break Ok(());
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

    if let Some(terminal) = terminal.take() {
        let mut reject_request_ids = backlog.take_unanswered_server_request_ids();
        reject_request_ids.extend(terminal.overflowed_server_request_ids.iter().cloned());
        cleanup_terminal_socket(stream, endpoint, &terminal, reject_request_ids).await?;
        return Err(terminal.io_error());
    }

    write_jsonrpc_message(
        stream,
        JSONRPCMessage::Notification(jsonrpc_notification_from_client_notification(
            ClientNotification::Initialized,
        )),
        endpoint,
    )
    .await?;

    Ok(InitializedRemoteConnection {
        backlog,
        terminal,
        server_version,
        codex_home,
    })
}

fn app_server_event_from_notification(notification: JSONRPCNotification) -> Option<AppServerEvent> {
    match ServerNotification::try_from(notification) {
        Ok(notification) => Some(AppServerEvent::ServerNotification(notification)),
        Err(_) => None,
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
    timeout(
        SHUTDOWN_TIMEOUT,
        write_jsonrpc_message_unbounded(stream, message, endpoint),
    )
    .await
    .map_err(|_| {
        IoError::new(
            ErrorKind::TimedOut,
            format!("timed out writing websocket message to `{endpoint}`"),
        )
    })?
}

async fn write_jsonrpc_message_unbounded<S>(
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

    fn server_request(id: i64) -> AppServerEvent {
        AppServerEvent::ServerRequest(ServerRequest::CurrentTimeRead {
            request_id: RequestId::Integer(id),
            params: codex_app_server_protocol::CurrentTimeReadParams {
                thread_id: "thread".to_string(),
            },
        })
    }

    #[test]
    fn initialization_backlog_counts_best_effort_loss_at_capacity_one() {
        let mut backlog = RemoteEventBacklog::new(1);
        backlog
            .enqueue(AppServerEvent::Lagged { skipped: 10 })
            .expect("first event should fit");
        backlog
            .enqueue(AppServerEvent::Lagged { skipped: 11 })
            .expect("best-effort overflow should be accounted for");
        backlog
            .enqueue(AppServerEvent::Lagged { skipped: 12 })
            .expect("best-effort overflow should remain accounted for");

        let events = backlog.finalize("terminal".to_string());
        assert!(matches!(events[0], AppServerEvent::Lagged { skipped: 10 }));
        assert!(matches!(events[1], AppServerEvent::Lagged { skipped: 2 }));
        assert!(matches!(
            events[2],
            AppServerEvent::Disconnected { ref message } if message == "terminal"
        ));
    }

    #[test]
    fn initialization_backlog_uses_the_connection_capacity_for_all_required_events() {
        let mut backlog = RemoteEventBacklog::new(128);
        for id in 0..33 {
            backlog
                .enqueue(server_request(id))
                .expect("33 initial required events must fit at capacity 128");
        }

        assert_eq!(backlog.events.len(), 33);
        assert_eq!(backlog.take_unanswered_server_request_ids().len(), 33);
    }

    #[test]
    fn required_server_request_overflow_keeps_all_unanswered_ids_for_rejection() {
        let mut backlog = RemoteEventBacklog::new(1);
        backlog
            .enqueue(server_request(1))
            .expect("first server request should fit");
        let public_request = backlog
            .pop_next_for_public()
            .expect("first server request should cross the public boundary");
        assert!(matches!(
            public_request,
            AppServerEvent::ServerRequest(ref request) if request.id() == &RequestId::Integer(1)
        ));
        backlog
            .enqueue(server_request(2))
            .expect("second server request should occupy the private backlog");
        let overflow = backlog
            .enqueue(server_request(3))
            .expect_err("third required server request should overflow");
        assert_eq!(overflow.server_request_id, Some(RequestId::Integer(3)));

        assert_eq!(
            backlog.take_unanswered_server_request_ids(),
            vec![RequestId::Integer(1), RequestId::Integer(2)]
        );
    }

    #[test]
    fn public_server_request_ownership_is_released_only_after_client_completion() {
        let mut backlog = RemoteEventBacklog::new(1);
        backlog
            .enqueue(server_request(1))
            .expect("server request should fit");
        backlog
            .pop_next_for_public()
            .expect("server request should cross the public boundary");

        assert!(backlog.begin_server_request_response(&RequestId::Integer(1)));
        backlog.complete_server_request(&RequestId::Integer(1));
        assert!(!backlog.begin_server_request_response(&RequestId::Integer(1)));
    }

    #[test]
    fn unanswered_public_server_requests_remain_bounded_by_both_event_channels() {
        let mut backlog = RemoteEventBacklog::new(1);
        for id in [1, 2] {
            backlog
                .enqueue(server_request(id))
                .expect("server request should fit while the combined C=1 queues have room");
            backlog
                .pop_next_for_public()
                .expect("server request should cross the public boundary");
        }

        let overflow = backlog
            .enqueue(server_request(3))
            .expect_err("unanswered public requests must not create unbounded ownership");
        assert_eq!(overflow.server_request_id, Some(RequestId::Integer(3)));
    }

    #[test]
    fn duplicate_server_request_id_does_not_create_a_second_prompt_or_response() {
        let mut backlog = RemoteEventBacklog::new(1);
        backlog
            .enqueue(server_request(1))
            .expect("first server request should fit");
        backlog
            .pop_next_for_public()
            .expect("first server request should cross the public boundary");
        backlog
            .enqueue(server_request(1))
            .expect("duplicate server request should be ignored");

        assert!(!backlog.has_pending_public_event());
        assert_eq!(
            backlog.take_unanswered_server_request_ids(),
            vec![RequestId::Integer(1)]
        );
    }

    #[test]
    fn response_attempt_is_not_rejected_again_during_terminal_cleanup() {
        let mut backlog = RemoteEventBacklog::new(1);
        backlog
            .enqueue(server_request(1))
            .expect("server request should fit");

        assert!(backlog.begin_server_request_response(&RequestId::Integer(1)));
        assert!(!backlog.begin_server_request_response(&RequestId::Integer(1)));
        assert!(backlog.take_unanswered_server_request_ids().is_empty());
    }

    #[tokio::test]
    async fn post_close_server_response_write_is_not_rejected_again_during_terminal_cleanup() {
        let (socket, peer) = tokio::io::duplex(64);
        drop(peer);
        let mut stream = WebSocketStream::from_raw_socket(
            socket,
            tokio_tungstenite::tungstenite::protocol::Role::Client,
            None,
        )
        .await;
        let mut pending_requests = HashMap::new();
        let mut backlog = RemoteEventBacklog::new(1);
        backlog
            .enqueue(server_request(1))
            .expect("server request should fit");
        let (response_tx, response_rx) = oneshot::channel();

        let terminal = handle_remote_command(
            RemoteClientCommand::ResolveServerRequest {
                request_id: RequestId::Integer(1),
                result: serde_json::json!({}),
                response_tx,
            },
            &mut stream,
            "test://closed-peer",
            &mut pending_requests,
            &mut backlog,
        )
        .await
        .expect("post-close response write should terminate the transport");

        let error = response_rx
            .await
            .expect("response write result should be returned")
            .expect_err("post-close response write should fail");
        assert_eq!(error.kind(), ErrorKind::BrokenPipe);
        assert_eq!(terminal.server_request_error.code, -32603);
        assert!(backlog.take_unanswered_server_request_ids().is_empty());
    }

    #[test]
    fn terminal_finalization_retains_fifo_then_lagged_then_one_disconnect() {
        let mut backlog = RemoteEventBacklog::new(2);
        backlog
            .enqueue(server_request(1))
            .expect("first event should fit");
        backlog
            .enqueue(AppServerEvent::Lagged { skipped: 7 })
            .expect("second event should fit");
        backlog
            .enqueue(AppServerEvent::Lagged { skipped: 8 })
            .expect("best-effort event should be accounted for");

        let events = backlog.finalize("terminal".to_string());
        assert!(matches!(
            events[0],
            AppServerEvent::ServerRequest(ref request) if request.id() == &RequestId::Integer(1)
        ));
        assert!(matches!(events[1], AppServerEvent::Lagged { skipped: 7 }));
        assert!(matches!(events[2], AppServerEvent::Lagged { skipped: 1 }));
        assert!(matches!(
            events[3],
            AppServerEvent::Disconnected { ref message } if message == "terminal"
        ));
    }

    #[tokio::test]
    async fn request_admission_waiter_wakes_when_terminal_closes_slots() {
        let (command_tx, _command_rx) = mpsc::channel(1);
        let request_slots = Arc::new(Semaphore::new(1));
        let held_slot = Arc::clone(&request_slots)
            .acquire_owned()
            .await
            .expect("test slot should acquire");
        let handle = RemoteAppServerRequestHandle {
            command_tx,
            request_slots: Arc::clone(&request_slots),
        };

        let request = tokio::spawn(async move {
            handle
                .request_json_rpc(JSONRPCRequest {
                    id: RequestId::Integer(1),
                    method: "account/read".to_string(),
                    params: None,
                    trace: None,
                })
                .await
        });
        tokio::task::yield_now().await;
        request_slots.close();
        drop(held_slot);

        let error = request
            .await
            .expect("request task should join")
            .expect_err("closed admission should fail the request");
        assert_eq!(error.kind(), ErrorKind::BrokenPipe);
    }

    #[tokio::test]
    async fn shutdown_control_bypasses_a_saturated_command_channel() {
        let (command_tx, _command_rx) = mpsc::channel(1);
        let (response_tx, _response_rx) = oneshot::channel();
        command_tx
            .try_send(RemoteClientCommand::Notify {
                notification: ClientNotification::Initialized,
                response_tx,
            })
            .expect("ordinary command channel should be saturated");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let worker_handle = tokio::spawn(async move {
            let _ = shutdown_rx.await;
            Ok(())
        });
        let (_event_tx, event_rx) = mpsc::channel::<AppServerEvent>(1);
        let client = RemoteAppServerClient {
            command_tx,
            request_slots: Arc::new(Semaphore::new(1)),
            shutdown_tx: Some(shutdown_tx),
            event_rx,
            server_version: None,
            codex_home: None,
            worker_handle,
        };

        client
            .shutdown()
            .await
            .expect("shutdown control should not wait for command capacity");
    }

    #[tokio::test]
    async fn shutdown_propagates_a_worker_close_failure() {
        let (command_tx, _command_rx) = mpsc::channel(1);
        let (shutdown_tx, _shutdown_rx) = oneshot::channel();
        let worker_handle = tokio::spawn(async { Err(IoError::other("close failed")) });
        let (_event_tx, event_rx) = mpsc::channel::<AppServerEvent>(1);
        let client = RemoteAppServerClient {
            command_tx,
            request_slots: Arc::new(Semaphore::new(1)),
            shutdown_tx: Some(shutdown_tx),
            event_rx,
            server_version: None,
            codex_home: None,
            worker_handle,
        };

        let error = client
            .shutdown()
            .await
            .expect_err("close failure must reach shutdown callers");
        assert!(error.to_string().contains("close failed"));
    }

    #[tokio::test]
    async fn shutdown_reports_a_bounded_timeout_when_worker_does_not_finish() {
        let (command_tx, _command_rx) = mpsc::channel(1);
        let (shutdown_tx, _shutdown_rx) = oneshot::channel();
        let worker_handle = tokio::spawn(async { std::future::pending::<IoResult<()>>().await });
        let (_event_tx, event_rx) = mpsc::channel::<AppServerEvent>(1);
        let client = RemoteAppServerClient {
            command_tx,
            request_slots: Arc::new(Semaphore::new(1)),
            shutdown_tx: Some(shutdown_tx),
            event_rx,
            server_version: None,
            codex_home: None,
            worker_handle,
        };

        let error = client
            .shutdown_with_timeout(Duration::from_millis(1))
            .await
            .expect_err("stalled worker should make shutdown time out");
        assert_eq!(error.kind(), ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn shutdown_tolerates_worker_exit_before_control_is_observed() {
        let (command_tx, mut command_rx) = mpsc::channel(1);
        let (_event_tx, event_rx) = mpsc::channel::<AppServerEvent>(1);
        let worker_handle = tokio::spawn(async move {
            let _ = command_rx.recv().await;
            Ok(())
        });
        let (shutdown_tx, _shutdown_rx) = oneshot::channel();
        let client = RemoteAppServerClient {
            command_tx,
            request_slots: Arc::new(Semaphore::new(1)),
            shutdown_tx: Some(shutdown_tx),
            event_rx,
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
