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
//! fanout keeps authoritative notifications lossless and drops only
//! reconstructible progress under saturation, with a lag marker for consumers.
//! Server requests are never
//! silently abandoned: if they cannot be queued they are failed back into
//! `MessageProcessor` with overload or internal errors so approval flows do
//! not hang indefinitely.
//!
//! # Relationship to `codex-app-server-client`
//!
//! This module provides the low-level runtime handle ([`InProcessClientHandle`]).
//! Higher-level callers (TUI, exec) should go through `codex-app-server-client`,
//! which wraps this module behind a worker task with async request/response
//! helpers, surface-specific startup policy, and bounded shutdown.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::io::Result as IoResult;
use std::sync::Arc;
use std::sync::RwLock;
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
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::timeout;
use toml::Value as TomlValue;
use tracing::warn;

const IN_PROCESS_CONNECTION_ID: ConnectionId = ConnectionId(0);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
// Covers both bounded runtime drains plus the analytics client's 25-second best-effort flush.
const SHUTDOWN_ACK_TIMEOUT: Duration = Duration::from_secs(35);
/// Default bounded channel capacity for in-process runtime queues.
pub const DEFAULT_IN_PROCESS_CHANNEL_CAPACITY: usize = CHANNEL_CAPACITY;

type PendingClientRequestResponse = std::result::Result<Result, JSONRPCErrorError>;

fn server_notification_requires_delivery(notification: &ServerNotification) -> bool {
    // This runtime queue is upstream of the `codex-app-server-client` facade.
    // Treat new notification kinds as authoritative by default so protocol
    // growth cannot silently create another droppable terminal/control state.
    // Command output is reconstructible from its completed item, and fuzzy
    // search updates are superseded by later full snapshots. Lag reports any
    // dropped progress before later writer traffic.
    !matches!(
        notification,
        ServerNotification::CommandExecutionOutputDelta(_)
            | ServerNotification::FuzzyFileSearchSessionUpdated(_)
    )
}

/// Result of attempting to forward one notification to the in-process client.
///
/// `Pending` is retained by the runtime loop itself. This preserves one bounded
/// authoritative event without parking the loop on `event_tx.send`, which would
/// make shutdown and client control messages unresponsive.
enum InProcessNotificationForward {
    Forwarded,
    Pending(InProcessServerEvent),
    Closed,
}

struct PendingInProcessNotificationForward {
    event: InProcessServerEvent,
    write_complete_tx: Option<oneshot::Sender<()>>,
}

/// Attempts a non-blocking notification forward.
///
/// Authoritative state remains pending under saturation. Reconstructible
/// progress stays best-effort and increments `skipped_events`; the runtime loop
/// emits that count as a `Lagged` marker before it reads another outgoing event.
fn try_forward_in_process_server_notification(
    event_tx: &mpsc::Sender<InProcessServerEvent>,
    event: InProcessServerEvent,
    skipped_events: &mut usize,
) -> InProcessNotificationForward {
    let notification = match &event {
        InProcessServerEvent::ServerNotification(notification) => notification,
        _ => unreachable!("only notification events reach the notification queue"),
    };

    if server_notification_requires_delivery(notification) {
        return match event_tx.try_send(event) {
            Ok(()) => InProcessNotificationForward::Forwarded,
            Err(mpsc::error::TrySendError::Full(event)) => {
                InProcessNotificationForward::Pending(event)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => InProcessNotificationForward::Closed,
        };
    }

    match event_tx.try_send(event) {
        Ok(()) => InProcessNotificationForward::Forwarded,
        Err(mpsc::error::TrySendError::Full(_)) => {
            *skipped_events = (*skipped_events).saturating_add(1);
            warn!(
                skipped = *skipped_events,
                "dropping best-effort in-process server notification (queue full)"
            );
            InProcessNotificationForward::Forwarded
        }
        Err(mpsc::error::TrySendError::Closed(_)) => InProcessNotificationForward::Closed,
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
    Shutdown {
        done_tx: oneshot::Sender<()>,
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
/// through `codex-app-server-client`, which adds worker-task buffering,
/// request/response helpers, and surface-specific startup policy.
pub struct InProcessClientHandle {
    client: InProcessClientSender,
    event_rx: mpsc::Receiver<InProcessServerEvent>,
    runtime_handle: tokio::task::JoinHandle<()>,
    #[cfg(test)]
    _test_codex_home: Option<tempfile::TempDir>,
    #[cfg(test)]
    _test_outgoing_message_sender: Option<Arc<OutgoingMessageSender>>,
    #[cfg(test)]
    _test_pending_required_notification: Arc<tokio::sync::Notify>,
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
        let mut runtime_handle = self.runtime_handle;
        #[cfg(test)]
        let test_outgoing_message_sender = self._test_outgoing_message_sender;
        // Test-only injection retains an otherwise-live sender. Drop it before
        // requesting shutdown so the runtime's outbound-router join observes
        // the same last-sender closure as production.
        #[cfg(test)]
        drop(test_outgoing_message_sender);
        let (done_tx, done_rx) = oneshot::channel();

        if self
            .client
            .client_tx
            .send(InProcessClientMessage::Shutdown { done_tx })
            .await
            .is_ok()
        {
            let _ = timeout(SHUTDOWN_ACK_TIMEOUT, done_rx).await;
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
    #[cfg(test)]
    let (test_outgoing_message_tx, test_outgoing_message_rx) = oneshot::channel();
    #[cfg(test)]
    let test_pending_required_notification = Arc::new(tokio::sync::Notify::new());
    #[cfg(test)]
    let runtime_test_pending_required_notification =
        Arc::clone(&test_pending_required_notification);

    let runtime_handle = tokio::spawn(async move {
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<OutgoingEnvelope>(channel_capacity);
        let auth_manager =
            AuthManager::shared_from_config(args.config.as_ref(), args.enable_codex_api_key_env)
                .await;
        let analytics_events_client =
            analytics_events_client_from_config(Arc::clone(&auth_manager), args.config.as_ref());
        let analytics_events_flush_client = analytics_events_client.clone();
        let outgoing_message_sender = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            analytics_events_client.clone(),
        ));
        #[cfg(test)]
        let _ = test_outgoing_message_tx.send(Arc::clone(&outgoing_message_sender));

        let (writer_tx, mut writer_rx) = mpsc::channel::<QueuedOutgoingMessage>(channel_capacity);
        let outbound_initialized = Arc::new(AtomicBool::new(false));
        let outbound_experimental_api_enabled = Arc::new(AtomicBool::new(false));
        let outbound_opted_out_notification_methods = Arc::new(RwLock::new(HashSet::new()));

        let mut outbound_connections = HashMap::<ConnectionId, OutboundConnectionState>::new();
        outbound_connections.insert(
            IN_PROCESS_CONNECTION_ID,
            OutboundConnectionState::new(
                writer_tx,
                Arc::clone(&outbound_initialized),
                Arc::clone(&outbound_experimental_api_enabled),
                Arc::clone(&outbound_opted_out_notification_methods),
                /*disconnect_sender*/ None,
            ),
        );
        let mut outbound_handle = tokio::spawn(async move {
            while let Some(envelope) = outgoing_rx.recv().await {
                route_outgoing_envelope(&mut outbound_connections, envelope).await;
            }
        });

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
        let mut shutdown_ack = None;
        let mut pending_authoritative_notification = None::<PendingInProcessNotificationForward>;
        let mut skipped_events = 0usize;

        loop {
            tokio::select! {
                permit = event_tx.reserve(), if skipped_events > 0 || pending_authoritative_notification.is_some() => {
                    match permit {
                        Ok(permit) => {
                            if skipped_events > 0 {
                                permit.send(InProcessServerEvent::Lagged {
                                    skipped: std::mem::take(&mut skipped_events),
                                });
                            } else {
                                let pending = pending_authoritative_notification
                                    .take()
                                    .expect("pending authoritative notification should exist");
                                permit.send(pending.event);
                                if let Some(write_complete_tx) = pending.write_complete_tx {
                                    let _ = write_complete_tx.send(());
                                }
                            }
                        }
                        Err(_) => break,
                    }
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
                        Some(InProcessClientMessage::Shutdown { done_tx }) => {
                            shutdown_ack = Some(done_tx);
                            break;
                        }
                        None => {
                            break;
                        }
                    }
                }
                queued_message = writer_rx.recv(), if skipped_events == 0 && pending_authoritative_notification.is_none() => {
                    let Some(queued_message) = queued_message else {
                        break;
                    };
                    let outgoing_message = queued_message.message;
                    let write_complete_tx = queued_message.write_complete_tx;
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
                        OutgoingMessage::Request(request) => {
                            // Send directly to avoid cloning; on failure the
                            // original value is returned inside the error.
                            if let Err(send_error) = event_tx
                                .try_send(InProcessServerEvent::ServerRequest(request))
                            {
                                let (error, inner) = match send_error {
                                    mpsc::error::TrySendError::Full(inner) => (
                                        JSONRPCErrorError {
                                            code: OVERLOADED_ERROR_CODE,
                                            message:
                                                "in-process server request queue is full".to_string(),
                                            data: None,
                                        },
                                        inner,
                                    ),
                                    mpsc::error::TrySendError::Closed(inner) => (
                                        internal_error(
                                            "in-process server request consumer is closed",
                                        ),
                                        inner,
                                    ),
                                };
                                let request_id = match inner {
                                    InProcessServerEvent::ServerRequest(req) => req.id().clone(),
                                    _ => unreachable!("we just sent a ServerRequest variant"),
                                };
                                outgoing_message_sender
                                    .notify_client_error(request_id, error)
                                    .await;
                            }
                        }
                        OutgoingMessage::AppServerNotification(envelope) => {
                            match try_forward_in_process_server_notification(
                                &event_tx,
                                InProcessServerEvent::ServerNotification(envelope.notification),
                                &mut skipped_events,
                            ) {
                                InProcessNotificationForward::Forwarded => {}
                                InProcessNotificationForward::Pending(event) => {
                                    pending_authoritative_notification =
                                        Some(PendingInProcessNotificationForward {
                                            event,
                                            write_complete_tx,
                                        });
                                    #[cfg(test)]
                                    runtime_test_pending_required_notification.notify_one();
                                    continue;
                                }
                                InProcessNotificationForward::Closed => break,
                            }
                        }
                    }
                    if let Some(write_complete_tx) = write_complete_tx {
                        let _ = write_complete_tx.send(());
                    }
                }
            }
        }

        if pending_authoritative_notification.is_some() || skipped_events > 0 {
            warn!(
                skipped = skipped_events,
                pending_authoritative = pending_authoritative_notification.is_some(),
                "discarding pending in-process delivery state during shutdown or closed event stream"
            );
        }
        // A retained write-completion sender can keep its producer waiting
        // while that producer still owns an outgoing sender. Release it before
        // draining dependent tasks so shutdown cannot form a custody cycle.
        drop(pending_authoritative_notification.take());
        drop(writer_rx);
        drop(processor_tx);
        outgoing_message_sender
            .cancel_all_requests(Some(internal_error(
                "in-process app-server runtime is shutting down",
            )))
            .await;
        // Drop the runtime's last sender before awaiting the router task so
        // `outgoing_rx.recv()` can observe channel closure and exit cleanly.
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
        if let Err(_elapsed) = timeout(SHUTDOWN_TIMEOUT, &mut outbound_handle).await {
            outbound_handle.abort();
            let _ = outbound_handle.await;
        }

        analytics_events_flush_client.flush().await;

        if let Some(done_tx) = shutdown_ack {
            let _ = done_tx.send(());
        }
    });

    #[cfg(test)]
    let test_outgoing_message_sender = test_outgoing_message_rx.await.map_err(|err| {
        IoError::new(
            ErrorKind::BrokenPipe,
            format!("in-process runtime exited before exposing test outgoing sender: {err}"),
        )
    })?;

    Ok(InProcessClientHandle {
        client: InProcessClientSender { client_tx },
        event_rx,
        runtime_handle,
        #[cfg(test)]
        _test_codex_home: None,
        #[cfg(test)]
        _test_outgoing_message_sender: Some(test_outgoing_message_sender),
        #[cfg(test)]
        _test_pending_required_notification: test_pending_required_notification,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::ClientInfo;
    use codex_app_server_protocol::ConfigRequirementsReadResponse;
    use codex_app_server_protocol::ExternalAgentConfigImportCompletedNotification;
    use codex_app_server_protocol::SessionSource as ApiSessionSource;
    use codex_app_server_protocol::ThreadStartParams;
    use codex_app_server_protocol::ThreadStartResponse;
    use codex_app_server_protocol::Turn;
    use codex_app_server_protocol::TurnCompletedNotification;
    use codex_app_server_protocol::TurnItemsView;
    use codex_app_server_protocol::TurnStatus;
    use codex_core::config::ConfigBuilder;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;
    use std::path::Path;
    use tempfile::TempDir;

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

    fn thread_goal_updated_notification(objective: &str) -> ServerNotification {
        ServerNotification::ThreadGoalUpdated(
            codex_app_server_protocol::ThreadGoalUpdatedNotification {
                thread_id: "thread".to_string(),
                turn_id: Some("turn".to_string()),
                goal: codex_app_server_protocol::ThreadGoal {
                    thread_id: "thread".to_string(),
                    objective: objective.to_string(),
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
            thread_goal_updated_notification("goal"),
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
                turn: Turn {
                    id: turn_id(),
                    items: Vec::new(),
                    items_view: TurnItemsView::NotLoaded,
                    status: TurnStatus::InProgress,
                    error: None,
                    started_at: Some(0),
                    completed_at: None,
                    duration_ms: None,
                },
            }),
            ServerNotification::TurnCompleted(TurnCompletedNotification {
                thread_id: thread_id(),
                turn: Turn {
                    id: turn_id(),
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
            }),
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
                ExternalAgentConfigImportCompletedNotification {
                    import_id: "import".to_string(),
                    item_type_results: Vec::new(),
                },
            ),
            ServerNotification::AgentMessageDelta(
                codex_app_server_protocol::AgentMessageDeltaNotification {
                    thread_id: thread_id(),
                    turn_id: turn_id(),
                    item_id: item_id(),
                    delta: "assistant".to_string(),
                },
            ),
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
    async fn tiny_event_queue_preserves_required_fifo_and_reports_dropped_progress() {
        for best_effort_notification in [
            command_execution_output_delta_notification("reconstructible progress"),
            fuzzy_file_search_session_updated_notification("coalescible snapshot"),
        ] {
            let mut client =
                start_test_client_with_capacity(SessionSource::Cli, /*channel_capacity*/ 1).await;
            let outgoing = Arc::clone(
                client
                    ._test_outgoing_message_sender
                    .as_ref()
                    .expect("test runtime should expose its outbound sender"),
            );

            // The first required event occupies the only event slot. Waiting
            // for the best-effort write acknowledgement makes its drop
            // deterministic. Each variant uses a fresh runtime so the lag
            // marker is drained before any later writer traffic is processed.
            outgoing
                .send_server_notification_to_connection_and_wait(
                    IN_PROCESS_CONNECTION_ID,
                    thread_goal_updated_notification("first"),
                )
                .await;
            outgoing
                .send_server_notification_to_connection_and_wait(
                    IN_PROCESS_CONNECTION_ID,
                    best_effort_notification,
                )
                .await;
            outgoing
                .send_server_notification(thread_goal_updated_notification("second"))
                .await;

            let first = timeout(Duration::from_secs(1), client.next_event())
                .await
                .expect("first required event should arrive")
                .expect("event stream should remain open");
            assert!(matches!(
                first,
                InProcessServerEvent::ServerNotification(ServerNotification::ThreadGoalUpdated(
                    notification
                )) if notification.goal.objective == "first"
            ));

            let lag = timeout(Duration::from_secs(1), client.next_event())
                .await
                .expect("lag marker should arrive")
                .expect("event stream should remain open");
            assert!(matches!(lag, InProcessServerEvent::Lagged { skipped: 1 }));

            let second = timeout(Duration::from_secs(1), client.next_event())
                .await
                .expect("second required event should arrive")
                .expect("event stream should remain open");
            assert!(matches!(
                second,
                InProcessServerEvent::ServerNotification(ServerNotification::ThreadGoalUpdated(
                    notification
                )) if notification.goal.objective == "second"
            ));

            drop(outgoing);
            client
                .shutdown()
                .await
                .expect("in-process runtime should shutdown cleanly");
        }
    }

    #[tokio::test]
    async fn full_required_event_queue_still_allows_orderly_runtime_shutdown() {
        let client =
            start_test_client_with_capacity(SessionSource::Cli, /*channel_capacity*/ 1).await;
        let outgoing = Arc::clone(
            client
                ._test_outgoing_message_sender
                .as_ref()
                .expect("test runtime should expose its outbound sender"),
        );

        outgoing
            .send_server_notification_to_connection_and_wait(
                IN_PROCESS_CONNECTION_ID,
                thread_goal_updated_notification("queued"),
            )
            .await;
        let pending_observed = client._test_pending_required_notification.notified();
        outgoing
            .send_server_notification(thread_goal_updated_notification("pending"))
            .await;
        timeout(Duration::from_secs(1), pending_observed)
            .await
            .expect("second required notification should become pending");
        drop(outgoing);

        timeout(Duration::from_secs(1), client.shutdown())
            .await
            .expect("a full required event queue must not delay shutdown acknowledgement")
            .expect("runtime should complete orderly shutdown without parent-task abort");
    }

    #[tokio::test]
    async fn shutdown_releases_pending_required_write_completion_before_task_joins() {
        let client =
            start_test_client_with_capacity(SessionSource::Cli, /*channel_capacity*/ 1).await;
        let outgoing = Arc::clone(
            client
                ._test_outgoing_message_sender
                .as_ref()
                .expect("test runtime should expose its outbound sender"),
        );

        outgoing
            .send_server_notification_to_connection_and_wait(
                IN_PROCESS_CONNECTION_ID,
                thread_goal_updated_notification("queued"),
            )
            .await;
        let pending_observed = client._test_pending_required_notification.notified();
        let (producer_released_tx, mut producer_released_rx) = oneshot::channel();
        let producer_handle = tokio::spawn(async move {
            outgoing
                .send_server_notification_to_connection_and_wait(
                    IN_PROCESS_CONNECTION_ID,
                    thread_goal_updated_notification("pending"),
                )
                .await;
            let _ = producer_released_tx.send(());
        });
        timeout(Duration::from_secs(1), pending_observed)
            .await
            .expect("second required notification should become pending");
        assert_eq!(
            producer_released_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty),
            "the real write-completion producer should remain blocked while its payload is pending"
        );

        let shutdown_handle = tokio::spawn(client.shutdown());
        timeout(Duration::from_secs(1), &mut producer_released_rx)
            .await
            .expect("shutdown should cancel the pending write-completion wait")
            .expect("pending write-completion producer should report release");
        timeout(Duration::from_secs(1), producer_handle)
            .await
            .expect("released producer should drop its outgoing sender promptly")
            .expect("write-completion producer should not panic");
        timeout(Duration::from_secs(1), shutdown_handle)
            .await
            .expect("shutdown should finish before its five-second task fallback")
            .expect("shutdown task should not panic")
            .expect("runtime should complete graceful shutdown with no stranded task sender");
    }

    #[tokio::test(start_paused = true)]
    async fn in_process_shutdown_waits_for_analytics_flush_budget() {
        let (client_tx, mut client_rx) = mpsc::channel(/*buffer*/ 1);
        let (_event_tx, event_rx) = mpsc::channel(/*buffer*/ 1);
        let completed = Arc::new(AtomicBool::new(false));
        let runtime_completed = Arc::clone(&completed);
        let runtime_handle = tokio::spawn(async move {
            let done_tx = match client_rx.recv().await {
                Some(InProcessClientMessage::Shutdown { done_tx }) => done_tx,
                _ => panic!("expected in-process shutdown request"),
            };
            tokio::time::sleep(SHUTDOWN_TIMEOUT + SHUTDOWN_TIMEOUT + Duration::from_secs(24)).await;
            runtime_completed.store(true, Ordering::Release);
            let _ = done_tx.send(());
        });
        let client = InProcessClientHandle {
            client: InProcessClientSender { client_tx },
            event_rx,
            runtime_handle,
            _test_codex_home: None,
            _test_outgoing_message_sender: None,
            _test_pending_required_notification: Arc::new(tokio::sync::Notify::new()),
        };

        client
            .shutdown()
            .await
            .expect("in-process runtime should shutdown cleanly");
        assert!(completed.load(Ordering::Acquire));
    }

    #[test]
    fn delivery_classifier_preserves_reviewed_consumer_state_notifications() {
        let notifications = reviewed_consumer_state_notifications();
        assert_eq!(notifications.len(), 32);
        assert!(
            notifications
                .iter()
                .all(server_notification_requires_delivery)
        );
        assert!(!server_notification_requires_delivery(
            &command_execution_output_delta_notification("best effort")
        ));
        assert!(!server_notification_requires_delivery(
            &fuzzy_file_search_session_updated_notification("coalescible")
        ));
    }
}
