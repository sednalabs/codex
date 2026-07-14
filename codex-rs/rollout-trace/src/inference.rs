//! Hot-path helpers for recording upstream inference attempts.
//!
//! The model client should not need to know whether rollout tracing is enabled.
//! A disabled context records nothing, which keeps one-shot HTTP calls,
//! WebSocket reuse, and retry/fallback attempts on the same code path.

use std::fmt::Display;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InferenceCallEvent;
use codex_protocol::protocol::InferenceCallStatus;
use codex_protocol::protocol::InferenceCallTransport;
use codex_protocol::protocol::TokenUsage;
use http::HeaderMap;
use http::HeaderValue;
use serde::Serialize;
use serde_json::Value as JsonValue;
use uuid::Uuid;

use crate::model::AgentThreadId;
use crate::model::CodexTurnId;
use crate::model::InferenceCallId;
use crate::payload::RawPayloadKind;
use crate::raw_event::RawTraceEventContext;
use crate::raw_event::RawTraceEventPayload;
use crate::writer::TraceWriter;

const INFERENCE_CALL_ID_HEADER: &str = "x-codex-inference-call-id";

/// Turn-local inference tracing context.
///
/// This is intentionally a no-op capable handle instead of an `Option` at each
/// transport callsite. Whether tracing is enabled is a session concern; retry,
/// fallback, and stream mapping code should always be able to say what happened
/// without first branching on trace availability.
#[derive(Clone, Debug)]
pub struct InferenceTraceContext {
    state: InferenceTraceContextState,
}

#[derive(Clone, Debug)]
enum InferenceTraceContextState {
    Disabled,
    Enabled(EnabledInferenceTraceContext),
}

#[derive(Clone, Debug)]
struct EnabledInferenceTraceContext {
    writer: Option<Arc<TraceWriter>>,
    thread_id: AgentThreadId,
    codex_turn_id: CodexTurnId,
    raw_trace_model: String,
    raw_trace_provider_name: String,
    configured_model: String,
    configured_provider: String,
    observation_thread_id: Option<ThreadId>,
    configured_service_tier: Option<String>,
}

/// One concrete upstream request attempt.
///
/// A Codex turn can create multiple attempts when auth recovery retries the
/// HTTP request or WebSocket setup falls back to HTTP. Completion is often
/// observed after the client returns the response stream, so the attempt owns
/// the terminal guard that prevents duplicate lifecycle events.
#[derive(Debug)]
pub struct InferenceTraceAttempt {
    state: InferenceTraceAttemptState,
}

#[derive(Debug)]
enum InferenceTraceAttemptState {
    Disabled,
    Enabled(EnabledInferenceTraceAttempt),
}

#[derive(Debug)]
struct EnabledInferenceTraceAttempt {
    context: EnabledInferenceTraceContext,
    observation: Option<InferenceAttemptObservation>,
    inference_call_id: InferenceCallId,
    terminal_recorded: AtomicBool,
}

#[derive(Debug)]
struct InferenceAttemptObservation {
    transport: InferenceCallTransport,
    requested_model: String,
    requested_service_tier: Option<String>,
    request_started_at_ms: i64,
}

/// Non-delta response payload saved for completed or interrupted inference streams.
///
/// We intentionally record completed output items instead of every stream delta
/// here. The raw stream can be added later as a separate payload class; this
/// response summary gives the reducer stable response identity when available
/// plus model-visible output without duplicating high-volume text deltas.
#[derive(Serialize)]
struct TracedResponseStreamOutput<'a> {
    response_id: Option<&'a str>,
    upstream_request_id: Option<&'a str>,
    token_usage: Option<&'a TokenUsage>,
    output_items: Vec<JsonValue>,
}

impl InferenceTraceContext {
    /// Builds a context that accepts trace calls and records nothing.
    pub fn disabled() -> Self {
        Self {
            state: InferenceTraceContextState::Disabled,
        }
    }

    /// Builds an enabled context for all upstream attempts made by one Codex turn.
    pub fn enabled(
        writer: Arc<TraceWriter>,
        thread_id: AgentThreadId,
        codex_turn_id: CodexTurnId,
        model: String,
        provider_name: String,
    ) -> Self {
        Self {
            state: InferenceTraceContextState::Enabled(EnabledInferenceTraceContext {
                writer: Some(writer),
                thread_id,
                codex_turn_id,
                raw_trace_model: model.clone(),
                raw_trace_provider_name: provider_name.clone(),
                configured_model: model,
                configured_provider: provider_name,
                observation_thread_id: None,
                configured_service_tier: None,
            }),
        }
    }

    /// Adds always-on, payload-free inference observation metadata.
    pub fn with_observations(
        self,
        thread_id: ThreadId,
        turn_id: String,
        configured_provider: String,
        configured_model: String,
        configured_service_tier: Option<String>,
    ) -> Self {
        let context = match self.state {
            InferenceTraceContextState::Disabled => EnabledInferenceTraceContext {
                writer: None,
                thread_id: thread_id.to_string(),
                codex_turn_id: turn_id,
                raw_trace_model: configured_model.clone(),
                raw_trace_provider_name: configured_provider.clone(),
                configured_model,
                configured_provider,
                observation_thread_id: Some(thread_id),
                configured_service_tier,
            },
            InferenceTraceContextState::Enabled(mut context) => {
                context.configured_model = configured_model;
                context.configured_provider = configured_provider;
                context.observation_thread_id = Some(thread_id);
                context.configured_service_tier = configured_service_tier;
                context
            }
        };
        Self {
            state: InferenceTraceContextState::Enabled(context),
        }
    }

    /// Starts a new attempt after the concrete provider request has been built.
    pub fn start_attempt(&self) -> InferenceTraceAttempt {
        let requested_model = match &self.state {
            InferenceTraceContextState::Disabled => String::new(),
            InferenceTraceContextState::Enabled(context) => context.raw_trace_model.clone(),
        };
        self.start_observed_attempt(
            InferenceCallTransport::ResponsesHttp,
            requested_model,
            /*requested_service_tier*/ None,
        )
    }

    /// Starts an attempt with the configured/requested observation boundary.
    pub fn start_observed_attempt(
        &self,
        transport: InferenceCallTransport,
        requested_model: String,
        requested_service_tier: Option<String>,
    ) -> InferenceTraceAttempt {
        let InferenceTraceContextState::Enabled(context) = &self.state else {
            return InferenceTraceAttempt::disabled();
        };

        InferenceTraceAttempt {
            state: InferenceTraceAttemptState::Enabled(EnabledInferenceTraceAttempt {
                context: context.clone(),
                observation: context.observation_thread_id.as_ref().map(|_| {
                    InferenceAttemptObservation {
                        transport,
                        requested_model,
                        requested_service_tier,
                        request_started_at_ms: now_unix_timestamp_ms(),
                    }
                }),
                inference_call_id: next_inference_call_id(),
                terminal_recorded: AtomicBool::new(false),
            }),
        }
    }
}

impl InferenceTraceAttempt {
    /// Builds an attempt that records nothing.
    pub fn disabled() -> Self {
        Self {
            state: InferenceTraceAttemptState::Disabled,
        }
    }

    fn inference_call_id(&self) -> Option<&str> {
        match &self.state {
            InferenceTraceAttemptState::Disabled => None,
            InferenceTraceAttemptState::Enabled(attempt) => {
                Some(attempt.inference_call_id.as_str())
            }
        }
    }

    /// Returns the durable started observation for this attempt.
    pub fn started_observation(&self) -> Option<InferenceCallEvent> {
        let InferenceTraceAttemptState::Enabled(attempt) = &self.state else {
            return None;
        };
        attempt.observation.as_ref().and_then(|observation| {
            observation
                .event(
                    &attempt.context,
                    &attempt.inference_call_id,
                    InferenceCallStatus::Started,
                    /*request_completed_at_ms*/ None,
                    /*response_id*/ None,
                    /*upstream_request_id*/ None,
                    /*observed_model*/ None,
                    /*observed_model_snapshot*/ None,
                    /*observed_service_tier*/ None,
                    /*token_usage*/ None,
                )
                .into_durable()
        })
    }

    /// Adds rollout-trace propagation headers for this attempt when tracing is enabled.
    pub fn add_request_headers(&self, headers: &mut HeaderMap) {
        let Some(inference_call_id) = self.inference_call_id() else {
            return;
        };
        let Ok(inference_call_id) = HeaderValue::from_str(inference_call_id) else {
            // These IDs are generated internally as UUID strings, so rejection
            // should be impossible in practice. Tracing remains best-effort,
            // though, and must never make provider requests fail.
            return;
        };

        headers.insert(INFERENCE_CALL_ID_HEADER, inference_call_id);
    }

    /// Records the request payload replay should treat as the model-visible inference input.
    ///
    /// This is usually the exact provider request. Callers may instead pass a
    /// logical request when the transport omits already-sent input, such as
    /// websocket reuse after an untraced warmup response.
    pub fn record_started(&self, request: &impl Serialize) {
        let InferenceTraceAttemptState::Enabled(attempt) = &self.state else {
            return;
        };
        let context = &attempt.context;
        let Some(writer) = &context.writer else {
            return;
        };
        let Some(request_payload) =
            write_json_payload_best_effort(writer, RawPayloadKind::InferenceRequest, request)
        else {
            return;
        };

        append_with_context_best_effort(
            context,
            RawTraceEventPayload::InferenceStarted {
                inference_call_id: attempt.inference_call_id.clone(),
                thread_id: context.thread_id.clone(),
                codex_turn_id: context.codex_turn_id.clone(),
                model: context.raw_trace_model.clone(),
                provider_name: context.raw_trace_provider_name.clone(),
                request_payload,
            },
        );
    }

    /// Records successful provider completion and serializes the observed output items.
    ///
    /// Callers pass protocol-native response items so this crate owns the
    /// trace-specific serialization rules. That keeps codex-core focused on
    /// transport behavior while preserving trace evidence that normal request
    /// serialization intentionally omits.
    pub fn record_completed(
        &self,
        response_id: &str,
        upstream_request_id: Option<&str>,
        token_usage: &Option<TokenUsage>,
        output_items: &[ResponseItem],
    ) {
        let _ = self.record_completed_observation(
            response_id,
            upstream_request_id,
            token_usage,
            output_items,
            /*observed_model*/ None,
            /*observed_model_snapshot*/ None,
            /*observed_service_tier*/ None,
        );
    }

    /// Records successful completion and returns its payload-free observation.
    #[allow(clippy::too_many_arguments)]
    pub fn record_completed_observation(
        &self,
        response_id: &str,
        upstream_request_id: Option<&str>,
        token_usage: &Option<TokenUsage>,
        output_items: &[ResponseItem],
        observed_model: Option<&str>,
        observed_model_snapshot: Option<&str>,
        observed_service_tier: Option<&str>,
    ) -> Option<InferenceCallEvent> {
        let Some(attempt) = self.take_terminal_attempt() else {
            return None;
        };
        if let Some(response_payload) = write_response_payload_best_effort(
            &attempt.context,
            Some(response_id),
            upstream_request_id,
            token_usage.as_ref(),
            output_items,
        ) {
            append_with_context_best_effort(
                &attempt.context,
                RawTraceEventPayload::InferenceCompleted {
                    inference_call_id: attempt.inference_call_id.clone(),
                    response_id: Some(response_id.to_string()),
                    upstream_request_id: upstream_request_id.map(str::to_string),
                    response_payload,
                },
            );
        }

        attempt.terminal_observation(
            InferenceCallStatus::Completed,
            Some(response_id),
            upstream_request_id,
            observed_model,
            observed_model_snapshot,
            observed_service_tier,
            token_usage.as_ref(),
        )
    }

    /// Records pre-response and mid-stream failures.
    pub fn record_failed(
        &self,
        error: impl Display,
        upstream_request_id: Option<&str>,
        output_items: &[ResponseItem],
    ) -> Option<InferenceCallEvent> {
        let Some(attempt) = self.take_terminal_attempt() else {
            return None;
        };
        if attempt.context.writer.is_some() {
            let partial_response_payload = if output_items.is_empty() {
                None
            } else {
                write_response_payload_best_effort(
                    &attempt.context,
                    /*response_id*/ None,
                    upstream_request_id,
                    /*token_usage*/ None,
                    output_items,
                )
            };
            append_with_context_best_effort(
                &attempt.context,
                RawTraceEventPayload::InferenceFailed {
                    inference_call_id: attempt.inference_call_id.clone(),
                    upstream_request_id: upstream_request_id.map(str::to_string),
                    error: error.to_string(),
                    partial_response_payload,
                },
            );
        }
        attempt.terminal_observation(
            InferenceCallStatus::Failed,
            /*response_id*/ None,
            upstream_request_id,
            /*observed_model*/ None,
            /*observed_model_snapshot*/ None,
            /*observed_service_tier*/ None,
            /*token_usage*/ None,
        )
    }

    /// Records a provider stream that Codex intentionally stopped consuming.
    ///
    /// This happens when the turn is interrupted or when mailbox delivery
    /// preempts the current sampling request. Complete output items observed
    /// before that point are retained as partial response evidence.
    pub fn record_cancelled(
        &self,
        reason: impl Display,
        upstream_request_id: Option<&str>,
        output_items: &[ResponseItem],
    ) -> Option<InferenceCallEvent> {
        let Some(attempt) = self.take_terminal_attempt() else {
            return None;
        };
        if attempt.context.writer.is_some() {
            let partial_response_payload = if output_items.is_empty() {
                None
            } else {
                write_response_payload_best_effort(
                    &attempt.context,
                    /*response_id*/ None,
                    upstream_request_id,
                    /*token_usage*/ None,
                    output_items,
                )
            };
            append_with_context_best_effort(
                &attempt.context,
                RawTraceEventPayload::InferenceCancelled {
                    inference_call_id: attempt.inference_call_id.clone(),
                    upstream_request_id: upstream_request_id.map(str::to_string),
                    reason: reason.to_string(),
                    partial_response_payload,
                },
            );
        }
        attempt.terminal_observation(
            InferenceCallStatus::Cancelled,
            /*response_id*/ None,
            upstream_request_id,
            /*observed_model*/ None,
            /*observed_model_snapshot*/ None,
            /*observed_service_tier*/ None,
            /*token_usage*/ None,
        )
    }

    fn take_terminal_attempt(&self) -> Option<&EnabledInferenceTraceAttempt> {
        let attempt = match &self.state {
            InferenceTraceAttemptState::Disabled => return None,
            InferenceTraceAttemptState::Enabled(attempt) => attempt,
        };
        if attempt.terminal_recorded.swap(true, Ordering::AcqRel) {
            return None;
        }
        Some(attempt)
    }
}

impl InferenceAttemptObservation {
    #[allow(clippy::too_many_arguments)]
    fn event(
        &self,
        context: &EnabledInferenceTraceContext,
        inference_call_id: &str,
        status: InferenceCallStatus,
        request_completed_at_ms: Option<i64>,
        response_id: Option<&str>,
        upstream_request_id: Option<&str>,
        observed_model: Option<&str>,
        observed_model_snapshot: Option<&str>,
        observed_service_tier: Option<&str>,
        token_usage: Option<&TokenUsage>,
    ) -> InferenceCallEvent {
        InferenceCallEvent {
            inference_call_id: inference_call_id.to_string(),
            thread_id: context
                .observation_thread_id
                .clone()
                .expect("observation attempts have a thread id"),
            turn_id: context.codex_turn_id.clone(),
            status,
            transport: self.transport,
            configured_provider: context.configured_provider.clone(),
            configured_model: context.configured_model.clone(),
            configured_service_tier: context.configured_service_tier.clone(),
            requested_model: self.requested_model.clone(),
            requested_service_tier: self.requested_service_tier.clone(),
            request_started_at_ms: self.request_started_at_ms,
            request_completed_at_ms,
            response_id: response_id.map(str::to_string),
            upstream_request_id: upstream_request_id.map(str::to_string),
            observed_model: observed_model.map(str::to_string),
            observed_model_snapshot: observed_model_snapshot.map(str::to_string),
            observed_service_tier: observed_service_tier.map(str::to_string),
            token_usage: token_usage.cloned(),
            truncated_fields: Vec::new(),
            omitted_fields: Vec::new(),
        }
    }
}

impl EnabledInferenceTraceAttempt {
    #[allow(clippy::too_many_arguments)]
    fn terminal_observation(
        &self,
        status: InferenceCallStatus,
        response_id: Option<&str>,
        upstream_request_id: Option<&str>,
        observed_model: Option<&str>,
        observed_model_snapshot: Option<&str>,
        observed_service_tier: Option<&str>,
        token_usage: Option<&TokenUsage>,
    ) -> Option<InferenceCallEvent> {
        self.observation.as_ref().and_then(|observation| {
            observation
                .event(
                    &self.context,
                    &self.inference_call_id,
                    status,
                    Some(now_unix_timestamp_ms()),
                    response_id,
                    upstream_request_id,
                    observed_model,
                    observed_model_snapshot,
                    observed_service_tier,
                    token_usage,
                )
                .into_durable()
        })
    }
}

/// Serializes a response item for trace evidence rather than future request construction.
///
/// The protocol serializer intentionally omits some readable reasoning content
/// when shaping items for later model requests. Rollout traces need the item as
/// Codex received it, so this helper restores that content in the raw payload.
pub(crate) fn trace_response_item_json(item: &ResponseItem) -> JsonValue {
    let mut value = serde_json::to_value(item).unwrap_or_else(|err| {
        serde_json::json!({
            "serialization_error": err.to_string(),
        })
    });

    if let ResponseItem::Reasoning {
        content: Some(content),
        ..
    } = item
        && let JsonValue::Object(object) = &mut value
    {
        object.insert(
            "content".to_string(),
            serde_json::to_value(content).unwrap_or_else(|err| {
                serde_json::json!({
                    "serialization_error": err.to_string(),
                })
            }),
        );
    }

    value
}

fn next_inference_call_id() -> InferenceCallId {
    Uuid::new_v4().to_string()
}

fn now_unix_timestamp_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or_default()
}

fn write_json_payload_best_effort(
    writer: &TraceWriter,
    kind: RawPayloadKind,
    payload: &impl Serialize,
) -> Option<crate::RawPayloadRef> {
    writer.write_json_payload(kind, payload).ok()
}

fn write_response_payload_best_effort(
    context: &EnabledInferenceTraceContext,
    response_id: Option<&str>,
    upstream_request_id: Option<&str>,
    token_usage: Option<&TokenUsage>,
    output_items: &[ResponseItem],
) -> Option<crate::RawPayloadRef> {
    let writer = context.writer.as_ref()?;
    let response_payload = TracedResponseStreamOutput {
        response_id,
        upstream_request_id,
        token_usage,
        output_items: output_items.iter().map(trace_response_item_json).collect(),
    };
    write_json_payload_best_effort(writer, RawPayloadKind::InferenceResponse, &response_payload)
}

fn append_with_context_best_effort(
    context: &EnabledInferenceTraceContext,
    payload: RawTraceEventPayload,
) {
    let Some(writer) = &context.writer else {
        return;
    };
    let event_context = RawTraceEventContext {
        thread_id: Some(context.thread_id.clone()),
        codex_turn_id: Some(context.codex_turn_id.clone()),
    };
    let _ = writer.append_with_context(event_context, payload);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use codex_protocol::ResponseItemId;
    use codex_protocol::models::ReasoningItemContent;
    use codex_protocol::models::ReasoningItemReasoningSummary;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::model::ExecutionStatus;
    use crate::replay_bundle;

    #[test]
    fn disabled_attempt_adds_no_request_headers() {
        let mut headers = HeaderMap::new();

        InferenceTraceAttempt::disabled().add_request_headers(&mut headers);

        assert!(headers.is_empty());
    }

    #[test]
    fn enabled_attempt_adds_inference_request_header() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let writer = Arc::new(TraceWriter::create(
            temp.path(),
            "trace-1".to_string(),
            "rollout-1".to_string(),
            "thread-root".to_string(),
        )?);
        let context = InferenceTraceContext::enabled(
            writer,
            "thread-root".to_string(),
            "turn-1".to_string(),
            "gpt-test".to_string(),
            "test-provider".to_string(),
        );
        let attempt = context.start_attempt();
        let mut headers = HeaderMap::new();

        attempt.add_request_headers(&mut headers);

        let header = headers
            .get(INFERENCE_CALL_ID_HEADER)
            .expect("inference header present");
        assert_eq!(Some(header.to_str()?), attempt.inference_call_id());
        assert!(Uuid::parse_str(header.to_str()?).is_ok());
        Ok(())
    }

    #[test]
    fn enabled_context_records_replayable_inference_attempt() -> anyhow::Result<()> {
        let temp = TempDir::new()?;
        let writer = Arc::new(TraceWriter::create(
            temp.path(),
            "trace-1".to_string(),
            "rollout-1".to_string(),
            "thread-root".to_string(),
        )?);
        writer.append(RawTraceEventPayload::ThreadStarted {
            thread_id: "thread-root".to_string(),
            agent_path: "/root".to_string(),
            metadata_payload: None,
        })?;
        writer.append(RawTraceEventPayload::CodexTurnStarted {
            codex_turn_id: "turn-1".to_string(),
            thread_id: "thread-root".to_string(),
        })?;
        let context = InferenceTraceContext::enabled(
            writer,
            "thread-root".to_string(),
            "turn-1".to_string(),
            "gpt-test".to_string(),
            "test-provider".to_string(),
        );

        let attempt = context.start_attempt();
        attempt.record_started(&json!({
            "model": "gpt-test",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hello"}]
            }],
        }));
        attempt.record_completed("resp-1", Some("req-1"), &None, &[]);

        let rollout = replay_bundle(temp.path())?;
        let inference = rollout
            .inference_calls
            .values()
            .next()
            .expect("recorded inference call");

        assert_eq!(rollout.inference_calls.len(), 1);
        assert_eq!(inference.thread_id, "thread-root");
        assert_eq!(inference.codex_turn_id, "turn-1");
        assert_eq!(inference.execution.status, ExecutionStatus::Completed);
        assert_eq!(inference.upstream_request_id, Some("req-1".to_string()));
        assert_eq!(rollout.raw_payloads.len(), 2);

        Ok(())
    }

    #[test]
    fn traced_response_item_preserves_reasoning_content_omitted_by_normal_serializer() {
        let item = ResponseItem::Reasoning {
            id: Some(ResponseItemId::with_suffix("rs", "1")),
            summary: vec![ReasoningItemReasoningSummary::SummaryText {
                text: "summary".to_string(),
            }],
            content: Some(vec![ReasoningItemContent::Text {
                text: "raw reasoning".to_string(),
            }]),
            encrypted_content: Some("encoded".to_string()),
            internal_chat_message_metadata_passthrough: None,
        };

        let normal = serde_json::to_value(&item).expect("response item serializes");
        let traced = trace_response_item_json(&item);

        assert_eq!(normal.get("content"), None);
        assert_eq!(
            traced,
            json!({
                "type": "reasoning",
                "id": "rs_1",
                "summary": [{"type": "summary_text", "text": "summary"}],
                "content": [{"type": "text", "text": "raw reasoning"}],
                "encrypted_content": "encoded",
            }),
        );
    }

    #[test]
    fn raw_trace_toggle_preserves_configured_and_requested_identity() -> anyhow::Result<()> {
        let thread_id = ThreadId::new();
        let configured_provider = "configured-provider";
        let configured_model = "configured-model-alias";
        let requested_model = "resolved-request-model";
        let requested_service_tier = Some("priority".to_string());

        let disabled = InferenceTraceContext::disabled()
            .with_observations(
                thread_id,
                "turn-identity".to_string(),
                configured_provider.to_string(),
                configured_model.to_string(),
                /*configured_service_tier*/ None,
            )
            .start_observed_attempt(
                InferenceCallTransport::ResponsesHttp,
                requested_model.to_string(),
                requested_service_tier.clone(),
            )
            .started_observation()
            .expect("disabled raw trace still records an observation");

        let temp = TempDir::new()?;
        let writer = Arc::new(TraceWriter::create(
            temp.path(),
            "trace-identity".to_string(),
            "rollout-identity".to_string(),
            thread_id.to_string(),
        )?);
        let enabled = InferenceTraceContext::enabled(
            writer,
            thread_id.to_string(),
            "turn-identity".to_string(),
            "raw-trace-model".to_string(),
            "raw-trace-provider".to_string(),
        )
        .with_observations(
            thread_id,
            "turn-identity".to_string(),
            configured_provider.to_string(),
            configured_model.to_string(),
            /*configured_service_tier*/ None,
        )
        .start_observed_attempt(
            InferenceCallTransport::ResponsesHttp,
            requested_model.to_string(),
            requested_service_tier,
        )
        .started_observation()
        .expect("enabled raw trace records an observation");

        assert_eq!(enabled.configured_provider, configured_provider);
        assert_eq!(enabled.configured_model, configured_model);
        assert_eq!(enabled.requested_model, requested_model);

        let normalize = |mut event: InferenceCallEvent| {
            event.inference_call_id.clear();
            event.request_started_at_ms = 0;
            event
        };
        assert_eq!(normalize(enabled), normalize(disabled));
        Ok(())
    }

    #[test]
    fn observations_keep_exact_usage_and_distinct_retry_boundaries() {
        let thread_id = ThreadId::new();
        let context = InferenceTraceContext::disabled().with_observations(
            thread_id,
            "turn-1".to_string(),
            "configured-provider".to_string(),
            "configured-model".to_string(),
            Some("fast".to_string()),
        );
        let attempt = context.start_observed_attempt(
            InferenceCallTransport::ResponsesHttp,
            "requested-model".to_string(),
            Some("priority".to_string()),
        );
        let started = attempt.started_observation().expect("started observation");
        let token_usage = TokenUsage {
            input_tokens: 101,
            cached_input_tokens: 23,
            output_tokens: 47,
            reasoning_output_tokens: 11,
            total_tokens: 148,
        };

        let completed = attempt
            .record_completed_observation(
                "resp-1",
                Some("req-1"),
                &Some(token_usage.clone()),
                /*output_items*/ &[],
                Some("observed-model"),
                Some("snapshot-1"),
                Some("priority"),
            )
            .expect("completed observation");

        assert_eq!(
            completed,
            InferenceCallEvent {
                status: InferenceCallStatus::Completed,
                request_completed_at_ms: completed.request_completed_at_ms,
                response_id: Some("resp-1".to_string()),
                upstream_request_id: Some("req-1".to_string()),
                observed_model: Some("observed-model".to_string()),
                observed_model_snapshot: Some("snapshot-1".to_string()),
                observed_service_tier: Some("priority".to_string()),
                token_usage: Some(token_usage),
                ..started
            }
        );
        let failed_attempt = context.start_observed_attempt(
            InferenceCallTransport::ResponsesWebsocket,
            "requested-model".to_string(),
            /*requested_service_tier*/ None,
        );
        let failed_started = failed_attempt.started_observation().expect("failed start");
        let failed = failed_attempt
            .record_failed("fallback", None, &[])
            .expect("failed observation");
        let cancelled_attempt = context.start_observed_attempt(
            InferenceCallTransport::ResponsesHttp,
            "requested-model".to_string(),
            /*requested_service_tier*/ None,
        );
        let cancelled_started = cancelled_attempt
            .started_observation()
            .expect("cancelled start");
        let cancelled = cancelled_attempt
            .record_cancelled("interrupted", Some("req-2"), &[])
            .expect("cancelled observation");

        assert_eq!(failed.inference_call_id, failed_started.inference_call_id);
        assert_eq!(
            cancelled.inference_call_id,
            cancelled_started.inference_call_id
        );
        assert_eq!(
            (
                failed.status,
                failed.transport,
                failed.token_usage,
                cancelled.status,
                cancelled.token_usage,
            ),
            (
                InferenceCallStatus::Failed,
                InferenceCallTransport::ResponsesWebsocket,
                None,
                InferenceCallStatus::Cancelled,
                None,
            )
        );
        assert_ne!(
            failed_started.inference_call_id,
            cancelled_started.inference_call_id
        );
    }
}
