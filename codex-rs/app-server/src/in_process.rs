//! In-process app-server runtime host for local embedders.
//!
//! This module runs the existing [`MessageProcessor`] and outbound routing logic
//! on Tokio tasks, but replaces socket/stdio transports with bounded in-memory
//! channels. The intent is to preserve app-server semantics while avoiding a
//! process boundary for CLI surfaces that run in the same process.
//!
//! # Lifecycle
//!
//! 1. Construct runtime state with [`InProcessStartArgs`].
//! 2. Call [`start`], which performs the `initialize` / `initialized` handshake
//!    internally and returns a ready-to-use [`InProcessClientHandle`].
//! 3. Send requests via [`InProcessClientHandle::request`], notifications via
//!    [`InProcessClientHandle::notify`], and consume events via
//!    [`InProcessClientHandle::next_event`].
//! 4. Terminate with [`InProcessClientHandle::shutdown`].
//!
//! # Transport model
//!
//! The runtime is transport-local but not protocol-free. Incoming requests are
//! typed [`ClientRequest`] values, yet responses still come back through the
//! same JSON-RPC result envelope that `MessageProcessor` uses for stdio and
//! websocket transports. This keeps in-process behavior aligned with
//! app-server rather than creating a second execution contract.
//!
//! # Backpressure
//!
//! Command submission uses `try_send` and can return `WouldBlock`, while event
//! fanout may drop notifications under saturation. Server requests are never
//! silently abandoned: required requests wait for event-queue capacity and are
//! failed back into `MessageProcessor` only when the consumer closes, so
//! approval flows do not hang indefinitely behind a dropped request.
//!
//! # Relationship to `codex-app-server-client`
//!
//! This module provides the low-level runtime handle ([`InProcessClientHandle`]).
//! Higher-level callers (TUI, exec) should go through `codex-app-server-client`,
//! which provides a separate worker-task facade with its own request/event
//! scheduling and shutdown contract. This runtime slice does not assert those
//! facade guarantees; the facade parity and regression coverage are a later
//! delivery stage.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
#[cfg(test)]
use std::future::Future;
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::io::Result as IoResult;
use std::sync::Arc;
#[cfg(test)]
use std::sync::OnceLock;
use std::sync::RwLock;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::analytics_utils::analytics_events_client_from_config;
use crate::config_manager::ConfigManager;
use crate::error_code::OVERLOADED_ERROR_CODE;
use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use crate::message_processor::ConnectionSessionState;
use crate::message_processor::MessageProcessor;
use crate::message_processor::MessageProcessorArgs;
use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingEnvelope;
use crate::outgoing_message::OutgoingMessage;
use crate::outgoing_message::OutgoingMessageSender;
use crate::outgoing_message::QueuedOutgoingMessage;
use crate::transport::CHANNEL_CAPACITY;
use crate::transport::OutboundConnectionState;
use crate::transport::route_outgoing_envelope;
use codex_analytics::AppServerRpcTransport;
use codex_app_server_protocol::ClientNotification;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ConfigWarningNotification;
use codex_app_server_protocol::InitializeParams;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::Result;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerRequest;
use codex_arg0::Arg0DispatchPaths;
use codex_config::CloudConfigBundleLoader;
use codex_config::LoaderOverrides;
use codex_config::ThreadConfigLoader;
use codex_core::check_execpolicy_for_warnings;
use codex_core::config::Config;
use codex_core::resolve_installation_id;
use codex_exec_server::EnvironmentManager;
use codex_feedback::CodexFeedback;
use codex_login::AuthManager;
use codex_protocol::protocol::SessionSource;
pub use codex_rollout::StateDbHandle;
pub use codex_state::log_db::LogDbLayer;
#[cfg(test)]
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::timeout;
use toml::Value as TomlValue;
use tracing::warn;

const IN_PROCESS_CONNECTION_ID: ConnectionId = ConnectionId(0);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
// Covers both bounded runtime drains plus the analytics client's 25-second best-effort flush.
const SHUTDOWN_ACK_TIMEOUT: Duration = Duration::from_secs(35);
#[cfg(test)]
static REQUIRED_EVENT_DELIVERY_PROBE: OnceLock<Arc<Notify>> = OnceLock::new();
/// Default bounded channel capacity for in-process runtime queues.
pub const DEFAULT_IN_PROCESS_CHANNEL_CAPACITY: usize = CHANNEL_CAPACITY;

type PendingClientRequestResponse = std::result::Result<Result, JSONRPCErrorError>;

/// Returns whether an in-process notification requires lossless delivery.
///
/// This is the authoritative classifier for the low-level runtime. Transcript
/// boundaries and terminal notifications block for bounded consumer capacity;
/// other notifications are best-effort and any loss is reported through
/// [`InProcessServerEvent::Lagged`]. The app-server-client facade has a
/// separate Stage 2 contract.
pub fn server_notification_requires_delivery(notification: &ServerNotification) -> bool {
    matches!(
        notification,
        ServerNotification::TurnCompleted(_)
            | ServerNotification::ThreadSettingsUpdated(_)
            | ServerNotification::ItemCompleted(_)
            | ServerNotification::ExternalAgentConfigImportCompleted(_)
            | ServerNotification::AgentMessageDelta(_)
            | ServerNotification::PlanDelta(_)
            | ServerNotification::ReasoningSummaryTextDelta(_)
            | ServerNotification::ReasoningSummaryPartAdded(_)
            | ServerNotification::ReasoningTextDelta(_)
    )
}

fn event_requires_delivery(event: &InProcessServerEvent) -> bool {
    match event {
        InProcessServerEvent::ServerNotification(notification) => {
            server_notification_requires_delivery(notification)
        }
        InProcessServerEvent::ServerRequest(_) => true,
        InProcessServerEvent::Lagged { .. } => false,
    }
}

fn event_loss_count(event: &InProcessServerEvent) -> usize {
    match event {
        // Stage 1 produces Lagged markers; it never receives them as input
        // from the lower-layer outgoing queue. Facade-side marker relaying is
        // a separate Stage 2 concern.
        InProcessServerEvent::Lagged { .. } => 0,
        InProcessServerEvent::ServerNotification(_) | InProcessServerEvent::ServerRequest(_) => 1,
    }
}

fn record_event_loss(skipped_events: &mut usize, event: &InProcessServerEvent) -> bool {
    let first_loss = *skipped_events == 0;
    *skipped_events = skipped_events.saturating_add(event_loss_count(event));
    first_loss
}

async fn forward_in_process_event(
    event_tx: &mpsc::Sender<InProcessServerEvent>,
    skipped_events: &mut usize,
    event: InProcessServerEvent,
) -> std::result::Result<(), InProcessServerEvent> {
    if *skipped_events > 0 {
        if event_requires_delivery(&event) {
            if event_tx
                .send(InProcessServerEvent::Lagged {
                    skipped: *skipped_events,
                })
                .await
                .is_err()
            {
                return Err(event);
            }
            *skipped_events = 0;
        } else {
            match event_tx.try_send(InProcessServerEvent::Lagged {
                skipped: *skipped_events,
            }) {
                Ok(()) => *skipped_events = 0,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    if record_event_loss(skipped_events, &event) {
                        warn!(
                            skipped = *skipped_events,
                            "dropping in-process server event (queue full)"
                        );
                    }
                    return Ok(());
                }
                Err(mpsc::error::TrySendError::Closed(_)) => return Err(event),
            }
        }
    }

    if event_requires_delivery(&event) {
        #[cfg(test)]
        if let Some(probe) = REQUIRED_EVENT_DELIVERY_PROBE.get()
            && matches!(
                &event,
                InProcessServerEvent::ServerNotification(
                    ServerNotification::TurnCompleted(notification)
                ) if notification.turn.id == "blocked-required-delivery"
            )
        {
            let mut send = Box::pin(event_tx.send(event));
            return std::future::poll_fn(|cx| {
                let polled = send.as_mut().poll(cx);
                if polled.is_pending() {
                    probe.notify_one();
                }
                polled.map(|result| result.map_err(|error| error.0))
            })
            .await;
        }
        return event_tx.send(event).await.map_err(|error| error.0);
    }

    match event_tx.try_send(event) {
        Ok(()) => Ok(()),
        Err(mpsc::error::TrySendError::Full(event)) => {
            if record_event_loss(skipped_events, &event) {
                warn!(
                    skipped = *skipped_events,
                    "dropping in-process server event (queue full)"
                );
            }
            Ok(())
        }
        Err(mpsc::error::TrySendError::Closed(event)) => Err(event),
    }
}

fn spawn_outbound_router(
    mut outgoing_rx: mpsc::Receiver<OutgoingEnvelope>,
    mut outbound_connections: HashMap<ConnectionId, OutboundConnectionState>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(envelope) = outgoing_rx.recv().await {
            route_outgoing_envelope(&mut outbound_connections, envelope).await;
        }
    })
}

async fn deliver_in_process_events(
    mut writer_rx: mpsc::Receiver<QueuedOutgoingMessage>,
    event_tx: mpsc::Sender<InProcessServerEvent>,
    outgoing_message_sender: Weak<OutgoingMessageSender>,
) {
    let mut skipped_events = 0usize;
    loop {
        let queued_message = tokio::select! {
            _ = event_tx.closed() => break,
            queued_message = writer_rx.recv() => {
                let Some(queued_message) = queued_message else { break; };
                queued_message
            }
        };
        let event = match queued_message.message {
            OutgoingMessage::Request(request) => InProcessServerEvent::ServerRequest(request),
            OutgoingMessage::AppServerNotification(envelope) => {
                InProcessServerEvent::ServerNotification(envelope.notification)
            }
            OutgoingMessage::Response(_) | OutgoingMessage::Error(_) => {
                warn!("received unexpected response-lane message in event delivery");
                continue;
            }
        };
        if let Err(undelivered_event) =
            forward_in_process_event(&event_tx, &mut skipped_events, event).await
        {
            if let InProcessServerEvent::ServerRequest(request) = undelivered_event {
                if let Some(outgoing_message_sender) = outgoing_message_sender.upgrade() {
                    outgoing_message_sender
                        .notify_client_error(
                            request.id().clone(),
                            internal_error("in-process server request consumer is closed"),
                        )
                        .await;
                }
            }
            break;
        }
        if let Some(write_complete_tx) = queued_message.write_complete_tx {
            let _ = write_complete_tx.send(());
        }
    }
}

/// Input needed to start an in-process app-server runtime.
///
/// These fields mirror the pieces of ambient process state that stdio and
/// websocket transports normally assemble before `MessageProcessor` starts.
#[derive(Clone)]
pub struct InProcessStartArgs {
    /// Resolved argv0 dispatch paths used by command execution internals.
    pub arg0_paths: Arg0DispatchPaths,
    /// Shared base config used to initialize core components.
    pub config: Arc<Config>,
    /// CLI config overrides that are already parsed into TOML values.
    pub cli_overrides: Vec<(String, TomlValue)>,
    /// Loader override knobs used by config API paths.
    pub loader_overrides: LoaderOverrides,
    /// Whether config API paths should reject unknown config fields.
    pub strict_config: bool,
    /// Preloaded cloud config bundle provider.
    pub cloud_config_bundle: CloudConfigBundleLoader,
    /// Loader used to fetch typed thread config sources before a thread starts.
    pub thread_config_loader: Arc<dyn ThreadConfigLoader>,
    /// Feedback sink used by app-server/core telemetry and logs.
    pub feedback: CodexFeedback,
    /// SQLite tracing layer used to flush recently emitted logs before feedback upload.
    pub log_db: Option<LogDbLayer>,
    /// Process-wide SQLite state handle shared with embedded app-server consumers.
    pub state_db: Option<StateDbHandle>,
    /// Environment manager used by core execution and filesystem operations.
    pub environment_manager: Arc<EnvironmentManager>,
    /// Startup warnings emitted after initialize succeeds.
    pub config_warnings: Vec<ConfigWarningNotification>,
    /// Session source stamped into thread/session metadata.
    pub session_source: SessionSource,
    /// Whether auth loading should honor the `CODEX_API_KEY` environment variable.
    pub enable_codex_api_key_env: bool,
    /// Initialize params used for initial handshake.
    pub initialize: InitializeParams,
    /// Capacity used for all runtime queues (clamped to at least 1).
    pub channel_capacity: usize,
}

/// Event emitted from the app-server to the in-process client.
///
/// [`Lagged`](Self::Lagged) is a transport health marker, not an application
/// event — it signals that the consumer fell behind and some events were dropped.
#[derive(Debug, Clone)]
pub enum InProcessServerEvent {
    /// Server request that requires client response/rejection.
    ServerRequest(ServerRequest),
    /// App-server notification directed to the embedded client.
    ServerNotification(ServerNotification),
    /// Indicates one or more events were dropped due to backpressure.
    Lagged { skipped: usize },
}

/// Internal message sent from [`InProcessClientHandle`] methods to the runtime task.
///
/// Requests carry a oneshot sender for the response; notifications and server-request
/// replies are fire-and-forget from the caller's perspective (transport errors are
/// caught by `try_send` on the outer channel).
enum InProcessClientMessage {
    Request {
        request: Box<ClientRequest>,
        response_tx: oneshot::Sender<PendingClientRequestResponse>,
    },
    Notification {
        notification: ClientNotification,
    },
    ServerRequestResponse {
        request_id: RequestId,
        result: Result,
    },
    ServerRequestError {
        request_id: RequestId,
        error: JSONRPCErrorError,
    },
    #[cfg(test)]
    ServerRequestAfterRequiredEvent {
        notification: ServerNotification,
        request: codex_app_server_protocol::ServerRequestPayload,
        response_tx: oneshot::Sender<(
            RequestId,
            oneshot::Receiver<crate::outgoing_message::ClientRequestResult>,
        )>,
    },
}

enum ProcessorCommand {
    Request(Box<ClientRequest>),
    Notification(ClientNotification),
}

#[derive(Clone)]
pub struct InProcessClientSender {
    client_tx: mpsc::Sender<InProcessClientMessage>,
}

impl InProcessClientSender {
    pub async fn request(&self, request: ClientRequest) -> IoResult<PendingClientRequestResponse> {
        let (response_tx, response_rx) = oneshot::channel();
        self.try_send_client_message(InProcessClientMessage::Request {
            request: Box::new(request),
            response_tx,
        })?;
        response_rx.await.map_err(|err| {
            IoError::new(
                ErrorKind::BrokenPipe,
                format!("in-process request response channel closed: {err}"),
            )
        })
    }

    pub fn notify(&self, notification: ClientNotification) -> IoResult<()> {
        self.try_send_client_message(InProcessClientMessage::Notification { notification })
    }

    pub fn respond_to_server_request(&self, request_id: RequestId, result: Result) -> IoResult<()> {
        self.try_send_client_message(InProcessClientMessage::ServerRequestResponse {
            request_id,
            result,
        })
    }

    pub fn fail_server_request(
        &self,
        request_id: RequestId,
        error: JSONRPCErrorError,
    ) -> IoResult<()> {
        self.try_send_client_message(InProcessClientMessage::ServerRequestError {
            request_id,
            error,
        })
    }

    #[cfg(test)]
    async fn server_request_after_required_event(
        &self,
        notification: ServerNotification,
        request: codex_app_server_protocol::ServerRequestPayload,
    ) -> IoResult<(
        RequestId,
        oneshot::Receiver<crate::outgoing_message::ClientRequestResult>,
    )> {
        let (response_tx, response_rx) = oneshot::channel();
        self.client_tx
            .send(InProcessClientMessage::ServerRequestAfterRequiredEvent {
                notification,
                request,
                response_tx,
            })
            .await
            .map_err(|_| {
                IoError::new(
                    ErrorKind::BrokenPipe,
                    "in-process app-server runtime is closed",
                )
            })?;
        response_rx.await.map_err(|_| {
            IoError::new(
                ErrorKind::BrokenPipe,
                "in-process test server-request channel is closed",
            )
        })
    }

    fn try_send_client_message(&self, message: InProcessClientMessage) -> IoResult<()> {
        match self.client_tx.try_send(message) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(IoError::new(
                ErrorKind::WouldBlock,
                "in-process app-server client queue is full",
            )),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(IoError::new(
                ErrorKind::BrokenPipe,
                "in-process app-server runtime is closed",
            )),
        }
    }
}

/// Handle used by an in-process client to call app-server and consume events.
///
/// This is the low-level runtime handle. Higher-level callers should usually go
/// through `codex-app-server-client`. Its worker-task buffering,
/// request/response helpers, and surface-specific startup policy are a
/// separate facade contract and are not established by this runtime slice.
pub struct InProcessClientHandle {
    client: InProcessClientSender,
    event_rx: mpsc::Receiver<InProcessServerEvent>,
    runtime_handle: tokio::task::JoinHandle<()>,
    shutdown_tx: oneshot::Sender<()>,
    shutdown_ack_rx: oneshot::Receiver<()>,
    #[cfg(test)]
    _test_codex_home: Option<tempfile::TempDir>,
}

impl InProcessClientHandle {
    /// Sends a typed client request into the in-process runtime.
    ///
    /// The returned value is a transport-level `IoResult` containing either a
    /// JSON-RPC success payload or JSON-RPC error payload. Callers must keep
    /// request IDs unique among concurrent requests; reusing an in-flight ID
    /// produces an `INVALID_REQUEST` response and can make request routing
    /// ambiguous in the caller.
    pub async fn request(&self, request: ClientRequest) -> IoResult<PendingClientRequestResponse> {
        self.client.request(request).await
    }

    /// Sends a typed client notification into the in-process runtime.
    ///
    /// Notifications do not have an application-level response. Transport
    /// errors indicate queue saturation or closed runtime.
    pub fn notify(&self, notification: ClientNotification) -> IoResult<()> {
        self.client.notify(notification)
    }

    /// Resolves a pending [`ServerRequest`](InProcessServerEvent::ServerRequest).
    ///
    /// This should be used only with request IDs received from the current
    /// runtime event stream; sending arbitrary IDs has no effect on app-server
    /// state and can mask a stuck approval flow in the caller.
    pub fn respond_to_server_request(&self, request_id: RequestId, result: Result) -> IoResult<()> {
        self.client.respond_to_server_request(request_id, result)
    }

    /// Rejects a pending [`ServerRequest`](InProcessServerEvent::ServerRequest).
    ///
    /// Use this when the embedder cannot satisfy a server request; leaving
    /// requests unanswered can stall turn progress.
    pub fn fail_server_request(
        &self,
        request_id: RequestId,
        error: JSONRPCErrorError,
    ) -> IoResult<()> {
        self.client.fail_server_request(request_id, error)
    }

    /// Receives the next server event from the in-process runtime.
    ///
    /// Returns `None` when the runtime task exits and no more events are
    /// available.
    pub async fn next_event(&mut self) -> Option<InProcessServerEvent> {
        self.event_rx.recv().await
    }

    /// Requests runtime shutdown and waits for worker termination.
    ///
    /// Shutdown is bounded by internal timeouts and may abort background tasks
    /// if graceful drain does not complete in time.
    pub async fn shutdown(self) -> IoResult<()> {
        let Self {
            client,
            event_rx,
            mut runtime_handle,
            shutdown_tx,
            shutdown_ack_rx,
            #[cfg(test)]
            _test_codex_home,
        } = self;
        // Required event delivery may be waiting for capacity. Close the
        // consumer side before asking the runtime to drain so a blocked send
        // observes closure instead of holding shutdown behind the event queue.
        // Shutdown control has its own unbounded-by-the-client-queue signal.
        // Send it before dropping the event receiver so a full client command
        // queue cannot starve cleanup or its completion acknowledgment.
        let shutdown_signaled = shutdown_tx.send(()).is_ok();
        drop(event_rx);
        drop(client);
        if shutdown_signaled {
            let _ = timeout(SHUTDOWN_ACK_TIMEOUT, shutdown_ack_rx).await;
        }

        if let Err(_elapsed) = timeout(SHUTDOWN_TIMEOUT, &mut runtime_handle).await {
            runtime_handle.abort();
            let _ = runtime_handle.await;
        }
        Ok(())
    }

    pub fn sender(&self) -> InProcessClientSender {
        self.client.clone()
    }
}

/// Starts an in-process app-server runtime and performs initialize handshake.
///
/// This function sends `initialize` followed by `initialized` before returning
/// the handle, so callers receive a ready-to-use runtime. If initialize fails,
/// the runtime is shut down and an `InvalidData` error is returned.
pub async fn start(mut args: InProcessStartArgs) -> IoResult<InProcessClientHandle> {
    if let Ok(Some(err)) = check_execpolicy_for_warnings(&args.config.config_layer_stack).await {
        let (path, range) = crate::exec_policy_warning_location(&err);
        args.config_warnings.push(ConfigWarningNotification {
            summary: "Error parsing rules; custom rules not applied.".to_string(),
            details: Some(err.to_string()),
            path,
            range,
        });
    }
    let initialize = args.initialize.clone();
    let client = start_uninitialized(args).await?;

    let initialize_response = client
        .request(ClientRequest::Initialize {
            request_id: RequestId::Integer(0),
            params: initialize,
        })
        .await?;
    if let Err(error) = initialize_response {
        let _ = client.shutdown().await;
        return Err(IoError::new(
            ErrorKind::InvalidData,
            format!("in-process initialize failed: {}", error.message),
        ));
    }
    client.notify(ClientNotification::Initialized)?;

    Ok(client)
}

async fn start_uninitialized(args: InProcessStartArgs) -> IoResult<InProcessClientHandle> {
    let channel_capacity = args.channel_capacity.max(1);
    let installation_id = resolve_installation_id(&args.config.codex_home).await?;
    let (client_tx, mut client_rx) = mpsc::channel::<InProcessClientMessage>(channel_capacity);
    let (event_tx, event_rx) = mpsc::channel::<InProcessServerEvent>(channel_capacity);
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let (shutdown_ack_tx, shutdown_ack_rx) = oneshot::channel::<()>();

    let runtime_handle = tokio::spawn(async move {
        let (event_outgoing_tx, event_outgoing_rx) =
            mpsc::channel::<OutgoingEnvelope>(channel_capacity);
        let (response_outgoing_tx, response_outgoing_rx) =
            mpsc::channel::<OutgoingEnvelope>(channel_capacity);
        let auth_manager =
            AuthManager::shared_from_config(args.config.as_ref(), args.enable_codex_api_key_env)
                .await;
        let analytics_events_client =
            analytics_events_client_from_config(Arc::clone(&auth_manager), args.config.as_ref());
        let analytics_events_flush_client = analytics_events_client.clone();
        let outgoing_message_sender = Arc::new(OutgoingMessageSender::new_with_senders(
            event_outgoing_tx,
            response_outgoing_tx,
            analytics_events_client.clone(),
        ));

        let (event_writer_tx, event_writer_rx) =
            mpsc::channel::<QueuedOutgoingMessage>(channel_capacity);
        let (response_writer_tx, mut response_writer_rx) =
            mpsc::channel::<QueuedOutgoingMessage>(channel_capacity);
        let outbound_initialized = Arc::new(AtomicBool::new(false));
        let outbound_experimental_api_enabled = Arc::new(AtomicBool::new(false));
        let outbound_opted_out_notification_methods = Arc::new(RwLock::new(HashSet::new()));

        let mut event_outbound_connections =
            HashMap::<ConnectionId, OutboundConnectionState>::new();
        event_outbound_connections.insert(
            IN_PROCESS_CONNECTION_ID,
            OutboundConnectionState::new(
                event_writer_tx,
                Arc::clone(&outbound_initialized),
                Arc::clone(&outbound_experimental_api_enabled),
                Arc::clone(&outbound_opted_out_notification_methods),
                /*disconnect_sender*/ None,
            ),
        );
        let mut response_outbound_connections =
            HashMap::<ConnectionId, OutboundConnectionState>::new();
        response_outbound_connections.insert(
            IN_PROCESS_CONNECTION_ID,
            OutboundConnectionState::new(
                response_writer_tx,
                Arc::clone(&outbound_initialized),
                Arc::clone(&outbound_experimental_api_enabled),
                Arc::clone(&outbound_opted_out_notification_methods),
                /*disconnect_sender*/ None,
            ),
        );
        let mut event_outbound_handle =
            spawn_outbound_router(event_outgoing_rx, event_outbound_connections);
        let mut response_outbound_handle =
            spawn_outbound_router(response_outgoing_rx, response_outbound_connections);
        let event_outgoing = Arc::downgrade(&outgoing_message_sender);
        let mut event_delivery_handle = tokio::spawn(async move {
            deliver_in_process_events(event_writer_rx, event_tx, event_outgoing).await;
        });
        let mut event_delivery_finished = false;

        let processor_outgoing = Arc::clone(&outgoing_message_sender);
        let config_manager = ConfigManager::new(
            args.config.codex_home.to_path_buf(),
            args.cli_overrides,
            args.loader_overrides,
            args.strict_config,
            args.cloud_config_bundle,
            args.arg0_paths.clone(),
            args.thread_config_loader,
        );
        let (processor_tx, mut processor_rx) = mpsc::channel::<ProcessorCommand>(channel_capacity);
        let mut processor_handle = tokio::spawn(async move {
            let processor = Arc::new(MessageProcessor::new(MessageProcessorArgs {
                outgoing: Arc::clone(&processor_outgoing),
                analytics_events_client,
                arg0_paths: args.arg0_paths,
                config: args.config,
                config_manager,
                environment_manager: args.environment_manager,
                feedback: args.feedback,
                log_db: args.log_db,
                state_db: args.state_db,
                config_warnings: args.config_warnings,
                session_source: args.session_source,
                auth_manager,
                installation_id,
                code_mode_session_provider: None,
                rpc_transport: AppServerRpcTransport::InProcess,
                remote_control_handle: None,
                plugin_startup_tasks: crate::PluginStartupTasks::Start,
            }));
            let mut thread_created_rx = processor.thread_created_receiver();
            let session = Arc::new(ConnectionSessionState::new());
            let mut listen_for_threads = true;

            loop {
                tokio::select! {
                    command = processor_rx.recv() => {
                        match command {
                            Some(ProcessorCommand::Request(request)) => {
                                let was_initialized = session.initialized();
                                processor
                                    .process_client_request(
                                        IN_PROCESS_CONNECTION_ID,
                                        *request,
                                        Arc::clone(&session),
                                        &outbound_initialized,
                                    )
                                    .await;
                                let opted_out_notification_methods_snapshot =
                                    session.opted_out_notification_methods();
                                let experimental_api_enabled =
                                    session.experimental_api_enabled();
                                let is_initialized = session.initialized();
                                if let Ok(mut opted_out_notification_methods) =
                                    outbound_opted_out_notification_methods.write()
                                {
                                    *opted_out_notification_methods =
                                        opted_out_notification_methods_snapshot;
                                } else {
                                    warn!("failed to update outbound opted-out notifications");
                                }
                                outbound_experimental_api_enabled.store(
                                    experimental_api_enabled,
                                    Ordering::Release,
                                );
                                if !was_initialized && is_initialized {
                                    processor.send_initialize_notifications().await;
                                }
                            }
                            Some(ProcessorCommand::Notification(notification)) => {
                                processor.process_client_notification(notification).await;
                            }
                            None => {
                                break;
                            }
                        }
                    }
                    created = thread_created_rx.recv(), if listen_for_threads => {
                        match created {
                            Ok(thread_id) => {
                                let connection_ids = if session.initialized() {
                                    vec![IN_PROCESS_CONNECTION_ID]
                                } else {
                                    Vec::<ConnectionId>::new()
                                };
                                processor
                                    .try_attach_thread_listener(thread_id, connection_ids)
                                    .await;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                warn!("thread_created receiver lagged; skipping resync");
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                listen_for_threads = false;
                            }
                        }
                    }
                }
            }

            processor.clear_runtime_references();
            processor.cancel_active_login().await;
            processor
                .connection_closed(IN_PROCESS_CONNECTION_ID, &session)
                .await;
            processor.clear_all_thread_listeners().await;
            processor.drain_background_tasks().await;
            processor.shutdown_threads().await;
        });
        let mut pending_request_responses =
            HashMap::<RequestId, oneshot::Sender<PendingClientRequestResponse>>::new();
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    break;
                }
                message = client_rx.recv() => {
                    match message {
                        Some(InProcessClientMessage::Request { request, response_tx }) => {
                            let request = *request;
                            let request_id = request.id().clone();
                            match pending_request_responses.entry(request_id.clone()) {
                                Entry::Vacant(entry) => {
                                    entry.insert(response_tx);
                                }
                                Entry::Occupied(_) => {
                                    let _ = response_tx.send(Err(invalid_request(format!(
                                        "duplicate request id: {request_id:?}"
                                    ))));
                                    continue;
                                }
                            }

                            match processor_tx.try_send(ProcessorCommand::Request(Box::new(request))) {
                                Ok(()) => {}
                                Err(mpsc::error::TrySendError::Full(_)) => {
                                    if let Some(response_tx) =
                                        pending_request_responses.remove(&request_id)
                                    {
                                        let _ = response_tx.send(Err(JSONRPCErrorError {
                                            code: OVERLOADED_ERROR_CODE,
                                            message: "in-process app-server request queue is full"
                                                .to_string(),
                                            data: None,
                                        }));
                                    }
                                }
                                Err(mpsc::error::TrySendError::Closed(_)) => {
                                    if let Some(response_tx) =
                                        pending_request_responses.remove(&request_id)
                                    {
                                        let _ = response_tx.send(Err(internal_error(
                                            "in-process app-server request processor is closed",
                                        )));
                                    }
                                    break;
                                }
                            }
                        }
                        Some(InProcessClientMessage::Notification { notification }) => {
                            match processor_tx.try_send(ProcessorCommand::Notification(notification)) {
                                Ok(()) => {}
                                Err(mpsc::error::TrySendError::Full(_)) => {
                                    warn!("dropping in-process client notification (queue full)");
                                }
                                Err(mpsc::error::TrySendError::Closed(_)) => {
                                    break;
                                }
                            }
                        }
                        Some(InProcessClientMessage::ServerRequestResponse { request_id, result }) => {
                            outgoing_message_sender
                                .notify_client_response(request_id, result)
                                .await;
                        }
                        Some(InProcessClientMessage::ServerRequestError { request_id, error }) => {
                            outgoing_message_sender
                                .notify_client_error(request_id, error)
                                .await;
                        }
                        #[cfg(test)]
                        Some(InProcessClientMessage::ServerRequestAfterRequiredEvent {
                            notification,
                            request,
                            response_tx,
                        }) => {
                            debug_assert!(server_notification_requires_delivery(&notification));
                            outgoing_message_sender
                                .send_server_notification_to_connection(
                                    IN_PROCESS_CONNECTION_ID,
                                    notification,
                                )
                                .await;
                            let pending = outgoing_message_sender.send_request(request).await;
                            let _ = response_tx.send(pending);
                        }
                        None => {
                            break;
                        }
                    }
                }
                queued_message = response_writer_rx.recv() => {
                    let Some(queued_message) = queued_message else {
                        break;
                    };
                    let outgoing_message = queued_message.message;
                    match outgoing_message {
                        OutgoingMessage::Response(response) => {
                            if let Some(response_tx) = pending_request_responses.remove(&response.id) {
                                let _ = response_tx.send(Ok(response.result));
                            } else {
                                warn!(
                                    request_id = ?response.id,
                                    "dropping unmatched in-process response"
                                );
                            }
                        }
                        OutgoingMessage::Error(error) => {
                            if let Some(response_tx) = pending_request_responses.remove(&error.id) {
                                let _ = response_tx.send(Err(error.error));
                            } else {
                                warn!(
                                    request_id = ?error.id,
                                    "dropping unmatched in-process error response"
                                );
                            }
                        }
                        OutgoingMessage::Request(_) | OutgoingMessage::AppServerNotification(_) => {
                            warn!("received unexpected event-lane message in response delivery");
                        }
                    }
                    if let Some(write_complete_tx) = queued_message.write_complete_tx {
                        let _ = write_complete_tx.send(());
                    }
                }
                _ = &mut event_delivery_handle => {
                    event_delivery_finished = true;
                    break;
                }
            }
        }

        drop(response_writer_rx);
        drop(processor_tx);
        outgoing_message_sender
            .cancel_all_requests(Some(internal_error(
                "in-process app-server runtime is shutting down",
            )))
            .await;
        // Drop the runtime's sender before awaiting the delivery and router
        // tasks so both bounded ingress receivers can observe channel closure.
        drop(outgoing_message_sender);
        for (_, response_tx) in pending_request_responses {
            let _ = response_tx.send(Err(internal_error(
                "in-process app-server runtime is shutting down",
            )));
        }

        if let Err(_elapsed) = timeout(SHUTDOWN_TIMEOUT, &mut processor_handle).await {
            processor_handle.abort();
            let _ = processor_handle.await;
        }
        if let Err(_elapsed) = timeout(SHUTDOWN_TIMEOUT, async {
            if !event_delivery_finished {
                let _ = (&mut event_delivery_handle).await;
                event_delivery_finished = true;
            }
            let _ = (&mut event_outbound_handle).await;
            let _ = (&mut response_outbound_handle).await;
        })
        .await
        {
            if !event_delivery_finished {
                event_delivery_handle.abort();
            }
            event_outbound_handle.abort();
            response_outbound_handle.abort();
            if !event_delivery_finished {
                let _ = event_delivery_handle.await;
            }
            let _ = event_outbound_handle.await;
            let _ = response_outbound_handle.await;
        }

        analytics_events_flush_client.flush().await;

        let _ = shutdown_ack_tx.send(());
    });

    Ok(InProcessClientHandle {
        client: InProcessClientSender { client_tx },
        event_rx,
        runtime_handle,
        shutdown_tx,
        shutdown_ack_rx,
        #[cfg(test)]
        _test_codex_home: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::AccountUpdatedNotification;
    use codex_app_server_protocol::ClientInfo;
    use codex_app_server_protocol::ConfigRequirementsReadResponse;
    use codex_app_server_protocol::ExternalAgentConfigImportCompletedNotification;
    use codex_app_server_protocol::ReasoningSummaryPartAddedNotification;
    use codex_app_server_protocol::ServerNotificationEnvelope;
    use codex_app_server_protocol::SessionSource as ApiSessionSource;
    use codex_app_server_protocol::ThreadStartParams;
    use codex_app_server_protocol::ThreadStartResponse;
    use codex_app_server_protocol::ToolRequestUserInputParams;
    use codex_app_server_protocol::ToolRequestUserInputResponse;
    use codex_app_server_protocol::Turn;
    use codex_app_server_protocol::TurnCompletedNotification;
    use codex_app_server_protocol::TurnItemsView;
    use codex_app_server_protocol::TurnStatus;
    use codex_core::config::ConfigBuilder;
    use pretty_assertions::assert_eq;
    use std::path::Path;
    use tempfile::TempDir;

    fn test_outbound_connections(
        writer: mpsc::Sender<QueuedOutgoingMessage>,
    ) -> HashMap<ConnectionId, OutboundConnectionState> {
        HashMap::from([(
            IN_PROCESS_CONNECTION_ID,
            OutboundConnectionState::new(
                writer,
                Arc::new(AtomicBool::new(true)),
                Arc::new(AtomicBool::new(false)),
                Arc::new(RwLock::new(HashSet::new())),
                /*disconnect_sender*/ None,
            ),
        )])
    }

    fn turn_completed_notification(turn_id: &str) -> ServerNotification {
        ServerNotification::TurnCompleted(TurnCompletedNotification {
            thread_id: "thread-1".to_string(),
            turn: Turn {
                id: turn_id.to_string(),
                items: Vec::new(),
                items_view: TurnItemsView::NotLoaded,
                status: TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: Some(0),
                duration_ms: None,
            },
            final_model: None,
            model_snapshot: None,
        })
    }

    fn queued_notification(notification: ServerNotification) -> QueuedOutgoingMessage {
        QueuedOutgoingMessage::new(OutgoingMessage::AppServerNotification(
            ServerNotificationEnvelope {
                notification,
                emitted_at_ms: None,
            },
        ))
    }

    async fn wait_for_channel_capacity<T>(sender: &mpsc::Sender<T>, expected: usize) {
        timeout(Duration::from_secs(1), async {
            while sender.capacity() != expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("channel should reach expected capacity");
    }

    fn turn_id_and_write_completion(
        queued_message: QueuedOutgoingMessage,
    ) -> (String, Option<oneshot::Sender<()>>) {
        let OutgoingMessage::AppServerNotification(envelope) = queued_message.message else {
            panic!("expected server notification");
        };
        let ServerNotification::TurnCompleted(notification) = envelope.notification else {
            panic!("expected turn/completed notification");
        };
        (notification.turn.id, queued_message.write_complete_tx)
    }

    async fn build_test_config(codex_home: &Path) -> Config {
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

    async fn start_test_client_with_capacity(
        session_source: SessionSource,
        channel_capacity: usize,
    ) -> InProcessClientHandle {
        let codex_home = TempDir::new().expect("temp dir");
        let config = Arc::new(build_test_config(codex_home.path()).await);
        let state_db = codex_rollout::state_db::try_init(config.as_ref())
            .await
            .expect("state db should initialize for in-process test");
        let args = InProcessStartArgs {
            arg0_paths: Arg0DispatchPaths::default(),
            config,
            cli_overrides: Vec::new(),
            loader_overrides: LoaderOverrides::default(),
            strict_config: false,
            cloud_config_bundle: CloudConfigBundleLoader::default(),
            thread_config_loader: Arc::new(codex_config::NoopThreadConfigLoader),
            feedback: CodexFeedback::new(),
            log_db: None,
            state_db: Some(state_db),
            environment_manager: Arc::new(EnvironmentManager::default_for_tests()),
            config_warnings: Vec::new(),
            session_source,
            enable_codex_api_key_env: false,
            initialize: InitializeParams {
                client_info: ClientInfo {
                    name: "codex-in-process-test".to_string(),
                    title: None,
                    version: "0.0.0".to_string(),
                },
                capabilities: None,
            },
            channel_capacity,
        };
        let mut client = start(args).await.expect("in-process runtime should start");
        client._test_codex_home = Some(codex_home);
        client
    }

    async fn start_test_client(session_source: SessionSource) -> InProcessClientHandle {
        start_test_client_with_capacity(session_source, DEFAULT_IN_PROCESS_CHANNEL_CAPACITY).await
    }

    async fn request_retrying_transient_overload(
        client: &InProcessClientHandle,
        request: ClientRequest,
    ) -> IoResult<PendingClientRequestResponse> {
        timeout(Duration::from_secs(1), async {
            loop {
                match client.request(request.clone()).await {
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        tokio::task::yield_now().await;
                    }
                    Ok(Err(error)) if error.code == OVERLOADED_ERROR_CODE => {
                        tokio::task::yield_now().await;
                    }
                    result => break result,
                }
            }
        })
        .await
        .map_err(|_| IoError::new(ErrorKind::TimedOut, "request remained blocked"))?
    }

    #[tokio::test]
    async fn in_process_start_initializes_and_handles_typed_v2_request() {
        let client = start_test_client(SessionSource::Cli).await;
        let response = client
            .request(ClientRequest::ConfigRequirementsRead {
                request_id: RequestId::Integer(1),
                params: None,
            })
            .await
            .expect("request transport should work")
            .expect("request should succeed");
        assert!(response.is_object());

        let _parsed: ConfigRequirementsReadResponse =
            serde_json::from_value(response).expect("response should match v2 schema");
        client
            .shutdown()
            .await
            .expect("in-process runtime should shutdown cleanly");
    }

    #[tokio::test]
    async fn in_process_start_uses_requested_session_source_for_thread_start() {
        for (requested_source, expected_source) in [
            (SessionSource::Cli, ApiSessionSource::Cli),
            (SessionSource::Exec, ApiSessionSource::Exec),
        ] {
            let client = start_test_client(requested_source).await;
            let response = client
                .request(ClientRequest::ThreadStart {
                    request_id: RequestId::Integer(2),
                    params: ThreadStartParams {
                        ephemeral: Some(true),
                        ..ThreadStartParams::default()
                    },
                })
                .await
                .expect("request transport should work")
                .expect("thread/start should succeed");
            let parsed: ThreadStartResponse =
                serde_json::from_value(response).expect("thread/start response should parse");
            assert_eq!(parsed.thread.source, expected_source);
            client
                .shutdown()
                .await
                .expect("in-process runtime should shutdown cleanly");
        }
    }

    #[tokio::test]
    async fn in_process_start_clamps_zero_channel_capacity() {
        let client =
            start_test_client_with_capacity(SessionSource::Cli, /*channel_capacity*/ 0).await;
        let response = loop {
            match client
                .request(ClientRequest::ConfigRequirementsRead {
                    request_id: RequestId::Integer(4),
                    params: None,
                })
                .await
            {
                Ok(response) => break response.expect("request should succeed"),
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    tokio::task::yield_now().await;
                }
                Err(err) => panic!("request transport should work: {err}"),
            }
        };
        let _parsed: ConfigRequirementsReadResponse =
            serde_json::from_value(response).expect("response should match v2 schema");
        client
            .shutdown()
            .await
            .expect("in-process runtime should shutdown cleanly");
    }

    #[tokio::test]
    async fn responses_bypass_saturated_in_process_event_router() {
        let (event_outgoing_tx, event_outgoing_rx) = mpsc::channel(/*buffer*/ 1);
        let event_outgoing_probe = event_outgoing_tx.clone();
        let (response_outgoing_tx, response_outgoing_rx) = mpsc::channel(/*buffer*/ 1);
        let response_outgoing_probe = response_outgoing_tx.clone();
        let (event_writer_tx, mut event_writer_rx) = mpsc::channel(/*buffer*/ 1);
        let event_writer_probe = event_writer_tx.clone();
        let (response_writer_tx, mut response_writer_rx) = mpsc::channel(/*buffer*/ 1);
        let response_writer_probe = response_writer_tx.clone();

        let event_router = spawn_outbound_router(
            event_outgoing_rx,
            test_outbound_connections(event_writer_tx),
        );
        let response_router = spawn_outbound_router(
            response_outgoing_rx,
            test_outbound_connections(response_writer_tx),
        );
        let outgoing = Arc::new(OutgoingMessageSender::new_with_senders(
            event_outgoing_tx,
            response_outgoing_tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));

        outgoing
            .send_server_notification_to_connection(
                IN_PROCESS_CONNECTION_ID,
                turn_completed_notification("first"),
            )
            .await;
        wait_for_channel_capacity(&event_writer_probe, /*expected*/ 0).await;

        outgoing
            .send_server_notification_to_connection(
                IN_PROCESS_CONNECTION_ID,
                turn_completed_notification("blocked"),
            )
            .await;
        wait_for_channel_capacity(&event_outgoing_probe, /*expected*/ 1).await;
        assert_eq!(event_writer_probe.capacity(), 0);

        let queued_outgoing = Arc::clone(&outgoing);
        let queued_event = tokio::spawn(async move {
            queued_outgoing
                .send_server_notification_to_connection_and_wait(
                    IN_PROCESS_CONNECTION_ID,
                    turn_completed_notification("queued"),
                )
                .await;
        });
        wait_for_channel_capacity(&event_outgoing_probe, /*expected*/ 0).await;
        assert!(!event_router.is_finished());
        assert!(!queued_event.is_finished());

        let success_id = crate::outgoing_message::ConnectionRequestId {
            connection_id: IN_PROCESS_CONNECTION_ID,
            request_id: RequestId::Integer(10),
        };
        outgoing
            .send_response(
                success_id.clone(),
                codex_app_server_protocol::ClientResponsePayload::ThreadArchive(
                    codex_app_server_protocol::ThreadArchiveResponse {},
                ),
            )
            .await;
        wait_for_channel_capacity(&response_writer_probe, /*expected*/ 0).await;

        let error_id = crate::outgoing_message::ConnectionRequestId {
            connection_id: IN_PROCESS_CONNECTION_ID,
            request_id: RequestId::Integer(11),
        };
        let expected_error = internal_error("expected error");
        outgoing
            .send_error(error_id.clone(), expected_error.clone())
            .await;
        wait_for_channel_capacity(&response_outgoing_probe, /*expected*/ 1).await;

        let success = response_writer_rx
            .recv()
            .await
            .expect("success should route before event release");
        let OutgoingMessage::Response(success) = success.message else {
            panic!("expected normal JSON-RPC success");
        };
        assert_eq!(success.id, success_id.request_id);
        assert_eq!(success.result, serde_json::json!({}));

        let error = response_writer_rx
            .recv()
            .await
            .expect("error should route before event release");
        let OutgoingMessage::Error(error) = error.message else {
            panic!("expected normal JSON-RPC error");
        };
        assert_eq!(error.id, error_id.request_id);
        assert_eq!(error.error, expected_error);
        assert_eq!(event_writer_probe.capacity(), 0);

        let first = event_writer_rx
            .recv()
            .await
            .expect("first event should route");
        let blocked = event_writer_rx
            .recv()
            .await
            .expect("blocked event should route after first drains");
        let queued = event_writer_rx
            .recv()
            .await
            .expect("queued event should preserve event FIFO");
        let (first_id, first_write_complete_tx) = turn_id_and_write_completion(first);
        let (blocked_id, blocked_write_complete_tx) = turn_id_and_write_completion(blocked);
        let (queued_id, queued_write_complete_tx) = turn_id_and_write_completion(queued);
        assert_eq!(
            [first_id.as_str(), blocked_id.as_str(), queued_id.as_str()],
            ["first", "blocked", "queued"]
        );
        assert!(first_write_complete_tx.is_none());
        assert!(blocked_write_complete_tx.is_none());
        assert!(!queued_event.is_finished());
        queued_write_complete_tx
            .expect("queued event should retain write-completion ownership")
            .send(())
            .expect("event sender should still await write completion");
        queued_event
            .await
            .expect("queued event sender task should finish");

        drop(outgoing);
        drop(event_outgoing_probe);
        drop(response_outgoing_probe);
        event_router
            .await
            .expect("event router should stop cleanly");
        response_router
            .await
            .expect("response router should stop cleanly");
    }

    #[test]
    fn event_loss_tracking_marks_only_the_first_drop_in_each_burst() {
        let mut skipped_events = 0;
        let event = InProcessServerEvent::ServerNotification(ServerNotification::AccountUpdated(
            AccountUpdatedNotification {
                auth_mode: None,
                plan_type: None,
            },
        ));

        assert!(record_event_loss(&mut skipped_events, &event));
        assert_eq!(skipped_events, 1);
        assert!(!record_event_loss(&mut skipped_events, &event));
        assert_eq!(skipped_events, 2);

        skipped_events = 0;
        assert!(record_event_loss(&mut skipped_events, &event));
        assert_eq!(skipped_events, 1);
    }

    #[tokio::test]
    async fn event_delivery_aggregates_loss_before_required_event_and_server_request_fifo() {
        let (writer_tx, writer_rx) = mpsc::channel(/*buffer*/ 8);
        let (event_tx, mut event_rx) = mpsc::channel(/*buffer*/ 1);
        let event_probe = event_tx.clone();
        let delivery = tokio::spawn(deliver_in_process_events(
            writer_rx,
            event_tx,
            Weak::<OutgoingMessageSender>::new(),
        ));

        writer_tx
            .send(queued_notification(ServerNotification::AccountUpdated(
                AccountUpdatedNotification {
                    auth_mode: None,
                    plan_type: None,
                },
            )))
            .await
            .expect("first best-effort event should enter delivery");
        wait_for_channel_capacity(&event_probe, /*expected*/ 0).await;
        let mut dropped_completions = Vec::new();
        for _ in 0..2 {
            let (write_complete_tx, write_complete_rx) = oneshot::channel();
            let mut queued = queued_notification(ServerNotification::AccountUpdated(
                AccountUpdatedNotification {
                    auth_mode: None,
                    plan_type: None,
                },
            ));
            queued.write_complete_tx = Some(write_complete_tx);
            writer_tx
                .send(queued)
                .await
                .expect("later best-effort event should enter delivery");
            dropped_completions.push(write_complete_rx);
        }
        for completion in dropped_completions {
            completion
                .await
                .expect("dropped best-effort event should release write ownership");
        }
        writer_tx
            .send(queued_notification(
                ServerNotification::ReasoningSummaryPartAdded(
                    ReasoningSummaryPartAddedNotification {
                        thread_id: "thread".to_string(),
                        turn_id: "turn".to_string(),
                        item_id: "reasoning".to_string(),
                        summary_index: 1,
                    },
                ),
            ))
            .await
            .expect("required reasoning boundary should enter delivery");
        let request_id = RequestId::String("ordered-request".to_string());
        writer_tx
            .send(QueuedOutgoingMessage::new(OutgoingMessage::Request(
                ServerRequest::ToolRequestUserInput {
                    request_id: request_id.clone(),
                    params: ToolRequestUserInputParams {
                        thread_id: "thread".to_string(),
                        turn_id: "turn".to_string(),
                        item_id: "item".to_string(),
                        questions: Vec::new(),
                        is_blocking: true,
                        auto_resolution_ms: None,
                    },
                },
            )))
            .await
            .expect("server request should enter delivery");

        let first = event_rx.recv().await.expect("first event should arrive");
        let lagged = event_rx.recv().await.expect("lag marker should arrive");
        let required = event_rx.recv().await.expect("required event should arrive");
        let request = event_rx.recv().await.expect("server request should arrive");
        assert!(matches!(
            first,
            InProcessServerEvent::ServerNotification(ServerNotification::AccountUpdated(_))
        ));
        assert!(matches!(
            lagged,
            InProcessServerEvent::Lagged { skipped: 2 }
        ));
        assert!(matches!(
            required,
            InProcessServerEvent::ServerNotification(
                ServerNotification::ReasoningSummaryPartAdded(notification)
            ) if notification.summary_index == 1
        ));
        assert!(matches!(
            request,
            InProcessServerEvent::ServerRequest(request) if request.id() == &request_id
        ));

        drop(writer_tx);
        delivery.await.expect("event delivery should stop cleanly");
    }

    #[tokio::test]
    async fn idle_event_consumer_closure_terminates_runtime_with_retained_sender() {
        let client =
            start_test_client_with_capacity(SessionSource::Cli, /*channel_capacity*/ 1).await;
        let sender = client.sender();

        drop(client);

        timeout(Duration::from_secs(2), sender.client_tx.closed())
            .await
            .expect("idle event-consumer closure should terminate the runtime");
        let error = sender
            .request(ClientRequest::ConfigRequirementsRead {
                request_id: RequestId::Integer(9),
                params: None,
            })
            .await
            .expect_err("retained sender must fail after consumer closure");
        assert_eq!(error.kind(), ErrorKind::BrokenPipe);
    }

    #[tokio::test]
    async fn shutdown_signal_unblocks_saturated_required_event_with_full_client_queue() {
        let (client_tx, client_rx) = mpsc::channel(/*buffer*/ 1);
        let (event_tx, event_rx) = mpsc::channel(/*buffer*/ 1);
        let (saturated_tx, saturated_rx) = oneshot::channel();
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let (shutdown_ack_tx, shutdown_ack_rx) = oneshot::channel();
        let completed = Arc::new(AtomicBool::new(false));
        let runtime_completed = Arc::clone(&completed);
        client_tx
            .try_send(InProcessClientMessage::Notification {
                notification: ClientNotification::Initialized,
            })
            .expect("client command queue should be full before shutdown");
        let runtime_handle = tokio::spawn(async move {
            let _client_rx = client_rx;
            event_tx
                .send(InProcessServerEvent::ServerNotification(
                    turn_completed_notification("queued"),
                ))
                .await
                .expect("first required event should saturate the queue");
            let _ = saturated_tx.send(());
            let blocked_send = tokio::select! {
                result = event_tx.send(InProcessServerEvent::ServerNotification(
                    turn_completed_notification("blocked"),
                )) => result,
                _ = &mut shutdown_rx => event_tx
                    .send(InProcessServerEvent::ServerNotification(
                        turn_completed_notification("blocked"),
                    ))
                    .await,
            };
            assert!(
                blocked_send.is_err(),
                "dropping the shutdown receiver should unblock required delivery"
            );
            runtime_completed.store(true, Ordering::Release);
            let _ = shutdown_ack_tx.send(());
        });
        let client = InProcessClientHandle {
            client: InProcessClientSender { client_tx },
            event_rx,
            runtime_handle,
            shutdown_tx,
            shutdown_ack_rx,
            _test_codex_home: None,
        };

        saturated_rx
            .await
            .expect("required event queue should become saturated");
        timeout(Duration::from_secs(1), client.shutdown())
            .await
            .expect("saturated required delivery should not consume shutdown timeout")
            .expect("in-process runtime should shutdown cleanly");
        assert!(completed.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn real_handle_shutdown_unblocks_saturated_required_delivery() {
        let required_event_delivery_probe = Arc::new(Notify::new());
        let _ = REQUIRED_EVENT_DELIVERY_PROBE.set(Arc::clone(&required_event_delivery_probe));
        let client =
            start_test_client_with_capacity(SessionSource::Cli, /*channel_capacity*/ 1).await;

        request_retrying_transient_overload(
            &client,
            ClientRequest::ThreadStart {
                request_id: RequestId::Integer(20),
                params: ThreadStartParams {
                    ephemeral: Some(true),
                    ..ThreadStartParams::default()
                },
            },
        )
        .await
        .expect("thread/start transport should remain live")
        .expect("thread/start should succeed while events are retained");

        timeout(Duration::from_secs(1), async {
            while client.event_rx.capacity() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("thread/start should saturate the retained event queue");

        let sender = client.sender();
        let request_sequence = tokio::spawn(async move {
            sender
                .server_request_after_required_event(
                    turn_completed_notification("blocked-required-delivery"),
                    codex_app_server_protocol::ServerRequestPayload::ToolRequestUserInput(
                        ToolRequestUserInputParams {
                            thread_id: "thread-1".to_string(),
                            turn_id: "turn-1".to_string(),
                            item_id: "request-user-input-1".to_string(),
                            questions: Vec::new(),
                            is_blocking: true,
                            auto_resolution_ms: None,
                        },
                    ),
                )
                .await
        });

        timeout(
            Duration::from_secs(1),
            required_event_delivery_probe.notified(),
        )
        .await
        .expect("required event should reach the blocked event route");
        assert_eq!(
            client.event_rx.capacity(),
            0,
            "required event remains blocked behind the retained event"
        );

        timeout(Duration::from_secs(2), client.shutdown())
            .await
            .expect("real lower-layer shutdown should close saturated event delivery promptly")
            .expect("in-process runtime should shutdown cleanly");
        request_sequence.abort();
        let _ = request_sequence.await;
    }

    #[tokio::test]
    async fn test_seam_server_request_preserves_fifo_response_and_shutdown_progress() {
        let mut client =
            start_test_client_with_capacity(SessionSource::Cli, /*channel_capacity*/ 1).await;
        let sender = client.sender();
        let request_sequence = tokio::spawn(async move {
            sender
                .server_request_after_required_event(
                    turn_completed_notification("before-server-request"),
                    codex_app_server_protocol::ServerRequestPayload::ToolRequestUserInput(
                        ToolRequestUserInputParams {
                            thread_id: "thread-1".to_string(),
                            turn_id: "turn-1".to_string(),
                            item_id: "request-user-input-1".to_string(),
                            questions: Vec::new(),
                            is_blocking: true,
                            auto_resolution_ms: None,
                        },
                    ),
                )
                .await
        });

        assert_eq!(client.event_rx.max_capacity(), 1);
        timeout(Duration::from_secs(2), async {
            while client.event_rx.capacity() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("preceding event should saturate capacity-one delivery");

        timeout(Duration::from_secs(2), async {
            loop {
                let event = client
                    .next_event()
                    .await
                    .expect("event stream should remain open");
                match event {
                    InProcessServerEvent::ServerNotification(
                        ServerNotification::TurnCompleted(notification),
                    ) if notification.turn.id == "before-server-request" => break,
                    InProcessServerEvent::ServerRequest(_) => {
                        panic!("server request bypassed preceding required event");
                    }
                    InProcessServerEvent::Lagged { .. }
                    | InProcessServerEvent::ServerNotification(_) => {}
                }
            }
        })
        .await
        .expect("preceding required event should saturate capacity-one delivery");

        let (expected_request_id, response_rx) = timeout(Duration::from_secs(2), request_sequence)
            .await
            .expect("server request should enter the real outbound route")
            .expect("server-request task should not panic")
            .expect("server request should be admitted");
        let request = timeout(Duration::from_secs(2), client.next_event())
            .await
            .expect("server request should make progress after the required event drains")
            .expect("event stream should remain open");
        let InProcessServerEvent::ServerRequest(ServerRequest::ToolRequestUserInput {
            request_id,
            ..
        }) = request
        else {
            panic!("expected request_user_input server request after required event");
        };
        assert_eq!(request_id, expected_request_id);

        let response = serde_json::to_value(ToolRequestUserInputResponse {
            answers: HashMap::new(),
        })
        .expect("request_user_input response should serialize");
        client
            .respond_to_server_request(request_id, response.clone())
            .expect("server request response should enter the real client route");
        let resolved = timeout(Duration::from_secs(2), response_rx)
            .await
            .expect("server request response should make runtime progress")
            .expect("server request callback should remain open")
            .expect("server request should resolve successfully");
        assert_eq!(resolved, response);

        let config = timeout(
            Duration::from_secs(2),
            client.request(ClientRequest::ConfigRequirementsRead {
                request_id: RequestId::Integer(22),
                params: None,
            }),
        )
        .await
        .expect("ordinary response should remain live after server request resolution")
        .expect("config request transport should remain live")
        .expect("config request should succeed");
        let _parsed: ConfigRequirementsReadResponse =
            serde_json::from_value(config).expect("config response should match v2 schema");

        timeout(Duration::from_secs(2), client.shutdown())
            .await
            .expect("shutdown should remain live after server request resolution")
            .expect("in-process runtime should shutdown cleanly");
    }

    #[tokio::test(start_paused = true)]
    async fn in_process_shutdown_waits_for_analytics_flush_budget() {
        let (client_tx, _client_rx) = mpsc::channel(/*buffer*/ 1);
        let (_event_tx, event_rx) = mpsc::channel(/*buffer*/ 1);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (shutdown_ack_tx, shutdown_ack_rx) = oneshot::channel();
        let completed = Arc::new(AtomicBool::new(false));
        let runtime_completed = Arc::clone(&completed);
        let runtime_handle = tokio::spawn(async move {
            shutdown_rx
                .await
                .expect("expected in-process shutdown signal");
            tokio::time::sleep(SHUTDOWN_TIMEOUT + SHUTDOWN_TIMEOUT + Duration::from_secs(24)).await;
            runtime_completed.store(true, Ordering::Release);
            let _ = shutdown_ack_tx.send(());
        });
        let client = InProcessClientHandle {
            client: InProcessClientSender { client_tx },
            event_rx,
            runtime_handle,
            shutdown_tx,
            shutdown_ack_rx,
            _test_codex_home: None,
        };

        client
            .shutdown()
            .await
            .expect("in-process runtime should shutdown cleanly");
        assert!(completed.load(Ordering::Acquire));
    }

    #[test]
    fn guaranteed_delivery_helpers_cover_terminal_server_notifications() {
        assert!(server_notification_requires_delivery(
            &ServerNotification::TurnCompleted(TurnCompletedNotification {
                thread_id: "thread-1".to_string(),
                turn: Turn {
                    id: "turn-1".to_string(),
                    items: Vec::new(),
                    items_view: TurnItemsView::NotLoaded,
                    status: TurnStatus::Completed,
                    error: None,
                    started_at: None,
                    completed_at: Some(0),
                    duration_ms: None,
                },
                final_model: None,
                model_snapshot: None,
            })
        ));
        assert!(server_notification_requires_delivery(
            &ServerNotification::ExternalAgentConfigImportCompleted(
                ExternalAgentConfigImportCompletedNotification {
                    import_id: "import".to_string(),
                    item_type_results: Vec::new(),
                },
            )
        ));
        assert!(server_notification_requires_delivery(
            &ServerNotification::ReasoningSummaryPartAdded(ReasoningSummaryPartAddedNotification {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item_id: "reasoning-1".to_string(),
                summary_index: 0,
            },)
        ));
    }
}
