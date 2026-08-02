use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_analytics::AnalyticsEventsClient;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::Result;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerNotificationEnvelope;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ServerRequestPayload;
use codex_app_server_protocol::ServerResponse;
use codex_app_server_transport::ThreadScopedServerNotification;
use codex_app_server_transport::ThreadScopedServerRequest;
use codex_otel::span_w3c_trace_context;
use codex_protocol::ThreadId;
use codex_protocol::protocol::W3cTraceContext;
use codex_protocol::request_permissions::RequestPermissionsResponse;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing::Instrument;
use tracing::Span;
use tracing::warn;
use uuid::Uuid;

use crate::error_code::internal_error;
use crate::server_request_error::TURN_TRANSITION_PENDING_REQUEST_ERROR_REASON;
pub(crate) use codex_app_server_transport::ConnectionId;
pub(crate) use codex_app_server_transport::OutgoingError;
pub(crate) use codex_app_server_transport::OutgoingMessage;
pub(crate) use codex_app_server_transport::OutgoingResponse;
pub(crate) use codex_app_server_transport::QueuedOutgoingMessage;

#[cfg(test)]
use codex_protocol::account::PlanType;

pub(crate) type ClientRequestResult = std::result::Result<Result, JSONRPCErrorError>;

/// Stable identifier for a client request scoped to a transport connection.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ConnectionRequestId {
    pub(crate) connection_id: ConnectionId,
    pub(crate) request_id: RequestId,
}

/// Trace data we keep for an incoming request until we send its final
/// response or error.
#[derive(Clone)]
pub(crate) struct RequestContext {
    request_id: ConnectionRequestId,
    span: Span,
    parent_trace: Option<W3cTraceContext>,
}

impl RequestContext {
    pub(crate) fn new(
        request_id: ConnectionRequestId,
        span: Span,
        parent_trace: Option<W3cTraceContext>,
    ) -> Self {
        Self {
            request_id,
            span,
            parent_trace,
        }
    }

    pub(crate) fn request_trace(&self) -> Option<W3cTraceContext> {
        span_w3c_trace_context(&self.span).or_else(|| self.parent_trace.clone())
    }

    pub(crate) fn span(&self) -> Span {
        self.span.clone()
    }

    fn record_turn_id(&self, turn_id: &str) {
        self.span.record("turn.id", turn_id);
    }
}

#[derive(Debug)]
pub(crate) enum OutgoingEnvelope {
    ToConnection {
        connection_id: ConnectionId,
        message: OutgoingMessage,
        write_complete_tx: Option<oneshot::Sender<()>>,
    },
    Broadcast {
        message: OutgoingMessage,
    },
}

/// Sends messages to the client and manages request callbacks.
pub(crate) struct OutgoingMessageSender {
    next_server_request_id: AtomicI64,
    sender: mpsc::Sender<OutgoingEnvelope>,
    request_id_to_callback: Mutex<HashMap<RequestId, PendingCallbackEntry>>,
    /// The immutable subscriber identities through which a thread-scoped
    /// request was issued. Its resolution must close that same lifecycle,
    /// even when the connection has since reattached with a newer token.
    thread_request_resolution_targets: Mutex<HashMap<RequestId, Vec<ThreadSubscriptionTarget>>>,
    /// Incoming requests that are still waiting on a final response or error.
    /// We keep them here because this is where responses, errors, and
    /// disconnect cleanup all get handled.
    request_contexts: Mutex<HashMap<ConnectionRequestId, RequestContext>>,
    /// Fresh, connection-local identities for active thread subscriptions.
    /// A queued event keeps the identity with which it was emitted; replacing
    /// this map entry therefore cannot relabel old traffic.
    thread_subscription_ids: Mutex<HashMap<(ConnectionId, ThreadId), String>>,
    analytics_events_client: AnalyticsEventsClient,
}

#[derive(Clone)]
pub(crate) struct ThreadScopedOutgoingMessageSender {
    outgoing: Arc<OutgoingMessageSender>,
    /// Immutable identities captured when the listener accepted an event.
    ///
    /// A replacement subscription may reuse the same connection and thread
    /// ids, so retaining only those two ids would let a delayed event acquire
    /// the replacement token while it is emitted. These targets deliberately
    /// carry the original token all the way to the transport envelope.
    thread_subscriptions: Arc<Vec<ThreadSubscriptionTarget>>,
    thread_id: ThreadId,
}

/// A connection-local thread identity captured at listener ingress.
///
/// This is intentionally a value, rather than a lookup key. Callers that
/// hold a captured target must send with this exact token and must not call
/// `ensure_thread_subscription` while handling that event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ThreadSubscriptionTarget {
    connection_id: ConnectionId,
    thread_id: ThreadId,
    thread_subscription_id: String,
}

impl ThreadSubscriptionTarget {
    pub(crate) fn captured(
        connection_id: ConnectionId,
        thread_id: ThreadId,
        thread_subscription_id: String,
    ) -> Self {
        Self {
            connection_id,
            thread_id,
            thread_subscription_id,
        }
    }
}

struct PendingCallbackEntry {
    callback: oneshot::Sender<ClientRequestResult>,
    thread_id: Option<ThreadId>,
    request: ServerRequest,
}

impl ThreadScopedOutgoingMessageSender {
    pub(crate) fn from_captured_thread_subscriptions(
        outgoing: Arc<OutgoingMessageSender>,
        thread_subscriptions: Vec<ThreadSubscriptionTarget>,
        thread_id: ThreadId,
    ) -> Self {
        Self {
            outgoing,
            thread_subscriptions: Arc::new(thread_subscriptions),
            thread_id,
        }
    }

    #[cfg(test)]
    pub(crate) fn new(
        outgoing: Arc<OutgoingMessageSender>,
        connection_ids: Vec<ConnectionId>,
        thread_id: ThreadId,
    ) -> Self {
        let thread_subscriptions = connection_ids
            .into_iter()
            .map(|connection_id| {
                ThreadSubscriptionTarget::captured(
                    connection_id,
                    thread_id,
                    Uuid::now_v7().to_string(),
                )
            })
            .collect();
        Self::from_captured_thread_subscriptions(outgoing, thread_subscriptions, thread_id)
    }

    pub(crate) async fn send_request(
        &self,
        payload: ServerRequestPayload,
    ) -> (RequestId, oneshot::Receiver<ClientRequestResult>) {
        self.outgoing
            .send_request_to_thread_subscriptions(
                self.thread_subscriptions.as_slice(),
                payload,
                self.thread_id,
            )
            .await
    }

    pub(crate) fn track_effective_permissions_approval_response(
        &self,
        request_id: RequestId,
        response: RequestPermissionsResponse,
    ) {
        self.outgoing
            .analytics_events_client
            .track_effective_permissions_approval_response(
                now_unix_timestamp_ms(),
                request_id,
                response,
            );
    }

    pub(crate) async fn send_server_notification(&self, notification: ServerNotification) {
        self.outgoing
            .analytics_events_client
            .track_notification(notification.clone());
        if self.thread_subscriptions.is_empty() {
            return;
        }
        self.outgoing
            .send_server_notification_to_thread_subscriptions(
                self.thread_subscriptions.as_slice(),
                notification,
            )
            .await;
    }

    pub(crate) async fn send_global_server_notification(&self, notification: ServerNotification) {
        self.outgoing.send_server_notification(notification).await;
    }

    pub(crate) async fn abort_pending_server_requests(&self) {
        self.outgoing
            .cancel_requests_for_thread(
                self.thread_id,
                Some({
                    let mut error = internal_error(
                        "client request resolved because the turn state was changed",
                    );
                    error.data = Some(serde_json::json!({
                        "reason": TURN_TRANSITION_PENDING_REQUEST_ERROR_REASON,
                    }));
                    error
                }),
            )
            .await
    }

    pub(crate) async fn send_response<T>(&self, request_id: ConnectionRequestId, response: T)
    where
        T: Into<ClientResponsePayload>,
    {
        self.outgoing.send_response(request_id, response).await;
    }

    pub(crate) async fn send_error(
        &self,
        request_id: ConnectionRequestId,
        error: impl Into<JSONRPCErrorError>,
    ) {
        self.outgoing.send_error(request_id, error).await;
    }
}

impl OutgoingMessageSender {
    pub(crate) fn new(
        sender: mpsc::Sender<OutgoingEnvelope>,
        analytics_events_client: AnalyticsEventsClient,
    ) -> Self {
        Self {
            next_server_request_id: AtomicI64::new(0),
            sender,
            request_id_to_callback: Mutex::new(HashMap::new()),
            thread_request_resolution_targets: Mutex::new(HashMap::new()),
            request_contexts: Mutex::new(HashMap::new()),
            thread_subscription_ids: Mutex::new(HashMap::new()),
            analytics_events_client,
        }
    }

    /// Creates a new immutable identity for one connection's presentation of a
    /// thread. Callers register it before emitting the successful attach
    /// response, which makes replay sent immediately after that response
    /// attributable without relying on client-side timing.
    pub(crate) async fn register_thread_subscription(
        &self,
        connection_id: ConnectionId,
        thread_id: ThreadId,
    ) -> String {
        let subscription_id = Uuid::now_v7().to_string();
        self.thread_subscription_ids
            .lock()
            .await
            .insert((connection_id, thread_id), subscription_id.clone());
        subscription_id
    }

    /// Returns the existing identity for an already attached thread, or
    /// registers one before a listener can emit any thread-scoped traffic.
    /// Explicit start/resume/fork flows call `register_thread_subscription`
    /// first to force a fresh identity; background attachment paths use this
    /// method so they cannot fall back to unscoped traffic.
    pub(crate) async fn ensure_thread_subscription(
        &self,
        connection_id: ConnectionId,
        thread_id: ThreadId,
    ) -> String {
        self.ensure_thread_subscription_with_status(connection_id, thread_id)
            .await
            .0
    }

    /// Returns an immutable subscription identity and whether this call created
    /// it. Automatic listener attachment uses the latter to send its one-time
    /// lifecycle handshake without duplicating explicit start/resume/fork
    /// notifications that already registered an identity.
    pub(crate) async fn ensure_thread_subscription_with_status(
        &self,
        connection_id: ConnectionId,
        thread_id: ThreadId,
    ) -> (String, bool) {
        let mut subscriptions = self.thread_subscription_ids.lock().await;
        match subscriptions.entry((connection_id, thread_id)) {
            std::collections::hash_map::Entry::Occupied(entry) => (entry.get().clone(), false),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let subscription_id = Uuid::now_v7().to_string();
                entry.insert(subscription_id.clone());
                (subscription_id, true)
            }
        }
    }

    pub(crate) async fn unregister_thread_subscription(
        &self,
        connection_id: ConnectionId,
        thread_id: ThreadId,
    ) {
        self.thread_subscription_ids
            .lock()
            .await
            .remove(&(connection_id, thread_id));
    }

    pub(crate) async fn unregister_thread_subscriptions_for_thread(&self, thread_id: ThreadId) {
        self.thread_subscription_ids
            .lock()
            .await
            .retain(|(_, candidate_thread_id), _| *candidate_thread_id != thread_id);
    }

    pub(crate) async fn register_request_context(&self, request_context: RequestContext) {
        let mut request_contexts = self.request_contexts.lock().await;
        if request_contexts
            .insert(request_context.request_id.clone(), request_context)
            .is_some()
        {
            warn!("replaced unresolved request context");
        }
    }

    pub(crate) async fn connection_closed(&self, connection_id: ConnectionId) {
        let mut request_contexts = self.request_contexts.lock().await;
        request_contexts.retain(|request_id, _| request_id.connection_id != connection_id);
        drop(request_contexts);
        self.thread_subscription_ids
            .lock()
            .await
            .retain(|(candidate_connection_id, _), _| *candidate_connection_id != connection_id);
    }

    pub(crate) async fn request_trace_context(
        &self,
        request_id: &ConnectionRequestId,
    ) -> Option<W3cTraceContext> {
        let request_contexts = self.request_contexts.lock().await;
        request_contexts
            .get(request_id)
            .and_then(RequestContext::request_trace)
    }

    pub(crate) async fn record_request_turn_id(
        &self,
        request_id: &ConnectionRequestId,
        turn_id: &str,
    ) {
        let request_contexts = self.request_contexts.lock().await;
        if let Some(request_context) = request_contexts.get(request_id) {
            request_context.record_turn_id(turn_id);
        }
    }

    async fn take_request_context(
        &self,
        request_id: &ConnectionRequestId,
    ) -> Option<RequestContext> {
        let mut request_contexts = self.request_contexts.lock().await;
        request_contexts.remove(request_id)
    }

    #[cfg(test)]
    async fn request_context_count(&self) -> usize {
        self.request_contexts.lock().await.len()
    }

    pub(crate) async fn send_request(
        &self,
        request: ServerRequestPayload,
    ) -> (RequestId, oneshot::Receiver<ClientRequestResult>) {
        self.send_request_to_connections(
            /*connection_ids*/ None, request, /*thread_id*/ None,
        )
        .await
    }

    fn next_request_id(&self) -> RequestId {
        RequestId::Integer(self.next_server_request_id.fetch_add(1, Ordering::Relaxed))
    }

    pub(crate) async fn send_request_to_connections(
        &self,
        connection_ids: Option<&[ConnectionId]>,
        request: ServerRequestPayload,
        thread_id: Option<ThreadId>,
    ) -> (RequestId, oneshot::Receiver<ClientRequestResult>) {
        let id = self.next_request_id();
        let outgoing_message_id = id.clone();
        let request = request.request_with_id(outgoing_message_id.clone());

        let (tx_approve, rx_approve) = oneshot::channel();
        {
            let mut request_id_to_callback = self.request_id_to_callback.lock().await;
            request_id_to_callback.insert(
                id,
                PendingCallbackEntry {
                    callback: tx_approve,
                    thread_id: thread_id.clone(),
                    request: request.clone(),
                },
            );
        }

        let send_result = match connection_ids {
            None => {
                self.sender
                    .send(OutgoingEnvelope::Broadcast {
                        message: OutgoingMessage::Request(request.clone()),
                    })
                    .await
            }
            Some(connection_ids) => {
                let mut send_error = None;
                for connection_id in connection_ids {
                    let message = match thread_id {
                        Some(thread_id) => {
                            let thread_subscription_id = self
                                .ensure_thread_subscription(*connection_id, thread_id)
                                .await;
                            OutgoingMessage::ThreadScopedRequest(ThreadScopedServerRequest {
                                request: request.clone(),
                                thread_subscription_id,
                            })
                        }
                        None => OutgoingMessage::Request(request.clone()),
                    };
                    if let Err(err) = self
                        .sender
                        .send(OutgoingEnvelope::ToConnection {
                            connection_id: *connection_id,
                            message,
                            write_complete_tx: None,
                        })
                        .await
                    {
                        send_error = Some(err);
                        break;
                    } else {
                        self.analytics_events_client
                            .track_server_request(connection_id.0, request.clone());
                    }
                }
                match send_error {
                    Some(err) => Err(err),
                    None => Ok(()),
                }
            }
        };

        if let Err(err) = send_result {
            warn!("failed to send request {outgoing_message_id:?} to client: {err:?}");
            let mut request_id_to_callback = self.request_id_to_callback.lock().await;
            request_id_to_callback.remove(&outgoing_message_id);
        }
        (outgoing_message_id, rx_approve)
    }

    /// Sends a thread-scoped server request through identities captured by a
    /// listener. Unlike `send_request_to_connections`, this must never mint
    /// or look up another subscription token: a delayed request belongs to
    /// the lifecycle that captured it, even if that lifecycle was replaced
    /// before this send reaches the transport queue.
    pub(crate) async fn send_request_to_thread_subscriptions(
        &self,
        thread_subscriptions: &[ThreadSubscriptionTarget],
        request: ServerRequestPayload,
        thread_id: ThreadId,
    ) -> (RequestId, oneshot::Receiver<ClientRequestResult>) {
        let id = self.next_request_id();
        let outgoing_message_id = id.clone();
        let request = request.request_with_id(outgoing_message_id.clone());

        let (tx_approve, rx_approve) = oneshot::channel();
        {
            let mut request_id_to_callback = self.request_id_to_callback.lock().await;
            request_id_to_callback.insert(
                id,
                PendingCallbackEntry {
                    callback: tx_approve,
                    thread_id: Some(thread_id),
                    request: request.clone(),
                },
            );
        }
        {
            let mut resolution_targets = self.thread_request_resolution_targets.lock().await;
            resolution_targets.insert(outgoing_message_id.clone(), thread_subscriptions.to_vec());
        }

        let mut send_error = None;
        for thread_subscription in thread_subscriptions {
            debug_assert_eq!(thread_subscription.thread_id, thread_id);
            let message = OutgoingMessage::ThreadScopedRequest(ThreadScopedServerRequest {
                request: request.clone(),
                thread_subscription_id: thread_subscription.thread_subscription_id.clone(),
            });
            if let Err(err) = self
                .sender
                .send(OutgoingEnvelope::ToConnection {
                    connection_id: thread_subscription.connection_id,
                    message,
                    write_complete_tx: None,
                })
                .await
            {
                send_error = Some(err);
                break;
            } else {
                self.analytics_events_client
                    .track_server_request(thread_subscription.connection_id.0, request.clone());
            }
        }

        if let Some(err) = send_error {
            warn!(
                "failed to send captured thread request {outgoing_message_id:?} to client: {err:?}"
            );
            let mut request_id_to_callback = self.request_id_to_callback.lock().await;
            request_id_to_callback.remove(&outgoing_message_id);
            self.thread_request_resolution_targets
                .lock()
                .await
                .remove(&outgoing_message_id);
        }
        (outgoing_message_id, rx_approve)
    }

    pub(crate) async fn replay_requests_to_connection_for_thread(
        &self,
        connection_id: ConnectionId,
        thread_id: ThreadId,
    ) {
        let requests = self.pending_requests_for_thread(thread_id).await;
        for request in requests {
            let thread_subscription_id = self
                .ensure_thread_subscription(connection_id, thread_id)
                .await;
            let message = OutgoingMessage::ThreadScopedRequest(ThreadScopedServerRequest {
                request,
                thread_subscription_id,
            });
            if let Err(err) = self
                .sender
                .send(OutgoingEnvelope::ToConnection {
                    connection_id,
                    message,
                    write_complete_tx: None,
                })
                .await
            {
                warn!("failed to resend request to client: {err:?}");
            }
        }
    }

    pub(crate) async fn notify_client_response(&self, id: RequestId, result: Result) {
        let entry = self.take_request_callback(&id).await;

        match entry {
            Some((id, entry)) => {
                let completed_at_ms = now_unix_timestamp_ms();
                if let Ok(response) = entry.request.response_from_result(result.clone())
                    && !matches!(response, ServerResponse::PermissionsRequestApproval { .. })
                {
                    self.analytics_events_client
                        .track_server_response(completed_at_ms, response);
                }
                if let Err(err) = entry.callback.send(Ok(result)) {
                    warn!("could not notify callback for {id:?} due to: {err:?}");
                }
            }
            None => {
                warn!("could not find callback for {id:?}");
            }
        }
    }

    pub(crate) async fn notify_client_error(&self, id: RequestId, error: JSONRPCErrorError) {
        let entry = self.take_request_callback(&id).await;

        match entry {
            Some((id, entry)) => {
                warn!("client responded with error for {id:?}: {error:?}");
                self.analytics_events_client
                    .track_server_request_aborted(now_unix_timestamp_ms(), id.clone());
                if let Err(err) = entry.callback.send(Err(error)) {
                    warn!("could not notify callback for {id:?} due to: {err:?}");
                }
            }
            None => {
                warn!("could not find callback for {id:?}");
            }
        }
    }

    pub(crate) async fn cancel_request(&self, id: &RequestId) -> bool {
        let entry = self.take_request_callback(id).await;
        self.thread_request_resolution_targets
            .lock()
            .await
            .remove(id);
        if let Some((request_id, _entry)) = entry {
            self.analytics_events_client
                .track_server_request_aborted(now_unix_timestamp_ms(), request_id);
            true
        } else {
            false
        }
    }

    pub(crate) async fn cancel_all_requests(&self, error: Option<JSONRPCErrorError>) {
        let entries = {
            let mut request_id_to_callback = self.request_id_to_callback.lock().await;
            request_id_to_callback
                .drain()
                .map(|(_, entry)| entry)
                .collect::<Vec<_>>()
        };
        self.thread_request_resolution_targets.lock().await.clear();

        for entry in entries {
            self.analytics_events_client
                .track_server_request_aborted(now_unix_timestamp_ms(), entry.request.id().clone());
            if let Some(error) = error.as_ref()
                && let Err(err) = entry.callback.send(Err(error.clone()))
            {
                let request_id = entry.request.id();
                warn!("could not notify callback for {request_id:?} due to: {err:?}");
            }
        }
    }

    async fn take_request_callback(
        &self,
        id: &RequestId,
    ) -> Option<(RequestId, PendingCallbackEntry)> {
        let mut request_id_to_callback = self.request_id_to_callback.lock().await;
        request_id_to_callback.remove_entry(id)
    }

    /// Takes the immutable targets captured for a thread-scoped request's
    /// resolution. A missing entry is intentionally not reconstructed from
    /// the current subscription map: that request has no safe lifecycle to
    /// address after its original target was discarded.
    pub(crate) async fn take_thread_request_resolution_targets(
        &self,
        id: &RequestId,
    ) -> Option<Vec<ThreadSubscriptionTarget>> {
        self.thread_request_resolution_targets
            .lock()
            .await
            .remove(id)
    }

    pub(crate) async fn pending_requests_for_thread(
        &self,
        thread_id: ThreadId,
    ) -> Vec<ServerRequest> {
        let request_id_to_callback = self.request_id_to_callback.lock().await;
        let mut requests = request_id_to_callback
            .values()
            .filter_map(|entry| {
                (entry.thread_id == Some(thread_id)).then_some(entry.request.clone())
            })
            .collect::<Vec<_>>();
        requests.sort_by(|left, right| left.id().cmp(right.id()));
        requests
    }

    pub(crate) async fn cancel_requests_for_thread(
        &self,
        thread_id: ThreadId,
        error: Option<JSONRPCErrorError>,
    ) {
        let (entries, request_ids) = {
            let mut request_id_to_callback = self.request_id_to_callback.lock().await;
            let request_ids = request_id_to_callback
                .iter()
                .filter_map(|(request_id, entry)| {
                    (entry.thread_id == Some(thread_id)).then_some(request_id.clone())
                })
                .collect::<Vec<_>>();

            let mut entries = Vec::with_capacity(request_ids.len());
            for request_id in &request_ids {
                if let Some(entry) = request_id_to_callback.remove(request_id) {
                    entries.push(entry);
                }
            }
            (entries, request_ids)
        };
        {
            let mut resolution_targets = self.thread_request_resolution_targets.lock().await;
            for request_id in request_ids {
                resolution_targets.remove(&request_id);
            }
        }

        for entry in entries {
            self.analytics_events_client
                .track_server_request_aborted(now_unix_timestamp_ms(), entry.request.id().clone());
            if let Some(error) = error.as_ref()
                && let Err(err) = entry.callback.send(Err(error.clone()))
            {
                let request_id = entry.request.id();
                warn!("could not notify callback for {request_id:?} due to: {err:?}",);
            }
        }
    }

    /// Captures every active presentation of a thread for a listener event.
    ///
    /// The returned values remain valid transport identities even if the
    /// subscription map changes later. They must be sent with the explicit
    /// target APIs below rather than converted back into connection ids.
    pub(crate) async fn thread_subscription_targets_for_thread(
        &self,
        thread_id: ThreadId,
    ) -> Vec<ThreadSubscriptionTarget> {
        self.thread_subscription_ids
            .lock()
            .await
            .iter()
            .filter_map(|((connection_id, candidate_thread_id), subscription_id)| {
                (*candidate_thread_id == thread_id).then(|| {
                    ThreadSubscriptionTarget::captured(
                        *connection_id,
                        thread_id,
                        subscription_id.clone(),
                    )
                })
            })
            .collect()
    }

    /// Sends a notification through identities captured by a listener or
    /// teardown path. This deliberately does not consult or recreate the
    /// subscription map: a stale token is fenced by the client, while a
    /// terminal notification captured before teardown still reaches every
    /// subscriber that observed that lifecycle.
    pub(crate) async fn send_server_notification_to_thread_subscriptions(
        &self,
        thread_subscriptions: &[ThreadSubscriptionTarget],
        notification: ServerNotification,
    ) {
        tracing::trace!(
            targeted_connections = thread_subscriptions.len(),
            "app-server event: {notification}"
        );
        if let Some(thread_id) = server_notification_thread_id(&notification) {
            debug_assert!(
                thread_subscriptions
                    .iter()
                    .all(|thread_subscription| thread_subscription.thread_id == thread_id)
            );
        }
        let envelope = timestamped_server_notification_envelope(notification);
        for thread_subscription in thread_subscriptions {
            if let Err(err) = self
                .sender
                .send(OutgoingEnvelope::ToConnection {
                    connection_id: thread_subscription.connection_id,
                    message: OutgoingMessage::ThreadScopedNotification(
                        ThreadScopedServerNotification {
                            envelope: envelope.clone(),
                            thread_subscription_id: thread_subscription
                                .thread_subscription_id
                                .clone(),
                        },
                    ),
                    write_complete_tx: None,
                })
                .await
            {
                warn!("failed to send captured thread notification to client: {err:?}");
            }
        }
    }

    async fn thread_scoped_notification_message(
        &self,
        connection_id: ConnectionId,
        notification: &ServerNotification,
        envelope: &ServerNotificationEnvelope,
    ) -> OutgoingMessage {
        let Some(thread_id) = server_notification_thread_id(notification) else {
            return OutgoingMessage::AppServerNotification(envelope.clone());
        };
        let thread_subscription_id = self
            .ensure_thread_subscription(connection_id, thread_id)
            .await;
        OutgoingMessage::ThreadScopedNotification(ThreadScopedServerNotification {
            envelope: envelope.clone(),
            thread_subscription_id,
        })
    }

    pub(crate) async fn send_response<T>(&self, request_id: ConnectionRequestId, response: T)
    where
        T: Into<ClientResponsePayload>,
    {
        self.send_response_as_inner(request_id, response.into(), /*thread_originator*/ None)
            .await;
    }

    pub(crate) async fn send_response_with_thread_originator<T>(
        &self,
        request_id: ConnectionRequestId,
        response: T,
        thread_originator: String,
    ) where
        T: Into<ClientResponsePayload>,
    {
        self.send_response_as_inner(request_id, response.into(), Some(thread_originator))
            .await;
    }

    pub(crate) async fn send_response_as(
        &self,
        request_id: ConnectionRequestId,
        response: ClientResponsePayload,
    ) {
        self.send_response_as_inner(request_id, response, /*thread_originator*/ None)
            .await;
    }

    async fn send_response_as_inner(
        &self,
        request_id: ConnectionRequestId,
        response: ClientResponsePayload,
        thread_originator: Option<String>,
    ) {
        let connection_id = request_id.connection_id;
        let request_id_for_analytics = request_id.request_id.clone();
        let serialized_response = response
            .into_jsonrpc_parts_and_payload(request_id.request_id.clone())
            .map(|(id, result, response)| {
                if let Some(response) = response {
                    match thread_originator {
                        Some(thread_originator) => {
                            self.analytics_events_client
                                .track_response_with_thread_originator(
                                    connection_id.0,
                                    request_id_for_analytics,
                                    response,
                                    thread_originator,
                                );
                        }
                        None => {
                            self.analytics_events_client.track_response(
                                connection_id.0,
                                request_id_for_analytics,
                                response,
                            );
                        }
                    }
                }
                (id, result)
            });
        let request_context = self.take_request_context(&request_id).await;

        match serialized_response {
            Ok((id, result)) => {
                let outgoing_message = OutgoingMessage::Response(OutgoingResponse { id, result });
                self.send_outgoing_message_to_connection(
                    request_context,
                    connection_id,
                    outgoing_message,
                    "response",
                )
                .await;
            }
            Err(err) => {
                self.send_error_inner(
                    request_context,
                    request_id,
                    internal_error(format!("failed to serialize response: {err}")),
                )
                .await;
            }
        }
    }

    pub(crate) async fn send_server_notification(&self, notification: ServerNotification) {
        self.send_server_notification_to_connections(&[], notification)
            .await;
    }

    pub(crate) async fn send_server_notification_to_connections(
        &self,
        connection_ids: &[ConnectionId],
        notification: ServerNotification,
    ) {
        tracing::trace!(
            targeted_connections = connection_ids.len(),
            "app-server event: {notification}"
        );
        let envelope = timestamped_server_notification_envelope(notification.clone());
        if connection_ids.is_empty() {
            if let Some(thread_id) = server_notification_thread_id(&notification) {
                let subscriptions = self.thread_subscription_targets_for_thread(thread_id).await;
                if !subscriptions.is_empty() {
                    for thread_subscription in subscriptions {
                        if let Err(err) = self
                            .sender
                            .send(OutgoingEnvelope::ToConnection {
                                connection_id: thread_subscription.connection_id,
                                message: OutgoingMessage::ThreadScopedNotification(
                                    ThreadScopedServerNotification {
                                        envelope: envelope.clone(),
                                        thread_subscription_id: thread_subscription
                                            .thread_subscription_id,
                                    },
                                ),
                                write_complete_tx: None,
                            })
                            .await
                        {
                            warn!("failed to send server notification to client: {err:?}");
                        }
                    }
                    return;
                }
                tracing::debug!(
                    %thread_id,
                    "dropping thread-bound notification without an active subscription"
                );
                return;
            }
            if let Err(err) = self
                .sender
                .send(OutgoingEnvelope::Broadcast {
                    message: OutgoingMessage::AppServerNotification(envelope),
                })
                .await
            {
                warn!("failed to send server notification to client: {err:?}");
            }
            return;
        }
        for connection_id in connection_ids {
            let message = self
                .thread_scoped_notification_message(*connection_id, &notification, &envelope)
                .await;
            if let Err(err) = self
                .sender
                .send(OutgoingEnvelope::ToConnection {
                    connection_id: *connection_id,
                    message,
                    write_complete_tx: None,
                })
                .await
            {
                warn!("failed to send server notification to client: {err:?}");
            }
        }
    }

    pub(crate) async fn send_server_notification_to_connection(
        &self,
        connection_id: ConnectionId,
        notification: ServerNotification,
    ) {
        tracing::trace!("app-server event: {notification}");
        let envelope = timestamped_server_notification_envelope(notification.clone());
        let outgoing_message = self
            .thread_scoped_notification_message(connection_id, &notification, &envelope)
            .await;
        if let Err(err) = self
            .sender
            .send(OutgoingEnvelope::ToConnection {
                connection_id,
                message: outgoing_message,
                write_complete_tx: None,
            })
            .await
        {
            warn!("failed to send server notification to client: {err:?}");
        }
    }

    pub(crate) async fn send_server_notification_to_connection_and_wait(
        &self,
        connection_id: ConnectionId,
        notification: ServerNotification,
    ) {
        tracing::trace!("app-server event: {notification}");
        let envelope = timestamped_server_notification_envelope(notification.clone());
        let outgoing_message = self
            .thread_scoped_notification_message(connection_id, &notification, &envelope)
            .await;
        let (write_complete_tx, write_complete_rx) = oneshot::channel();
        if let Err(err) = self
            .sender
            .send(OutgoingEnvelope::ToConnection {
                connection_id,
                message: outgoing_message,
                write_complete_tx: Some(write_complete_tx),
            })
            .await
        {
            warn!("failed to send server notification to client: {err:?}");
        }
        let _ = write_complete_rx.await;
    }

    pub(crate) async fn send_error(
        &self,
        request_id: ConnectionRequestId,
        error: impl Into<JSONRPCErrorError>,
    ) {
        let request_context = self.take_request_context(&request_id).await;
        self.send_error_inner(request_context, request_id, error.into())
            .await;
    }

    pub(crate) async fn send_result<T, E>(
        &self,
        request_id: ConnectionRequestId,
        result: std::result::Result<T, E>,
    ) where
        T: Into<ClientResponsePayload>,
        E: Into<JSONRPCErrorError>,
    {
        match result {
            Ok(response) => {
                self.send_response(request_id, response).await;
            }
            Err(error) => self.send_error(request_id, error).await,
        }
    }

    async fn send_error_inner(
        &self,
        request_context: Option<RequestContext>,
        request_id: ConnectionRequestId,
        error: JSONRPCErrorError,
    ) {
        let outgoing_message = OutgoingMessage::Error(OutgoingError {
            id: request_id.request_id,
            error,
        });
        self.send_outgoing_message_to_connection(
            request_context,
            request_id.connection_id,
            outgoing_message,
            "error",
        )
        .await;
    }

    async fn send_outgoing_message_to_connection(
        &self,
        request_context: Option<RequestContext>,
        connection_id: ConnectionId,
        message: OutgoingMessage,
        message_kind: &'static str,
    ) {
        let send_fut = self.sender.send(OutgoingEnvelope::ToConnection {
            connection_id,
            message,
            write_complete_tx: None,
        });
        let send_result = if let Some(request_context) = request_context {
            send_fut.instrument(request_context.span()).await
        } else {
            send_fut.await
        };

        if let Err(err) = send_result {
            warn!("failed to send {message_kind} to client: {err:?}");
        }
    }
}

fn server_notification_thread_id(notification: &ServerNotification) -> Option<ThreadId> {
    let thread_id = match notification {
        ServerNotification::Error(notification) => Some(notification.thread_id.as_str()),
        ServerNotification::ThreadStarted(notification) => Some(notification.thread.id.as_str()),
        ServerNotification::ThreadStatusChanged(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::ThreadArchived(notification) => Some(notification.thread_id.as_str()),
        ServerNotification::ThreadDeleted(notification) => Some(notification.thread_id.as_str()),
        ServerNotification::ThreadUnarchived(notification) => Some(notification.thread_id.as_str()),
        ServerNotification::ThreadClosed(notification) => Some(notification.thread_id.as_str()),
        ServerNotification::ThreadNameUpdated(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::ThreadTokenUsageUpdated(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::ThreadGoalUpdated(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::ThreadGoalCleared(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::ThreadSettingsUpdated(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::TurnStarted(notification) => Some(notification.thread_id.as_str()),
        ServerNotification::HookStarted(notification) => Some(notification.thread_id.as_str()),
        ServerNotification::TurnCompleted(notification) => Some(notification.thread_id.as_str()),
        ServerNotification::HookCompleted(notification) => Some(notification.thread_id.as_str()),
        ServerNotification::TurnDiffUpdated(notification) => Some(notification.thread_id.as_str()),
        ServerNotification::TurnPlanUpdated(notification) => Some(notification.thread_id.as_str()),
        ServerNotification::ItemStarted(notification) => Some(notification.thread_id.as_str()),
        ServerNotification::ItemGuardianApprovalReviewStarted(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::ItemGuardianApprovalReviewCompleted(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::ItemCompleted(notification) => Some(notification.thread_id.as_str()),
        ServerNotification::RawResponseItemCompleted(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::RawResponseCompleted(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::AgentMessageDelta(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::PlanDelta(notification) => Some(notification.thread_id.as_str()),
        ServerNotification::CommandExecutionOutputDelta(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::TerminalInteraction(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::FileChangeOutputDelta(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::FileChangePatchUpdated(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::ServerRequestResolved(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::McpToolCallProgress(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::ReasoningSummaryTextDelta(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::ReasoningSummaryPartAdded(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::ReasoningTextDelta(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::ContextCompacted(notification) => Some(notification.thread_id.as_str()),
        ServerNotification::ModelRerouted(notification) => Some(notification.thread_id.as_str()),
        ServerNotification::ModelVerification(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::ModelSafetyBufferingUpdated(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::TurnModerationMetadata(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::ThreadRealtimeStarted(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::ThreadRealtimeItemAdded(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::ThreadRealtimeTranscriptDelta(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::ThreadRealtimeTranscriptDone(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::ThreadRealtimeOutputAudioDelta(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::ThreadRealtimeSdp(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::ThreadRealtimeError(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::ThreadRealtimeClosed(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::Warning(notification) => notification.thread_id.as_deref(),
        ServerNotification::GuardianWarning(notification) => Some(notification.thread_id.as_str()),
        ServerNotification::McpServerStatusUpdated(notification) => {
            notification.thread_id.as_deref()
        }
        ServerNotification::SkillsChanged(_)
        | ServerNotification::McpServerOauthLoginCompleted(_)
        | ServerNotification::AccountUpdated(_)
        | ServerNotification::AccountRateLimitsUpdated(_)
        | ServerNotification::AppListUpdated(_)
        | ServerNotification::EnvironmentConnected(_)
        | ServerNotification::EnvironmentDisconnected(_)
        | ServerNotification::RemoteControlStatusChanged(_)
        | ServerNotification::ExternalAgentConfigImportProgress(_)
        | ServerNotification::ExternalAgentConfigImportCompleted(_)
        | ServerNotification::DeprecationNotice(_)
        | ServerNotification::ConfigWarning(_)
        | ServerNotification::FuzzyFileSearchSessionUpdated(_)
        | ServerNotification::FuzzyFileSearchSessionCompleted(_)
        | ServerNotification::CommandExecOutputDelta(_)
        | ServerNotification::ProcessOutputDelta(_)
        | ServerNotification::ProcessExited(_)
        | ServerNotification::FsChanged(_)
        | ServerNotification::WindowsWorldWritableWarning(_)
        | ServerNotification::WindowsSandboxSetupCompleted(_)
        | ServerNotification::AccountLoginCompleted(_) => None,
    };
    thread_id.and_then(|thread_id| ThreadId::from_string(thread_id).ok())
}

fn now_unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or_default()
}

fn timestamped_server_notification_envelope(
    notification: ServerNotification,
) -> ServerNotificationEnvelope {
    ServerNotificationEnvelope {
        notification,
        emitted_at_ms: Some(now_unix_timestamp_ms().try_into().unwrap_or_default()),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use codex_app_server_protocol::AccountLoginCompletedNotification;
    use codex_app_server_protocol::AccountRateLimitsUpdatedNotification;
    use codex_app_server_protocol::AccountUpdatedNotification;
    use codex_app_server_protocol::ApplyPatchApprovalParams;
    use codex_app_server_protocol::AuthMode;
    use codex_app_server_protocol::CommandExecutionApprovalDecision;
    use codex_app_server_protocol::CommandExecutionRequestApprovalParams;
    use codex_app_server_protocol::ConfigWarningNotification;
    use codex_app_server_protocol::CurrentTimeReadParams;
    use codex_app_server_protocol::DynamicToolCallParams;
    use codex_app_server_protocol::FileChangeRequestApprovalParams;
    use codex_app_server_protocol::GuardianWarningNotification;
    use codex_app_server_protocol::ModelRerouteReason;
    use codex_app_server_protocol::ModelReroutedNotification;
    use codex_app_server_protocol::ModelVerification;
    use codex_app_server_protocol::ModelVerificationNotification;
    use codex_app_server_protocol::RateLimitSnapshot;
    use codex_app_server_protocol::RateLimitWindow;
    use codex_app_server_protocol::ServerRequestResolvedNotification;
    use codex_app_server_protocol::ServerResponse;
    use codex_app_server_protocol::ThreadArchivedNotification;
    use codex_app_server_protocol::ThreadClosedNotification;
    use codex_app_server_protocol::ToolRequestUserInputParams;
    use codex_app_server_protocol::TurnModerationMetadataNotification;
    use codex_protocol::ThreadId;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::time::timeout;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn verify_server_notification_serialization() {
        let notification =
            ServerNotification::AccountLoginCompleted(AccountLoginCompletedNotification {
                login_id: Some(Uuid::nil().to_string()),
                success: true,
                error: None,
            });

        let jsonrpc_notification =
            OutgoingMessage::AppServerNotification(ServerNotificationEnvelope {
                notification,
                emitted_at_ms: Some(1_234),
            });
        assert_eq!(
            json!({
                "method": "account/login/completed",
                "params": {
                    "loginId": Uuid::nil().to_string(),
                    "success": true,
                    "error": null,
                },
                "emittedAtMs": 1_234,
            }),
            serde_json::to_value(jsonrpc_notification)
                .expect("ensure the strum macros serialize the method field correctly"),
            "ensure the strum macros serialize the method field correctly"
        );
    }

    #[test]
    fn verify_account_login_completed_notification_serialization() {
        let notification =
            ServerNotification::AccountLoginCompleted(AccountLoginCompletedNotification {
                login_id: Some(Uuid::nil().to_string()),
                success: true,
                error: None,
            });

        assert_eq!(
            json!({
                "method": "account/login/completed",
                "params": {
                    "loginId": Uuid::nil().to_string(),
                    "success": true,
                    "error": null,
                },
            }),
            serde_json::to_value(notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[test]
    fn verify_account_rate_limits_notification_serialization() {
        let notification =
            ServerNotification::AccountRateLimitsUpdated(AccountRateLimitsUpdatedNotification {
                rate_limits: RateLimitSnapshot {
                    limit_id: Some("codex".to_string()),
                    limit_name: None,
                    primary: Some(RateLimitWindow {
                        used_percent: 25,
                        window_duration_mins: Some(15),
                        resets_at: Some(123),
                    }),
                    secondary: None,
                    credits: None,
                    individual_limit: None,
                    spend_control_reached: None,
                    plan_type: Some(PlanType::Plus),
                    rate_limit_reached_type: None,
                },
            });

        assert_eq!(
            json!({
                "method": "account/rateLimits/updated",
                "params": {
                        "rateLimits": {
                        "limitId": "codex",
                        "limitName": null,
                        "primary": {
                            "usedPercent": 25,
                            "windowDurationMins": 15,
                            "resetsAt": 123
                        },
                        "secondary": null,
                        "credits": null,
                        "individualLimit": null,
                        "spendControlReached": null,
                        "planType": "plus",
                        "rateLimitReachedType": null
                    }
                },
            }),
            serde_json::to_value(notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[test]
    fn verify_account_updated_notification_serialization() {
        let notification = ServerNotification::AccountUpdated(AccountUpdatedNotification {
            auth_mode: Some(AuthMode::ApiKey),
            plan_type: None,
        });

        assert_eq!(
            json!({
                "method": "account/updated",
                "params": {
                    "authMode": "apikey",
                    "planType": null
                },
            }),
            serde_json::to_value(notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[test]
    fn verify_config_warning_notification_serialization() {
        let notification = ServerNotification::ConfigWarning(ConfigWarningNotification {
            summary: "Config error: using defaults".to_string(),
            details: Some("error loading config: bad config".to_string()),
            path: None,
            range: None,
        });

        assert_eq!(
            json!( {
                "method": "configWarning",
                "params": {
                    "summary": "Config error: using defaults",
                    "details": "error loading config: bad config",
                },
            }),
            serde_json::to_value(notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[test]
    fn verify_guardian_warning_notification_serialization() {
        let notification = ServerNotification::GuardianWarning(GuardianWarningNotification {
            thread_id: "thread-1".to_string(),
            message: "Automatic approval review denied the requested action.".to_string(),
        });

        assert_eq!(
            json!({
                "method": "guardianWarning",
                "params": {
                    "threadId": "thread-1",
                    "message": "Automatic approval review denied the requested action.",
                },
            }),
            serde_json::to_value(notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[test]
    fn verify_model_rerouted_notification_serialization() {
        let notification = ServerNotification::ModelRerouted(ModelReroutedNotification {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            from_model: "gpt-5.3-codex".to_string(),
            to_model: "gpt-5.2".to_string(),
            reason: ModelRerouteReason::HighRiskCyberActivity,
        });

        assert_eq!(
            json!({
                "method": "model/rerouted",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "fromModel": "gpt-5.3-codex",
                    "toModel": "gpt-5.2",
                    "reason": "highRiskCyberActivity",
                },
            }),
            serde_json::to_value(notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[test]
    fn verify_model_verification_notification_serialization() {
        let notification = ServerNotification::ModelVerification(ModelVerificationNotification {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            verifications: vec![ModelVerification::TrustedAccessForCyber],
        });

        assert_eq!(
            json!({
                "method": "model/verification",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "verifications": ["trustedAccessForCyber"],
                },
            }),
            serde_json::to_value(notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[test]
    fn verify_turn_moderation_metadata_notification_serialization() {
        let notification =
            ServerNotification::TurnModerationMetadata(TurnModerationMetadataNotification {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                metadata: json!({"presentation": "inline"}),
            });

        assert_eq!(
            json!({
                "method": "turn/moderationMetadata",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "metadata": {"presentation": "inline"},
                },
            }),
            serde_json::to_value(notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[test]
    fn server_request_response_from_result_decodes_typed_response() {
        let request = ServerRequest::CommandExecutionRequestApproval {
            request_id: RequestId::Integer(7),
            params: CommandExecutionRequestApprovalParams {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item_id: "item-1".to_string(),
                started_at_ms: 0,
                approval_id: None,
                environment_id: None,
                reason: None,
                network_approval_context: None,
                command: Some("echo hi".to_string()),
                cwd: None,
                command_actions: None,
                additional_permissions: None,
                proposed_execpolicy_amendment: None,
                proposed_network_policy_amendments: None,
                available_decisions: None,
            },
        };

        let response = request
            .response_from_result(json!({
                "decision": "acceptForSession",
            }))
            .expect("decode typed server response");

        let ServerResponse::CommandExecutionRequestApproval {
            request_id,
            response,
        } = response
        else {
            panic!("expected command execution approval response");
        };
        assert_eq!(request_id, RequestId::Integer(7));
        assert_eq!(
            response.decision,
            CommandExecutionApprovalDecision::AcceptForSession
        );
    }
    #[tokio::test]
    async fn send_response_routes_to_target_connection() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        let request_id = ConnectionRequestId {
            connection_id: ConnectionId(42),
            request_id: RequestId::Integer(7),
        };

        outgoing
            .send_response(
                request_id.clone(),
                ClientResponsePayload::ThreadArchive(
                    codex_app_server_protocol::ThreadArchiveResponse {},
                ),
            )
            .await;

        let envelope = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("should receive envelope before timeout")
            .expect("channel should contain one message");

        match envelope {
            OutgoingEnvelope::ToConnection {
                connection_id,
                message,
                ..
            } => {
                assert_eq!(connection_id, ConnectionId(42));
                let OutgoingMessage::Response(response) = message else {
                    panic!("expected response message");
                };
                assert_eq!(response.id, request_id.request_id);
                assert_eq!(response.result, json!({}));
            }
            other => panic!("expected targeted response envelope, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_response_clears_registered_request_context() {
        let (tx, _rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        let request_id = ConnectionRequestId {
            connection_id: ConnectionId(42),
            request_id: RequestId::Integer(7),
        };

        outgoing
            .register_request_context(RequestContext::new(
                request_id.clone(),
                tracing::info_span!("app_server.request", rpc.method = "thread/start"),
                /*parent_trace*/ None,
            ))
            .await;
        assert_eq!(outgoing.request_context_count().await, 1);

        outgoing
            .send_response(
                request_id,
                ClientResponsePayload::ThreadArchive(
                    codex_app_server_protocol::ThreadArchiveResponse {},
                ),
            )
            .await;

        assert_eq!(outgoing.request_context_count().await, 0);
    }

    #[tokio::test]
    async fn send_error_routes_to_target_connection() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        let request_id = ConnectionRequestId {
            connection_id: ConnectionId(9),
            request_id: RequestId::Integer(3),
        };
        let error = internal_error("boom");

        outgoing.send_error(request_id.clone(), error.clone()).await;

        let envelope = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("should receive envelope before timeout")
            .expect("channel should contain one message");

        match envelope {
            OutgoingEnvelope::ToConnection {
                connection_id,
                message,
                ..
            } => {
                assert_eq!(connection_id, ConnectionId(9));
                let OutgoingMessage::Error(outgoing_error) = message else {
                    panic!("expected error message");
                };
                assert_eq!(outgoing_error.id, RequestId::Integer(3));
                assert_eq!(outgoing_error.error, error);
            }
            other => panic!("expected targeted error envelope, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_server_notification_to_connections_reuses_timestamp() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(2);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());

        outgoing
            .send_server_notification_to_connections(
                &[ConnectionId(1), ConnectionId(2)],
                ServerNotification::ConfigWarning(ConfigWarningNotification {
                    summary: "test".to_string(),
                    details: None,
                    path: None,
                    range: None,
                }),
            )
            .await;

        let timestamps = [
            rx.recv()
                .await
                .expect("first connection should receive notification"),
            rx.recv()
                .await
                .expect("second connection should receive notification"),
        ]
        .map(|envelope| match envelope {
            OutgoingEnvelope::ToConnection {
                message: OutgoingMessage::AppServerNotification(envelope),
                ..
            } => envelope.emitted_at_ms,
            _ => panic!("expected targeted server notification"),
        });

        assert_eq!(timestamps[0], timestamps[1]);
    }

    #[tokio::test]
    async fn thread_notification_keeps_its_original_subscription_identity_after_reattach() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(2);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        let connection_id = ConnectionId(9);
        let thread_id = ThreadId::new();
        let old_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;

        outgoing
            .send_server_notification_to_connection(
                connection_id,
                ServerNotification::ThreadClosed(ThreadClosedNotification {
                    thread_id: thread_id.to_string(),
                }),
            )
            .await;
        let first = rx
            .recv()
            .await
            .expect("first thread notification should be queued");

        let new_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        outgoing
            .send_server_notification_to_connection(
                connection_id,
                ServerNotification::ThreadClosed(ThreadClosedNotification {
                    thread_id: thread_id.to_string(),
                }),
            )
            .await;
        let second = rx
            .recv()
            .await
            .expect("second thread notification should be queued");

        let subscription_id = |envelope: OutgoingEnvelope| match envelope {
            OutgoingEnvelope::ToConnection {
                message: OutgoingMessage::ThreadScopedNotification(notification),
                ..
            } => notification.thread_subscription_id,
            other => panic!("expected tagged thread notification, got {other:?}"),
        };
        assert_eq!(subscription_id(first), old_subscription_id);
        assert_eq!(subscription_id(second), new_subscription_id);
    }

    #[tokio::test]
    async fn current_time_request_keeps_its_subscription_identity_after_reattach() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(2);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        let connection_id = ConnectionId(9);
        let thread_id = ThreadId::new();
        let old_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;

        let connection_ids = [connection_id];
        let (_request_id, _response_rx) = outgoing
            .send_request_to_connections(
                Some(&connection_ids),
                ServerRequestPayload::CurrentTimeRead(CurrentTimeReadParams {
                    thread_id: thread_id.to_string(),
                }),
                Some(thread_id),
            )
            .await;
        let stale = rx
            .recv()
            .await
            .expect("current-time request should be queued");

        let new_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        let (_request_id, _response_rx) = outgoing
            .send_request_to_connections(
                Some(&connection_ids),
                ServerRequestPayload::CurrentTimeRead(CurrentTimeReadParams {
                    thread_id: thread_id.to_string(),
                }),
                Some(thread_id),
            )
            .await;
        let current = rx
            .recv()
            .await
            .expect("replacement request should be queued");

        let subscription_and_thread_id = |envelope: OutgoingEnvelope| match envelope {
            OutgoingEnvelope::ToConnection {
                message: OutgoingMessage::ThreadScopedRequest(request),
                ..
            } => {
                let ServerRequest::CurrentTimeRead { params, .. } = request.request else {
                    panic!("expected tagged current-time request");
                };
                (request.thread_subscription_id, params.thread_id)
            }
            other => panic!("expected tagged current-time request, got {other:?}"),
        };

        assert_eq!(
            subscription_and_thread_id(stale),
            (old_subscription_id, thread_id.to_string()),
            "a request queued before reattach must retain its old subscription token"
        );
        assert_eq!(
            subscription_and_thread_id(current),
            (new_subscription_id, thread_id.to_string()),
            "a request after reattach must be delivered through the replacement token"
        );
    }

    #[tokio::test]
    async fn captured_listener_targets_keep_old_tokens_after_reattach() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(6);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let connection_id = ConnectionId(9);
        let thread_id = ThreadId::new();
        let old_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        let old_listener = ThreadScopedOutgoingMessageSender::from_captured_thread_subscriptions(
            outgoing.clone(),
            outgoing
                .thread_subscription_targets_for_thread(thread_id)
                .await,
            thread_id,
        );

        let new_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        let new_listener = ThreadScopedOutgoingMessageSender::from_captured_thread_subscriptions(
            outgoing.clone(),
            outgoing
                .thread_subscription_targets_for_thread(thread_id)
                .await,
            thread_id,
        );

        old_listener
            .send_server_notification(ServerNotification::ThreadClosed(ThreadClosedNotification {
                thread_id: thread_id.to_string(),
            }))
            .await;
        let (old_request_id, _old_request_rx) = old_listener
            .send_request(ServerRequestPayload::CurrentTimeRead(
                CurrentTimeReadParams {
                    thread_id: thread_id.to_string(),
                },
            ))
            .await;
        let old_resolution_targets = outgoing
            .take_thread_request_resolution_targets(&old_request_id)
            .await
            .expect("old request should retain its original targets");
        outgoing
            .send_server_notification_to_thread_subscriptions(
                &old_resolution_targets,
                ServerNotification::ServerRequestResolved(ServerRequestResolvedNotification {
                    thread_id: thread_id.to_string(),
                    request_id: old_request_id,
                }),
            )
            .await;

        new_listener
            .send_server_notification(ServerNotification::ThreadClosed(ThreadClosedNotification {
                thread_id: thread_id.to_string(),
            }))
            .await;
        let (new_request_id, _new_request_rx) = new_listener
            .send_request(ServerRequestPayload::CurrentTimeRead(
                CurrentTimeReadParams {
                    thread_id: thread_id.to_string(),
                },
            ))
            .await;
        let new_resolution_targets = outgoing
            .take_thread_request_resolution_targets(&new_request_id)
            .await
            .expect("new request should retain its original targets");
        outgoing
            .send_server_notification_to_thread_subscriptions(
                &new_resolution_targets,
                ServerNotification::ServerRequestResolved(ServerRequestResolvedNotification {
                    thread_id: thread_id.to_string(),
                    request_id: new_request_id,
                }),
            )
            .await;

        let mut subscription_ids = Vec::new();
        for _ in 0..6 {
            let envelope = rx.recv().await.expect("captured message should be queued");
            let subscription_id = match envelope {
                OutgoingEnvelope::ToConnection {
                    connection_id: received_connection_id,
                    message: OutgoingMessage::ThreadScopedNotification(notification),
                    ..
                } => {
                    assert_eq!(received_connection_id, connection_id);
                    notification.thread_subscription_id
                }
                OutgoingEnvelope::ToConnection {
                    connection_id: received_connection_id,
                    message: OutgoingMessage::ThreadScopedRequest(request),
                    ..
                } => {
                    assert_eq!(received_connection_id, connection_id);
                    request.thread_subscription_id
                }
                envelope => panic!("expected captured thread message, got {envelope:?}"),
            };
            subscription_ids.push(subscription_id);
        }

        assert!(
            subscription_ids[..3]
                .iter()
                .all(|subscription_id| subscription_id == &old_subscription_id)
        );
        assert!(
            subscription_ids[3..]
                .iter()
                .all(|subscription_id| subscription_id == &new_subscription_id)
        );
    }

    #[tokio::test]
    async fn captured_terminal_recipients_survive_teardown_without_recreation() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(2);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        let thread_id = ThreadId::new();
        let first_connection = ConnectionId(1);
        let second_connection = ConnectionId(2);
        let first_subscription_id = outgoing
            .register_thread_subscription(first_connection, thread_id)
            .await;
        let second_subscription_id = outgoing
            .register_thread_subscription(second_connection, thread_id)
            .await;
        let terminal_recipients = outgoing
            .thread_subscription_targets_for_thread(thread_id)
            .await;

        outgoing
            .unregister_thread_subscriptions_for_thread(thread_id)
            .await;
        outgoing
            .send_server_notification_to_thread_subscriptions(
                &terminal_recipients,
                ServerNotification::ThreadArchived(ThreadArchivedNotification {
                    thread_id: thread_id.to_string(),
                }),
            )
            .await;

        let mut delivered = HashMap::new();
        for _ in 0..2 {
            let envelope = rx.recv().await.expect("terminal event should be queued");
            let OutgoingEnvelope::ToConnection {
                connection_id,
                message: OutgoingMessage::ThreadScopedNotification(notification),
                ..
            } = envelope
            else {
                panic!("expected captured terminal thread notification");
            };
            delivered.insert(connection_id, notification.thread_subscription_id);
        }
        assert_eq!(
            delivered.get(&first_connection),
            Some(&first_subscription_id)
        );
        assert_eq!(
            delivered.get(&second_connection),
            Some(&second_subscription_id)
        );
    }

    #[tokio::test]
    async fn send_server_notification_to_connection_and_wait_tracks_write_completion() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        let send_task = tokio::spawn(async move {
            outgoing
                .send_server_notification_to_connection_and_wait(
                    ConnectionId(42),
                    ServerNotification::ModelRerouted(ModelReroutedNotification {
                        thread_id: "thread-1".to_string(),
                        turn_id: "turn-1".to_string(),
                        from_model: "gpt-5.3-codex".to_string(),
                        to_model: "gpt-5.2".to_string(),
                        reason: ModelRerouteReason::HighRiskCyberActivity,
                    }),
                )
                .await
        });

        let envelope = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("should receive envelope before timeout")
            .expect("channel should contain one message");
        let OutgoingEnvelope::ToConnection {
            connection_id,
            message,
            write_complete_tx,
        } = envelope
        else {
            panic!("expected targeted server notification envelope");
        };
        assert_eq!(connection_id, ConnectionId(42));
        let envelope = match message {
            OutgoingMessage::AppServerNotification(envelope) => envelope,
            OutgoingMessage::ThreadScopedNotification(notification) => notification.envelope,
            _ => panic!("expected app-server notification"),
        };
        assert!(
            envelope
                .emitted_at_ms
                .is_some_and(|emitted_at_ms| emitted_at_ms > 0)
        );
        write_complete_tx
            .expect("write completion sender should be attached")
            .send(())
            .expect("receiver should still be waiting");

        timeout(Duration::from_secs(1), send_task)
            .await
            .expect("send task should finish after write completion is signaled")
            .expect("send task should not panic");
    }

    #[tokio::test]
    async fn connection_closed_clears_registered_request_contexts() {
        let (tx, _rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        let closed_connection_request = ConnectionRequestId {
            connection_id: ConnectionId(9),
            request_id: RequestId::Integer(3),
        };
        let open_connection_request = ConnectionRequestId {
            connection_id: ConnectionId(10),
            request_id: RequestId::Integer(4),
        };

        outgoing
            .register_request_context(RequestContext::new(
                closed_connection_request,
                tracing::info_span!("app_server.request", rpc.method = "turn/interrupt"),
                /*parent_trace*/ None,
            ))
            .await;
        outgoing
            .register_request_context(RequestContext::new(
                open_connection_request,
                tracing::info_span!("app_server.request", rpc.method = "turn/start"),
                /*parent_trace*/ None,
            ))
            .await;
        assert_eq!(outgoing.request_context_count().await, 2);

        outgoing.connection_closed(ConnectionId(9)).await;

        assert_eq!(outgoing.request_context_count().await, 1);
    }

    #[tokio::test]
    async fn notify_client_error_forwards_error_to_waiter() {
        let (tx, _rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());

        let (request_id, wait_for_result) = outgoing
            .send_request(ServerRequestPayload::ApplyPatchApproval(
                ApplyPatchApprovalParams {
                    conversation_id: ThreadId::new(),
                    call_id: "call-id".to_string(),
                    file_changes: HashMap::new(),
                    reason: None,
                    grant_root: None,
                },
            ))
            .await;

        let error = internal_error("refresh failed");

        outgoing
            .notify_client_error(request_id, error.clone())
            .await;

        let result = timeout(Duration::from_secs(1), wait_for_result)
            .await
            .expect("wait should not time out")
            .expect("waiter should receive a callback");
        assert_eq!(result, Err(error));
    }

    #[tokio::test]
    async fn pending_requests_for_thread_returns_thread_requests_in_request_id_order() {
        let (tx, _rx) = mpsc::channel::<OutgoingEnvelope>(8);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let thread_id = ThreadId::new();
        let thread_outgoing = ThreadScopedOutgoingMessageSender::new(
            outgoing.clone(),
            vec![ConnectionId(1)],
            thread_id,
        );

        let (dynamic_tool_request_id, _dynamic_tool_waiter) = thread_outgoing
            .send_request(ServerRequestPayload::DynamicToolCall(
                DynamicToolCallParams {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-1".to_string(),
                    call_id: "call-0".to_string(),
                    namespace: None,
                    tool: "tool".to_string(),
                    arguments: json!({}),
                },
            ))
            .await;
        let (first_request_id, _first_waiter) = thread_outgoing
            .send_request(ServerRequestPayload::ToolRequestUserInput(
                ToolRequestUserInputParams {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-1".to_string(),
                    item_id: "call-1".to_string(),
                    questions: vec![],
                    auto_resolution_ms: None,
                },
            ))
            .await;
        let (second_request_id, _second_waiter) = thread_outgoing
            .send_request(ServerRequestPayload::FileChangeRequestApproval(
                FileChangeRequestApprovalParams {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-1".to_string(),
                    item_id: "call-2".to_string(),
                    started_at_ms: 0,
                    reason: None,
                    grant_root: None,
                },
            ))
            .await;
        let pending_requests = outgoing.pending_requests_for_thread(thread_id).await;
        assert_eq!(
            pending_requests
                .iter()
                .map(ServerRequest::id)
                .collect::<Vec<_>>(),
            vec![
                &dynamic_tool_request_id,
                &first_request_id,
                &second_request_id
            ]
        );
    }

    #[tokio::test]
    async fn cancel_requests_for_thread_cancels_all_thread_requests() {
        let (tx, _rx) = mpsc::channel::<OutgoingEnvelope>(8);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let thread_id = ThreadId::new();
        let thread_outgoing = ThreadScopedOutgoingMessageSender::new(
            outgoing.clone(),
            vec![ConnectionId(1)],
            thread_id,
        );

        let (_dynamic_tool_request_id, dynamic_tool_waiter) = thread_outgoing
            .send_request(ServerRequestPayload::DynamicToolCall(
                DynamicToolCallParams {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-1".to_string(),
                    call_id: "call-0".to_string(),
                    namespace: None,
                    tool: "tool".to_string(),
                    arguments: json!({}),
                },
            ))
            .await;
        let (_request_id, user_input_waiter) = thread_outgoing
            .send_request(ServerRequestPayload::ToolRequestUserInput(
                ToolRequestUserInputParams {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-1".to_string(),
                    item_id: "call-1".to_string(),
                    questions: vec![],
                    auto_resolution_ms: None,
                },
            ))
            .await;
        let error = internal_error("tracked request cancelled");

        outgoing
            .cancel_requests_for_thread(thread_id, Some(error.clone()))
            .await;

        let dynamic_tool_result = timeout(Duration::from_secs(1), dynamic_tool_waiter)
            .await
            .expect("dynamic tool waiter should resolve")
            .expect("dynamic tool waiter should receive a callback");
        let user_input_result = timeout(Duration::from_secs(1), user_input_waiter)
            .await
            .expect("user input waiter should resolve")
            .expect("user input waiter should receive a callback");
        assert_eq!(dynamic_tool_result, Err(error.clone()));
        assert_eq!(user_input_result, Err(error));
        assert!(
            outgoing
                .pending_requests_for_thread(thread_id)
                .await
                .is_empty()
        );
    }
}
