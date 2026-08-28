//! Synchronous, payload-free observations for one physical inference attempt.
//!
//! This seam is deliberately separate from request construction. A caller
//! creates a descriptor from the exact request/admission decision, records the
//! physical-open boundary immediately before transport I/O, and records one
//! terminal outcome once the provider or local control plane has settled the
//! attempt. In particular, a local denial and an uncertain transport outcome
//! can never be represented as a provider success.

use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;

use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InferenceCallEvent;
use codex_protocol::protocol::InferenceCallSource;
use codex_protocol::protocol::InferenceCallStatus;
use codex_protocol::protocol::InferenceCallTransport;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TokenUsage;

use crate::RawPayloadKind;
use crate::RawTraceEventContext;
use crate::RawTraceEventPayload;
use crate::TraceWriter;
use crate::writer::unix_time_ms;

/// The source of work that caused the inference attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InferenceRequestKind {
    /// A normal model turn.
    Turn,
    /// Local history compaction.
    LocalCompaction,
    /// Remote compaction through the v2 endpoint.
    RemoteCompactionV2,
    /// Remote compaction through the original endpoint.
    RemoteCompact,
}

/// Protocol-neutral copy of the explicit host tool source for this attempt.
///
/// This keeps rollout-trace independent from Core and extension crates while
/// ensuring a continuity check cannot be reclassified from child identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InferenceObservationSource {
    Direct,
    HostContinuityCheck,
    CodeMode {
        cell_id: String,
        runtime_tool_call_id: String,
    },
}

impl InferenceObservationSource {
    /// Constructs explicit Code Mode provenance for a runtime tool call.
    pub fn code_mode(cell_id: String, runtime_tool_call_id: String) -> Self {
        Self::CodeMode {
            cell_id,
            runtime_tool_call_id,
        }
    }
}

/// Whether the request used a cached transport/request state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InferenceCacheState {
    /// No cache decision was available at the observation boundary.
    Unknown,
    /// A cached request/session state was reused.
    Hit,
    /// Cache lookup was performed and did not produce reusable state.
    Miss,
    /// This request kind has no cache semantics.
    NotApplicable,
}

/// Admission state attached to the exact request attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InferenceAdmission {
    /// Admission was not evaluated by the caller.
    Unknown,
    /// The admission broker authorized one physical request.
    Admitted,
    /// Local admission rejected the request before transport I/O.
    Denied,
}

/// Provider and model identity captured for one exact physical request.
///
/// Configured and effective provider identity are intentionally separate. The
/// effective provider must come from the resolved request/admission state; it
/// must never be inferred from `configured_provider`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferenceProviderIdentity {
    /// Provider key selected by local configuration.
    pub configured_provider: String,
    /// Model alias or setting selected by local configuration, when present.
    pub configured_model: Option<String>,
    /// Model value placed on the exact physical request.
    pub requested_model: String,
    /// Provider that owns the exact physical request after routing resolution.
    pub effective_provider: String,
    /// Model resolved for the exact physical request.
    pub effective_model: String,
    /// Service tier placed on the exact physical request, when present.
    pub requested_service_tier: Option<String>,
}

/// Immutable local identity and provenance for one inference attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferenceAttemptMetadata {
    /// Local correlation identity joining the open and terminal observations.
    pub inference_call_id: String,
    /// Current thread identity. For a child this is the child thread ID.
    pub thread_id: ThreadId,
    /// Turn or activation that owns the attempt.
    pub turn_id: String,
    /// Parent thread when this attempt belongs to a child; `None` for a root.
    pub parent_thread_id: Option<ThreadId>,
    /// Spawn call identity when this child was created by a spawn operation.
    pub spawn_request_id: Option<String>,
    /// Explicit local source, when the caller has one. `None` means unknown.
    pub source: Option<InferenceObservationSource>,
    /// Session/source provenance for the attempt.
    pub session_source: SessionSource,
    /// Kind of model work being attempted.
    pub request_kind: InferenceRequestKind,
    /// Physical transport route selected for the request.
    pub transport: InferenceCallTransport,
    /// Provider/model identity resolved before the physical-open boundary.
    pub provider: InferenceProviderIdentity,
    /// Cache state observed for this attempt.
    pub cache_state: InferenceCacheState,
    /// Admission state observed for this attempt.
    pub admission: InferenceAdmission,
    /// Caller-provided start timestamp. This is captured at the pre-I/O seam.
    pub request_started_at_ms: i64,
}

/// A terminal result explicitly returned by the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderTerminalResult {
    /// The provider completed the request and returned a response.
    Completed {
        response_id: Option<String>,
        upstream_request_id: Option<String>,
        observed_provider: Option<String>,
        observed_model: Option<String>,
        observed_model_snapshot: Option<String>,
        observed_service_tier: Option<String>,
        token_usage: Option<TokenUsage>,
    },
    /// The provider returned a recognized usage-limit result.
    UsageLimitReached {
        upstream_request_id: Option<String>,
        detail: Option<String>,
    },
    /// The provider returned a terminal error response.
    Failed {
        upstream_request_id: Option<String>,
        error: String,
    },
    /// The provider acknowledged the stream but the caller cancelled it.
    Cancelled {
        upstream_request_id: Option<String>,
        reason: String,
    },
}

/// One lifecycle observation delivered to a sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InferenceObservationEvent {
    /// The caller reached the exact pre-I/O physical request boundary.
    PhysicalRequestOpened { attempt: InferenceAttemptMetadata },
    /// The provider supplied a terminal result for an opened request.
    ProviderTerminal {
        attempt: InferenceAttemptMetadata,
        result: ProviderTerminalResult,
    },
    /// Local admission or policy rejected the request before transport I/O.
    LocalDenial {
        attempt: InferenceAttemptMetadata,
        reason: String,
    },
    /// Transport ended without enough evidence to claim a provider result.
    TransportUncertain {
        attempt: InferenceAttemptMetadata,
        reason: String,
    },
}

/// Synchronous sink for attempt/outcome observations.
///
/// Implementations should keep this method short and non-blocking. The
/// recorder calls it synchronously after linearizing each lifecycle transition.
pub trait InferenceObservationSink: Send + Sync + 'static {
    /// Receives one ordered observation event.
    fn record(&self, event: InferenceObservationEvent);
}

/// Durable observation sink backed by the append-only rollout trace writer.
///
/// The sink serializes the bounded protocol event as a protocol payload and
/// appends it through the existing `ProtocolEventObserved` raw seam. Payload
/// write/append failures are intentionally best-effort: tracing must not turn
/// a provider request into a failure, and the lifecycle event has already been
/// linearized before this callback runs.
#[derive(Debug)]
pub struct TraceWriterInferenceObservationSink {
    writer: Arc<TraceWriter>,
}

impl TraceWriterInferenceObservationSink {
    pub fn new(writer: Arc<TraceWriter>) -> Self {
        Self { writer }
    }
}

impl InferenceObservationSink for TraceWriterInferenceObservationSink {
    fn record(&self, event: InferenceObservationEvent) {
        let Some(event) = protocol_event_for_observation(event) else {
            return;
        };
        let thread_id = event.thread_id.clone();
        let turn_id = event.turn_id.clone();
        let Ok(event_payload) = self.writer.write_json_payload_compact(
            RawPayloadKind::ProtocolEvent,
            &EventMsg::InferenceCall(event),
        ) else {
            return;
        };
        let _ = self.writer.append_with_context(
            RawTraceEventContext {
                thread_id: Some(thread_id.to_string()),
                codex_turn_id: Some(turn_id),
            },
            RawTraceEventPayload::ProtocolEventObserved {
                event_type: "inference_call".to_string(),
                event_payload,
            },
        );
    }
}

fn protocol_event_for_observation(event: InferenceObservationEvent) -> Option<InferenceCallEvent> {
    let (
        attempt,
        status,
        request_completed_at_ms,
        response_id,
        upstream_request_id,
        observed_provider,
        observed_model,
        observed_model_snapshot,
        observed_service_tier,
        token_usage,
        outcome_detail,
    ) = match event {
        InferenceObservationEvent::PhysicalRequestOpened { attempt } => (
            attempt,
            InferenceCallStatus::Started,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        ),
        InferenceObservationEvent::ProviderTerminal { attempt, result } => match result {
            ProviderTerminalResult::Completed {
                response_id,
                upstream_request_id,
                observed_provider,
                observed_model,
                observed_model_snapshot,
                observed_service_tier,
                token_usage,
            } => (
                attempt,
                InferenceCallStatus::Completed,
                Some(unix_time_ms()),
                response_id,
                upstream_request_id,
                observed_provider,
                observed_model,
                observed_model_snapshot,
                observed_service_tier,
                token_usage,
                None,
            ),
            ProviderTerminalResult::UsageLimitReached {
                upstream_request_id,
                detail: _detail,
            } => (
                attempt,
                InferenceCallStatus::UsageLimitReached,
                Some(unix_time_ms()),
                None,
                upstream_request_id,
                None,
                None,
                None,
                None,
                None,
                Some("usage_limit_reached".to_string()),
            ),
            ProviderTerminalResult::Failed {
                upstream_request_id,
                error: _error,
            } => (
                attempt,
                InferenceCallStatus::Failed,
                Some(unix_time_ms()),
                None,
                upstream_request_id,
                None,
                None,
                None,
                None,
                None,
                Some("provider_failed".to_string()),
            ),
            ProviderTerminalResult::Cancelled {
                upstream_request_id,
                reason: _reason,
            } => (
                attempt,
                InferenceCallStatus::Cancelled,
                Some(unix_time_ms()),
                None,
                upstream_request_id,
                None,
                None,
                None,
                None,
                None,
                Some("cancelled".to_string()),
            ),
        },
        InferenceObservationEvent::LocalDenial {
            attempt,
            reason: _reason,
        } => (
            attempt,
            InferenceCallStatus::LocalDenied,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("local_denied".to_string()),
        ),
        InferenceObservationEvent::TransportUncertain {
            attempt,
            reason: _reason,
        } => (
            attempt,
            InferenceCallStatus::TransportUncertain,
            Some(unix_time_ms()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("transport_uncertain".to_string()),
        ),
    };

    let source = attempt.source.as_ref().map(|source| match source {
        InferenceObservationSource::Direct => InferenceCallSource::Direct,
        InferenceObservationSource::HostContinuityCheck => InferenceCallSource::HostContinuityCheck,
        InferenceObservationSource::CodeMode {
            cell_id,
            runtime_tool_call_id,
        } => InferenceCallSource::code_mode(cell_id.clone(), runtime_tool_call_id.clone()),
    });

    InferenceCallEvent {
        inference_call_id: attempt.inference_call_id,
        thread_id: attempt.thread_id,
        turn_id: attempt.turn_id,
        spawn_request_id: attempt.spawn_request_id,
        status,
        transport: attempt.transport,
        source,
        configured_provider: attempt.provider.configured_provider,
        configured_model: attempt.provider.configured_model,
        requested_model: attempt.provider.requested_model,
        effective_provider: attempt.provider.effective_provider,
        effective_model: attempt.provider.effective_model,
        requested_service_tier: attempt.provider.requested_service_tier,
        request_started_at_ms: attempt.request_started_at_ms,
        request_completed_at_ms,
        response_id,
        upstream_request_id,
        observed_provider,
        observed_model,
        observed_model_snapshot,
        observed_service_tier,
        outcome_detail,
        token_usage,
        truncated_fields: None,
        omitted_fields: None,
    }
    .into_durable()
}

/// In-memory sink intended for tests and local composition probes.
#[cfg(test)]
#[derive(Clone, Debug, Default)]
pub struct InMemoryInferenceObservationSink {
    events: Arc<Mutex<Vec<InferenceObservationEvent>>>,
}

#[cfg(test)]
impl InMemoryInferenceObservationSink {
    /// Returns a point-in-time copy of all events in delivery order.
    pub fn events(&self) -> Vec<InferenceObservationEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Removes all captured events.
    pub fn clear(&self) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

#[cfg(test)]
impl InferenceObservationSink for InMemoryInferenceObservationSink {
    fn record(&self, event: InferenceObservationEvent) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
    }
}

#[derive(Debug, Default)]
struct LifecycleState {
    physical_request_opened: bool,
    terminal_recorded: bool,
}

struct RecorderState {
    attempt: InferenceAttemptMetadata,
    sink: Arc<dyn InferenceObservationSink>,
    lifecycle: Mutex<LifecycleState>,
    /// Serializes callbacks without extending the lifecycle mutex's critical section.
    delivery: Mutex<()>,
}

/// Linearized recorder for one physical inference attempt.
///
/// Clones share lifecycle state, so a setup guard and a stream mapper cannot
/// both record terminal outcomes. The recorder never creates a physical
/// request, mutates a request, or adds correlation headers.
#[derive(Clone)]
pub struct InferenceObservationRecorder {
    state: Arc<RecorderState>,
}

impl fmt::Debug for InferenceObservationRecorder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InferenceObservationRecorder")
            .field("attempt", &self.state.attempt)
            .finish_non_exhaustive()
    }
}

impl InferenceObservationRecorder {
    /// Creates a recorder backed by the supplied synchronous sink.
    pub fn new(attempt: InferenceAttemptMetadata, sink: Arc<dyn InferenceObservationSink>) -> Self {
        Self {
            state: Arc::new(RecorderState {
                attempt,
                sink,
                lifecycle: Mutex::new(LifecycleState::default()),
                delivery: Mutex::new(()),
            }),
        }
    }

    /// Creates a no-op recorder for callers that do not need observations.
    pub fn disabled(attempt: InferenceAttemptMetadata) -> Self {
        Self::new(attempt, Arc::new(NoopInferenceObservationSink))
    }

    /// Returns the immutable exact request metadata used by every event.
    pub fn attempt(&self) -> &InferenceAttemptMetadata {
        &self.state.attempt
    }

    /// Records the pre-I/O boundary once.
    ///
    /// Call this immediately before the transport starts the physical request.
    /// The API intentionally cannot infer whether a caller has crossed that
    /// boundary; it only guarantees that one recorder emits at most one open.
    pub fn record_physical_request_opened(&self) -> Result<(), InferenceObservationError> {
        self.record_event(|lifecycle| {
            if self.state.attempt.admission == InferenceAdmission::Denied {
                return Err(InferenceObservationError::AdmissionDenied);
            }
            if lifecycle.physical_request_opened {
                return Err(InferenceObservationError::PhysicalRequestAlreadyOpened);
            }
            if lifecycle.terminal_recorded {
                return Err(InferenceObservationError::TerminalAlreadyRecorded);
            }
            lifecycle.physical_request_opened = true;
            Ok(InferenceObservationEvent::PhysicalRequestOpened {
                attempt: self.state.attempt.clone(),
            })
        })
    }

    /// Records one explicit provider terminal result after physical open.
    pub fn record_provider_terminal(
        &self,
        result: ProviderTerminalResult,
    ) -> Result<(), InferenceObservationError> {
        self.record_terminal(|attempt| InferenceObservationEvent::ProviderTerminal {
            attempt,
            result,
        })
    }

    /// Records a local denial without implying that transport I/O occurred.
    pub fn record_local_denial(
        &self,
        reason: impl Into<String>,
    ) -> Result<(), InferenceObservationError> {
        let reason = reason.into();
        self.record_event(|lifecycle| {
            if lifecycle.physical_request_opened {
                return Err(InferenceObservationError::PhysicalRequestAlreadyOpened);
            }
            if lifecycle.terminal_recorded {
                return Err(InferenceObservationError::TerminalAlreadyRecorded);
            }
            if self.state.attempt.admission != InferenceAdmission::Denied {
                return Err(InferenceObservationError::AdmissionNotDenied);
            }
            lifecycle.terminal_recorded = true;
            Ok(InferenceObservationEvent::LocalDenial {
                attempt: self.state.attempt.clone(),
                reason,
            })
        })
    }

    /// Records a transport outcome for which provider execution is uncertain.
    pub fn record_transport_uncertain(
        &self,
        reason: impl Into<String>,
    ) -> Result<(), InferenceObservationError> {
        self.record_terminal(|attempt| InferenceObservationEvent::TransportUncertain {
            attempt,
            reason: reason.into(),
        })
    }

    fn record_terminal(
        &self,
        event: impl FnOnce(InferenceAttemptMetadata) -> InferenceObservationEvent,
    ) -> Result<(), InferenceObservationError> {
        self.record_event(|lifecycle| {
            if !lifecycle.physical_request_opened {
                return Err(InferenceObservationError::PhysicalRequestNotOpened);
            }
            if lifecycle.terminal_recorded {
                return Err(InferenceObservationError::TerminalAlreadyRecorded);
            }
            lifecycle.terminal_recorded = true;
            Ok(event(self.state.attempt.clone()))
        })
    }

    fn record_event(
        &self,
        transition: impl FnOnce(
            &mut LifecycleState,
        ) -> Result<InferenceObservationEvent, InferenceObservationError>,
    ) -> Result<(), InferenceObservationError> {
        // Delivery serialization defines lifecycle event order. The lifecycle
        // mutex is released before arbitrary sink code runs, so a sink cannot
        // hold lifecycle state hostage while it blocks. Sink panics propagate
        // after the transition is committed; the event is never rolled back.
        let _delivery = self
            .state
            .delivery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let event = {
            let mut lifecycle = self
                .state
                .lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            transition(&mut lifecycle)?
        };
        self.state.sink.record(event);
        Ok(())
    }

    /// Settles an opened attempt whose transport ended without provider evidence.
    pub fn settle_abandoned(&self) -> Result<(), InferenceObservationError> {
        self.record_transport_uncertain("observation recorder abandoned before terminal outcome")
    }
}

impl Drop for InferenceObservationRecorder {
    fn drop(&mut self) {
        // Only the final handle may safely settle an abandoned attempt. Drop
        // is best-effort and suppresses sink panics because unwinding from a
        // destructor would otherwise abort an unrelated provider task.
        if Arc::strong_count(&self.state) != 1 {
            return;
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = self.settle_abandoned();
        }));
        drop(result);
    }
}

struct NoopInferenceObservationSink;

impl InferenceObservationSink for NoopInferenceObservationSink {
    fn record(&self, _event: InferenceObservationEvent) {}
}

/// Lifecycle errors returned when a caller violates the observation contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InferenceObservationError {
    /// A physical open was attempted more than once.
    PhysicalRequestAlreadyOpened,
    /// A terminal event was attempted before physical open.
    PhysicalRequestNotOpened,
    /// A terminal event was attempted after another terminal event.
    TerminalAlreadyRecorded,
    /// Admission denied the request, so physical open is impossible.
    AdmissionDenied,
    /// A local-denial event requires denied admission state.
    AdmissionNotDenied,
}

impl fmt::Display for InferenceObservationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::PhysicalRequestAlreadyOpened => "physical inference request already opened",
            Self::PhysicalRequestNotOpened => "physical inference request was not opened",
            Self::TerminalAlreadyRecorded => "inference terminal outcome already recorded",
            Self::AdmissionDenied => "denied admission cannot open a physical request",
            Self::AdmissionNotDenied => "local denial requires denied admission state",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for InferenceObservationError {}

#[cfg(test)]
#[path = "inference_observation_tests.rs"]
mod tests;
