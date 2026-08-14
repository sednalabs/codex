//! Shared in-process app-server client facade for CLI surfaces.
//!
//! This crate wraps [`codex_app_server::in_process`] behind a single async API
//! used by surfaces like TUI and exec. It centralizes:
//!
//! - Runtime startup and initialize-capabilities handshake.
//! - Typed caller-provided startup identity (`SessionSource` + client name).
//! - Typed and raw request/notification dispatch.
//! - Server request resolution and rejection.
//! - Event consumption with backpressure signaling ([`InProcessServerEvent::Lagged`]).
//! - Bounded graceful shutdown with abort fallback.
//!
//! The facade interposes a worker task between the caller and the underlying
//! [`InProcessClientHandle`](codex_app_server::in_process::InProcessClientHandle),
//! bridging async `mpsc` channels on both sides. Queues are bounded so overload
//! surfaces as channel-full errors rather than unbounded memory growth.

mod path;
mod remote;

use std::error::Error;
use std::fmt;
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::io::Result as IoResult;
use std::sync::Arc;
use std::time::Duration;

pub use codex_app_server::app_server_control_socket_path;
pub use codex_app_server::in_process::DEFAULT_IN_PROCESS_CHANNEL_CAPACITY;
pub use codex_app_server::in_process::InProcessServerEvent;
use codex_app_server::in_process::InProcessStartArgs;
use codex_app_server::in_process::LogDbLayer;
pub use codex_app_server::in_process::StateDbHandle;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::ClientNotification;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ConfigWarningNotification;
use codex_app_server_protocol::InitializeCapabilities;
use codex_app_server_protocol::InitializeParams;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::Result as JsonRpcResult;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerRequest;
use codex_arg0::Arg0DispatchPaths;
use codex_config::CloudConfigBundleLoader;
use codex_config::LoaderOverrides;
use codex_config::NoopThreadConfigLoader;
use codex_config::RemoteThreadConfigLoader;
use codex_config::ThreadConfigLoader;
use codex_core::config::Config;
pub use codex_core::otel_init::build_provider as build_otel_provider;
pub use codex_exec_server::EnvironmentManager;
pub use codex_exec_server::ExecServerRuntimePaths;
use codex_feedback::CodexFeedback;
use codex_protocol::protocol::SessionSource;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::timeout;
use toml::Value as TomlValue;
use tracing::warn;

pub use crate::path::AppServerPath;
pub use crate::remote::RemoteAppServerClient;
pub use crate::remote::RemoteAppServerConnectArgs;
pub use crate::remote::RemoteAppServerEndpoint;

/// Transitional access to core-only embedded app-server types.
///
/// New TUI behavior should prefer the app-server protocol methods. This
/// module exists so clients can remove a direct `codex-core` dependency
/// while legacy startup/config paths are migrated to RPCs.
pub mod legacy_core {
    pub mod config {
        pub use codex_core::config::*;

        pub mod edit {
            pub use codex_core::config::edit::*;
        }
    }
}

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
// Covers the embedded drain, its analytics flush, and final task join.
const IN_PROCESS_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(45);

/// Raw app-server request result for typed in-process requests.
///
/// Even on the in-process path, successful responses still travel back through
/// the same JSON-RPC result envelope used by socket/stdio transports because
/// `MessageProcessor` continues to produce that shape internally.
pub type RequestResult = std::result::Result<JsonRpcResult, JSONRPCErrorError>;

#[derive(Debug, Clone)]
pub enum AppServerEvent {
    Lagged { skipped: usize },
    ServerNotification(ServerNotification),
    ServerRequest(ServerRequest),
    Disconnected { message: String },
}

impl From<InProcessServerEvent> for AppServerEvent {
    fn from(value: InProcessServerEvent) -> Self {
        match value {
            InProcessServerEvent::Lagged { skipped } => Self::Lagged { skipped },
            InProcessServerEvent::ServerNotification(notification) => {
                Self::ServerNotification(notification)
            }
            InProcessServerEvent::ServerRequest(request) => Self::ServerRequest(request),
        }
    }
}

fn event_requires_delivery(event: &InProcessServerEvent) -> bool {
    // These transcript and terminal events must remain lossless. Dropping
    // streamed assistant text or the authoritative completed item can leave
    // the TUI with permanently corrupted markdown, while dropping completion
    // notifications can leave surfaces waiting forever.
    match event {
        InProcessServerEvent::ServerNotification(notification) => {
            server_notification_requires_delivery(notification)
        }
        _ => false,
    }
}

/// Returns `true` for notifications that must survive backpressure.
///
/// Server notifications are authoritative by default, including future
/// protocol additions. Command-output progress is reconstructible from its
/// completed item, and fuzzy-search updates are superseded by later full
/// snapshots; those two variants are explicitly best-effort.
///
/// Both the in-process and remote transports delegate to this function so the
/// classification stays in sync.
pub(crate) fn server_notification_requires_delivery(notification: &ServerNotification) -> bool {
    !matches!(
        notification,
        ServerNotification::CommandExecutionOutputDelta(_)
            | ServerNotification::FuzzyFileSearchSessionUpdated(_)
    )
}

/// Outcome of attempting to forward a single event to the consumer channel.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForwardEventResult {
    /// The event was delivered (or intentionally dropped); the stream is healthy.
    Continue,
    /// The consumer channel is closed; the caller should stop producing events.
    DisableStream,
}

fn skipped_event_count(event: &InProcessServerEvent) -> usize {
    match event {
        InProcessServerEvent::Lagged { skipped } => *skipped,
        _ => 1,
    }
}

/// Forwards a single in-process event to the consumer, respecting the
/// lossless/best-effort split.
///
/// Lossless events (transcript deltas, item/turn completions) block until the
/// consumer drains capacity. Best-effort events use `try_send` and increment
/// `skipped_events` on failure. When a lag marker needs to be flushed before a
/// lossless event, the flush itself blocks so the marker is never lost.
///
/// If a dropped event is a `ServerRequest`, `reject_server_request` is called
/// so the server does not wait for a response that will never come.
#[cfg(test)]
async fn forward_in_process_event<F>(
    event_tx: &mpsc::Sender<InProcessServerEvent>,
    skipped_events: &mut usize,
    event: InProcessServerEvent,
    mut reject_server_request: F,
) -> ForwardEventResult
where
    F: FnMut(ServerRequest),
{
    if *skipped_events > 0 {
        if event_requires_delivery(&event) {
            if event_tx
                .send(InProcessServerEvent::Lagged {
                    skipped: *skipped_events,
                })
                .await
                .is_err()
            {
                return ForwardEventResult::DisableStream;
            }
            *skipped_events = 0;
        } else {
            match event_tx.try_send(InProcessServerEvent::Lagged {
                skipped: *skipped_events,
            }) {
                Ok(()) => {
                    *skipped_events = 0;
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    *skipped_events = skipped_events.saturating_add(skipped_event_count(&event));
                    warn!("dropping in-process app-server event because consumer queue is full");
                    if let InProcessServerEvent::ServerRequest(request) = event {
                        reject_server_request(request);
                    }
                    return ForwardEventResult::Continue;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    return ForwardEventResult::DisableStream;
                }
            }
        }
    }

    if event_requires_delivery(&event) {
        if event_tx.send(event).await.is_err() {
            return ForwardEventResult::DisableStream;
        }
        return ForwardEventResult::Continue;
    }

    match event_tx.try_send(event) {
        Ok(()) => ForwardEventResult::Continue,
        Err(mpsc::error::TrySendError::Full(event)) => {
            *skipped_events = skipped_events.saturating_add(skipped_event_count(&event));
            warn!("dropping in-process app-server event because consumer queue is full");
            if let InProcessServerEvent::ServerRequest(request) = event {
                reject_server_request(request);
            }
            ForwardEventResult::Continue
        }
        Err(mpsc::error::TrySendError::Closed(_)) => ForwardEventResult::DisableStream,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TryForwardEventResult {
    Forwarded,
    Pending,
    Closed,
}

/// Attempts to forward one worker event without waiting for consumer capacity.
fn try_forward_in_process_event<F>(
    event_tx: &mpsc::Sender<InProcessServerEvent>,
    skipped_events: &mut usize,
    pending_event: &mut Option<InProcessServerEvent>,
    event: InProcessServerEvent,
    mut reject_server_request: F,
) -> TryForwardEventResult
where
    F: FnMut(ServerRequest),
{
    if event_requires_delivery(&event) {
        return match event_tx.try_send(event) {
            Ok(()) => TryForwardEventResult::Forwarded,
            Err(mpsc::error::TrySendError::Full(event)) => {
                debug_assert!(pending_event.is_none());
                *pending_event = Some(event);
                TryForwardEventResult::Pending
            }
            Err(mpsc::error::TrySendError::Closed(_)) => TryForwardEventResult::Closed,
        };
    }

    match event_tx.try_send(event) {
        Ok(()) => TryForwardEventResult::Forwarded,
        Err(mpsc::error::TrySendError::Full(event)) => {
            *skipped_events = skipped_events.saturating_add(skipped_event_count(&event));
            warn!("dropping in-process app-server event because consumer queue is full");
            if let InProcessServerEvent::ServerRequest(request) = event {
                reject_server_request(request);
            }
            TryForwardEventResult::Forwarded
        }
        Err(mpsc::error::TrySendError::Closed(_)) => TryForwardEventResult::Closed,
    }
}

/// Layered error for [`InProcessAppServerClient::request_typed`].
///
/// This keeps transport failures, server-side JSON-RPC failures, and response
/// decode failures distinct so callers can decide whether to retry, surface a
/// server error, or treat the response as an internal request/response mismatch.
#[derive(Debug)]
pub enum TypedRequestError {
    Transport {
        method: String,
        source: IoError,
    },
    Server {
        method: String,
        source: JSONRPCErrorError,
    },
    Deserialize {
        method: String,
        source: serde_json::Error,
    },
}

impl fmt::Display for TypedRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport { method, source } => {
                write!(f, "{method} transport error: {source}")
            }
            Self::Server { method, source } => {
                write!(
                    f,
                    "{method} failed: {} (code {})",
                    source.message, source.code
                )?;
                if let Some(data) = source.data.as_ref() {
                    write!(f, ", data: {data}")?;
                }
                Ok(())
            }
            Self::Deserialize { method, source } => {
                write!(f, "{method} response decode error: {source}")
            }
        }
    }
}

impl Error for TypedRequestError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Transport { source, .. } => Some(source),
            Self::Server { .. } => None,
            Self::Deserialize { source, .. } => Some(source),
        }
    }
}

#[derive(Clone)]
pub struct InProcessClientStartArgs {
    /// Resolved argv0 dispatch paths used by command execution internals.
    pub arg0_paths: Arg0DispatchPaths,
    /// Shared config used to initialize app-server runtime.
    pub config: Arc<Config>,
    /// CLI config overrides that are already parsed into TOML values.
    pub cli_overrides: Vec<(String, TomlValue)>,
    /// Loader override knobs used by config API paths.
    pub loader_overrides: LoaderOverrides,
    /// Whether config API paths should reject unknown config fields.
    pub strict_config: bool,
    /// Preloaded cloud config bundle provider.
    pub cloud_config_bundle: CloudConfigBundleLoader,
    /// Feedback sink used by app-server/core telemetry and logs.
    pub feedback: CodexFeedback,
    /// SQLite tracing layer used to flush recently emitted logs before feedback upload.
    pub log_db: Option<LogDbLayer>,
    /// Process-wide SQLite state handle shared with the embedded app-server.
    pub state_db: Option<StateDbHandle>,
    /// Environment manager used by core execution and filesystem operations.
    pub environment_manager: Arc<EnvironmentManager>,
    /// Startup warnings emitted after initialize succeeds.
    pub config_warnings: Vec<ConfigWarningNotification>,
    /// Session source recorded in app-server thread metadata.
    pub session_source: SessionSource,
    /// Whether auth loading should honor the `CODEX_API_KEY` environment variable.
    pub enable_codex_api_key_env: bool,
    /// Client name reported during initialize.
    pub client_name: String,
    /// Client version reported during initialize.
    pub client_version: String,
    /// Whether experimental APIs are requested at initialize time.
    pub experimental_api: bool,
    /// Whether MCP servers may send `openai/form` elicitation requests.
    pub mcp_server_openai_form_elicitation: bool,
    /// Notification methods this client opts out of receiving.
    pub opt_out_notification_methods: Vec<String>,
    /// Queue capacity for command/event channels (clamped to at least 1).
    pub channel_capacity: usize,
}

fn configured_thread_config_loader(config: &Config) -> Arc<dyn ThreadConfigLoader> {
    match config.experimental_thread_config_endpoint.as_deref() {
        Some(endpoint) => Arc::new(RemoteThreadConfigLoader::new(endpoint)),
        None => Arc::new(NoopThreadConfigLoader),
    }
}

impl InProcessClientStartArgs {
    /// Builds initialize params from caller-provided metadata.
    pub fn initialize_params(&self) -> InitializeParams {
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

    fn into_runtime_start_args(self) -> InProcessStartArgs {
        let initialize = self.initialize_params();
        let thread_config_loader = configured_thread_config_loader(&self.config);
        InProcessStartArgs {
            arg0_paths: self.arg0_paths,
            config: self.config,
            cli_overrides: self.cli_overrides,
            loader_overrides: self.loader_overrides,
            strict_config: self.strict_config,
            cloud_config_bundle: self.cloud_config_bundle,
            thread_config_loader,
            feedback: self.feedback,
            log_db: self.log_db,
            state_db: self.state_db,
            environment_manager: self.environment_manager,
            config_warnings: self.config_warnings,
            session_source: self.session_source,
            enable_codex_api_key_env: self.enable_codex_api_key_env,
            initialize,
            channel_capacity: self.channel_capacity,
        }
    }
}

/// Internal command sent from public facade methods to the worker task.
///
/// Each variant carries a oneshot sender so the caller can `await` the
/// result without holding a mutable reference to the client.
enum ClientCommand {
    Request {
        request: Box<ClientRequest>,
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
    #[cfg(test)]
    InjectServerEvent {
        event: InProcessServerEvent,
        started_tx: oneshot::Sender<()>,
        response_tx: oneshot::Sender<TryForwardEventResult>,
    },
}

/// Async facade over the in-process app-server runtime.
///
/// This type owns a worker task that bridges between:
/// - caller-facing async `mpsc` channels used by TUI/exec
/// - [`codex_app_server::in_process::InProcessClientHandle`], which speaks to
///   the embedded `MessageProcessor`
///
/// The facade intentionally preserves the server's request/notification/event
/// model instead of exposing direct core runtime handles. That keeps in-process
/// callers aligned with app-server behavior while still avoiding a process
/// boundary.
pub struct InProcessAppServerClient {
    command_tx: mpsc::Sender<ClientCommand>,
    event_rx: mpsc::Receiver<InProcessServerEvent>,
    worker_handle: tokio::task::JoinHandle<()>,
    #[cfg(test)]
    _test_pending_required_event: Arc<tokio::sync::Notify>,
}

#[derive(Clone)]
pub struct InProcessAppServerRequestHandle {
    command_tx: mpsc::Sender<ClientCommand>,
}

#[derive(Clone)]
pub enum AppServerRequestHandle {
    InProcess(InProcessAppServerRequestHandle),
    Remote(crate::remote::RemoteAppServerRequestHandle),
}

pub enum AppServerClient {
    InProcess(InProcessAppServerClient),
    Remote(RemoteAppServerClient),
}

impl InProcessAppServerClient {
    /// Starts the in-process runtime and facade worker task.
    ///
    /// The returned client is ready for requests and event consumption. If the
    /// internal event queue is saturated later, server requests are rejected
    /// with overload error instead of being silently dropped.
    pub async fn start(args: InProcessClientStartArgs) -> IoResult<Self> {
        let channel_capacity = args.channel_capacity.max(1);
        let mut handle =
            codex_app_server::in_process::start(args.into_runtime_start_args()).await?;
        let request_sender = handle.sender();
        let (command_tx, mut command_rx) = mpsc::channel::<ClientCommand>(channel_capacity);
        let (event_tx, event_rx) = mpsc::channel::<InProcessServerEvent>(channel_capacity);
        #[cfg(test)]
        let test_pending_required_event = Arc::new(tokio::sync::Notify::new());
        #[cfg(test)]
        let worker_test_pending_required_event = Arc::clone(&test_pending_required_event);

        let worker_handle = tokio::spawn(async move {
            let mut event_stream_enabled = true;
            let mut skipped_events = 0usize;
            let mut pending_required_event = None::<InProcessServerEvent>;
            loop {
                tokio::select! {
                    permit = event_tx.reserve(), if skipped_events > 0 || pending_required_event.is_some() => {
                        match permit {
                            Ok(permit) => {
                                if skipped_events > 0 {
                                    permit.send(InProcessServerEvent::Lagged {
                                        skipped: std::mem::take(&mut skipped_events),
                                    });
                                } else if let Some(event) = pending_required_event.take() {
                                    permit.send(event);
                                } else {
                                    continue;
                                }
                            }
                            Err(_) => {
                                skipped_events = 0;
                                pending_required_event = None;
                                event_stream_enabled = false;
                            }
                        }
                    }
                    command = command_rx.recv() => {
                        match command {
                            Some(ClientCommand::Request { request, response_tx }) => {
                                let request_sender = request_sender.clone();
                                // Request waits happen on a detached task so
                                // this loop can keep draining runtime events
                                // while the request is blocked on client input.
                                tokio::spawn(async move {
                                    let result = request_sender.request(*request).await;
                                    let _ = response_tx.send(result);
                                });
                            }
                            Some(ClientCommand::Notify {
                                notification,
                                response_tx,
                            }) => {
                                let result = request_sender.notify(notification);
                                let _ = response_tx.send(result);
                            }
                            Some(ClientCommand::ResolveServerRequest {
                                request_id,
                                result,
                                response_tx,
                            }) => {
                                let send_result =
                                    request_sender.respond_to_server_request(request_id, result);
                                let _ = response_tx.send(send_result);
                            }
                            Some(ClientCommand::RejectServerRequest {
                                request_id,
                                error,
                                response_tx,
                            }) => {
                                let send_result = request_sender.fail_server_request(request_id, error);
                                let _ = response_tx.send(send_result);
                            }
                            Some(ClientCommand::Shutdown { response_tx }) => {
                                let shutdown_result = handle.shutdown().await;
                                let _ = response_tx.send(shutdown_result);
                                break;
                            }
                            #[cfg(test)]
                            Some(ClientCommand::InjectServerEvent {
                                event,
                                started_tx,
                                response_tx,
                            }) => {
                                let _ = started_tx.send(());
                                assert!(
                                    skipped_events == 0 && pending_required_event.is_none(),
                                    "test injection requires an empty facade delivery state",
                                );
                                let result = try_forward_in_process_event(
                                    &event_tx,
                                    &mut skipped_events,
                                    &mut pending_required_event,
                                    event,
                                    |_| {},
                                );
                                if result == TryForwardEventResult::Pending {
                                    worker_test_pending_required_event.notify_one();
                                } else if result == TryForwardEventResult::Closed {
                                    event_stream_enabled = false;
                                }
                                let _ = response_tx.send(result);
                            }
                            None => {
                                let _ = handle.shutdown().await;
                                break;
                            }
                        }
                    }
                    event = handle.next_event(), if event_stream_enabled && skipped_events == 0 && pending_required_event.is_none() => {
                        let Some(event) = event else {
                            break;
                        };
                        if let InProcessServerEvent::ServerRequest(
                            ServerRequest::ChatgptAuthTokensRefresh { request_id, .. }
                        ) = &event
                        {
                            let send_result = request_sender.fail_server_request(
                                request_id.clone(),
                                JSONRPCErrorError {
                                    code: -32000,
                                    message: "chatgpt auth token refresh is not supported for in-process app-server clients".to_string(),
                                    data: None,
                                },
                            );
                            if let Err(err) = send_result {
                                warn!(
                                    "failed to reject unsupported chatgpt auth token refresh request: {err}"
                                );
                            }
                            continue;
                        }

                        match try_forward_in_process_event(
                            &event_tx,
                            &mut skipped_events,
                            &mut pending_required_event,
                            event,
                            |request| {
                                let _ = request_sender.fail_server_request(
                                    request.id().clone(),
                                    JSONRPCErrorError {
                                        code: -32001,
                                        message: "in-process app-server event queue is full"
                                            .to_string(),
                                        data: None,
                                    },
                                );
                            },
                        ) {
                            TryForwardEventResult::Forwarded => {}
                            TryForwardEventResult::Pending => {
                                #[cfg(test)]
                                worker_test_pending_required_event.notify_one();
                            }
                            TryForwardEventResult::Closed => {
                                event_stream_enabled = false;
                            }
                        }
                    }
                }
            }
        });

        Ok(Self {
            command_tx,
            event_rx,
            worker_handle,
            #[cfg(test)]
            _test_pending_required_event: test_pending_required_event,
        })
    }

    pub fn request_handle(&self) -> InProcessAppServerRequestHandle {
        InProcessAppServerRequestHandle {
            command_tx: self.command_tx.clone(),
        }
    }

    /// Sends a typed client request and returns raw JSON-RPC result.
    ///
    /// Callers that expect a concrete response type should usually prefer
    /// [`request_typed`](Self::request_typed).
    pub async fn request(&self, request: ClientRequest) -> IoResult<RequestResult> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ClientCommand::Request {
                request: Box::new(request),
                response_tx,
            })
            .await
            .map_err(|_| {
                IoError::new(
                    ErrorKind::BrokenPipe,
                    "in-process app-server worker channel is closed",
                )
            })?;
        response_rx.await.map_err(|_| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "in-process app-server request channel is closed",
            )
        })?
    }

    /// Sends a typed client request and decodes the successful response body.
    ///
    /// This still deserializes from a JSON value produced by app-server's
    /// JSON-RPC result envelope. Because the caller chooses `T`, `Deserialize`
    /// failures indicate an internal request/response mismatch at the call site
    /// (or an in-process bug), not transport skew from an external client.
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

    /// Sends a typed client notification.
    pub async fn notify(&self, notification: ClientNotification) -> IoResult<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ClientCommand::Notify {
                notification,
                response_tx,
            })
            .await
            .map_err(|_| {
                IoError::new(
                    ErrorKind::BrokenPipe,
                    "in-process app-server worker channel is closed",
                )
            })?;
        response_rx.await.map_err(|_| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "in-process app-server notify channel is closed",
            )
        })?
    }

    /// Resolves a pending server request.
    ///
    /// This should only be called with request IDs obtained from the current
    /// client's event stream.
    pub async fn resolve_server_request(
        &self,
        request_id: RequestId,
        result: JsonRpcResult,
    ) -> IoResult<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ClientCommand::ResolveServerRequest {
                request_id,
                result,
                response_tx,
            })
            .await
            .map_err(|_| {
                IoError::new(
                    ErrorKind::BrokenPipe,
                    "in-process app-server worker channel is closed",
                )
            })?;
        response_rx.await.map_err(|_| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "in-process app-server resolve channel is closed",
            )
        })?
    }

    /// Rejects a pending server request with JSON-RPC error payload.
    pub async fn reject_server_request(
        &self,
        request_id: RequestId,
        error: JSONRPCErrorError,
    ) -> IoResult<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ClientCommand::RejectServerRequest {
                request_id,
                error,
                response_tx,
            })
            .await
            .map_err(|_| {
                IoError::new(
                    ErrorKind::BrokenPipe,
                    "in-process app-server worker channel is closed",
                )
            })?;
        response_rx.await.map_err(|_| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "in-process app-server reject channel is closed",
            )
        })?
    }

    /// Returns the next in-process event, or `None` when worker exits.
    ///
    /// Callers are expected to drain this stream promptly. If they fall behind,
    /// the worker emits [`InProcessServerEvent::Lagged`] markers and may reject
    /// pending server requests rather than letting approval flows hang.
    pub async fn next_event(&mut self) -> Option<InProcessServerEvent> {
        self.event_rx.recv().await
    }

    /// Shuts down worker and in-process runtime with bounded wait.
    ///
    /// If graceful shutdown exceeds timeout, the worker task is aborted to
    /// avoid leaking background tasks in embedding callers.
    pub async fn shutdown(self) -> IoResult<()> {
        let Self {
            command_tx,
            event_rx,
            worker_handle,
            ..
        } = self;
        let mut worker_handle = worker_handle;
        // Drop the caller-facing receiver before asking the worker to shut
        // down. This releases any retained required event or pending lag state
        // before the worker reaches `handle.shutdown()`.
        drop(event_rx);
        let (response_tx, response_rx) = oneshot::channel();
        if command_tx
            .send(ClientCommand::Shutdown { response_tx })
            .await
            .is_ok()
            && let Ok(command_result) = timeout(IN_PROCESS_SHUTDOWN_TIMEOUT, response_rx).await
        {
            command_result.map_err(|_| {
                IoError::new(
                    ErrorKind::BrokenPipe,
                    "in-process app-server shutdown channel is closed",
                )
            })??;
        }

        if let Err(_elapsed) = timeout(IN_PROCESS_SHUTDOWN_TIMEOUT, &mut worker_handle).await {
            worker_handle.abort();
            let _ = worker_handle.await;
        }
        Ok(())
    }
}

impl InProcessAppServerRequestHandle {
    pub async fn request(&self, request: ClientRequest) -> IoResult<RequestResult> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(ClientCommand::Request {
                request: Box::new(request),
                response_tx,
            })
            .await
            .map_err(|_| {
                IoError::new(
                    ErrorKind::BrokenPipe,
                    "in-process app-server worker channel is closed",
                )
            })?;
        response_rx.await.map_err(|_| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "in-process app-server request channel is closed",
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

impl AppServerRequestHandle {
    pub async fn request(&self, request: ClientRequest) -> IoResult<RequestResult> {
        match self {
            Self::InProcess(handle) => handle.request(request).await,
            Self::Remote(handle) => handle.request(request).await,
        }
    }

    pub async fn request_typed<T>(&self, request: ClientRequest) -> Result<T, TypedRequestError>
    where
        T: DeserializeOwned,
    {
        match self {
            Self::InProcess(handle) => handle.request_typed(request).await,
            Self::Remote(handle) => handle.request_typed(request).await,
        }
    }
}

impl AppServerClient {
    pub fn codex_home(&self, local_codex_home: &AbsolutePathBuf) -> Option<AppServerPath> {
        match self {
            Self::InProcess(_) => Some(AppServerPath::from_app_server(
                local_codex_home.display().to_string(),
            )),
            Self::Remote(client) => client.codex_home().map(AppServerPath::from_app_server),
        }
    }

    pub async fn request(&self, request: ClientRequest) -> IoResult<RequestResult> {
        match self {
            Self::InProcess(client) => client.request(request).await,
            Self::Remote(client) => client.request(request).await,
        }
    }

    pub async fn request_typed<T>(&self, request: ClientRequest) -> Result<T, TypedRequestError>
    where
        T: DeserializeOwned,
    {
        match self {
            Self::InProcess(client) => client.request_typed(request).await,
            Self::Remote(client) => client.request_typed(request).await,
        }
    }

    pub async fn notify(&self, notification: ClientNotification) -> IoResult<()> {
        match self {
            Self::InProcess(client) => client.notify(notification).await,
            Self::Remote(client) => client.notify(notification).await,
        }
    }

    pub async fn resolve_server_request(
        &self,
        request_id: RequestId,
        result: JsonRpcResult,
    ) -> IoResult<()> {
        match self {
            Self::InProcess(client) => client.resolve_server_request(request_id, result).await,
            Self::Remote(client) => client.resolve_server_request(request_id, result).await,
        }
    }

    pub async fn reject_server_request(
        &self,
        request_id: RequestId,
        error: JSONRPCErrorError,
    ) -> IoResult<()> {
        match self {
            Self::InProcess(client) => client.reject_server_request(request_id, error).await,
            Self::Remote(client) => client.reject_server_request(request_id, error).await,
        }
    }

    pub async fn next_event(&mut self) -> Option<AppServerEvent> {
        match self {
            Self::InProcess(client) => client.next_event().await.map(Into::into),
            Self::Remote(client) => client.next_event().await,
        }
    }

    pub async fn shutdown(self) -> IoResult<()> {
        match self {
            Self::InProcess(client) => client.shutdown().await,
            Self::Remote(client) => client.shutdown().await,
        }
    }

    pub fn request_handle(&self) -> AppServerRequestHandle {
        match self {
            Self::InProcess(client) => AppServerRequestHandle::InProcess(client.request_handle()),
            Self::Remote(client) => AppServerRequestHandle::Remote(client.request_handle()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::AccountUpdatedNotification;
    use codex_app_server_protocol::ConfigRequirementsReadResponse;
    use codex_app_server_protocol::GetAccountResponse;
    use codex_app_server_protocol::JSONRPCMessage;
    use codex_app_server_protocol::JSONRPCRequest;
    use codex_app_server_protocol::JSONRPCResponse;
    use codex_app_server_protocol::ServerNotification;
    use codex_app_server_protocol::SessionSource as ApiSessionSource;
    use codex_app_server_protocol::ThreadStartParams;
    use codex_app_server_protocol::ThreadStartResponse;
    use codex_app_server_protocol::ToolRequestUserInputParams;
    use codex_app_server_protocol::ToolRequestUserInputQuestion;
    use codex_core::config::ConfigBuilder;
    use codex_core::init_state_db;
    use codex_uds::UnixListener;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use futures::SinkExt;
    use futures::StreamExt;
    use pretty_assertions::assert_eq;
    use std::ops::Deref;
    use std::path::Path;
    use tempfile::TempDir;
    use tokio::net::TcpListener;
    use tokio::time::Duration;
    use tokio::time::timeout;
    use tokio_tungstenite::accept_async;
    use tokio_tungstenite::accept_hdr_async;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::handshake::server::Request as WebSocketRequest;
    use tokio_tungstenite::tungstenite::handshake::server::Response as WebSocketResponse;
    use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;

    async fn build_test_config() -> Config {
        match ConfigBuilder::default().build().await {
            Ok(config) => config,
            Err(_) => Config::load_default_with_cli_overrides(Vec::new())
                .await
                .expect("default config should load"),
        }
    }

    async fn build_test_config_for_codex_home(codex_home: &Path) -> Config {
        match ConfigBuilder::default()
            .codex_home(codex_home.to_path_buf())
            .build()
            .await
        {
            Ok(config) => config,
            Err(_) => Config::load_default_with_cli_overrides_for_codex_home(
                codex_home.to_path_buf(),
                Vec::new(),
            )
            .await
            .expect("default config should load"),
        }
    }

    struct TestClient {
        _codex_home: TempDir,
        client: InProcessAppServerClient,
    }

    impl Deref for TestClient {
        type Target = InProcessAppServerClient;

        fn deref(&self) -> &Self::Target {
            &self.client
        }
    }

    impl TestClient {
        async fn shutdown(self) -> IoResult<()> {
            self.client.shutdown().await
        }
    }

    async fn start_test_client_with_capacity(
        session_source: SessionSource,
        channel_capacity: usize,
    ) -> TestClient {
        let codex_home = TempDir::new().expect("temp dir");
        let config = Arc::new(build_test_config_for_codex_home(codex_home.path()).await);
        let state_db = init_state_db(config.as_ref())
            .await
            .expect("state db should initialize for in-process test");
        let client = InProcessAppServerClient::start(InProcessClientStartArgs {
            arg0_paths: Arg0DispatchPaths::default(),
            config,
            cli_overrides: Vec::new(),
            loader_overrides: LoaderOverrides::default(),
            strict_config: false,
            cloud_config_bundle: CloudConfigBundleLoader::default(),
            feedback: CodexFeedback::new(),
            log_db: None,
            state_db: Some(state_db),
            environment_manager: Arc::new(EnvironmentManager::default_for_tests()),
            config_warnings: Vec::new(),
            session_source,
            enable_codex_api_key_env: false,
            client_name: "codex-app-server-client-test".to_string(),
            client_version: "0.0.0-test".to_string(),
            experimental_api: true,
            mcp_server_openai_form_elicitation: false,
            opt_out_notification_methods: Vec::new(),
            channel_capacity,
        })
        .await
        .expect("in-process app-server client should start");

        TestClient {
            _codex_home: codex_home,
            client,
        }
    }

    async fn start_test_client(session_source: SessionSource) -> TestClient {
        start_test_client_with_capacity(session_source, DEFAULT_IN_PROCESS_CHANNEL_CAPACITY).await
    }

    fn required_thread_closed_event(thread_id: &str) -> InProcessServerEvent {
        InProcessServerEvent::ServerNotification(ServerNotification::ThreadClosed(
            codex_app_server_protocol::ThreadClosedNotification {
                thread_id: thread_id.to_string(),
            },
        ))
    }

    async fn start_test_client_with_pending_facade_event() -> TestClient {
        let client =
            start_test_client_with_capacity(SessionSource::Cli, /*channel_capacity*/ 1).await;

        let (first_started_tx, first_started_rx) = oneshot::channel();
        let (first_response_tx, first_response_rx) = oneshot::channel();
        client
            .command_tx
            .send(ClientCommand::InjectServerEvent {
                event: required_thread_closed_event("queued"),
                started_tx: first_started_tx,
                response_tx: first_response_tx,
            })
            .await
            .expect("first injection should reach the worker");
        first_started_rx
            .await
            .expect("first injection should start");
        assert_eq!(
            first_response_rx
                .await
                .expect("first injection should finish"),
            TryForwardEventResult::Forwarded
        );

        let pending_observed = client._test_pending_required_event.notified();
        let (second_started_tx, second_started_rx) = oneshot::channel();
        let (second_response_tx, second_response_rx) = oneshot::channel();
        client
            .command_tx
            .send(ClientCommand::InjectServerEvent {
                event: required_thread_closed_event("pending"),
                started_tx: second_started_tx,
                response_tx: second_response_tx,
            })
            .await
            .expect("second injection should reach the worker");
        second_started_rx
            .await
            .expect("second injection should start");
        assert_eq!(
            second_response_rx
                .await
                .expect("second injection should enter worker custody"),
            TryForwardEventResult::Pending
        );
        timeout(Duration::from_secs(1), pending_observed)
            .await
            .expect("worker should report finite pending custody");

        client
    }

    async fn assert_pending_facade_events_deliver_in_fifo(client: &mut TestClient) {
        let first = timeout(Duration::from_secs(1), client.client.next_event())
            .await
            .expect("queued event should arrive")
            .expect("facade event stream should stay open");
        let second = timeout(Duration::from_secs(1), client.client.next_event())
            .await
            .expect("pending event should arrive after capacity is released")
            .expect("facade event stream should stay open");
        assert!(matches!(
            first,
            InProcessServerEvent::ServerNotification(ServerNotification::ThreadClosed(
                notification
            )) if notification.thread_id == "queued"
        ));
        assert!(matches!(
            second,
            InProcessServerEvent::ServerNotification(ServerNotification::ThreadClosed(
                notification
            )) if notification.thread_id == "pending"
        ));
    }

    async fn start_test_remote_server<F, Fut>(handler: F) -> String
    where
        F: FnOnce(tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> Fut
            + Send
            + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        start_test_remote_server_with_auth(/*expected_auth_token*/ None, handler).await
    }

    async fn start_test_remote_server_with_auth<F, Fut>(
        expected_auth_token: Option<String>,
        handler: F,
    ) -> String
    where
        F: FnOnce(tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> Fut
            + Send
            + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener.local_addr().expect("listener address");
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept should succeed");
            let websocket = accept_hdr_async(
                stream,
                move |request: &WebSocketRequest, response: WebSocketResponse| {
                    let provided_auth_token = request
                        .headers()
                        .get(AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    let expected_auth_token = expected_auth_token
                        .as_ref()
                        .map(|token| format!("Bearer {token}"));
                    assert_eq!(provided_auth_token, expected_auth_token);
                    Ok(response)
                },
            )
            .await
            .expect("websocket upgrade should succeed");
            handler(websocket).await;
        });
        format!("ws://{addr}")
    }

    async fn expect_remote_initialize<S>(websocket: &mut tokio_tungstenite::WebSocketStream<S>)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let JSONRPCMessage::Request(request) = read_websocket_message(websocket).await else {
            panic!("expected initialize request");
        };
        assert_eq!(request.method, "initialize");
        write_websocket_message(
            websocket,
            JSONRPCMessage::Response(JSONRPCResponse {
                id: request.id,
                result: serde_json::json!({
                    "userAgent": "codex_cli_rs/9.8.7-test (Test OS; x86_64) rust",
                    "codexHome": "/server/.codex",
                }),
            }),
        )
        .await;

        let JSONRPCMessage::Notification(notification) = read_websocket_message(websocket).await
        else {
            panic!("expected initialized notification");
        };
        assert_eq!(notification.method, "initialized");
    }

    async fn read_websocket_message<S>(
        websocket: &mut tokio_tungstenite::WebSocketStream<S>,
    ) -> JSONRPCMessage
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        loop {
            let frame = websocket
                .next()
                .await
                .expect("frame should be available")
                .expect("frame should decode");
            match frame {
                Message::Text(text) => {
                    return serde_json::from_str::<JSONRPCMessage>(&text)
                        .expect("text frame should be valid JSON-RPC");
                }
                Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {
                    continue;
                }
                Message::Close(_) => panic!("unexpected close frame"),
            }
        }
    }

    async fn write_websocket_message<S>(
        websocket: &mut tokio_tungstenite::WebSocketStream<S>,
        message: JSONRPCMessage,
    ) where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        websocket
            .send(Message::Text(
                serde_json::to_string(&message)
                    .expect("message should serialize")
                    .into(),
            ))
            .await
            .expect("message should send");
    }

    async fn write_remote_server_notification<S>(
        websocket: &mut tokio_tungstenite::WebSocketStream<S>,
        notification: ServerNotification,
    ) where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        write_websocket_message(
            websocket,
            JSONRPCMessage::Notification(
                serde_json::from_value(
                    serde_json::to_value(notification).expect("notification should serialize"),
                )
                .expect("notification should convert to JSON-RPC"),
            ),
        )
        .await;
    }

    async fn expect_websocket_close<S>(websocket: &mut tokio_tungstenite::WebSocketStream<S>)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        loop {
            let frame = websocket
                .next()
                .await
                .expect("close frame should be available")
                .expect("close frame should decode");
            match frame {
                Message::Close(_) => return,
                Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                Message::Text(text) => panic!("unexpected text before close: {text}"),
            }
        }
    }

    fn remote_thread_closed_notification(thread_id: &str) -> ServerNotification {
        ServerNotification::ThreadClosed(codex_app_server_protocol::ThreadClosedNotification {
            thread_id: thread_id.to_string(),
        })
    }

    fn remote_get_account_request(request_id: i64) -> ClientRequest {
        ClientRequest::GetAccount {
            request_id: RequestId::Integer(request_id),
            params: codex_app_server_protocol::GetAccountParams {
                refresh_token: false,
            },
        }
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

    fn fuzzy_file_search_session_updated_notification(query: &str) -> ServerNotification {
        ServerNotification::FuzzyFileSearchSessionUpdated(
            codex_app_server_protocol::FuzzyFileSearchSessionUpdatedNotification {
                session_id: "search".to_string(),
                query: query.to_string(),
                files: Vec::new(),
            },
        )
    }

    fn reviewed_consumer_state_notifications() -> Vec<ServerNotification> {
        let thread_id = || "thread".to_string();
        let turn_id = || "turn".to_string();
        let item_id = || "item".to_string();
        let token_usage = || {
            let usage = codex_app_server_protocol::TokenUsageBreakdown {
                total_tokens: 25,
                input_tokens: 20,
                cached_input_tokens: 0,
                cache_write_input_tokens: 0,
                output_tokens: 5,
                reasoning_output_tokens: 0,
            };
            codex_app_server_protocol::ThreadTokenUsage {
                total: usage.clone(),
                last: usage,
                model_context_window: Some(100),
            }
        };

        vec![
            ServerNotification::ThreadStarted(
                codex_app_server_protocol::ThreadStartedNotification {
                    thread: codex_app_server_protocol::Thread {
                        id: thread_id(),
                        extra: None,
                        session_id: thread_id(),
                        forked_from_id: None,
                        parent_thread_id: Some("parent".to_string()),
                        preview: "child".to_string(),
                        ephemeral: false,
                        is_pinned: false,
                        history_mode: Default::default(),
                        model_provider: "openai".to_string(),
                        model: Some("gpt-test".to_string()),
                        reasoning_effort: None,
                        created_at: 0,
                        updated_at: 0,
                        recency_at: None,
                        status: codex_app_server_protocol::ThreadStatus::Idle,
                        path: None,
                        cwd: AbsolutePathBuf::from_absolute_path("/tmp")
                            .expect("test cwd should be absolute"),
                        cli_version: "test".to_string(),
                        source: ApiSessionSource::Unknown,
                        can_accept_direct_input: None,
                        thread_source: None,
                        agent_nickname: Some("Child".to_string()),
                        agent_role: Some("explorer".to_string()),
                        git_info: None,
                        name: None,
                        turns: Vec::new(),
                    },
                },
            ),
            ServerNotification::ThreadClosed(codex_app_server_protocol::ThreadClosedNotification {
                thread_id: thread_id(),
            }),
            ServerNotification::ThreadDeleted(
                codex_app_server_protocol::ThreadDeletedNotification {
                    thread_id: thread_id(),
                },
            ),
            ServerNotification::ThreadArchived(
                codex_app_server_protocol::ThreadArchivedNotification {
                    thread_id: thread_id(),
                },
            ),
            ServerNotification::ThreadUnarchived(
                codex_app_server_protocol::ThreadUnarchivedNotification {
                    thread_id: thread_id(),
                },
            ),
            ServerNotification::ThreadStatusChanged(
                codex_app_server_protocol::ThreadStatusChangedNotification {
                    thread_id: thread_id(),
                    status: codex_app_server_protocol::ThreadStatus::Idle,
                },
            ),
            ServerNotification::ThreadGoalUpdated(
                codex_app_server_protocol::ThreadGoalUpdatedNotification {
                    thread_id: thread_id(),
                    turn_id: Some(turn_id()),
                    goal: codex_app_server_protocol::ThreadGoal {
                        thread_id: thread_id(),
                        objective: "goal".to_string(),
                        status: codex_app_server_protocol::ThreadGoalStatus::Active,
                        token_budget: Some(100),
                        tokens_used: 25,
                        time_used_seconds: 1,
                        created_at: 0,
                        updated_at: 0,
                    },
                },
            ),
            ServerNotification::ThreadGoalCleared(
                codex_app_server_protocol::ThreadGoalClearedNotification {
                    thread_id: thread_id(),
                },
            ),
            ServerNotification::ThreadTokenUsageUpdated(
                codex_app_server_protocol::ThreadTokenUsageUpdatedNotification {
                    thread_id: thread_id(),
                    turn_id: turn_id(),
                    token_usage: token_usage(),
                },
            ),
            ServerNotification::ThreadNameUpdated(
                codex_app_server_protocol::ThreadNameUpdatedNotification {
                    thread_id: thread_id(),
                    thread_name: Some("renamed".to_string()),
                },
            ),
            ServerNotification::ServerRequestResolved(
                codex_app_server_protocol::ServerRequestResolvedNotification {
                    thread_id: thread_id(),
                    request_id: RequestId::Integer(7),
                },
            ),
            ServerNotification::AccountRateLimitsUpdated(
                codex_app_server_protocol::AccountRateLimitsUpdatedNotification {
                    rate_limits: codex_app_server_protocol::RateLimitSnapshot {
                        limit_id: Some("codex".to_string()),
                        limit_name: None,
                        primary: None,
                        secondary: None,
                        credits: None,
                        individual_limit: None,
                        spend_control_reached: Some(true),
                        plan_type: None,
                        rate_limit_reached_type: None,
                    },
                },
            ),
            ServerNotification::AccountLoginCompleted(
                codex_app_server_protocol::AccountLoginCompletedNotification {
                    login_id: Some("login".to_string()),
                    success: true,
                    error: None,
                },
            ),
            ServerNotification::AccountUpdated(
                codex_app_server_protocol::AccountUpdatedNotification {
                    auth_mode: None,
                    plan_type: None,
                },
            ),
            ServerNotification::McpServerOauthLoginCompleted(
                codex_app_server_protocol::McpServerOauthLoginCompletedNotification {
                    name: "server".to_string(),
                    thread_id: Some(thread_id()),
                    success: true,
                    error: None,
                },
            ),
            ServerNotification::ProcessExited(
                codex_app_server_protocol::ProcessExitedNotification {
                    process_handle: "process".to_string(),
                    exit_code: 0,
                    stdout: String::new(),
                    stdout_cap_reached: false,
                    stderr: String::new(),
                    stderr_cap_reached: false,
                },
            ),
            ServerNotification::FuzzyFileSearchSessionCompleted(
                codex_app_server_protocol::FuzzyFileSearchSessionCompletedNotification {
                    session_id: "search".to_string(),
                },
            ),
            ServerNotification::WindowsSandboxSetupCompleted(
                codex_app_server_protocol::WindowsSandboxSetupCompletedNotification {
                    mode: codex_app_server_protocol::WindowsSandboxSetupMode::Unelevated,
                    success: true,
                    error: None,
                },
            ),
            ServerNotification::TurnStarted(codex_app_server_protocol::TurnStartedNotification {
                thread_id: thread_id(),
                turn: codex_app_server_protocol::Turn {
                    id: turn_id(),
                    items: Vec::new(),
                    items_view: codex_app_server_protocol::TurnItemsView::Full,
                    status: codex_app_server_protocol::TurnStatus::InProgress,
                    error: None,
                    started_at: Some(0),
                    completed_at: None,
                    duration_ms: None,
                },
            }),
            turn_completed_notification(),
            ServerNotification::ThreadSettingsUpdated(
                codex_app_server_protocol::ThreadSettingsUpdatedNotification {
                    thread_id: thread_id(),
                    thread_settings: codex_app_server_protocol::ThreadSettings {
                        cwd: AbsolutePathBuf::from_absolute_path("/tmp")
                            .expect("test cwd should be absolute"),
                        approval_policy: codex_app_server_protocol::AskForApproval::Never,
                        approvals_reviewer: codex_app_server_protocol::ApprovalsReviewer::User,
                        sandbox_policy: codex_app_server_protocol::SandboxPolicy::DangerFullAccess,
                        active_permission_profile: None,
                        model: "gpt-test".to_string(),
                        model_provider: "openai".to_string(),
                        service_tier: None,
                        effort: None,
                        summary: None,
                        collaboration_mode: codex_protocol::config_types::CollaborationMode {
                            mode: codex_protocol::config_types::ModeKind::Default,
                            settings: codex_protocol::config_types::Settings {
                                model: "gpt-test".to_string(),
                                reasoning_effort: None,
                                developer_instructions: None,
                            },
                        },
                        multi_agent_mode: Default::default(),
                        personality: None,
                    },
                },
            ),
            ServerNotification::ItemStarted(codex_app_server_protocol::ItemStartedNotification {
                thread_id: thread_id(),
                turn_id: turn_id(),
                started_at_ms: 0,
                item: codex_app_server_protocol::ThreadItem::AgentMessage {
                    id: item_id(),
                    text: "assistant".to_string(),
                    phase: None,
                    memory_citation: None,
                },
            }),
            item_completed_notification("complete transcript"),
            ServerNotification::ExternalAgentConfigImportCompleted(
                codex_app_server_protocol::ExternalAgentConfigImportCompletedNotification {
                    import_id: "import".to_string(),
                    item_type_results: Vec::new(),
                },
            ),
            agent_message_delta_notification("assistant"),
            ServerNotification::PlanDelta(codex_app_server_protocol::PlanDeltaNotification {
                thread_id: thread_id(),
                turn_id: turn_id(),
                item_id: item_id(),
                delta: "plan".to_string(),
            }),
            ServerNotification::ReasoningSummaryTextDelta(
                codex_app_server_protocol::ReasoningSummaryTextDeltaNotification {
                    thread_id: thread_id(),
                    turn_id: turn_id(),
                    item_id: item_id(),
                    delta: "summary".to_string(),
                    summary_index: 0,
                },
            ),
            ServerNotification::ReasoningTextDelta(
                codex_app_server_protocol::ReasoningTextDeltaNotification {
                    thread_id: thread_id(),
                    turn_id: turn_id(),
                    item_id: item_id(),
                    delta: "reasoning".to_string(),
                    content_index: 0,
                },
            ),
            ServerNotification::ThreadRealtimeStarted(
                codex_app_server_protocol::ThreadRealtimeStartedNotification {
                    thread_id: thread_id(),
                    realtime_session_id: Some("realtime".to_string()),
                    version: codex_protocol::protocol::RealtimeConversationVersion::V1,
                },
            ),
            ServerNotification::ThreadRealtimeSdp(
                codex_app_server_protocol::ThreadRealtimeSdpNotification {
                    thread_id: thread_id(),
                    sdp: "answer".to_string(),
                },
            ),
            ServerNotification::ThreadRealtimeError(
                codex_app_server_protocol::ThreadRealtimeErrorNotification {
                    thread_id: thread_id(),
                    message: "error".to_string(),
                },
            ),
            ServerNotification::ThreadRealtimeClosed(
                codex_app_server_protocol::ThreadRealtimeClosedNotification {
                    thread_id: thread_id(),
                    reason: Some("done".to_string()),
                },
            ),
        ]
    }

    fn agent_message_delta_notification(delta: &str) -> ServerNotification {
        ServerNotification::AgentMessageDelta(
            codex_app_server_protocol::AgentMessageDeltaNotification {
                thread_id: "thread".to_string(),
                turn_id: "turn".to_string(),
                item_id: "item".to_string(),
                delta: delta.to_string(),
            },
        )
    }

    fn item_completed_notification(text: &str) -> ServerNotification {
        ServerNotification::ItemCompleted(codex_app_server_protocol::ItemCompletedNotification {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            completed_at_ms: 0,
            item: codex_app_server_protocol::ThreadItem::AgentMessage {
                id: "item".to_string(),
                text: text.to_string(),
                phase: None,
                memory_citation: None,
            },
        })
    }

    fn turn_completed_notification() -> ServerNotification {
        ServerNotification::TurnCompleted(codex_app_server_protocol::TurnCompletedNotification {
            final_model: None,
            model_snapshot: None,
            thread_id: "thread".to_string(),
            turn: codex_app_server_protocol::Turn {
                id: "turn".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: Vec::new(),
                status: codex_app_server_protocol::TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: Some(0),
                duration_ms: Some(1),
            },
        })
    }

    fn test_remote_connect_args(websocket_url: String) -> RemoteAppServerConnectArgs {
        RemoteAppServerConnectArgs {
            endpoint: RemoteAppServerEndpoint::WebSocket {
                websocket_url,
                auth_token: None,
            },
            client_name: "codex-app-server-client-test".to_string(),
            client_version: "0.0.0-test".to_string(),
            experimental_api: true,
            mcp_server_openai_form_elicitation: false,
            opt_out_notification_methods: Vec::new(),
            channel_capacity: 8,
        }
    }

    #[test]
    fn remote_initialize_params_forward_openai_form_capability() {
        let mut args = test_remote_connect_args("ws://localhost/rpc".to_string());
        args.mcp_server_openai_form_elicitation = true;

        assert!(
            args.initialize_params()
                .capabilities
                .expect("initialize capabilities")
                .mcp_server_openai_form_elicitation
        );
    }

    #[tokio::test]
    async fn typed_request_roundtrip_works() {
        let client = start_test_client(SessionSource::Exec).await;
        let _response: ConfigRequirementsReadResponse = client
            .request_typed(ClientRequest::ConfigRequirementsRead {
                request_id: RequestId::Integer(1),
                params: None,
            })
            .await
            .expect("typed request should succeed");
        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn typed_request_reports_json_rpc_errors() {
        let client = start_test_client(SessionSource::Exec).await;
        let err = client
            .request_typed::<ConfigRequirementsReadResponse>(ClientRequest::ThreadRead {
                request_id: RequestId::Integer(99),
                params: codex_app_server_protocol::ThreadReadParams {
                    thread_id: "missing-thread".to_string(),
                    include_turns: false,
                },
            })
            .await
            .expect_err("missing thread should return a JSON-RPC error");
        assert!(
            err.to_string().starts_with("thread/read failed:"),
            "expected method-qualified JSON-RPC failure message"
        );
        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn caller_provided_session_source_is_applied() {
        for (session_source, expected_source) in [
            (SessionSource::Exec, ApiSessionSource::Exec),
            (SessionSource::Cli, ApiSessionSource::Cli),
        ] {
            let client = start_test_client(session_source).await;
            let parsed: ThreadStartResponse = client
                .request_typed(ClientRequest::ThreadStart {
                    request_id: RequestId::Integer(2),
                    params: ThreadStartParams {
                        ephemeral: Some(true),
                        ..ThreadStartParams::default()
                    },
                })
                .await
                .expect("thread/start should succeed");
            assert_eq!(parsed.thread.source, expected_source);
            client.shutdown().await.expect("shutdown should complete");
        }
    }

    #[tokio::test]
    async fn threads_started_via_app_server_are_visible_through_typed_requests() {
        let client = start_test_client(SessionSource::Cli).await;

        let response: ThreadStartResponse = client
            .request_typed(ClientRequest::ThreadStart {
                request_id: RequestId::Integer(3),
                params: ThreadStartParams {
                    ephemeral: Some(true),
                    ..ThreadStartParams::default()
                },
            })
            .await
            .expect("thread/start should succeed");
        let read = client
            .request_typed::<codex_app_server_protocol::ThreadReadResponse>(
                ClientRequest::ThreadRead {
                    request_id: RequestId::Integer(4),
                    params: codex_app_server_protocol::ThreadReadParams {
                        thread_id: response.thread.id.clone(),
                        include_turns: false,
                    },
                },
            )
            .await
            .expect("thread/read should return the newly started thread");
        assert_eq!(read.thread.id, response.thread.id);

        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn tiny_channel_capacity_still_supports_request_roundtrip() {
        let client =
            start_test_client_with_capacity(SessionSource::Exec, /*channel_capacity*/ 1).await;
        let _response: ConfigRequirementsReadResponse = client
            .request_typed(ClientRequest::ConfigRequirementsRead {
                request_id: RequestId::Integer(1),
                params: None,
            })
            .await
            .expect("typed request should succeed");
        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn forward_in_process_event_preserves_transcript_notifications_under_backpressure() {
        let (event_tx, mut event_rx) = mpsc::channel(1);
        event_tx
            .send(InProcessServerEvent::ServerNotification(
                command_execution_output_delta_notification("stdout-1"),
            ))
            .await
            .expect("initial event should enqueue");

        let mut skipped_events = 0usize;
        let result = forward_in_process_event(
            &event_tx,
            &mut skipped_events,
            InProcessServerEvent::ServerNotification(command_execution_output_delta_notification(
                "stdout-2",
            )),
            |_| {},
        )
        .await;
        assert_eq!(result, ForwardEventResult::Continue);
        assert_eq!(skipped_events, 1);

        let receive_task = tokio::spawn(async move {
            let mut events = Vec::new();
            for _ in 0..5 {
                events.push(
                    timeout(Duration::from_secs(2), event_rx.recv())
                        .await
                        .expect("event should arrive before timeout")
                        .expect("event stream should stay open"),
                );
            }
            events
        });

        for notification in [
            agent_message_delta_notification("hello"),
            item_completed_notification("hello"),
            turn_completed_notification(),
        ] {
            let result = forward_in_process_event(
                &event_tx,
                &mut skipped_events,
                InProcessServerEvent::ServerNotification(notification),
                |_| {},
            )
            .await;
            assert_eq!(result, ForwardEventResult::Continue);
        }
        assert_eq!(skipped_events, 0);

        let events = receive_task
            .await
            .expect("receiver task should join successfully");
        assert!(matches!(
            &events[0],
            InProcessServerEvent::ServerNotification(
                ServerNotification::CommandExecutionOutputDelta(notification)
            ) if notification.delta == "stdout-1"
        ));
        assert!(matches!(
            &events[1],
            InProcessServerEvent::Lagged { skipped: 1 }
        ));
        assert!(matches!(
            &events[2],
            InProcessServerEvent::ServerNotification(ServerNotification::AgentMessageDelta(
                notification
            )) if notification.delta == "hello"
        ));
        assert!(matches!(
            &events[3],
            InProcessServerEvent::ServerNotification(ServerNotification::ItemCompleted(
                notification
            )) if matches!(
                &notification.item,
                codex_app_server_protocol::ThreadItem::AgentMessage { text, .. } if text == "hello"
            )
        ));
        assert!(matches!(
            &events[4],
            InProcessServerEvent::ServerNotification(ServerNotification::TurnCompleted(
                notification
            )) if notification.turn.status == codex_app_server_protocol::TurnStatus::Completed
        ));
    }

    #[tokio::test]
    async fn in_process_facade_aggregates_a_dropped_lower_lag_marker() {
        let (event_tx, mut event_rx) = mpsc::channel(1);
        event_tx
            .send(InProcessServerEvent::ServerNotification(
                command_execution_output_delta_notification("queued"),
            ))
            .await
            .expect("initial event should enqueue");
        let mut skipped_events = 0usize;

        assert_eq!(
            forward_in_process_event(
                &event_tx,
                &mut skipped_events,
                InProcessServerEvent::Lagged { skipped: 7 },
                |_| {},
            )
            .await,
            ForwardEventResult::Continue
        );
        assert_eq!(skipped_events, 7);
        assert!(matches!(
            event_rx.recv().await,
            Some(InProcessServerEvent::ServerNotification(
                ServerNotification::CommandExecutionOutputDelta(_)
            ))
        ));

        let receive_task = tokio::spawn(async move {
            let lag = event_rx.recv().await.expect("lag marker should arrive");
            let required = event_rx.recv().await.expect("required event should arrive");
            (lag, required)
        });
        assert_eq!(
            forward_in_process_event(
                &event_tx,
                &mut skipped_events,
                InProcessServerEvent::ServerNotification(agent_message_delta_notification(
                    "required",
                )),
                |_| {},
            )
            .await,
            ForwardEventResult::Continue
        );
        assert_eq!(skipped_events, 0);
        let (lag, required) = receive_task.await.expect("receiver should join");
        assert!(matches!(lag, InProcessServerEvent::Lagged { skipped: 7 }));
        assert!(matches!(
            required,
            InProcessServerEvent::ServerNotification(ServerNotification::AgentMessageDelta(
                notification
            )) if notification.delta == "required"
        ));
    }

    #[tokio::test]
    async fn in_process_facade_reports_dropped_fuzzy_snapshot_before_completion() {
        let (event_tx, mut event_rx) = mpsc::channel(1);
        event_tx
            .send(InProcessServerEvent::ServerNotification(
                command_execution_output_delta_notification("queued"),
            ))
            .await
            .expect("initial event should enqueue");
        let mut skipped_events = 0usize;

        assert_eq!(
            forward_in_process_event(
                &event_tx,
                &mut skipped_events,
                InProcessServerEvent::ServerNotification(
                    fuzzy_file_search_session_updated_notification("snapshot"),
                ),
                |_| {},
            )
            .await,
            ForwardEventResult::Continue
        );
        assert_eq!(skipped_events, 1);

        let completion = ServerNotification::FuzzyFileSearchSessionCompleted(
            codex_app_server_protocol::FuzzyFileSearchSessionCompletedNotification {
                session_id: "search".to_string(),
            },
        );
        let (lag, delivered) = {
            let delivery = forward_in_process_event(
                &event_tx,
                &mut skipped_events,
                InProcessServerEvent::ServerNotification(completion),
                |_| {},
            );
            tokio::pin!(delivery);
            assert!(
                timeout(Duration::from_millis(20), &mut delivery)
                    .await
                    .is_err(),
                "completion should wait while the facade queue is saturated"
            );

            assert!(matches!(
                event_rx.recv().await,
                Some(InProcessServerEvent::ServerNotification(
                    ServerNotification::CommandExecutionOutputDelta(_)
                ))
            ));
            let receive_events = async {
                let lag = event_rx.recv().await.expect("lag marker should arrive");
                let delivered = event_rx
                    .recv()
                    .await
                    .expect("completion event should arrive");
                (lag, delivered)
            };
            let (delivery_result, received) = tokio::join!(delivery.as_mut(), receive_events);
            assert_eq!(delivery_result, ForwardEventResult::Continue);
            received
        };
        assert_eq!(skipped_events, 0);
        assert!(matches!(lag, InProcessServerEvent::Lagged { skipped: 1 }));
        assert!(matches!(
            delivered,
            InProcessServerEvent::ServerNotification(
                ServerNotification::FuzzyFileSearchSessionCompleted(_)
            )
        ));
    }

    #[tokio::test]
    async fn in_process_facade_preserves_reviewed_consumer_state_under_backpressure() {
        let notifications = reviewed_consumer_state_notifications();
        assert_eq!(notifications.len(), 32);

        for notification in notifications {
            let expected = std::mem::discriminant(&notification);
            let (event_tx, mut event_rx) = mpsc::channel(1);
            event_tx
                .send(InProcessServerEvent::Lagged { skipped: 1 })
                .await
                .expect("initial event should enqueue");
            let mut skipped_events = 0usize;
            {
                let delivery = forward_in_process_event(
                    &event_tx,
                    &mut skipped_events,
                    InProcessServerEvent::ServerNotification(notification),
                    |_| {},
                );
                tokio::pin!(delivery);

                assert!(
                    timeout(Duration::from_millis(20), &mut delivery)
                        .await
                        .is_err(),
                    "required notification must block rather than be dropped"
                );
                assert!(matches!(
                    event_rx.recv().await,
                    Some(InProcessServerEvent::Lagged { skipped: 1 })
                ));
                assert_eq!(delivery.as_mut().await, ForwardEventResult::Continue);
            }
            assert_eq!(skipped_events, 0);
            let delivered = event_rx.recv().await.expect("required event should arrive");
            let InProcessServerEvent::ServerNotification(delivered) = delivered else {
                panic!("expected server notification");
            };
            assert_eq!(std::mem::discriminant(&delivered), expected);
        }
    }

    #[tokio::test]
    async fn remote_typed_request_roundtrip_works() {
        let websocket_url = start_test_remote_server(|mut websocket| async move {
            expect_remote_initialize(&mut websocket).await;
            let JSONRPCMessage::Request(request) = read_websocket_message(&mut websocket).await
            else {
                panic!("expected account/read request");
            };
            assert_eq!(request.method, "account/read");
            write_websocket_message(
                &mut websocket,
                JSONRPCMessage::Response(JSONRPCResponse {
                    id: request.id,
                    result: serde_json::to_value(GetAccountResponse {
                        account: None,
                        requires_openai_auth: false,
                    })
                    .expect("response should serialize"),
                }),
            )
            .await;
            websocket.close(None).await.expect("close should succeed");
        })
        .await;
        let client = RemoteAppServerClient::connect(test_remote_connect_args(websocket_url))
            .await
            .expect("remote client should connect");

        assert_eq!(client.server_version(), Some("9.8.7-test"));
        assert_eq!(client.codex_home(), Some("/server/.codex"));
        let response: GetAccountResponse = client
            .request_typed(ClientRequest::GetAccount {
                request_id: RequestId::Integer(1),
                params: codex_app_server_protocol::GetAccountParams {
                    refresh_token: false,
                },
            })
            .await
            .expect("typed request should succeed");
        assert_eq!(response.account, None);

        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn remote_unix_socket_typed_request_roundtrip_works() {
        let socket_dir = TempDir::new().expect("socket dir");
        let socket_path = AbsolutePathBuf::from_absolute_path(socket_dir.path().join("codex.sock"))
            .expect("socket path should resolve");
        let mut listener = UnixListener::bind(socket_path.as_path())
            .await
            .expect("listener should bind");
        tokio::spawn(async move {
            let stream = listener.accept().await.expect("accept should succeed");
            let mut websocket = accept_async(stream)
                .await
                .expect("websocket upgrade should succeed");
            expect_remote_initialize(&mut websocket).await;
            let JSONRPCMessage::Request(request) = read_websocket_message(&mut websocket).await
            else {
                panic!("expected account/read request");
            };
            assert_eq!(request.method, "account/read");
            write_websocket_message(
                &mut websocket,
                JSONRPCMessage::Response(JSONRPCResponse {
                    id: request.id,
                    result: serde_json::to_value(GetAccountResponse {
                        account: None,
                        requires_openai_auth: false,
                    })
                    .expect("response should serialize"),
                }),
            )
            .await;
            websocket.close(None).await.expect("close should succeed");
        });
        let client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
            endpoint: RemoteAppServerEndpoint::UnixSocket { socket_path },
            client_name: "codex-app-server-client-test".to_string(),
            client_version: "0.0.0-test".to_string(),
            experimental_api: true,
            mcp_server_openai_form_elicitation: false,
            opt_out_notification_methods: Vec::new(),
            channel_capacity: 8,
        })
        .await
        .expect("remote client should connect");

        let response: GetAccountResponse = client
            .request_typed(ClientRequest::GetAccount {
                request_id: RequestId::Integer(1),
                params: codex_app_server_protocol::GetAccountParams {
                    refresh_token: false,
                },
            })
            .await
            .expect("typed request should succeed");
        assert_eq!(response.account, None);

        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn remote_typed_request_accepts_large_single_frame_response() {
        let padding = "x".repeat((17 << 20) + 1024);
        let websocket_url = start_test_remote_server(move |mut websocket| async move {
            expect_remote_initialize(&mut websocket).await;
            let JSONRPCMessage::Request(request) = read_websocket_message(&mut websocket).await
            else {
                panic!("expected account/read request");
            };
            assert_eq!(request.method, "account/read");
            write_websocket_message(
                &mut websocket,
                JSONRPCMessage::Response(JSONRPCResponse {
                    id: request.id,
                    result: serde_json::json!({
                        "account": null,
                        "requiresOpenaiAuth": false,
                        "padding": padding,
                    }),
                }),
            )
            .await;
            websocket.close(None).await.expect("close should succeed");
        })
        .await;
        let client = RemoteAppServerClient::connect(test_remote_connect_args(websocket_url))
            .await
            .expect("remote client should connect");

        let response: GetAccountResponse = client
            .request_typed(ClientRequest::GetAccount {
                request_id: RequestId::Integer(1),
                params: codex_app_server_protocol::GetAccountParams {
                    refresh_token: false,
                },
            })
            .await
            .expect("large typed request should succeed");
        assert_eq!(
            response,
            GetAccountResponse {
                account: None,
                requires_openai_auth: false,
            }
        );

        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn remote_connect_includes_auth_header_when_configured() {
        let auth_token = "remote-bearer-token".to_string();
        let websocket_url = start_test_remote_server_with_auth(
            Some(auth_token.clone()),
            |mut websocket| async move {
                expect_remote_initialize(&mut websocket).await;
                websocket.close(None).await.expect("close should succeed");
            },
        )
        .await;
        let client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
            endpoint: RemoteAppServerEndpoint::WebSocket {
                websocket_url,
                auth_token: Some(auth_token),
            },
            client_name: "codex-app-server-client-test".to_string(),
            client_version: "0.0.0-test".to_string(),
            experimental_api: true,
            mcp_server_openai_form_elicitation: false,
            opt_out_notification_methods: Vec::new(),
            channel_capacity: 8,
        })
        .await
        .expect("remote client should connect");

        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn remote_connect_rejects_non_loopback_ws_when_auth_configured() {
        let result = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
            endpoint: RemoteAppServerEndpoint::WebSocket {
                websocket_url: "ws://example.com:4500".to_string(),
                auth_token: Some("remote-bearer-token".to_string()),
            },
            client_name: "codex-app-server-client-test".to_string(),
            client_version: "0.0.0-test".to_string(),
            experimental_api: true,
            mcp_server_openai_form_elicitation: false,
            opt_out_notification_methods: Vec::new(),
            channel_capacity: 8,
        })
        .await;
        let err = match result {
            Ok(_) => panic!("non-loopback ws should be rejected before connect"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert!(
            err.to_string()
                .contains("remote auth tokens require `wss://` or loopback `ws://` URLs")
        );
    }

    #[test]
    fn remote_auth_token_transport_policy_allows_wss_and_loopback_ws() {
        assert!(crate::remote::websocket_url_supports_auth_token(
            &url::Url::parse("wss://example.com:443").expect("wss URL should parse")
        ));
        assert!(crate::remote::websocket_url_supports_auth_token(
            &url::Url::parse("ws://127.0.0.1:4500").expect("loopback ws URL should parse")
        ));
        assert!(!crate::remote::websocket_url_supports_auth_token(
            &url::Url::parse("ws://example.com:4500").expect("non-loopback ws URL should parse")
        ));
    }

    #[tokio::test]
    async fn remote_duplicate_request_id_keeps_original_waiter() {
        let (first_request_seen_tx, first_request_seen_rx) = tokio::sync::oneshot::channel();
        let websocket_url = start_test_remote_server(|mut websocket| async move {
            expect_remote_initialize(&mut websocket).await;
            let JSONRPCMessage::Request(request) = read_websocket_message(&mut websocket).await
            else {
                panic!("expected account/read request");
            };
            assert_eq!(request.method, "account/read");
            first_request_seen_tx
                .send(request.id.clone())
                .expect("request id should send");
            assert!(
                timeout(
                    Duration::from_millis(100),
                    read_websocket_message(&mut websocket)
                )
                .await
                .is_err(),
                "duplicate request should not be forwarded to the server"
            );
            write_websocket_message(
                &mut websocket,
                JSONRPCMessage::Response(JSONRPCResponse {
                    id: request.id,
                    result: serde_json::to_value(GetAccountResponse {
                        account: None,
                        requires_openai_auth: false,
                    })
                    .expect("response should serialize"),
                }),
            )
            .await;
            let _ = websocket.next().await;
        })
        .await;
        let client = RemoteAppServerClient::connect(test_remote_connect_args(websocket_url))
            .await
            .expect("remote client should connect");
        let first_request_handle = client.request_handle();
        let second_request_handle = first_request_handle.clone();

        let first_request = tokio::spawn(async move {
            first_request_handle
                .request_typed::<GetAccountResponse>(ClientRequest::GetAccount {
                    request_id: RequestId::Integer(1),
                    params: codex_app_server_protocol::GetAccountParams {
                        refresh_token: false,
                    },
                })
                .await
        });

        let first_request_id = first_request_seen_rx
            .await
            .expect("server should observe the first request");
        assert_eq!(first_request_id, RequestId::Integer(1));

        let second_err = second_request_handle
            .request_typed::<GetAccountResponse>(ClientRequest::GetAccount {
                request_id: RequestId::Integer(1),
                params: codex_app_server_protocol::GetAccountParams {
                    refresh_token: false,
                },
            })
            .await
            .expect_err("duplicate request id should be rejected");
        assert_eq!(
            second_err.to_string(),
            "account/read transport error: duplicate remote app-server request id `1`"
        );

        let first_response = first_request
            .await
            .expect("first request task should join")
            .expect("first request should succeed");
        assert_eq!(
            first_response,
            GetAccountResponse {
                account: None,
                requires_openai_auth: false,
            }
        );

        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn remote_notifications_arrive_over_websocket() {
        let websocket_url = start_test_remote_server(|mut websocket| async move {
            expect_remote_initialize(&mut websocket).await;
            write_websocket_message(
                &mut websocket,
                JSONRPCMessage::Notification(
                    serde_json::from_value(
                        serde_json::to_value(ServerNotification::AccountUpdated(
                            AccountUpdatedNotification {
                                auth_mode: None,
                                plan_type: None,
                            },
                        ))
                        .expect("notification should serialize"),
                    )
                    .expect("notification should convert to JSON-RPC"),
                ),
            )
            .await;
        })
        .await;
        let mut client = RemoteAppServerClient::connect(test_remote_connect_args(websocket_url))
            .await
            .expect("remote client should connect");

        let event = client.next_event().await.expect("event should arrive");
        assert!(matches!(
            event,
            AppServerEvent::ServerNotification(ServerNotification::AccountUpdated(_))
        ));

        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn remote_backpressure_preserves_transcript_notifications() {
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let websocket_url = start_test_remote_server(|mut websocket| async move {
            expect_remote_initialize(&mut websocket).await;
            for notification in [
                command_execution_output_delta_notification("stdout-1"),
                command_execution_output_delta_notification("stdout-2"),
                agent_message_delta_notification("hello"),
                item_completed_notification("hello"),
                turn_completed_notification(),
            ] {
                write_websocket_message(
                    &mut websocket,
                    JSONRPCMessage::Notification(
                        serde_json::from_value(
                            serde_json::to_value(notification)
                                .expect("notification should serialize"),
                        )
                        .expect("notification should convert to JSON-RPC"),
                    ),
                )
                .await;
            }
            let JSONRPCMessage::Request(request) = read_websocket_message(&mut websocket).await
            else {
                panic!("client should send an account request after the transcript burst");
            };
            write_websocket_message(
                &mut websocket,
                JSONRPCMessage::Response(JSONRPCResponse {
                    id: request.id,
                    result: serde_json::to_value(GetAccountResponse {
                        account: None,
                        requires_openai_auth: false,
                    })
                    .expect("response should serialize"),
                }),
            )
            .await;
            let _ = done_rx.await;
        })
        .await;
        let mut client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
            channel_capacity: 1,
            ..test_remote_connect_args(websocket_url)
        })
        .await
        .expect("remote client should connect");

        let response: GetAccountResponse = timeout(
            Duration::from_secs(2),
            client.request_typed(ClientRequest::GetAccount {
                request_id: RequestId::Integer(93),
                params: codex_app_server_protocol::GetAccountParams {
                    refresh_token: false,
                },
            }),
        )
        .await
        .expect("response after the transcript burst should arrive before event draining")
        .expect("account request should succeed");
        assert_eq!(
            response,
            GetAccountResponse {
                account: None,
                requires_openai_auth: false,
            }
        );

        let first_event = timeout(Duration::from_secs(2), client.next_event())
            .await
            .expect("first event should arrive before timeout")
            .expect("event stream should stay open");
        assert!(matches!(
            first_event,
            AppServerEvent::ServerNotification(ServerNotification::CommandExecutionOutputDelta(
                notification
            )) if notification.delta == "stdout-1"
        ));

        let mut remaining_events = Vec::new();
        for _ in 0..4 {
            remaining_events.push(
                timeout(Duration::from_secs(2), client.next_event())
                    .await
                    .expect("event should arrive before timeout")
                    .expect("event stream should stay open"),
            );
        }

        let mut transcript_event_names = Vec::new();
        for event in &remaining_events {
            match event {
                AppServerEvent::Lagged { skipped: 1 } => {}
                AppServerEvent::ServerNotification(
                    ServerNotification::CommandExecutionOutputDelta(notification),
                ) if notification.delta == "stdout-2" => {}
                AppServerEvent::ServerNotification(ServerNotification::AgentMessageDelta(
                    notification,
                )) if notification.delta == "hello" => {
                    transcript_event_names.push("agent_message_delta");
                }
                AppServerEvent::ServerNotification(ServerNotification::ItemCompleted(
                    notification,
                )) if matches!(
                    &notification.item,
                    codex_app_server_protocol::ThreadItem::AgentMessage { text, .. } if text == "hello"
                ) =>
                {
                    transcript_event_names.push("item_completed");
                }
                AppServerEvent::ServerNotification(ServerNotification::TurnCompleted(
                    notification,
                )) if notification.turn.status
                    == codex_app_server_protocol::TurnStatus::Completed =>
                {
                    transcript_event_names.push("turn_completed");
                }
                _ => panic!("unexpected remaining event: {event:?}"),
            }
        }
        assert_eq!(
            transcript_event_names,
            vec!["agent_message_delta", "item_completed", "turn_completed"]
        );

        done_tx
            .send(())
            .expect("server completion signal should send");
        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn remote_pending_required_event_keeps_control_commands_responsive() {
        let (controls_tx, controls_rx) = oneshot::channel();
        let (done_tx, done_rx) = oneshot::channel();
        let websocket_url = start_test_remote_server(|mut websocket| async move {
            expect_remote_initialize(&mut websocket).await;
            for thread_id in ["queued", "pending"] {
                let notification = ServerNotification::ThreadClosed(
                    codex_app_server_protocol::ThreadClosedNotification {
                        thread_id: thread_id.to_string(),
                    },
                );
                write_websocket_message(
                    &mut websocket,
                    JSONRPCMessage::Notification(
                        serde_json::from_value(
                            serde_json::to_value(notification)
                                .expect("notification should serialize"),
                        )
                        .expect("notification should convert to JSON-RPC"),
                    ),
                )
                .await;
            }

            let mut controls = Vec::new();
            let mut request_id = None;
            for _ in 0..4 {
                let message = read_websocket_message(&mut websocket).await;
                if let JSONRPCMessage::Request(request) = &message {
                    request_id = Some(request.id.clone());
                }
                controls.push(message);
            }
            controls_tx
                .send(controls)
                .expect("control observations should reach the test");
            write_websocket_message(
                &mut websocket,
                JSONRPCMessage::Response(JSONRPCResponse {
                    id: request_id.expect("account request should be observed"),
                    result: serde_json::to_value(GetAccountResponse {
                        account: None,
                        requires_openai_auth: false,
                    })
                    .expect("response should serialize"),
                }),
            )
            .await;
            let _ = done_rx.await;
        })
        .await;
        let mut client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
            channel_capacity: 1,
            ..test_remote_connect_args(websocket_url)
        })
        .await
        .expect("remote client should connect");

        timeout(
            Duration::from_secs(1),
            client._test_pending_required_event.notified(),
        )
        .await
        .expect("second required event should enter finite remote custody");

        let request_handle = client.request_handle();
        let request_task = tokio::spawn(async move {
            request_handle
                .request_typed::<GetAccountResponse>(ClientRequest::GetAccount {
                    request_id: RequestId::Integer(92),
                    params: codex_app_server_protocol::GetAccountParams {
                        refresh_token: false,
                    },
                })
                .await
        });
        timeout(
            Duration::from_secs(1),
            client.notify(ClientNotification::Initialized),
        )
        .await
        .expect("notify control should remain responsive")
        .expect("notify should reach the WebSocket");
        timeout(
            Duration::from_secs(1),
            client.resolve_server_request(
                RequestId::String("synthetic-resolve".to_string()),
                serde_json::json!({}),
            ),
        )
        .await
        .expect("resolve control should remain responsive")
        .expect("resolve should reach the WebSocket");
        timeout(
            Duration::from_secs(1),
            client.reject_server_request(
                RequestId::String("synthetic-reject".to_string()),
                JSONRPCErrorError {
                    code: -32001,
                    message: "test rejection".to_string(),
                    data: None,
                },
            ),
        )
        .await
        .expect("reject control should remain responsive")
        .expect("reject should reach the WebSocket");

        let controls = timeout(Duration::from_secs(1), controls_rx)
            .await
            .expect("server should observe all control messages")
            .expect("control observation channel should stay open");
        assert!(controls.iter().any(|message| matches!(
            message,
            JSONRPCMessage::Request(request) if request.method == "account/read"
        )));
        assert!(controls.iter().any(|message| matches!(
            message,
            JSONRPCMessage::Notification(notification) if notification.method == "initialized"
        )));
        assert!(controls.iter().any(|message| matches!(
            message,
            JSONRPCMessage::Response(response)
                if response.id == RequestId::String("synthetic-resolve".to_string())
        )));
        assert!(controls.iter().any(|message| matches!(
            message,
            JSONRPCMessage::Error(error)
                if error.id == RequestId::String("synthetic-reject".to_string())
        )));

        timeout(Duration::from_secs(1), request_task)
            .await
            .expect("request response should not wait for event delivery")
            .expect("request task should join")
            .expect("request should succeed while the required event remains pending");

        let first = timeout(Duration::from_secs(1), client.next_event())
            .await
            .expect("queued event should arrive")
            .expect("remote event stream should stay open");
        let second = timeout(Duration::from_secs(1), client.next_event())
            .await
            .expect("pending event should arrive after capacity is released")
            .expect("remote event stream should stay open");
        assert!(matches!(
            first,
            AppServerEvent::ServerNotification(ServerNotification::ThreadClosed(notification))
                if notification.thread_id == "queued"
        ));
        assert!(matches!(
            second,
            AppServerEvent::ServerNotification(ServerNotification::ThreadClosed(notification))
                if notification.thread_id == "pending"
        ));
        done_tx.send(()).expect("server should be released");
        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn remote_write_failure_preserves_pending_required_before_disconnect() {
        let (close_seen_tx, close_seen_rx) = oneshot::channel();
        let websocket_url = start_test_remote_server(|mut websocket| async move {
            expect_remote_initialize(&mut websocket).await;
            write_remote_server_notification(
                &mut websocket,
                remote_thread_closed_notification("queued"),
            )
            .await;
            write_remote_server_notification(
                &mut websocket,
                remote_thread_closed_notification("pending"),
            )
            .await;
            expect_websocket_close(&mut websocket).await;
            close_seen_tx
                .send(())
                .expect("close observation should reach the test");
        })
        .await;
        let mut client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
            channel_capacity: 1,
            ..test_remote_connect_args(websocket_url)
        })
        .await
        .expect("remote client should connect");

        timeout(
            Duration::from_secs(1),
            client._test_pending_required_event.notified(),
        )
        .await
        .expect("second required event should enter remote custody");
        client
            .close_stream_for_test()
            .await
            .expect("test stream close should succeed");
        timeout(Duration::from_secs(1), close_seen_rx)
            .await
            .expect("server should observe the close frame")
            .expect("close observation channel should stay open");

        let request_error = timeout(
            Duration::from_secs(1),
            client.request(remote_get_account_request(/*request_id*/ 93)),
        )
        .await
        .expect("failed request write should settle promptly")
        .expect_err("request waiter should receive the terminal transport error");
        assert_eq!(request_error.kind(), ErrorKind::BrokenPipe);
        assert!(request_error.to_string().contains("write failed"));

        let first = timeout(Duration::from_secs(1), client.next_event())
            .await
            .expect("queued required event should arrive")
            .expect("remote event stream should stay open");
        let second = timeout(Duration::from_secs(1), client.next_event())
            .await
            .expect("retained required event should arrive")
            .expect("remote event stream should stay open");
        let terminal = timeout(Duration::from_secs(1), client.next_event())
            .await
            .expect("terminal event should arrive")
            .expect("remote event stream should stay open");
        assert!(matches!(
            first,
            AppServerEvent::ServerNotification(ServerNotification::ThreadClosed(notification))
                if notification.thread_id == "queued"
        ));
        assert!(matches!(
            second,
            AppServerEvent::ServerNotification(ServerNotification::ThreadClosed(notification))
                if notification.thread_id == "pending"
        ));
        assert!(matches!(
            terminal,
            AppServerEvent::Disconnected { message } if message.contains("write failed")
        ));

        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn remote_write_failure_delivers_lag_before_disconnect() {
        let (close_seen_tx, close_seen_rx) = oneshot::channel();
        let websocket_url = start_test_remote_server(|mut websocket| async move {
            expect_remote_initialize(&mut websocket).await;
            write_remote_server_notification(
                &mut websocket,
                remote_thread_closed_notification("queued"),
            )
            .await;
            write_remote_server_notification(
                &mut websocket,
                command_execution_output_delta_notification("dropped"),
            )
            .await;
            expect_websocket_close(&mut websocket).await;
            close_seen_tx
                .send(())
                .expect("close observation should reach the test");
        })
        .await;
        let mut client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
            channel_capacity: 1,
            ..test_remote_connect_args(websocket_url)
        })
        .await
        .expect("remote client should connect");

        timeout(Duration::from_secs(1), client._test_pending_lag.notified())
            .await
            .expect("dropped best-effort event should establish pending lag");
        client
            .close_stream_for_test()
            .await
            .expect("test stream close should succeed");
        timeout(Duration::from_secs(1), close_seen_rx)
            .await
            .expect("server should observe the close frame")
            .expect("close observation channel should stay open");

        let request_error = timeout(
            Duration::from_secs(1),
            client.request(remote_get_account_request(/*request_id*/ 94)),
        )
        .await
        .expect("failed request write should settle promptly")
        .expect_err("request waiter should receive the terminal transport error");
        assert_eq!(request_error.kind(), ErrorKind::BrokenPipe);

        let first = timeout(Duration::from_secs(1), client.next_event())
            .await
            .expect("queued required event should arrive")
            .expect("remote event stream should stay open");
        let lag = timeout(Duration::from_secs(1), client.next_event())
            .await
            .expect("lag marker should arrive")
            .expect("remote event stream should stay open");
        let terminal = timeout(Duration::from_secs(1), client.next_event())
            .await
            .expect("terminal event should arrive")
            .expect("remote event stream should stay open");
        assert!(matches!(
            first,
            AppServerEvent::ServerNotification(ServerNotification::ThreadClosed(notification))
                if notification.thread_id == "queued"
        ));
        assert!(matches!(lag, AppServerEvent::Lagged { skipped: 1 }));
        assert!(matches!(
            terminal,
            AppServerEvent::Disconnected { message } if message.contains("write failed")
        ));

        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn remote_shutdown_closes_promptly_with_pending_required_event() {
        let (close_seen_tx, close_seen_rx) = oneshot::channel();
        let websocket_url = start_test_remote_server(|mut websocket| async move {
            expect_remote_initialize(&mut websocket).await;
            write_remote_server_notification(
                &mut websocket,
                remote_thread_closed_notification("queued"),
            )
            .await;
            write_remote_server_notification(
                &mut websocket,
                remote_thread_closed_notification("pending"),
            )
            .await;
            expect_websocket_close(&mut websocket).await;
            close_seen_tx
                .send(())
                .expect("close observation should reach the test");
        })
        .await;
        let client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
            channel_capacity: 1,
            ..test_remote_connect_args(websocket_url)
        })
        .await
        .expect("remote client should connect");

        timeout(
            Duration::from_secs(1),
            client._test_pending_required_event.notified(),
        )
        .await
        .expect("second required event should enter remote custody");
        let shutdown_task = tokio::spawn(client.shutdown());
        timeout(Duration::from_secs(1), close_seen_rx)
            .await
            .expect("server should promptly observe the shutdown close frame")
            .expect("close observation channel should stay open");
        timeout(Duration::from_secs(1), shutdown_task)
            .await
            .expect("shutdown should not wait for the fallback timeout")
            .expect("shutdown task should join")
            .expect("shutdown should complete");
    }

    #[tokio::test]
    async fn remote_server_request_resolution_roundtrip_works() {
        let websocket_url = start_test_remote_server(|mut websocket| async move {
            expect_remote_initialize(&mut websocket).await;
            let request_id = RequestId::String("srv-1".to_string());
            let server_request = JSONRPCRequest {
                id: request_id.clone(),
                method: "item/tool/requestUserInput".to_string(),
                params: Some(
                    serde_json::to_value(ToolRequestUserInputParams {
                        thread_id: "thread-1".to_string(),
                        turn_id: "turn-1".to_string(),
                        item_id: "call-1".to_string(),
                        questions: vec![ToolRequestUserInputQuestion {
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
                    .expect("params should serialize"),
                ),
                trace: None,
            };
            write_websocket_message(&mut websocket, JSONRPCMessage::Request(server_request)).await;

            let JSONRPCMessage::Response(response) = read_websocket_message(&mut websocket).await
            else {
                panic!("expected server request response");
            };
            assert_eq!(response.id, request_id);
        })
        .await;
        let mut client = RemoteAppServerClient::connect(test_remote_connect_args(websocket_url))
            .await
            .expect("remote client should connect");

        let AppServerEvent::ServerRequest(request) = client
            .next_event()
            .await
            .expect("request event should arrive")
        else {
            panic!("expected server request event");
        };
        client
            .resolve_server_request(request.id().clone(), serde_json::json!({}))
            .await
            .expect("server request should resolve");

        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn remote_server_request_received_during_initialize_is_delivered() {
        let websocket_url = start_test_remote_server(|mut websocket| async move {
            let JSONRPCMessage::Request(request) = read_websocket_message(&mut websocket).await
            else {
                panic!("expected initialize request");
            };
            assert_eq!(request.method, "initialize");

            let request_id = RequestId::String("srv-init".to_string());
            write_websocket_message(
                &mut websocket,
                JSONRPCMessage::Request(JSONRPCRequest {
                    id: request_id.clone(),
                    method: "item/tool/requestUserInput".to_string(),
                    params: Some(
                        serde_json::to_value(ToolRequestUserInputParams {
                            thread_id: "thread-1".to_string(),
                            turn_id: "turn-1".to_string(),
                            item_id: "call-1".to_string(),
                            questions: vec![ToolRequestUserInputQuestion {
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
                        .expect("params should serialize"),
                    ),
                    trace: None,
                }),
            )
            .await;
            write_websocket_message(
                &mut websocket,
                JSONRPCMessage::Response(JSONRPCResponse {
                    id: request.id,
                    result: serde_json::json!({}),
                }),
            )
            .await;

            let JSONRPCMessage::Notification(notification) =
                read_websocket_message(&mut websocket).await
            else {
                panic!("expected initialized notification");
            };
            assert_eq!(notification.method, "initialized");

            let JSONRPCMessage::Response(response) = read_websocket_message(&mut websocket).await
            else {
                panic!("expected server request response");
            };
            assert_eq!(response.id, request_id);
        })
        .await;
        let mut client = RemoteAppServerClient::connect(test_remote_connect_args(websocket_url))
            .await
            .expect("remote client should connect");

        let AppServerEvent::ServerRequest(request) = client
            .next_event()
            .await
            .expect("request event should arrive")
        else {
            panic!("expected server request event");
        };
        client
            .resolve_server_request(request.id().clone(), serde_json::json!({}))
            .await
            .expect("server request should resolve");

        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn remote_unknown_server_request_is_rejected() {
        let websocket_url = start_test_remote_server(|mut websocket| async move {
            expect_remote_initialize(&mut websocket).await;
            let request_id = RequestId::String("srv-unknown".to_string());
            write_websocket_message(
                &mut websocket,
                JSONRPCMessage::Request(JSONRPCRequest {
                    id: request_id.clone(),
                    method: "thread/unknown".to_string(),
                    params: None,
                    trace: None,
                }),
            )
            .await;

            let JSONRPCMessage::Error(response) = read_websocket_message(&mut websocket).await
            else {
                panic!("expected JSON-RPC error response");
            };
            assert_eq!(response.id, request_id);
            assert_eq!(response.error.code, -32601);
            assert_eq!(
                response.error.message,
                "unsupported remote app-server request `thread/unknown`"
            );
        })
        .await;
        let client = RemoteAppServerClient::connect(test_remote_connect_args(websocket_url))
            .await
            .expect("remote client should connect");

        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn remote_disconnect_surfaces_as_event() {
        let websocket_url = start_test_remote_server(|mut websocket| async move {
            expect_remote_initialize(&mut websocket).await;
            websocket.close(None).await.expect("close should succeed");
        })
        .await;
        let mut client = RemoteAppServerClient::connect(test_remote_connect_args(websocket_url))
            .await
            .expect("remote client should connect");

        let event = client
            .next_event()
            .await
            .expect("disconnect event should arrive");
        assert!(matches!(event, AppServerEvent::Disconnected { .. }));
    }

    #[test]
    fn typed_request_error_exposes_sources() {
        let transport = TypedRequestError::Transport {
            method: "config/read".to_string(),
            source: IoError::new(ErrorKind::BrokenPipe, "closed"),
        };
        assert_eq!(std::error::Error::source(&transport).is_some(), true);

        let server = TypedRequestError::Server {
            method: "thread/read".to_string(),
            source: JSONRPCErrorError {
                code: -32603,
                data: Some(serde_json::json!({"detail": "config lock mismatch"})),
                message: "internal".to_string(),
            },
        };
        assert_eq!(std::error::Error::source(&server).is_some(), false);
        assert_eq!(
            server.to_string(),
            "thread/read failed: internal (code -32603), data: {\"detail\":\"config lock mismatch\"}"
        );

        let deserialize = TypedRequestError::Deserialize {
            method: "thread/start".to_string(),
            source: serde_json::from_str::<u32>("\"nope\"")
                .expect_err("invalid integer should return deserialize error"),
        };
        assert_eq!(std::error::Error::source(&deserialize).is_some(), true);
    }

    #[tokio::test]
    async fn next_event_surfaces_lagged_markers() {
        let (command_tx, _command_rx) = mpsc::channel(1);
        let (event_tx, event_rx) = mpsc::channel(1);
        let worker_handle = tokio::spawn(async {});
        event_tx
            .send(InProcessServerEvent::Lagged { skipped: 3 })
            .await
            .expect("lagged marker should enqueue");
        drop(event_tx);

        let mut client = InProcessAppServerClient {
            command_tx,
            event_rx,
            worker_handle,
            _test_pending_required_event: Arc::new(tokio::sync::Notify::new()),
        };

        let event = timeout(Duration::from_secs(2), client.next_event())
            .await
            .expect("lagged marker should arrive before timeout");
        assert!(matches!(
            event,
            Some(InProcessServerEvent::Lagged { skipped: 3 })
        ));

        client.shutdown().await.expect("shutdown should complete");
    }

    #[test]
    fn event_requires_delivery_marks_transcript_and_terminal_events() {
        assert!(event_requires_delivery(
            &InProcessServerEvent::ServerNotification(
                codex_app_server_protocol::ServerNotification::TurnCompleted(
                    codex_app_server_protocol::TurnCompletedNotification {
                        final_model: None,
                        model_snapshot: None,
                        thread_id: "thread".to_string(),
                        turn: codex_app_server_protocol::Turn {
                            id: "turn".to_string(),
                            items_view: codex_app_server_protocol::TurnItemsView::Full,
                            items: Vec::new(),
                            status: codex_app_server_protocol::TurnStatus::Completed,
                            error: None,
                            started_at: None,
                            completed_at: Some(0),
                            duration_ms: None,
                        },
                    }
                )
            )
        ));
        assert!(event_requires_delivery(
            &InProcessServerEvent::ServerNotification(
                codex_app_server_protocol::ServerNotification::AgentMessageDelta(
                    codex_app_server_protocol::AgentMessageDeltaNotification {
                        thread_id: "thread".to_string(),
                        turn_id: "turn".to_string(),
                        item_id: "item".to_string(),
                        delta: "hello".to_string(),
                    }
                )
            )
        ));
        assert!(event_requires_delivery(
            &InProcessServerEvent::ServerNotification(
                codex_app_server_protocol::ServerNotification::ItemCompleted(
                    codex_app_server_protocol::ItemCompletedNotification {
                        thread_id: "thread".to_string(),
                        turn_id: "turn".to_string(),
                        completed_at_ms: 0,
                        item: codex_app_server_protocol::ThreadItem::AgentMessage {
                            id: "item".to_string(),
                            text: "hello".to_string(),
                            phase: None,
                            memory_citation: None,
                        },
                    }
                )
            )
        ));
        assert!(event_requires_delivery(
            &InProcessServerEvent::ServerNotification(
                codex_app_server_protocol::ServerNotification::ExternalAgentConfigImportCompleted(
                    codex_app_server_protocol::ExternalAgentConfigImportCompletedNotification {
                        import_id: "import".to_string(),
                        item_type_results: Vec::new(),
                    },
                )
            )
        ));
        assert!(!event_requires_delivery(&InProcessServerEvent::Lagged {
            skipped: 1
        }));
        assert!(!event_requires_delivery(
            &InProcessServerEvent::ServerNotification(
                codex_app_server_protocol::ServerNotification::CommandExecutionOutputDelta(
                    codex_app_server_protocol::CommandExecutionOutputDeltaNotification {
                        thread_id: "thread".to_string(),
                        turn_id: "turn".to_string(),
                        item_id: "item".to_string(),
                        delta: "stdout".to_string(),
                    }
                )
            )
        ));
        assert!(!event_requires_delivery(
            &InProcessServerEvent::ServerNotification(
                fuzzy_file_search_session_updated_notification("coalescible")
            )
        ));
    }

    #[tokio::test]
    async fn runtime_start_args_forward_environment_manager_and_openai_form_capability() {
        let config = Arc::new(build_test_config().await);
        let environment_manager = Arc::new(
            EnvironmentManager::create_for_tests(
                Some("ws://127.0.0.1:8765".to_string()),
                Some(
                    ExecServerRuntimePaths::new(
                        std::env::current_exe().expect("current exe"),
                        /*codex_linux_sandbox_exe*/ None,
                    )
                    .expect("runtime paths"),
                ),
            )
            .await,
        );

        let runtime_args = InProcessClientStartArgs {
            arg0_paths: Arg0DispatchPaths::default(),
            config: config.clone(),
            cli_overrides: Vec::new(),
            loader_overrides: LoaderOverrides::default(),
            strict_config: false,
            cloud_config_bundle: CloudConfigBundleLoader::default(),
            feedback: CodexFeedback::new(),
            log_db: None,
            state_db: None,
            environment_manager: environment_manager.clone(),
            config_warnings: Vec::new(),
            session_source: SessionSource::Exec,
            enable_codex_api_key_env: false,
            client_name: "codex-app-server-client-test".to_string(),
            client_version: "0.0.0-test".to_string(),
            experimental_api: true,
            mcp_server_openai_form_elicitation: true,
            opt_out_notification_methods: Vec::new(),
            channel_capacity: DEFAULT_IN_PROCESS_CHANNEL_CAPACITY,
        }
        .into_runtime_start_args();

        assert_eq!(runtime_args.config, config);
        assert!(
            runtime_args
                .initialize
                .capabilities
                .expect("initialize capabilities")
                .mcp_server_openai_form_elicitation
        );
        assert!(Arc::ptr_eq(
            &runtime_args.environment_manager,
            &environment_manager
        ));
        assert!(
            runtime_args
                .environment_manager
                .default_environment()
                .expect("default environment")
                .is_remote()
        );
    }

    #[tokio::test]
    async fn runtime_start_args_use_remote_thread_config_loader_when_configured() {
        let mut config = build_test_config().await;
        config.experimental_thread_config_endpoint = Some("not-a-valid-endpoint".to_string());

        let runtime_args = InProcessClientStartArgs {
            arg0_paths: Arg0DispatchPaths::default(),
            config: Arc::new(config),
            cli_overrides: Vec::new(),
            loader_overrides: LoaderOverrides::default(),
            strict_config: false,
            cloud_config_bundle: CloudConfigBundleLoader::default(),
            feedback: CodexFeedback::new(),
            log_db: None,
            state_db: None,
            environment_manager: Arc::new(EnvironmentManager::default_for_tests()),
            config_warnings: Vec::new(),
            session_source: SessionSource::Exec,
            enable_codex_api_key_env: false,
            client_name: "codex-app-server-client-test".to_string(),
            client_version: "0.0.0-test".to_string(),
            experimental_api: true,
            mcp_server_openai_form_elicitation: false,
            opt_out_notification_methods: Vec::new(),
            channel_capacity: DEFAULT_IN_PROCESS_CHANNEL_CAPACITY,
        }
        .into_runtime_start_args();

        let err = runtime_args
            .thread_config_loader
            .load(Default::default())
            .await
            .expect_err("configured remote loader should try to connect");
        assert_eq!(
            err.code(),
            codex_config::ThreadConfigLoadErrorCode::RequestFailed
        );
    }

    #[tokio::test]
    async fn shutdown_completes_promptly_without_retained_managers() {
        let client = start_test_client(SessionSource::Cli).await;

        timeout(Duration::from_secs(1), client.shutdown())
            .await
            .expect("shutdown should not wait for the 5s fallback timeout")
            .expect("shutdown should complete");
    }

    #[tokio::test]
    async fn in_process_pending_required_event_still_allows_request_control() {
        let mut client = start_test_client_with_pending_facade_event().await;
        let _: ConfigRequirementsReadResponse = timeout(
            Duration::from_secs(1),
            client.request_typed(ClientRequest::ConfigRequirementsRead {
                request_id: RequestId::Integer(91),
                params: None,
            }),
        )
        .await
        .expect("request control should remain responsive")
        .expect("request should succeed while a facade event is pending");
        assert_pending_facade_events_deliver_in_fifo(&mut client).await;
        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn in_process_pending_required_event_still_allows_resolve_control() {
        let mut client = start_test_client_with_pending_facade_event().await;
        timeout(
            Duration::from_secs(1),
            client.resolve_server_request(
                RequestId::String("synthetic-resolve".to_string()),
                serde_json::json!({}),
            ),
        )
        .await
        .expect("resolve control should remain responsive")
        .expect("resolve should enter the in-process runtime");
        assert_pending_facade_events_deliver_in_fifo(&mut client).await;
        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn in_process_pending_required_event_still_allows_reject_control() {
        let mut client = start_test_client_with_pending_facade_event().await;
        timeout(
            Duration::from_secs(1),
            client.reject_server_request(
                RequestId::String("synthetic-reject".to_string()),
                JSONRPCErrorError {
                    code: -32001,
                    message: "test rejection".to_string(),
                    data: None,
                },
            ),
        )
        .await
        .expect("reject control should remain responsive")
        .expect("reject should enter the in-process runtime");
        assert_pending_facade_events_deliver_in_fifo(&mut client).await;
        client.shutdown().await.expect("shutdown should complete");
    }

    #[tokio::test]
    async fn shutdown_releases_a_pending_required_facade_event() {
        let client = start_test_client_with_pending_facade_event().await;
        timeout(Duration::from_secs(1), client.shutdown())
            .await
            .expect("dropping the facade receiver should release pending custody")
            .expect("facade and runtime shutdown should complete");
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_waits_for_in_process_drain() {
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::Ordering;

        let (command_tx, mut command_rx) = mpsc::channel(1);
        let (_event_tx, event_rx) = mpsc::channel(1);
        let completed = Arc::new(AtomicBool::new(false));
        let worker_completed = Arc::clone(&completed);
        let worker_handle = tokio::spawn(async move {
            let response_tx = match command_rx.recv().await {
                Some(ClientCommand::Shutdown { response_tx }) => response_tx,
                _ => panic!("expected shutdown command"),
            };
            tokio::time::sleep(Duration::from_secs(30)).await;
            worker_completed.store(true, Ordering::Release);
            let _ = response_tx.send(Ok(()));
        });
        let client = InProcessAppServerClient {
            command_tx,
            event_rx,
            worker_handle,
            _test_pending_required_event: Arc::new(tokio::sync::Notify::new()),
        };

        client.shutdown().await.expect("shutdown should complete");
        assert!(completed.load(Ordering::Acquire));
    }
}
