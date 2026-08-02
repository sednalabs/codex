use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
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
use tokio::sync::watch;
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
    /// Thread-scoped request envelopes can wait behind a warning barrier after their callback is
    /// canceled. Keep an await-free ownership mirror so the barrier can drop them before they
    /// reach the client and wake capacity-blocked producers immediately on cancellation.
    active_thread_request_ids: StdMutex<HashSet<RequestId>>,
    thread_request_liveness_changed: watch::Sender<()>,
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
    thread_subscription_ids: StdMutex<HashMap<(ConnectionId, ThreadId), String>>,
    /// A targetless extension warning reserves a short, ordered barrier for a thread. Every
    /// captured thread-scoped notification and server request passes through this gate, so core
    /// listener events cannot overtake the warning while the listener itself keeps consuming
    /// input. The queue is bounded; pressure waits for release rather than silently dropping a
    /// meaningful later message.
    thread_outbound_barriers: StdMutex<HashMap<ThreadId, ThreadOutboundBarrier>>,
    analytics_events_client: AnalyticsEventsClient,
}

pub(crate) const MAX_DEFERRED_THREAD_OUTBOUND_MESSAGES: usize = 256;
pub(crate) const NO_LISTENER_THREAD_OUTBOUND_BARRIER_GENERATION: u64 = 0;

enum ThreadOutboundBarrierPhase {
    WaitingForWarning,
    Releasing,
}

struct ThreadOutboundBarrier {
    listener_generation: u64,
    phase: ThreadOutboundBarrierPhase,
    warning_delivery_invalidated: bool,
    deferred: VecDeque<OutgoingEnvelope>,
    capacity_changed: watch::Sender<()>,
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

    /// Drops retained resolution recipients when the listener can no longer serialize the
    /// corresponding `ServerRequestResolved` notification.
    pub(crate) async fn discard_thread_request_resolution_targets(&self, request_id: &RequestId) {
        self.outgoing
            .discard_thread_request_resolution_targets(request_id)
            .await;
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
        let (thread_request_liveness_changed, _thread_request_liveness_rx) = watch::channel(());
        Self {
            next_server_request_id: AtomicI64::new(0),
            sender,
            request_id_to_callback: Mutex::new(HashMap::new()),
            active_thread_request_ids: StdMutex::new(HashSet::new()),
            thread_request_liveness_changed,
            thread_request_resolution_targets: Mutex::new(HashMap::new()),
            request_contexts: Mutex::new(HashMap::new()),
            thread_subscription_ids: StdMutex::new(HashMap::new()),
            thread_outbound_barriers: StdMutex::new(HashMap::new()),
            analytics_events_client,
        }
    }

    /// Starts the central outbound ordering barrier for one listener generation. Extension
    /// ingress calls this synchronously after claiming the matching warning lease, before a
    /// later listener event can reach the transport. A second targetless warning is coalesced
    /// before it can create another barrier or enter a listener queue.
    pub(crate) fn begin_thread_outbound_barrier(
        &self,
        thread_id: ThreadId,
        listener_generation: u64,
    ) -> bool {
        let mut barriers = self
            .thread_outbound_barriers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if barriers.contains_key(&thread_id) {
            tracing::debug!(
                %thread_id,
                listener_generation,
                "coalescing targetless warning while an outbound barrier is already active"
            );
            return false;
        }
        let (capacity_changed, _capacity_changed_rx) = watch::channel(());
        barriers.insert(
            thread_id,
            ThreadOutboundBarrier {
                listener_generation,
                phase: ThreadOutboundBarrierPhase::WaitingForWarning,
                warning_delivery_invalidated: false,
                deferred: VecDeque::new(),
                capacity_changed,
            },
        );
        true
    }

    /// Publishes a resolved targetless warning ahead of every thread message captured while its
    /// barrier was active, or releases those messages after a timeout/drop without a warning.
    /// This method never waits for a subscriber; the detached warning control plane already
    /// performed that bounded work before releasing this transport gate.
    pub(crate) async fn release_thread_outbound_barrier(
        &self,
        thread_id: ThreadId,
        listener_generation: u64,
        warning: Option<(Vec<ThreadSubscriptionTarget>, ServerNotification)>,
    ) {
        if !self
            .begin_thread_outbound_barrier_release(thread_id, |generation| {
                generation == listener_generation
            })
        {
            return;
        }

        if let Some((thread_subscriptions, notification)) = warning
        {
            self.send_current_thread_outbound_warning(
                thread_id,
                listener_generation,
                &thread_subscriptions,
                notification,
            )
            .await;
        }
        self.drain_thread_outbound_barrier(thread_id, listener_generation)
            .await;
    }

    /// A listener replacement or failure drops the pending warning but must promptly release
    /// later client-visible output. Generation matching prevents an old task from releasing a
    /// replacement listener's newly established barrier.
    pub(crate) async fn release_thread_outbound_barrier_for_listener_generation(
        &self,
        thread_id: ThreadId,
        listener_generation: u64,
    ) {
        if self
            .begin_thread_outbound_barrier_release(thread_id, |generation| {
                generation == listener_generation
            })
        {
            self.drain_thread_outbound_barrier(thread_id, listener_generation)
                .await;
        }
    }

    /// Teardown invalidates any active warning (including the no-listener generation) and drains
    /// its retained output immediately. This is intentionally generation-agnostic: after a
    /// thread is removed, reloaded, or globally cleared, no warning lease may hold client-visible
    /// traffic until its normal bounded timeout.
    pub(crate) async fn release_thread_outbound_barrier_for_teardown(&self, thread_id: ThreadId) {
        let released_generation = {
            let mut barriers = self
                .thread_outbound_barriers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(barrier) = barriers.get_mut(&thread_id) else {
                return;
            };
            barrier.warning_delivery_invalidated = true;
            if matches!(barrier.phase, ThreadOutboundBarrierPhase::Releasing) {
                return;
            }
            barrier.phase = ThreadOutboundBarrierPhase::Releasing;
            barrier.listener_generation
        };
        self.drain_thread_outbound_barrier(thread_id, released_generation)
            .await;
    }

    /// A new listener invalidates older generation leases. Release their held traffic before
    /// registering the replacement so an abandoned warning cannot strand output until timeout.
    pub(crate) async fn release_thread_outbound_barriers_before_generation(
        &self,
        thread_id: ThreadId,
        next_listener_generation: u64,
    ) {
        let released_generation = {
            let mut barriers = self
                .thread_outbound_barriers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(barrier) = barriers.get_mut(&thread_id) else {
                return;
            };
            if barrier.listener_generation == NO_LISTENER_THREAD_OUTBOUND_BARRIER_GENERATION
                || barrier.listener_generation >= next_listener_generation
            {
                return;
            }
            barrier.warning_delivery_invalidated = true;
            if matches!(barrier.phase, ThreadOutboundBarrierPhase::Releasing) {
                return;
            }
            barrier.phase = ThreadOutboundBarrierPhase::Releasing;
            Some(barrier.listener_generation)
        };
        if let Some(released_generation) = released_generation {
            self.drain_thread_outbound_barrier(thread_id, released_generation)
                .await;
        }
    }

    fn begin_thread_outbound_barrier_release(
        &self,
        thread_id: ThreadId,
        generation_matches: impl FnOnce(u64) -> bool,
    ) -> bool {
        let mut barriers = self
            .thread_outbound_barriers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(barrier) = barriers.get_mut(&thread_id) else {
            return false;
        };
        if !generation_matches(barrier.listener_generation)
            || matches!(barrier.phase, ThreadOutboundBarrierPhase::Releasing)
        {
            return false;
        }
        barrier.phase = ThreadOutboundBarrierPhase::Releasing;
        true
    }

    async fn drain_thread_outbound_barrier(&self, thread_id: ThreadId, listener_generation: u64) {
        loop {
            let deferred = {
                let mut barriers = self
                    .thread_outbound_barriers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let Some(barrier) = barriers.get_mut(&thread_id) else {
                    return;
                };
                if barrier.listener_generation != listener_generation
                    || !matches!(barrier.phase, ThreadOutboundBarrierPhase::Releasing)
                {
                    return;
                }
                let deferred = barrier.deferred.pop_front();
                barrier.capacity_changed.send_replace(());
                if deferred.is_none() {
                    barrier.capacity_changed.send_replace(());
                    barriers.remove(&thread_id);
                }
                deferred
            };
            let Some(deferred) = deferred else {
                return;
            };
            if !self.thread_outbound_request_is_active(&deferred) {
                continue;
            }
            if let Err(error) = self.sender.send(deferred).await {
                tracing::warn!(
                    %thread_id,
                    listener_generation,
                    "failed to release deferred thread output to client: {error:?}"
                );
            }
        }
    }

    async fn send_or_defer_thread_outbound(
        &self,
        thread_id: ThreadId,
        outgoing: OutgoingEnvelope,
    ) -> std::result::Result<(), mpsc::error::SendError<OutgoingEnvelope>> {
        let mut outgoing = Some(outgoing);
        loop {
            let (capacity_wait, request_liveness_wait) = {
                let mut barriers = self
                    .thread_outbound_barriers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let request_is_active = outgoing
                    .as_ref()
                    .map_or(true, |outgoing| self.thread_outbound_request_is_active(outgoing));
                if !request_is_active {
                    return Ok(());
                }
                if let Some(barrier) = barriers.get_mut(&thread_id) {
                    if barrier.deferred.len() < MAX_DEFERRED_THREAD_OUTBOUND_MESSAGES {
                        barrier.deferred.push_back(
                            outgoing
                                .take()
                                .expect("outbound envelope should be retained"),
                        );
                        return Ok(());
                    }
                    (
                        Some(barrier.capacity_changed.subscribe()),
                        outgoing.as_ref().and_then(|outgoing| {
                            self.thread_outbound_request_id(outgoing)
                                .map(|_| self.thread_request_liveness_changed.subscribe())
                        }),
                    )
                } else {
                    (None, None)
                }
            };
            // The barrier queue is bounded to prevent warning/flood pressure from consuming
            // unbounded memory. Only after that explicit limit does a producer wait for the
            // already-bounded barrier to release; it never starts another warning timeout.
            if let Some(mut capacity_wait) = capacity_wait {
                if let Some(mut request_liveness_wait) = request_liveness_wait {
                    tokio::select! {
                        _ = capacity_wait.changed() => {}
                        _ = request_liveness_wait.changed() => {}
                    }
                } else {
                    let _ = capacity_wait.changed().await;
                }
            } else {
                break;
            }
        }
        if !outgoing
            .as_ref()
            .map_or(true, |outgoing| self.thread_outbound_request_is_active(outgoing))
        {
            return Ok(());
        }
        self.sender
            .send(outgoing.expect("outbound envelope should be retained"))
            .await
    }

    fn thread_outbound_request_id(&self, outgoing: &OutgoingEnvelope) -> Option<&RequestId> {
        let OutgoingEnvelope::ToConnection {
            message: OutgoingMessage::ThreadScopedRequest(request),
            ..
        } = outgoing
        else {
            return None;
        };
        Some(request.request.id())
    }

    fn thread_outbound_request_is_active(&self, outgoing: &OutgoingEnvelope) -> bool {
        self.thread_outbound_request_id(outgoing).map_or(true, |request_id| {
            self.active_thread_request_ids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(request_id)
        })
    }

    fn register_active_thread_request(&self, request_id: RequestId) {
        if self
            .active_thread_request_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(request_id)
        {
            self.thread_request_liveness_changed.send_replace(());
        }
    }

    fn remove_active_thread_request(&self, request_id: &RequestId) {
        if self
            .active_thread_request_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(request_id)
        {
            self.thread_request_liveness_changed.send_replace(());
        }
    }

    fn clear_active_thread_requests(&self) {
        let removed_active_requests = {
            let mut active_request_ids = self
                .active_thread_request_ids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if active_request_ids.is_empty() {
                false
            } else {
                active_request_ids.clear();
                true
            }
        };
        if removed_active_requests {
            self.thread_request_liveness_changed.send_replace(());
        }
    }

    /// Enqueues a targetless warning only while the matching barrier generation remains current.
    /// Reserving capacity happens before the barrier lock; once a permit is available, validation
    /// and `permit.send` are one non-awaiting critical section. A replacement that wins first
    /// therefore prevents this old warning from entering the transport after its lifecycle ended.
    async fn send_current_thread_outbound_warning(
        &self,
        thread_id: ThreadId,
        listener_generation: u64,
        thread_subscriptions: &[ThreadSubscriptionTarget],
        notification: ServerNotification,
    ) {
        let envelope = timestamped_server_notification_envelope(notification);
        for thread_subscription in thread_subscriptions {
            let permit = match self.sender.reserve().await {
                Ok(permit) => permit,
                Err(error) => {
                    tracing::warn!(
                        "failed to reserve transport for released targetless warning: {error:?}"
                    );
                    return;
                }
            };
            let warning_enqueued = {
                let barriers = self
                    .thread_outbound_barriers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let warning_is_current = barriers.get(&thread_id).is_some_and(|barrier| {
                    barrier.listener_generation == listener_generation
                        && matches!(barrier.phase, ThreadOutboundBarrierPhase::Releasing)
                        && !barrier.warning_delivery_invalidated
                });
                if warning_is_current {
                    permit.send(OutgoingEnvelope::ToConnection {
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
                    });
                }
                warning_is_current
            };
            if !warning_enqueued {
                return;
            }
        }
    }

    #[cfg(test)]
    pub(crate) async fn deferred_thread_outbound_count(&self, thread_id: ThreadId) -> usize {
        self.thread_outbound_barriers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&thread_id)
            .map(|barrier| barrier.deferred.len())
            .unwrap_or_default()
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
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
        let mut subscriptions = self
            .thread_subscription_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(connection_id, thread_id));
    }

    /// Removes a connection-local thread identity only when it is still the identity owned by
    /// the caller. Overlapping resume/start/fork attempts can replace this map entry before an
    /// older attempt discovers that its attach or hydration failed.
    pub(crate) async fn unregister_thread_subscription_if_matches(
        &self,
        connection_id: ConnectionId,
        thread_id: ThreadId,
        expected_subscription_id: &str,
    ) -> bool {
        let mut subscriptions = self
            .thread_subscription_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if subscriptions
            .get(&(connection_id, thread_id))
            .is_some_and(|subscription_id| subscription_id == expected_subscription_id)
        {
            subscriptions.remove(&(connection_id, thread_id));
            true
        } else {
            false
        }
    }

    /// Restores an already-active state-manager subscription only if no newer outgoing owner
    /// replaced the failed provisional attachment while its rollback was in flight.
    pub(crate) async fn restore_thread_subscription_if_unclaimed(
        &self,
        connection_id: ConnectionId,
        thread_id: ThreadId,
        subscription_id: String,
    ) -> bool {
        use std::collections::hash_map::Entry;

        let mut subscriptions = self
            .thread_subscription_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match subscriptions.entry((connection_id, thread_id)) {
            Entry::Vacant(entry) => {
                entry.insert(subscription_id);
                true
            }
            Entry::Occupied(entry) => entry.get() == &subscription_id,
        }
    }

    /// Returns whether an explicit attach attempt still owns this connection-local identity.
    pub(crate) async fn thread_subscription_matches(
        &self,
        connection_id: ConnectionId,
        thread_id: ThreadId,
        expected_subscription_id: &str,
    ) -> bool {
        self.thread_subscription_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(connection_id, thread_id))
            .is_some_and(|subscription_id| subscription_id == expected_subscription_id)
    }

    /// Captures the currently-owned subscription identity without creating one. Callers that
    /// subsequently unsubscribe must use this immutable value so an overlapping resume cannot
    /// have its newer token removed by stale cleanup.
    pub(crate) async fn current_thread_subscription_id(
        &self,
        connection_id: ConnectionId,
        thread_id: ThreadId,
    ) -> Option<String> {
        self.thread_subscription_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(connection_id, thread_id))
            .cloned()
    }

    pub(crate) async fn unregister_thread_subscriptions_for_thread(&self, thread_id: ThreadId) {
        self.thread_subscription_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
                id.clone(),
                PendingCallbackEntry {
                    callback: tx_approve,
                    thread_id: thread_id.clone(),
                    request: request.clone(),
                },
            );
        }
        if thread_id.is_some() {
            self.register_active_thread_request(outgoing_message_id.clone());
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
                    let outgoing = OutgoingEnvelope::ToConnection {
                        connection_id: *connection_id,
                        message,
                        write_complete_tx: None,
                    };
                    let send_result = match thread_id {
                        Some(thread_id) => {
                            self.send_or_defer_thread_outbound(thread_id, outgoing).await
                        }
                        None => self.sender.send(outgoing).await,
                    };
                    if let Err(err) = send_result {
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
            {
                let mut request_id_to_callback = self.request_id_to_callback.lock().await;
                request_id_to_callback.remove(&outgoing_message_id);
            }
            self.remove_active_thread_request(&outgoing_message_id);
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
                id.clone(),
                PendingCallbackEntry {
                    callback: tx_approve,
                    thread_id: Some(thread_id),
                    request: request.clone(),
                },
            );
        }
        self.register_active_thread_request(outgoing_message_id.clone());
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
                .send_or_defer_thread_outbound(
                    thread_id,
                    OutgoingEnvelope::ToConnection {
                        connection_id: thread_subscription.connection_id,
                        message,
                        write_complete_tx: None,
                    },
                )
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
            {
                let mut request_id_to_callback = self.request_id_to_callback.lock().await;
                request_id_to_callback.remove(&outgoing_message_id);
            }
            self.remove_active_thread_request(&outgoing_message_id);
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
        // Capture the already-established identity once. A resume/reconnect
        // must never mint a token while replaying a request because the
        // client has not received a corresponding lifecycle handshake for
        // that newly created token.
        let Some(replay_target) = self
            .thread_subscription_target_for_connection(connection_id, thread_id)
            .await
        else {
            tracing::debug!(
                ?connection_id,
                %thread_id,
                "dropping request replay without an active thread subscription"
            );
            return;
        };
        self.replay_requests_to_thread_subscription(&replay_target).await;
    }

    /// Replays requests to an immutable subscription target captured by a successful lifecycle
    /// response. Do not look up the current map entry here: a later resume may already own a
    /// replacement token, which must not receive replay traffic from the older lifecycle.
    pub(crate) async fn replay_requests_to_thread_subscription(
        &self,
        replay_target: &ThreadSubscriptionTarget,
    ) {
        let connection_id = replay_target.connection_id;
        let thread_id = replay_target.thread_id;
        let requests = self.pending_requests_for_thread(thread_id).await;
        for request in requests {
            // Preserve the original recipients (which can now be stale and
            // safely fenced) and add this exact replay target. When the
            // request resolves, every client that actually received it gets
            // the matching resolution without deriving a replacement token.
            let Some(replay_target_added) = self
                .append_thread_request_resolution_target(request.id(), replay_target.clone())
                .await
            else {
                continue;
            };
            let request_id = request.id().clone();
            let message = OutgoingMessage::ThreadScopedRequest(ThreadScopedServerRequest {
                request,
                thread_subscription_id: replay_target.thread_subscription_id.clone(),
            });
            if let Err(err) = self
                .send_or_defer_thread_outbound(
                    thread_id,
                    OutgoingEnvelope::ToConnection {
                        connection_id,
                        message,
                        write_complete_tx: None,
                    },
                )
                .await
            {
                if replay_target_added {
                    self.remove_thread_request_resolution_target(&request_id, &replay_target)
                        .await;
                }
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
        // A client rejection has no listener-side resolution notification. Drop the immutable
        // recipients now rather than retaining a target list until broad thread teardown.
        self.discard_thread_request_resolution_targets(&id).await;

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
        self.clear_active_thread_requests();
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
        let entry = {
            let mut request_id_to_callback = self.request_id_to_callback.lock().await;
            request_id_to_callback.remove_entry(id)
        };
        if entry.is_some() {
            self.remove_active_thread_request(id);
        }
        entry
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

    /// Discards retained resolution recipients for a thread-scoped request
    /// that completes without going through the listener resolution command
    /// (for example, the internal current-time provider).
    pub(crate) async fn discard_thread_request_resolution_targets(&self, id: &RequestId) {
        self.thread_request_resolution_targets
            .lock()
            .await
            .remove(id);
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
        for request_id in &request_ids {
            self.remove_active_thread_request(request_id);
        }
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
        self.thread_subscription_targets_for_thread_now(thread_id)
    }

    /// Captures active thread targets without waiting. This is used by the
    /// synchronous extension event sink so it can preserve listener-command
    /// ordering while still carrying the token present at event ingress.
    pub(crate) fn thread_subscription_targets_for_thread_now(
        &self,
        thread_id: ThreadId,
    ) -> Vec<ThreadSubscriptionTarget> {
        self.thread_subscription_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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

    /// Captures one already-active connection-local thread identity without
    /// creating a replacement. Callers that selected a connection before an
    /// asynchronous send must use this target directly, or safely decline to
    /// send when it has disappeared.
    pub(crate) async fn thread_subscription_target_for_connection(
        &self,
        connection_id: ConnectionId,
        thread_id: ThreadId,
    ) -> Option<ThreadSubscriptionTarget> {
        self.thread_subscription_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(connection_id, thread_id))
            .cloned()
            .map(|thread_subscription_id| {
                ThreadSubscriptionTarget::captured(connection_id, thread_id, thread_subscription_id)
            })
    }

    async fn append_thread_request_resolution_target(
        &self,
        request_id: &RequestId,
        target: ThreadSubscriptionTarget,
    ) -> Option<bool> {
        let mut resolution_targets = self.thread_request_resolution_targets.lock().await;
        let Some(targets) = resolution_targets.get_mut(request_id) else {
            return None;
        };
        if !targets.contains(&target) {
            targets.push(target);
            Some(true)
        } else {
            Some(false)
        }
    }

    async fn remove_thread_request_resolution_target(
        &self,
        request_id: &RequestId,
        target: &ThreadSubscriptionTarget,
    ) {
        let mut resolution_targets = self.thread_request_resolution_targets.lock().await;
        if let Some(targets) = resolution_targets.get_mut(request_id) {
            targets.retain(|candidate| candidate != target);
        }
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
                .send_or_defer_thread_outbound(
                    thread_subscription.thread_id,
                    OutgoingEnvelope::ToConnection {
                        connection_id: thread_subscription.connection_id,
                        message: OutgoingMessage::ThreadScopedNotification(
                            ThreadScopedServerNotification {
                                envelope: envelope.clone(),
                                thread_subscription_id: thread_subscription
                                    .thread_subscription_id
                                    .clone(),
                            },
                        },
                        write_complete_tx: None,
                    },
                )
                .await
            {
                warn!("failed to send captured thread notification to client: {err:?}");
            }
        }
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
        if let Some(thread_id) = server_notification_thread_id(&notification) {
            let thread_subscriptions = if connection_ids.is_empty() {
                self.thread_subscription_targets_for_thread(thread_id).await
            } else {
                let mut thread_subscriptions = Vec::with_capacity(connection_ids.len());
                for connection_id in connection_ids {
                    let thread_subscription_id = self
                        .ensure_thread_subscription(*connection_id, thread_id)
                        .await;
                    thread_subscriptions.push(ThreadSubscriptionTarget::captured(
                        *connection_id,
                        thread_id,
                        thread_subscription_id,
                    ));
                }
                thread_subscriptions
            };
            if thread_subscriptions.is_empty() {
                tracing::debug!(
                    %thread_id,
                    "dropping thread-bound notification without an active subscription"
                );
                return;
            }
            self.send_server_notification_to_thread_subscriptions(
                &thread_subscriptions,
                notification,
            )
            .await;
            return;
        }

        let envelope = timestamped_server_notification_envelope(notification);
        if connection_ids.is_empty() {
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
            if let Err(err) = self
                .sender
                .send(OutgoingEnvelope::ToConnection {
                    connection_id: *connection_id,
                    message: OutgoingMessage::AppServerNotification(envelope.clone()),
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
        if let Some(thread_id) = server_notification_thread_id(&notification) {
            let thread_subscription_id = self
                .ensure_thread_subscription(connection_id, thread_id)
                .await;
            let thread_subscription = ThreadSubscriptionTarget::captured(
                connection_id,
                thread_id,
                thread_subscription_id,
            );
            self.send_server_notification_to_thread_subscriptions(
                &[thread_subscription],
                notification,
            )
            .await;
            return;
        }
        let envelope = timestamped_server_notification_envelope(notification.clone());
        if let Err(err) = self
            .sender
            .send(OutgoingEnvelope::ToConnection {
                connection_id,
                message: OutgoingMessage::AppServerNotification(envelope),
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
        let (write_complete_tx, write_complete_rx) = oneshot::channel();
        let send_result = if let Some(thread_id) = server_notification_thread_id(&notification) {
            let thread_subscription_id = self
                .ensure_thread_subscription(connection_id, thread_id)
                .await;
            self.send_or_defer_thread_outbound(
                thread_id,
                OutgoingEnvelope::ToConnection {
                    connection_id,
                    message: OutgoingMessage::ThreadScopedNotification(
                        ThreadScopedServerNotification {
                            envelope,
                            thread_subscription_id,
                        },
                    ),
                    write_complete_tx: Some(write_complete_tx),
                },
            )
            .await
        } else {
            self.sender
                .send(OutgoingEnvelope::ToConnection {
                    connection_id,
                    message: OutgoingMessage::AppServerNotification(envelope),
                    write_complete_tx: Some(write_complete_tx),
                })
                .await
        };
        if let Err(err) = send_result {
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
        ServerNotification::EnvironmentConnected(notification)
        | ServerNotification::EnvironmentDisconnected(notification) => {
            Some(notification.thread_id.as_str())
        }
        ServerNotification::SkillsChanged(_)
        | ServerNotification::McpServerOauthLoginCompleted(_)
        | ServerNotification::AccountUpdated(_)
        | ServerNotification::AccountRateLimitsUpdated(_)
        | ServerNotification::AppListUpdated(_)
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
    use codex_app_server_protocol::EnvironmentConnectionNotification;
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
    use codex_app_server_protocol::ThreadGoal;
    use codex_app_server_protocol::ThreadGoalClearedNotification;
    use codex_app_server_protocol::ThreadGoalStatus;
    use codex_app_server_protocol::ThreadGoalUpdatedNotification;
    use codex_app_server_protocol::ToolRequestUserInputParams;
    use codex_app_server_protocol::TurnModerationMetadataNotification;
    use codex_app_server_protocol::WarningNotification;
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
    async fn environment_connection_notifications_are_scoped_to_their_thread_subscription() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing = OutgoingMessageSender::new(tx, AnalyticsEventsClient::disabled());
        let connected_thread_id = ThreadId::new();
        let unrelated_thread_id = ThreadId::new();
        let connected_connection_id = ConnectionId(1);
        let unrelated_connection_id = ConnectionId(2);
        let connected_subscription_id = outgoing
            .register_thread_subscription(connected_connection_id, connected_thread_id)
            .await;
        outgoing
            .register_thread_subscription(unrelated_connection_id, unrelated_thread_id)
            .await;

        outgoing
            .send_server_notification(ServerNotification::EnvironmentConnected(
                EnvironmentConnectionNotification {
                    thread_id: connected_thread_id.to_string(),
                    environment_id: "environment-a".to_string(),
                },
            ))
            .await;
        let OutgoingEnvelope::ToConnection {
            connection_id,
            message: OutgoingMessage::ThreadScopedNotification(notification),
            ..
        } = rx.recv().await.expect("connected environment event should be delivered")
        else {
            panic!("environment connection event must be thread scoped");
        };
        assert_eq!(connection_id, connected_connection_id);
        assert_eq!(notification.thread_subscription_id, connected_subscription_id);
        assert!(matches!(
            notification.envelope.notification,
            ServerNotification::EnvironmentConnected(_)
        ));
        assert!(
            timeout(Duration::from_millis(50), rx.recv()).await.is_err(),
            "an environment connection event must not reach an unrelated thread subscription"
        );

        outgoing
            .send_server_notification(ServerNotification::EnvironmentDisconnected(
                EnvironmentConnectionNotification {
                    thread_id: connected_thread_id.to_string(),
                    environment_id: "environment-a".to_string(),
                },
            ))
            .await;
        let OutgoingEnvelope::ToConnection {
            connection_id,
            message: OutgoingMessage::ThreadScopedNotification(notification),
            ..
        } = rx
            .recv()
            .await
            .expect("disconnected environment event should be delivered")
        else {
            panic!("environment disconnection event must be thread scoped");
        };
        assert_eq!(connection_id, connected_connection_id);
        assert_eq!(notification.thread_subscription_id, connected_subscription_id);
        assert!(matches!(
            notification.envelope.notification,
            ServerNotification::EnvironmentDisconnected(_)
        ));
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

        let old_target = outgoing
            .thread_subscription_target_for_connection(connection_id, thread_id)
            .await
            .expect("current-time request should capture the original subscription");
        let (_request_id, _response_rx) = outgoing
            .send_request_to_thread_subscriptions(
                &[old_target],
                ServerRequestPayload::CurrentTimeRead(CurrentTimeReadParams {
                    thread_id: thread_id.to_string(),
                }),
                thread_id,
            )
            .await;
        let stale = rx
            .recv()
            .await
            .expect("current-time request should be queued");

        let new_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        let new_target = outgoing
            .thread_subscription_target_for_connection(connection_id, thread_id)
            .await
            .expect("replacement current-time request should capture its subscription");
        let (_request_id, _response_rx) = outgoing
            .send_request_to_thread_subscriptions(
                &[new_target],
                ServerRequestPayload::CurrentTimeRead(CurrentTimeReadParams {
                    thread_id: thread_id.to_string(),
                }),
                thread_id,
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
    async fn delayed_goal_update_keeps_its_captured_token_after_reattach() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(2);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let connection_id = ConnectionId(9);
        let thread_id = ThreadId::new();
        let old_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        let delayed_listener = ThreadScopedOutgoingMessageSender::from_captured_thread_subscriptions(
            outgoing.clone(),
            outgoing
                .thread_subscription_targets_for_thread(thread_id)
                .await,
            thread_id,
        );

        let new_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        let current_listener = ThreadScopedOutgoingMessageSender::from_captured_thread_subscriptions(
            outgoing.clone(),
            outgoing
                .thread_subscription_targets_for_thread(thread_id)
                .await,
            thread_id,
        );

        let goal_update = |objective: &str| {
            ServerNotification::ThreadGoalUpdated(ThreadGoalUpdatedNotification {
                thread_id: thread_id.to_string(),
                turn_id: None,
                goal: ThreadGoal {
                    thread_id: thread_id.to_string(),
                    objective: objective.to_string(),
                    status: ThreadGoalStatus::Active,
                    token_budget: None,
                    tokens_used: 0,
                    time_used_seconds: 0,
                    created_at: 0,
                    updated_at: 0,
                },
            })
        };

        // The old listener emits only after the replacement has attached.
        // The captured target must fence it to the old subscription token.
        delayed_listener.send_server_notification(goal_update("old")).await;
        current_listener.send_server_notification(goal_update("new")).await;

        let subscription_and_objective = |envelope: OutgoingEnvelope| match envelope {
            OutgoingEnvelope::ToConnection {
                connection_id: received_connection_id,
                message: OutgoingMessage::ThreadScopedNotification(thread_notification),
                ..
            } => {
                assert_eq!(received_connection_id, connection_id);
                let subscription_id = thread_notification.thread_subscription_id;
                let ServerNotification::ThreadGoalUpdated(goal_notification) = thread_notification.envelope.notification
                else {
                    panic!("expected tagged goal update");
                };
                (subscription_id, goal_notification.goal.objective)
            }
            envelope => panic!("expected tagged goal update, got {envelope:?}"),
        };

        assert_eq!(
            subscription_and_objective(
                rx.recv()
                    .await
                    .expect("delayed goal update should be queued"),
            ),
            (old_subscription_id, "old".to_string())
        );
        assert_eq!(
            subscription_and_objective(
                rx.recv()
                    .await
                    .expect("replacement goal update should be queued"),
            ),
            (new_subscription_id, "new".to_string())
        );
    }

    #[tokio::test]
    async fn replayed_request_resolves_to_old_and_replayed_subscription_targets() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let connection_id = ConnectionId(9);
        let thread_id = ThreadId::new();
        let old_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        let old_target = outgoing
            .thread_subscription_target_for_connection(connection_id, thread_id)
            .await
            .expect("initial subscription should be captured");
        let (request_id, _request_rx) = outgoing
            .send_request_to_thread_subscriptions(
                &[old_target],
                ServerRequestPayload::ToolRequestUserInput(ToolRequestUserInputParams {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-1".to_string(),
                    item_id: "approval-1".to_string(),
                    questions: Vec::new(),
                    auto_resolution_ms: None,
                }),
                thread_id,
            )
            .await;

        outgoing
            .unregister_thread_subscription(connection_id, thread_id)
            .await;
        let replay_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        outgoing
            .replay_requests_to_connection_for_thread(connection_id, thread_id)
            .await;

        outgoing
            .notify_client_response(request_id.clone(), serde_json::json!({}))
            .await;
        assert!(
            outgoing
                .pending_requests_for_thread(thread_id)
                .await
                .is_empty(),
            "the response must clear the replayed pending request"
        );

        let resolution_targets = outgoing
            .take_thread_request_resolution_targets(&request_id)
            .await
            .expect("response resolution should retain every delivered target");
        assert_eq!(resolution_targets.len(), 2);
        assert!(
            resolution_targets
                .iter()
                .any(|target| target.thread_subscription_id == old_subscription_id)
        );
        assert!(
            resolution_targets
                .iter()
                .any(|target| target.thread_subscription_id == replay_subscription_id)
        );
        outgoing
            .send_server_notification_to_thread_subscriptions(
                &resolution_targets,
                ServerNotification::ServerRequestResolved(ServerRequestResolvedNotification {
                    thread_id: thread_id.to_string(),
                    request_id: request_id.clone(),
                }),
            )
            .await;

        let mut request_tokens = Vec::new();
        let mut resolution_tokens = Vec::new();
        for _ in 0..4 {
            let envelope = rx.recv().await.expect("replay traffic should be queued");
            match envelope {
                OutgoingEnvelope::ToConnection {
                    message: OutgoingMessage::ThreadScopedRequest(request),
                    ..
                } => request_tokens.push(request.thread_subscription_id),
                OutgoingEnvelope::ToConnection {
                    message: OutgoingMessage::ThreadScopedNotification(notification),
                    ..
                } => {
                    let ServerNotification::ServerRequestResolved(resolved) =
                        notification.envelope.notification
                    else {
                        panic!("expected replay resolution notification");
                    };
                    assert_eq!(resolved.request_id, request_id);
                    resolution_tokens.push(notification.thread_subscription_id);
                }
                envelope => panic!("expected captured replay traffic, got {envelope:?}"),
            }
        }

        assert_eq!(
            request_tokens,
            vec![old_subscription_id.clone(), replay_subscription_id.clone()]
        );
        assert_eq!(
            resolution_tokens,
            vec![old_subscription_id, replay_subscription_id]
        );
    }

    #[tokio::test]
    async fn captured_current_time_request_does_not_recreate_after_reattach() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(1);
        let outgoing =
            OutgoingMessageSender::new(tx, codex_analytics::AnalyticsEventsClient::disabled());
        let connection_id = ConnectionId(9);
        let thread_id = ThreadId::new();
        let old_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        let target = outgoing
            .thread_subscription_target_for_connection(connection_id, thread_id)
            .await
            .expect("current-time snapshot should capture its existing identity");

        let replacement_subscription_id = outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        let (request_id, _request_rx) = outgoing
            .send_request_to_thread_subscriptions(
                &[target],
                ServerRequestPayload::CurrentTimeRead(CurrentTimeReadParams {
                    thread_id: thread_id.to_string(),
                }),
                thread_id,
            )
            .await;

        let OutgoingEnvelope::ToConnection {
            message: OutgoingMessage::ThreadScopedRequest(request),
            ..
        } = rx
            .recv()
            .await
            .expect("current-time request should be queued")
        else {
            panic!("expected a captured current-time request");
        };
        assert_eq!(request.thread_subscription_id, old_subscription_id);
        let current_target = outgoing
            .thread_subscription_target_for_connection(connection_id, thread_id)
            .await
            .expect("replacement subscription should remain current");
        assert_eq!(
            current_target.thread_subscription_id,
            replacement_subscription_id
        );
        outgoing.cancel_request(&request_id).await;
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
    async fn thread_scoped_request_completion_discards_only_rejected_resolution_targets() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let connection_id = ConnectionId(9);
        let thread_id = ThreadId::new();
        outgoing
            .register_thread_subscription(connection_id, thread_id)
            .await;
        let target = outgoing
            .thread_subscription_target_for_connection(connection_id, thread_id)
            .await
            .expect("thread request should capture the registered target");

        let (rejected_request_id, rejected_waiter) = outgoing
            .send_request_to_thread_subscriptions(
                &[target.clone()],
                ServerRequestPayload::CurrentTimeRead(CurrentTimeReadParams {
                    thread_id: thread_id.to_string(),
                }),
                thread_id,
            )
            .await;
        let _ = rx
            .recv()
            .await
            .expect("bounded queue should contain the rejected request");
        let rejected_error = internal_error("request rejected");
        outgoing
            .notify_client_error(rejected_request_id.clone(), rejected_error.clone())
            .await;
        assert_eq!(
            rejected_waiter
                .await
                .expect("rejected request waiter should complete"),
            Err(rejected_error)
        );
        assert!(
            outgoing
                .take_thread_request_resolution_targets(&rejected_request_id)
                .await
                .is_none(),
            "a rejected request must discard its immutable resolution recipients exactly once"
        );

        let (successful_request_id, successful_waiter) = outgoing
            .send_request_to_thread_subscriptions(
                &[target],
                ServerRequestPayload::CurrentTimeRead(CurrentTimeReadParams {
                    thread_id: thread_id.to_string(),
                }),
                thread_id,
            )
            .await;
        let _ = rx
            .recv()
            .await
            .expect("bounded queue should contain the successful request");
        outgoing
            .notify_client_response(successful_request_id.clone(), serde_json::json!({}))
            .await;
        successful_waiter
            .await
            .expect("successful request waiter should complete")
            .expect("successful request should receive its result");
        assert!(
            outgoing
                .take_thread_request_resolution_targets(&successful_request_id)
                .await
                .is_some(),
            "the listener still needs successful request recipients for its resolution event"
        );
        assert!(
            outgoing
                .take_thread_request_resolution_targets(&successful_request_id)
                .await
                .is_none(),
            "the successful listener resolution consumes its recipients without teardown"
        );
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

    #[tokio::test]
    async fn canceling_a_257th_thread_request_drops_it_and_unblocks_the_full_barrier_wait() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(300);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let thread_id = ThreadId::new();
        let target = ThreadSubscriptionTarget::captured(
            ConnectionId(1),
            thread_id,
            "subscription-a".to_string(),
        );
        assert!(outgoing.begin_thread_outbound_barrier(thread_id, 1));
        let thread_outgoing = ThreadScopedOutgoingMessageSender::from_captured_thread_subscriptions(
            outgoing.clone(),
            vec![target.clone()],
            thread_id,
        );
        for _ in 0..MAX_DEFERRED_THREAD_OUTBOUND_MESSAGES {
            thread_outgoing
                .send_server_notification(ServerNotification::ThreadGoalCleared(
                    ThreadGoalClearedNotification {
                        thread_id: thread_id.to_string(),
                    },
                ))
                .await;
        }

        let request_outgoing = outgoing.clone();
        let request_thread_id = thread_id;
        let blocked_request = tokio::spawn(async move {
            request_outgoing
                .send_request_to_thread_subscriptions(
                    &[target],
                    ServerRequestPayload::CurrentTimeRead(CurrentTimeReadParams {
                        thread_id: request_thread_id.to_string(),
                    }),
                    request_thread_id,
                )
                .await
        });
        tokio::task::yield_now().await;
        assert!(
            !blocked_request.is_finished(),
            "the 257th request must wait for barrier capacity before cancellation"
        );
        let request_id = RequestId::Integer(0);
        assert_eq!(
            outgoing.pending_requests_for_thread(thread_id).await.len(),
            1,
            "the blocked request must retain its callback until cancellation"
        );

        assert!(
            outgoing.cancel_request(&request_id).await,
            "the capacity-blocked request must remain cancelable before it reaches transport"
        );
        let (returned_request_id, response_waiter) = timeout(Duration::from_secs(1), blocked_request)
            .await
            .expect("cancellation must wake the capacity-blocked producer")
            .expect("blocked request task should not panic");
        assert_eq!(returned_request_id, request_id);
        assert!(
            response_waiter.await.is_err(),
            "cancellation must drop the blocked request callback rather than leave it unresolved"
        );
        assert!(outgoing.pending_requests_for_thread(thread_id).await.is_empty());
        assert!(
            outgoing
                .take_thread_request_resolution_targets(&request_id)
                .await
                .is_none(),
            "cancellation must not leak callback targets for an envelope that never reached a client"
        );

        outgoing
            .release_thread_outbound_barrier(thread_id, 1, None)
            .await;
        for _ in 0..MAX_DEFERRED_THREAD_OUTBOUND_MESSAGES {
            let OutgoingEnvelope::ToConnection {
                message: OutgoingMessage::ThreadScopedNotification(notification),
                ..
            } = rx
                .recv()
                .await
                .expect("retained notifications should release after cancellation")
            else {
                panic!("the canceled request must not be released to the client");
            };
            assert!(matches!(
                notification.envelope.notification,
                ServerNotification::ThreadGoalCleared(_)
            ));
        }
        assert!(
            timeout(Duration::from_millis(50), rx.recv()).await.is_err(),
            "no stale request envelope may follow the retained notifications"
        );
    }

    #[tokio::test]
    async fn invalidated_warning_cannot_enqueue_after_waiting_for_transport_capacity() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let thread_id = ThreadId::new();
        let target = ThreadSubscriptionTarget::captured(
            ConnectionId(1),
            thread_id,
            "subscription-a".to_string(),
        );

        // Occupy the sole transport slot before the warning barrier begins. The release task
        // passes its old pre-await check in the regression case, then waits here for capacity.
        outgoing
            .send_server_notification_to_thread_subscriptions(
                &[target.clone()],
                ServerNotification::ThreadGoalCleared(ThreadGoalClearedNotification {
                    thread_id: thread_id.to_string(),
                }),
            )
            .await;
        assert!(outgoing.begin_thread_outbound_barrier(thread_id, 1));
        let releasing_outgoing = outgoing.clone();
        let releasing_target = target.clone();
        let release = tokio::spawn(async move {
            releasing_outgoing
                .release_thread_outbound_barrier(
                    thread_id,
                    1,
                    Some((
                        vec![releasing_target],
                        ServerNotification::Warning(WarningNotification {
                            thread_id: Some(thread_id.to_string()),
                            message: "old warning".to_string(),
                        }),
                    )),
                )
                .await;
        });
        tokio::task::yield_now().await;
        assert!(
            !release.is_finished(),
            "the warning release should be waiting only for the occupied transport slot"
        );

        // A replacement wins after release started but before its actual transport enqueue.
        // It must fence the old warning even though the old task is already in its release path.
        outgoing
            .release_thread_outbound_barriers_before_generation(thread_id, 2)
            .await;
        let OutgoingEnvelope::ToConnection {
            message: OutgoingMessage::ThreadScopedNotification(notification),
            ..
        } = rx.recv().await.expect("preexisting core output should drain")
        else {
            panic!("expected the preexisting tagged core notification");
        };
        assert!(matches!(
            notification.envelope.notification,
            ServerNotification::ThreadGoalCleared(_)
        ));
        timeout(Duration::from_secs(1), release)
            .await
            .expect("invalidated warning release should finish once capacity is available")
            .expect("warning release task should not panic");
        assert!(
            timeout(Duration::from_millis(50), rx.recv()).await.is_err(),
            "the invalidated warning must not enqueue through the replacement lifecycle"
        );
    }

    #[tokio::test]
    async fn teardown_releases_no_listener_and_listener_owned_outbound_gates() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));

        for listener_generation in [NO_LISTENER_THREAD_OUTBOUND_BARRIER_GENERATION, 7] {
            let thread_id = ThreadId::new();
            let target = ThreadSubscriptionTarget::captured(
                ConnectionId(1),
                thread_id,
                format!("subscription-{listener_generation}"),
            );
            assert!(outgoing.begin_thread_outbound_barrier(thread_id, listener_generation));
            outgoing
                .send_server_notification_to_thread_subscriptions(
                    &[target],
                    ServerNotification::ThreadGoalCleared(ThreadGoalClearedNotification {
                        thread_id: thread_id.to_string(),
                    }),
                )
                .await;
            assert_eq!(outgoing.deferred_thread_outbound_count(thread_id).await, 1);

            outgoing
                .release_thread_outbound_barrier_for_teardown(thread_id)
                .await;
            let OutgoingEnvelope::ToConnection {
                message: OutgoingMessage::ThreadScopedNotification(notification),
                ..
            } = rx
                .recv()
                .await
                .expect("teardown must drain retained output without waiting for warning timeout")
            else {
                panic!("expected retained tagged output after teardown release");
            };
            assert!(matches!(
                notification.envelope.notification,
                ServerNotification::ThreadGoalCleared(_)
            ));
        }
    }
}
