use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Instant;

use tokio::sync::RwLock;
use tokio::task::JoinError;
use tokio_util::either::Either;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;
use tracing::Instrument;
use tracing::info;
use tracing::instrument;
use tracing::trace_span;

use crate::agent::SpawnPublicationDecision;
use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::tools::call_trace;
use crate::tools::context::AbortedToolOutput;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::multi_agents_spec::MULTI_AGENT_V1_NAMESPACE;
use crate::tools::lifecycle::notify_tool_aborted;
use crate::tools::registry::AnyToolResult;
use crate::tools::registry::ToolArgumentDiffConsumer;
use crate::tools::router::ToolCall;
use crate::tools::router::ToolCallSource;
use codex_history::ResponseItemEnvelope;
use codex_protocol::error::CodexErr;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ToolResultMetadata;

struct ToolCallTimingGuard {
    started_at: Instant,
    execution_started_at: Arc<OnceLock<Instant>>,
    conversation_id: String,
    turn_id: String,
    call_id: String,
    tool_name: codex_tools::ToolName,
}

/// Connects the tool runtime's cancellation branch to the spawn owner's publication CAS.
///
/// This record is created before dispatch admission, so cancellation cannot race a delayed
/// handler past an unrecorded "no child yet" window.
struct SpawnPublicationGuard {
    control: crate::agent::AgentControl,
    parent_thread_id: codex_protocol::ThreadId,
    call_id: String,
}

fn is_multi_agent_spawn_call(call: &ToolCall, multi_agent_v2_namespace: Option<&str>) -> bool {
    call.tool_name.name.as_str() == "spawn_agent"
        && match call.tool_name.namespace.as_deref() {
            None | Some(MULTI_AGENT_V1_NAMESPACE) => true,
            Some(namespace) => Some(namespace) == multi_agent_v2_namespace,
        }
        && matches!(&call.payload, ToolPayload::Function { .. })
}

impl SpawnPublicationGuard {
    fn begin(
        session: &Session,
        multi_agent_v2_namespace: Option<&str>,
        call: &ToolCall,
    ) -> Option<Self> {
        if !is_multi_agent_spawn_call(call, multi_agent_v2_namespace) {
            return None;
        }
        let guard = Self {
            control: session.services.agent_control.clone(),
            parent_thread_id: session.thread_id,
            call_id: call.call_id.clone(),
        };
        guard
            .control
            .begin_tool_spawn_publication(guard.parent_thread_id, &guard.call_id);
        Some(guard)
    }

    fn cancel(&self) -> SpawnPublicationDecision {
        self.control
            .cancel_tool_spawn_publication(self.parent_thread_id, &self.call_id)
    }

    fn finish(&self) {
        self.control
            .finish_tool_spawn_publication(self.parent_thread_id, &self.call_id);
    }
}

impl Drop for SpawnPublicationGuard {
    fn drop(&mut self) {
        self.finish();
    }
}

#[derive(Clone)]
pub(crate) struct ToolCallRuntime {
    session: Arc<Session>,
    // Tool calls may run later, so retain the step whose tool list advertised them.
    step_context: Arc<StepContext>,
    tracker: SharedTurnDiffTracker,
    parallel_execution: Arc<RwLock<()>>,
}

impl ToolCallRuntime {
    pub(crate) fn new(
        session: Arc<Session>,
        step_context: Arc<StepContext>,
        tracker: SharedTurnDiffTracker,
    ) -> Self {
        Self {
            session,
            step_context,
            tracker,
            parallel_execution: Arc::new(RwLock::new(())),
        }
    }

    pub(crate) fn create_diff_consumer(
        &self,
        tool_name: &codex_tools::ToolName,
    ) -> Option<Box<dyn ToolArgumentDiffConsumer>> {
        self.step_context
            .tool_router
            .create_diff_consumer(tool_name)
    }

    #[instrument(level = "trace", skip_all)]
    pub(crate) fn handle_tool_call(
        self,
        call: ToolCall,
        cancellation_token: CancellationToken,
    ) -> impl std::future::Future<Output = Result<ResponseItemEnvelope, CodexErr>> {
        let error_call = call.clone();
        let source = call.direct_source();
        let recorder = self.session.services.executed_tool_calls.clone();
        let recorded_call = recorder.prepare_direct_call(&call, &source, &self.step_context);
        let step_context = Arc::clone(&self.step_context);
        let future =
            self.handle_tool_call_with_source(step_context, call, source, cancellation_token);
        async move {
            let result = future.await;
            let mut recorded_call =
                recorded_call.filter(|(_, recording)| recording.strong_count() > 0);
            let mut response = match result {
                Ok(result) => {
                    if let Some((call, _)) = recorded_call.as_mut()
                        && let Some(metadata) = result.result.tool_result_metadata()
                    {
                        call.set_tool_result_metadata(ToolResultMetadata::new(metadata));
                    }
                    result.into_response()
                }
                Err(FunctionCallError::Fatal(message)) => return Err(CodexErr::Fatal(message)),
                Err(other) => {
                    ResponseItemEnvelope::new(Self::failure_response(error_call, other).into())
                }
            };
            recorder.attach_direct_call_to_output(&mut response.item, recorded_call);
            Ok(response)
        }
    }

    #[instrument(level = "trace", skip_all)]
    pub(crate) fn handle_tool_call_with_source(
        self,
        step_context: Arc<StepContext>,
        call: ToolCall,
        source: ToolCallSource,
        cancellation_token: CancellationToken,
    ) -> impl std::future::Future<Output = Result<AnyToolResult, FunctionCallError>> {
        self.session
            .services
            .executed_tool_calls
            .record_tool_call(&call, &source, &step_context);
        let router = &step_context.tool_router;
        let supports_parallel = router.tool_supports_parallel(&call);
        let tool_runtime = router.tool_runtime(&call.tool_name);
        let router = Arc::clone(router);
        let session = Arc::clone(&self.session);
        let turn = Arc::clone(&step_context.turn);
        let tracker = Arc::clone(&self.tracker);
        let lock = Arc::clone(&self.parallel_execution);
        let invocation_cancellation_token = cancellation_token.clone();
        let started = Instant::now();
        let tool_call_timing_guard =
            ToolCallTimingGuard::capture(started, &session.thread_id, &turn.sub_id, &call, &source);
        let execution_started_at = tool_call_timing_guard
            .as_ref()
            .map(|timing| Arc::clone(&timing.execution_started_at));
        let abort_session = Arc::clone(&session);
        let abort_source = source.clone();
        let abort_turn = Arc::clone(&turn);
        let spawn_publication = SpawnPublicationGuard::begin(
            session.as_ref(),
            turn.config.multi_agent_v2.tool_namespace.as_deref(),
            &call,
        );
        let terminal_outcome_reached = Arc::new(AtomicBool::new(false));
        let dispatch_terminal_outcome_reached = Arc::clone(&terminal_outcome_reached);
        let dispatch_call = call.clone();
        let thread_id = session.thread_id;
        let trace_source = match &source {
            ToolCallSource::Direct | ToolCallSource::DirectPlaintextMessage => {
                call_trace::Source::Direct
            }
            ToolCallSource::CodeMode { .. } => call_trace::Source::CodeMode,
        };
        let dispatch_tool_name = call.tool_name.clone();
        let dispatch_call_id = call.call_id.clone();

        // Code-mode callbacks can resume outside the turn's local span ancestry.
        let dispatch_span = trace_span!(
            "dispatch_tool_call_with_code_mode_result",
            otel.name = %call.tool_name,
            tool_name = %call.tool_name,
            thread.id = %session.thread_id,
            call_id = call.call_id.as_str(),
            aborted = false,
        );
        let abort_dispatch_span = dispatch_span.clone();

        let mut dispatch_handle = AbortOnDropHandle::new(tokio::spawn(
            async move {
                if let Some(tool_runtime) = tool_runtime
                    && let Some(readiness) = tool_runtime.wait_until_ready(&session)
                {
                    readiness.await;
                }

                let guard = if supports_parallel {
                    Either::Left(lock.read().await)
                } else {
                    Either::Right(lock.write().await)
                };
                // Admission through the parallel-execution gate marks the end
                // of dispatch waiting and the start of handler execution.
                if let Some(execution_started_at) = execution_started_at {
                    let _ = execution_started_at.set(Instant::now());
                }

                let result = router
                    .dispatch_tool_call_with_terminal_outcome(
                        session,
                        step_context,
                        invocation_cancellation_token,
                        tracker,
                        dispatch_call,
                        source,
                        dispatch_terminal_outcome_reached,
                    )
                    .instrument(dispatch_span.clone())
                    .await;
                drop(guard);
                // The sampling loop collects results in order only after its stream ends.
                // Record readiness here, before either caller encodes or collects the result.
                // A fatal error still propagates to the caller instead of producing a tool
                // result; unlike a normal tool failure, it has no readiness event.
                if !matches!(&result, Err(FunctionCallError::Fatal(_))) {
                    call_trace::result_ready(
                        thread_id,
                        &turn.sub_id,
                        &dispatch_tool_name,
                        &dispatch_call_id,
                        trace_source,
                    );
                }
                result
            }
            .in_current_span(),
        ));

        async move {
            let _tool_call_timing_guard = tool_call_timing_guard;
            let result = tokio::select! {
                res = &mut dispatch_handle => res.map_err(Self::tool_task_join_error)?,
                _ = cancellation_token.cancelled() => {
                    if spawn_publication.as_ref().is_some_and(|publication| {
                        matches!(
                            publication.cancel(),
                            SpawnPublicationDecision::DeliveryOwned
                                | SpawnPublicationDecision::Published
                        )
                    }) {
                        // A spawned child may start its first turn before its parent-visible
                        // publication. Delivery ownership makes cancellation wait for this
                        // handler's actual published or failed outcome rather than falsely
                        // reporting an aborted parent while an invisible child can do work.
                        dispatch_handle.await.map_err(Self::tool_task_join_error)?
                    } else if terminal_outcome_reached_or_finished(
                        Some(&terminal_outcome_reached),
                        dispatch_handle.is_finished(),
                    ) {
                        dispatch_handle.await.map_err(Self::tool_task_join_error)?
                    } else {
                        let secs = started.elapsed().as_secs_f32().max(0.1);
                        abort_dispatch_span.record("aborted", true);
                        dispatch_handle.abort();
                        match dispatch_handle.await {
                            Ok(result) => return result,
                            Err(err) if err.is_cancelled() => {}
                            Err(err) => return Err(Self::tool_task_join_error(err)),
                        }
                        let response = Self::aborted_response(&call, secs);
                        call_trace::result_ready(
                            thread_id,
                            &abort_turn.sub_id,
                            &call.tool_name,
                            &call.call_id,
                            trace_source,
                        );
                        notify_tool_aborted(
                            abort_session.as_ref(),
                            abort_turn.as_ref(),
                            call.call_id.as_str(),
                            &call.tool_name,
                            abort_source,
                        )
                        .await;
                        Ok(response)
                    }
                },
            };
            result
        }
        .in_current_span()
    }
}

impl ToolCallRuntime {
    fn tool_task_join_error(err: JoinError) -> FunctionCallError {
        FunctionCallError::Fatal(format!("tool task failed to receive: {err:?}"))
    }

    fn failure_response(call: ToolCall, err: FunctionCallError) -> ResponseInputItem {
        let message = err.to_string();
        match call.payload {
            ToolPayload::ToolSearch { .. } => ResponseInputItem::ToolSearchOutput {
                call_id: call.call_id,
                status: "completed".to_string(),
                execution: "client".to_string(),
                tools: Vec::new(),
            },
            ToolPayload::Custom { .. } => ResponseInputItem::CustomToolCallOutput {
                call_id: call.call_id,
                name: None,
                output: codex_protocol::models::FunctionCallOutputPayload {
                    body: codex_protocol::models::FunctionCallOutputBody::Text(message),
                    success: Some(false),
                },
            },
            _ => ResponseInputItem::FunctionCallOutput {
                call_id: call.call_id,
                output: codex_protocol::models::FunctionCallOutputPayload {
                    body: codex_protocol::models::FunctionCallOutputBody::Text(message),
                    success: Some(false),
                },
            },
        }
    }

    fn aborted_response(call: &ToolCall, secs: f32) -> AnyToolResult {
        AnyToolResult {
            call_id: call.call_id.clone(),
            payload: call.payload.clone(),
            result: Box::new(AbortedToolOutput {
                message: Self::abort_message(call, secs),
            }),
            post_tool_use_payload: None,
        }
    }

    fn abort_message(call: &ToolCall, secs: f32) -> String {
        if call.tool_name.is_default_namespace() && call.tool_name.name == "exec_command" {
            format!("Wall time: {secs:.1} seconds\naborted by user")
        } else {
            format!("aborted by user after {secs:.1}s")
        }
    }
}

fn terminal_outcome_reached_or_finished(
    terminal_outcome_reached: Option<&Arc<AtomicBool>>,
    handle_finished: bool,
) -> bool {
    terminal_outcome_reached
        .is_some_and(|terminal_outcome_reached| terminal_outcome_reached.load(Ordering::Acquire))
        || handle_finished
}

impl ToolCallTimingGuard {
    fn capture(
        started_at: Instant,
        conversation_id: &impl std::fmt::Display,
        turn_id: &str,
        call: &ToolCall,
        source: &ToolCallSource,
    ) -> Option<Self> {
        // Code-mode calls are nested within a direct code-mode tool call whose
        // timing already includes them. Suppress nested guards so consumers do
        // not mistake overlapping events for independent tool-call latency.
        if !matches!(
            source,
            ToolCallSource::Direct | ToolCallSource::DirectPlaintextMessage
        ) || !tracing::enabled!(tracing::Level::INFO)
        {
            return None;
        }

        Some(Self {
            started_at,
            execution_started_at: Arc::new(OnceLock::new()),
            conversation_id: conversation_id.to_string(),
            turn_id: turn_id.to_string(),
            call_id: call.call_id.clone(),
            tool_name: call.tool_name.clone(),
        })
    }
}

impl Drop for ToolCallTimingGuard {
    fn drop(&mut self) {
        let completed_at = Instant::now();
        // Snapshot once so a concurrently-starting dispatch cannot make one
        // event internally inconsistent.
        let execution_started_at = self
            .execution_started_at
            .get()
            .copied()
            .filter(|execution_started_at| *execution_started_at <= completed_at);
        let duration_ms = |duration: std::time::Duration| u64::try_from(duration.as_millis()).ok();
        let total_duration_ms = duration_ms(completed_at.duration_since(self.started_at));
        let dispatch_duration_ms = execution_started_at.map_or_else(
            || total_duration_ms,
            |execution_started_at| {
                duration_ms(execution_started_at.duration_since(self.started_at))
            },
        );
        let handler_duration_ms = execution_started_at.map_or(Some(0), |execution_started_at| {
            duration_ms(completed_at.duration_since(execution_started_at))
        });

        macro_rules! log_tool_call {
            ($dispatch_duration_ms:expr, $handler_duration_ms:expr, $total_duration_ms:expr) => {
                info!(
                    event.name = "codex.tool_call",
                    trace_id = %codex_otel::current_span_trace_id().unwrap_or_default(),
                    conversation.id = %self.conversation_id,
                    turn_id = %self.turn_id,
                    tool_name = %self.tool_name,
                    call_id = %self.call_id,
                    tool_source = "direct",
                    execution_started = execution_started_at.is_some(),
                    dispatch_duration_ms = $dispatch_duration_ms,
                    handler_duration_ms = $handler_duration_ms,
                    total_duration_ms = $total_duration_ms,
                    "tool call completed"
                );
            };
        }

        match (dispatch_duration_ms, handler_duration_ms, total_duration_ms) {
            (Some(dispatch_duration_ms), Some(handler_duration_ms), Some(total_duration_ms)) => {
                log_tool_call!(dispatch_duration_ms, handler_duration_ms, total_duration_ms);
            }
            _ => {
                log_tool_call!(
                    tracing::field::Empty,
                    tracing::field::Empty,
                    tracing::field::Empty
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::time::Duration;

    use crate::StartThreadOptions;
    use crate::ThreadManager;
    use crate::session::step_context::StepContext;
    use crate::tools::context::FunctionToolOutput;
    use crate::tools::context::ToolInvocation;
    use crate::tools::handlers::ToolSearchHandlerCache;
    use crate::tools::handlers::multi_agents::SpawnAgentHandler;
    use crate::tools::registry::CoreToolRuntime;
    use crate::tools::registry::ToolExecutor;
    use crate::tools::registry::ToolRegistry;
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
    use crate::tools::router::ToolRouterParams;
=======
    use crate::tools::router::ToolRouter;
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
    use crate::turn_diff_tracker::TurnDiffTracker;
    use codex_extension_api::ToolCallOutcome;
    use codex_features::Feature;
    use codex_login::CodexAuth;
    use codex_model_provider_info::built_in_model_providers;
    use codex_protocol::AgentPath;
    use codex_protocol::models::FunctionCallOutputBody;
    use codex_protocol::models::FunctionCallOutputPayload;
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
    use codex_tools::ToolName;
=======
    use codex_protocol::openai_models::ToolMode;
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
    use pretty_assertions::assert_eq;
    use tokio::sync::Notify;
    use tokio::sync::oneshot;
    use tracing_test::internal::MockWriter;

    #[test]
    fn tool_call_timing_guard_ignores_code_mode_source() {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let call = ToolCall {
                tool_name: codex_tools::ToolName::plain("test_tool"),
                call_id: "call-1".to_string(),
                payload: ToolPayload::Function {
                    arguments: "{}".to_string(),
                },
                encrypted_function_args: None,
            };
            let direct_guard = ToolCallTimingGuard::capture(
                Instant::now(),
                &"conversation-id",
                "turn-id",
                &call,
                &ToolCallSource::Direct,
            );
            assert!(
                direct_guard.is_some(),
                "direct tool calls should create a timing guard"
            );
            drop(direct_guard);

            let code_mode_guard = ToolCallTimingGuard::capture(
                Instant::now(),
                &"conversation-id",
                "turn-id",
                &call,
                &ToolCallSource::CodeMode {
                    cell_id: "cell-1".to_string(),
                    runtime_tool_call_id: "runtime-call-1".to_string(),
                },
            );
            assert!(
                code_mode_guard.is_none(),
                "nested code-mode calls should not create overlapping timing events"
            );
        });
    }

    #[tokio::test]
    async fn cancellation_before_dispatch_admission_logs_dispatch_only_timing() -> anyhow::Result<()>
    {
        let (session, turn_context) = crate::session::tests::make_session_and_context().await;
        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context);
        let tool_name = codex_tools::ToolName::plain("test_tool");
        let handler = Arc::new(ImmediateHandler {
            tool_name: tool_name.clone(),
        }) as Arc<dyn CoreToolRuntime>;
        let step_context = StepContext::for_test(Arc::clone(&turn_context));
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([handler]),
            Vec::new(),
            ToolMode::Direct,
            BTreeMap::new(),
            /*tool_namespaces_info*/ None,
            &[],
        ));
        let step_context = step_context.with_tool_router_for_test(router);
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
        let runtime = ToolCallRuntime::new(session, step_context, tracker);
        let execution_gate = Arc::clone(&runtime.parallel_execution);
        let execution_gate_guard = execution_gate
            .try_write_owned()
            .expect("execution gate should be available before dispatch starts");
        let (release_execution_gate_tx, release_execution_gate_rx) = std::sync::mpsc::channel();
        let execution_gate_task = tokio::task::spawn_blocking(move || {
            let _execution_gate_guard = execution_gate_guard;
            release_execution_gate_rx
                .recv()
                .expect("test should release the execution gate");
        });

        let buffer: &'static std::sync::Mutex<Vec<u8>> =
            Box::leak(Box::new(std::sync::Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(MockWriter::new(buffer))
            .finish();
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);

        let cancellation_token = CancellationToken::new();
        let call = ToolCall {
            tool_name,
            call_id: "call-1".to_string(),
            payload: ToolPayload::Function {
                arguments: "{}".to_string(),
            },
            encrypted_function_args: None,
        };
        let response_task =
            tokio::spawn(runtime.handle_tool_call(call, cancellation_token.clone()));
        cancellation_token.cancel();
        tokio::time::timeout(Duration::from_secs(1), response_task)
            .await
            .expect("timed out waiting for cancelled tool response")
            .expect("cancelled tool response task should join")
            .expect("cancelled tool call should produce a response");

        let logs = String::from_utf8(
            buffer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        )?;
        let timing_events = logs
            .lines()
            .filter(|line| line.contains("event.name=\"codex.tool_call\""))
            .collect::<Vec<_>>();
        assert_eq!(
            timing_events.len(),
            1,
            "cancelled tool call should emit exactly one timing event; logs:\n{logs}"
        );
        let timing_event = timing_events[0];
        assert!(
            timing_event.contains("execution_started=false"),
            "tool cancelled before admission should not report execution started: {timing_event}"
        );
        assert!(
            timing_event.contains("handler_duration_ms=0"),
            "tool cancelled before admission should report zero handler duration: {timing_event}"
        );
        fn duration_field(timing_event: &str, field_name: &str) -> Option<u64> {
            let prefix = format!("{field_name}=");
            timing_event.split_whitespace().find_map(|field| {
                field
                    .strip_prefix(&prefix)
                    .and_then(|value| value.parse::<u64>().ok())
            })
        }

        let dispatch_duration_ms = duration_field(timing_event, "dispatch_duration_ms")
            .expect("timing event should include dispatch_duration_ms");
        let total_duration_ms = duration_field(timing_event, "total_duration_ms")
            .expect("timing event should include total_duration_ms");
        assert_eq!(
            dispatch_duration_ms, total_duration_ms,
            "tool cancelled before admission should attribute all elapsed time to dispatch: {timing_event}"
        );
        release_execution_gate_tx
            .send(())
            .expect("execution gate task should remain available");
        execution_gate_task
            .await
            .expect("execution gate task should join");

        Ok(())
    }

    fn tool_call(tool_name: &str) -> ToolCall {
        ToolCall {
            tool_name: ToolName::plain(tool_name),
            call_id: "call-1".to_string(),
            payload: ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        }
    }

    struct ImmediateHandler {
        tool_name: codex_tools::ToolName,
    }

    impl ToolExecutor<ToolInvocation> for ImmediateHandler {
        fn tool_name(&self) -> codex_tools::ToolName {
            self.tool_name.clone()
        }

        fn spec(&self) -> codex_tools::ToolSpec {
            codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
                name: self.tool_name.name.clone(),
                description: "Immediate test tool.".to_string(),
                strict: false,
                defer_loading: None,
                parameters: codex_tools::JsonSchema::default(),
                output_schema: None,
            })
        }

        fn handle<'a>(&'a self, _invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
        where
            ToolInvocation: 'a,
        {
            Box::pin(async {
                Ok(
                    Box::new(FunctionToolOutput::from_text("ok".to_string(), Some(true)))
                        as Box<dyn crate::tools::context::ToolOutput>,
                )
            })
        }
    }

    impl CoreToolRuntime for ImmediateHandler {}

    struct BlockingFinishContributor {
        records: Arc<std::sync::Mutex<Vec<ToolCallOutcome>>>,
        finish_started: std::sync::Mutex<Option<oneshot::Sender<()>>>,
        allow_finish: Arc<Notify>,
    }

    impl codex_extension_api::ToolLifecycleContributor for BlockingFinishContributor {
        fn on_tool_finish<'a>(
            &'a self,
            input: codex_extension_api::ToolFinishInput<'a>,
        ) -> codex_extension_api::ToolLifecycleFuture<'a> {
            let records = Arc::clone(&self.records);
            let allow_finish = Arc::clone(&self.allow_finish);
            let finish_started = self
                .finish_started
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            let outcome = input.outcome;
            Box::pin(async move {
                if let Some(finish_started) = finish_started {
                    let _ = finish_started.send(());
                }
                allow_finish.notified().await;
                records
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(outcome);
            })
        }
    }

    #[tokio::test]
    async fn cancellation_after_handler_finishes_preserves_completed_lifecycle()
    -> anyhow::Result<()> {
        let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
        let records = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (finish_started_tx, finish_started_rx) = oneshot::channel();
        let allow_finish = Arc::new(Notify::new());
        let mut builder =
            codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
        builder.tool_lifecycle_contributor(Arc::new(BlockingFinishContributor {
            records: Arc::clone(&records),
            finish_started: std::sync::Mutex::new(Some(finish_started_tx)),
            allow_finish: Arc::clone(&allow_finish),
        }));
        session.services.extensions = Arc::new(builder.build());

        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context);
        let tool_name = codex_tools::ToolName::plain("test_tool");
        let handler = Arc::new(ImmediateHandler {
            tool_name: tool_name.clone(),
        }) as Arc<dyn CoreToolRuntime>;
        let step_context = StepContext::for_test(Arc::clone(&turn_context));
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([handler]),
            Vec::new(),
            ToolMode::Direct,
            BTreeMap::new(),
            /*tool_namespaces_info*/ None,
            &[],
        ));
        let step_context = step_context.with_tool_router_for_test(router);
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
        let runtime = ToolCallRuntime::new(session, step_context, tracker);
        let cancellation_token = CancellationToken::new();
        let call = ToolCall {
            tool_name,
            call_id: "call-1".to_string(),
            payload: ToolPayload::Function {
                arguments: "{}".to_string(),
            },
            encrypted_function_args: None,
        };

        let response_task =
            tokio::spawn(runtime.handle_tool_call(call, cancellation_token.clone()));
        tokio::time::timeout(Duration::from_secs(1), finish_started_rx)
            .await
            .expect("timed out waiting for lifecycle notification to start")
            .expect("lifecycle notification should start");
        cancellation_token.cancel();
        tokio::time::sleep(Duration::from_millis(10)).await;
        allow_finish.notify_waiters();

        let response = tokio::time::timeout(Duration::from_secs(1), response_task)
            .await
            .expect("timed out waiting for tool response")
            .expect("tool response task should join")?;
        let expected_response = ResponseInputItem::FunctionCallOutput {
            call_id: "call-1".to_string(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("ok".to_string()),
                success: Some(true),
            },
        };
        assert_eq!(
            ResponseItemEnvelope::new(expected_response.into()),
            response
        );

        let actual = records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect::<Vec<_>>();
        assert_eq!(vec![ToolCallOutcome::Completed { success: true }], actual);

        Ok(())
    }
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e

    #[test]
    fn abort_message_uses_shell_style_for_shell_like_tools() {
        let call = tool_call("shell_command");

        assert_eq!(
            ToolCallRuntime::abort_message(&call, /*secs*/ 1.25),
            "Wall time: 1.2 seconds\naborted by user"
        );
    }

    #[test]
    fn abort_message_uses_generic_style_for_other_tools() {
        let call = tool_call("spawn_agent");

        assert_eq!(
            ToolCallRuntime::abort_message(&call, /*secs*/ 1.25),
            "aborted by user after 1.2s"
        );
    }

    #[tokio::test]
    async fn configured_v2_spawn_cancellation_after_new_thread_reconciles_before_runtime_abort()
    -> anyhow::Result<()> {
        let (mut session, mut turn_context) =
            crate::session::tests::make_session_and_context().await;
        let mut config = (*turn_context.config).clone();
        config
            .features
            .enable(Feature::MultiAgentV2)
            .expect("test config should enable MultiAgentV2");
        config.multi_agent_v2.hide_spawn_agent_metadata = false;
        config.multi_agent_v2.tool_namespace = Some("profile_agents".to_string());
        turn_context.multi_agent_version = config.multi_agent_version_from_features();
        turn_context.config = Arc::new(config.clone());

        let manager = ThreadManager::with_models_provider_for_tests(
            CodexAuth::from_api_key("dummy"),
            built_in_model_providers(/*openai_base_url*/ None)["openai"].clone(),
        );
        let root = manager
            .start_thread(StartThreadOptions::new(config))
            .await
            .expect("root thread should start");
        session.services.agent_control = manager.agent_control();
        session.thread_id = root.thread_id;
        let (child_created, resume_spawn) = session
            .services
            .agent_control
            .pause_spawn_after_new_thread_for_test();
        let mut created_threads = manager.subscribe_thread_created();

        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context);
        let step_context = StepContext::for_test(Arc::clone(&turn_context));
        let tool_search_handler_cache = ToolSearchHandlerCache::default();
        let router = Arc::new(ToolRouter::from_context(
            step_context.turn.as_ref(),
            &step_context.environments,
            step_context.mcp.as_ref(),
            ToolRouterParams {
                tool_runtimes: Vec::new(),
                tool_suggest_candidates: None,
                extension_tool_executors: Vec::new(),
                wait_for_environment_tool_config: None,
                dynamic_tools: &[],
            },
            &tool_search_handler_cache,
        ));
        let call = ToolCall {
            tool_name: ToolName::namespaced("profile_agents", "spawn_agent"),
            call_id: "call-1".to_string(),
            payload: ToolPayload::Function {
                arguments: serde_json::json!({
                    "message": "must not be delivered",
                    "task_name": "cancelled_worker",
                    "fork_turns": "none"
                })
                .to_string(),
            },
        };
        assert!(
            router.tool_waits_for_runtime_cancellation(&call),
            "the production configured-V2 namespace wrapper must retain the handler's cancellation contract"
        );
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
        let runtime = ToolCallRuntime::new(router, Arc::clone(&session), step_context, tracker);

        let cancellation_token = CancellationToken::new();
        let mut response_task =
            tokio::spawn(runtime.handle_tool_call(call, cancellation_token.clone()));

        let child_thread_id = tokio::time::timeout(Duration::from_secs(5), child_created)
            .await
            .expect("configured V2 runtime spawn should reach NewThread")
            .expect("post-NewThread hook should identify the child");
        assert_eq!(
            session
                .services
                .agent_control
                .tool_spawn_publication_decision_for_test(session.thread_id, "call-1"),
            SpawnPublicationDecision::Pending,
            "the production configured V2 router must register its publication guard before delivery"
        );
        assert!(
            session
                .services
                .agent_control
                .get_agent_metadata(child_thread_id)
                .is_none(),
            "the provisional configured-V2 child must not enter the public registry"
        );
        assert!(
            created_threads.try_recv().is_err(),
            "the provisional configured-V2 child must not notify parent-visible creation"
        );
        assert!(
            manager.captured_ops().is_empty(),
            "cancellation before delivery must not submit initial child work"
        );
        cancellation_token.cancel();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if session
                    .services
                    .agent_control
                    .tool_spawn_publication_decision_for_test(session.thread_id, "call-1")
                    == SpawnPublicationDecision::CancellationOwned
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("runtime cancellation should win the configured V2 publication guard");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut response_task)
                .await
                .is_err(),
            "the configured-V2 wrapper must not drop the handler before cancellation-owned cleanup"
        );
        assert!(
            manager.get_thread(child_thread_id).await.is_ok(),
            "the paused handler must retain the exact private child until it performs cleanup"
        );
        resume_spawn.notify_one();

        let response = tokio::time::timeout(Duration::from_secs(5), response_task)
            .await
            .expect("timed out waiting for configured V2 cancellation")
            .expect("configured V2 runtime task should join")?;
        let ResponseInputItem::FunctionCallOutput { output, .. } = response else {
            anyhow::bail!("cancelled V2 spawn should return a function output");
        };
        let FunctionCallOutputBody::Text(message) = output.body else {
            anyhow::bail!("cancelled V2 spawn output should be text");
        };
        assert!(
            message.contains("aborted by user"),
            "configured V2 cancellation must return the runtime abort result: {message}"
        );
        assert!(
            session
                .services
                .agent_control
                .get_agent_metadata(child_thread_id)
                .is_none(),
            "a cancellation-owned configured-V2 child must remain absent from the registry"
        );
        assert!(
            manager.get_thread(child_thread_id).await.is_err(),
            "runtime abort must return only after the exact private child is removed"
        );
        assert!(
            created_threads.try_recv().is_err(),
            "cancellation-owned cleanup must not emit a thread-created notification"
        );
        assert!(
            manager.captured_ops().is_empty(),
            "cancellation-owned cleanup must preserve no-initial-work semantics"
        );
        assert_eq!(
            session.services.agent_control.v2_resident_count_for_test(),
            0,
            "private configured-V2 cancellation must reconcile residency before abort returns"
        );
        let cancelled_path = AgentPath::root()
            .join("cancelled_worker")
            .expect("configured task name should be a valid agent path");
        assert!(
            session
                .services
                .agent_control
                .spawn_capacity_and_path_are_available_for_test(
                    /*max_threads*/ 1,
                    &cancelled_path
                ),
            "private configured-V2 cancellation must release capacity and the requested path"
        );

        Ok(())
    }

    #[tokio::test]
    async fn v1_spawn_cancellation_after_initial_delivery_returns_published_result()
    -> anyhow::Result<()> {
        let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
        let config = (*turn_context.config).clone();

        let manager = ThreadManager::with_models_provider_for_tests(
            CodexAuth::from_api_key("dummy"),
            built_in_model_providers(/*openai_base_url*/ None)["openai"].clone(),
        );
        let root = manager
            .start_thread(StartThreadOptions::new(config))
            .await
            .expect("root thread should start");
        session.services.agent_control = manager.agent_control();
        session.thread_id = root.thread_id;
        let (initial_delivery_finished, resume_spawn) = session
            .services
            .agent_control
            .pause_spawn_after_initial_delivery_for_test();
        let mut created_threads = manager.subscribe_thread_created();

        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context);
        let handler = Arc::new(SpawnAgentHandler::default()) as Arc<dyn CoreToolRuntime>;
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([handler]),
            Vec::new(),
        ));
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
        let runtime = ToolCallRuntime::new(
            router,
            Arc::clone(&session),
            StepContext::for_test(Arc::clone(&turn_context)),
            tracker,
        );
        let cancellation_token = CancellationToken::new();
        let mut response_task = tokio::spawn(
            runtime.handle_tool_call(
                ToolCall {
                    tool_name: ToolName::namespaced(MULTI_AGENT_V1_NAMESPACE, "spawn_agent"),
                    call_id: "call-1".to_string(),
                    payload: ToolPayload::Function {
                        arguments: serde_json::json!({
                            "message": "delivery owns this child"
                        })
                        .to_string(),
                    },
                },
                cancellation_token.clone(),
            ),
        );

        let child_thread_id =
            tokio::time::timeout(Duration::from_secs(5), initial_delivery_finished)
                .await
                .expect("V1 runtime spawn should finish initial delivery")
                .expect("post-initial-delivery hook should identify the child");
        assert!(
            session
                .services
                .agent_control
                .get_agent_metadata(child_thread_id)
                .is_none(),
            "initial V1 delivery must remain private until publication"
        );
        assert!(
            created_threads.try_recv().is_err(),
            "initial V1 delivery must not notify a parent-visible child before publication"
        );
        assert_eq!(
            manager
                .captured_ops()
                .iter()
                .filter(|(thread_id, _)| *thread_id == child_thread_id)
                .count(),
            1,
            "the V1 child must receive exactly one initial delivery before publication"
        );
        cancellation_token.cancel();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if session
                    .services
                    .agent_control
                    .tool_spawn_publication_decision_for_test(session.thread_id, "call-1")
                    == SpawnPublicationDecision::DeliveryOwned
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancellation after V1 delivery must observe delivery ownership");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut response_task)
                .await
                .is_err(),
            "the V1 runtime must wait for the delivery-owning handler rather than return a false abort"
        );
        resume_spawn.notify_one();

        let response = tokio::time::timeout(Duration::from_secs(5), response_task)
            .await
            .expect("delivery-owning V1 runtime spawn should return")
            .expect("runtime task should join")?;
        let ResponseInputItem::FunctionCallOutput { output, .. } = response else {
            anyhow::bail!("V1 spawn should return a function output");
        };
        assert_eq!(output.success, Some(true));
        let FunctionCallOutputBody::Text(message) = output.body else {
            anyhow::bail!("V1 spawn output should be text");
        };
        assert!(
            !message.contains("aborted by user"),
            "delivery ownership must preserve the V1 spawn result instead of a false abort: {message}"
        );
        assert!(
            session
                .services
                .agent_control
                .get_agent_metadata(child_thread_id)
                .is_some(),
            "a delivery-owned V1 child must publish rather than remain invisibly active"
        );
        assert!(
            manager.get_thread(child_thread_id).await.is_ok(),
            "a delivery-owned V1 child remains manager-owned after its published result"
        );
        assert_eq!(
            created_threads
                .recv()
                .await
                .expect("published V1 child should notify once"),
            child_thread_id
        );

        Ok(())
    }

    #[tokio::test]
    async fn configured_v2_spawn_cancellation_after_initial_delivery_returns_published_result()
    -> anyhow::Result<()> {
        let (mut session, mut turn_context) =
            crate::session::tests::make_session_and_context().await;
        let mut config = (*turn_context.config).clone();
        config
            .features
            .enable(Feature::MultiAgentV2)
            .expect("test config should enable MultiAgentV2");
        config.multi_agent_v2.hide_spawn_agent_metadata = false;
        config.multi_agent_v2.tool_namespace = Some("profile_agents".to_string());
        turn_context.multi_agent_version = config.multi_agent_version_from_features();
        turn_context.config = Arc::new(config.clone());

        let manager = ThreadManager::with_models_provider_for_tests(
            CodexAuth::from_api_key("dummy"),
            built_in_model_providers(/*openai_base_url*/ None)["openai"].clone(),
        );
        let root = manager
            .start_thread(StartThreadOptions::new(config))
            .await
            .expect("root thread should start");
        session.services.agent_control = manager.agent_control();
        session.thread_id = root.thread_id;
        let (initial_delivery_finished, resume_spawn) = session
            .services
            .agent_control
            .pause_spawn_after_initial_delivery_for_test();
        let mut created_threads = manager.subscribe_thread_created();

        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context);
        let step_context = StepContext::for_test(Arc::clone(&turn_context));
        let tool_search_handler_cache = ToolSearchHandlerCache::default();
        let router = Arc::new(ToolRouter::from_context(
            step_context.turn.as_ref(),
            &step_context.environments,
            step_context.mcp.as_ref(),
            ToolRouterParams {
                tool_runtimes: Vec::new(),
                tool_suggest_candidates: None,
                extension_tool_executors: Vec::new(),
                wait_for_environment_tool_config: None,
                dynamic_tools: &[],
            },
            &tool_search_handler_cache,
        ));
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
        let runtime = ToolCallRuntime::new(router, Arc::clone(&session), step_context, tracker);
        let cancellation_token = CancellationToken::new();
        let mut response_task = tokio::spawn(
            runtime.handle_tool_call(
                ToolCall {
                    tool_name: ToolName::namespaced("profile_agents", "spawn_agent"),
                    call_id: "call-1".to_string(),
                    payload: ToolPayload::Function {
                        arguments: serde_json::json!({
                            "message": "delivery owns this child",
                            "task_name": "delivery_owned_worker",
                            "fork_turns": "none"
                        })
                        .to_string(),
                    },
                },
                cancellation_token.clone(),
            ),
        );

        let child_thread_id =
            tokio::time::timeout(Duration::from_secs(5), initial_delivery_finished)
                .await
                .expect("configured V2 runtime spawn should finish initial delivery")
                .expect("post-initial-delivery hook should identify the child");
        assert!(
            session
                .services
                .agent_control
                .get_agent_metadata(child_thread_id)
                .is_none(),
            "initial configured-V2 delivery must remain private until publication"
        );
        assert!(
            created_threads.try_recv().is_err(),
            "initial configured-V2 delivery must not notify parent-visible creation"
        );
        let initial_delivery_count = manager
            .captured_ops()
            .iter()
            .filter(|(thread_id, _)| *thread_id == child_thread_id)
            .count();
        assert_eq!(
            initial_delivery_count, 1,
            "the configured-V2 child must receive exactly one initial delivery before publication"
        );
        cancellation_token.cancel();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if session
                    .services
                    .agent_control
                    .tool_spawn_publication_decision_for_test(session.thread_id, "call-1")
                    == SpawnPublicationDecision::DeliveryOwned
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancellation after configured-V2 delivery must observe delivery ownership");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut response_task)
                .await
                .is_err(),
            "the configured-V2 runtime must wait for delivery rather than return a false abort"
        );
        resume_spawn.notify_one();

        let response = tokio::time::timeout(Duration::from_secs(5), response_task)
            .await
            .expect("delivery-owning configured-V2 runtime spawn should return")
            .expect("configured-V2 runtime task should join")?;
        let ResponseInputItem::FunctionCallOutput { output, .. } = response else {
            anyhow::bail!("configured-V2 spawn should return a function output");
        };
        assert_eq!(output.success, Some(true));
        let FunctionCallOutputBody::Text(message) = output.body else {
            anyhow::bail!("configured-V2 spawn output should be text");
        };
        assert!(
            !message.contains("aborted by user"),
            "delivery ownership must preserve the configured-V2 result instead of a false abort: {message}"
        );
        assert!(
            session
                .services
                .agent_control
                .get_agent_metadata(child_thread_id)
                .is_some(),
            "a delivery-owned configured-V2 child must publish rather than remain invisible"
        );
        assert_eq!(
            created_threads
                .recv()
                .await
                .expect("published configured-V2 child should notify once"),
            child_thread_id
        );

        Ok(())
    }

    #[test]
    fn spawn_publication_recognizes_v1_and_configured_v2_namespaces() {
        let mut call = tool_call("spawn_agent");
        assert!(is_multi_agent_spawn_call(&call, Some("agents")));

        call.tool_name = codex_tools::ToolName::namespaced(MULTI_AGENT_V1_NAMESPACE, "spawn_agent");
        assert!(is_multi_agent_spawn_call(&call, Some("agents")));

        call.tool_name = codex_tools::ToolName::namespaced("profile_agents", "spawn_agent");
        assert!(is_multi_agent_spawn_call(&call, Some("profile_agents")));
        assert!(!is_multi_agent_spawn_call(&call, Some("agents")));
    }

    #[test]
    fn terminal_outcome_helper_defaults_to_handle_state_without_capability_flag() {
        assert!(!terminal_outcome_reached_or_finished(
            /*terminal_outcome_reached*/ None, /*handle_finished*/ false
        ));
        assert!(terminal_outcome_reached_or_finished(
            /*terminal_outcome_reached*/ None, /*handle_finished*/ true
        ));
    }

    #[test]
    fn terminal_outcome_helper_honors_capability_flag() {
        let reached = Arc::new(AtomicBool::new(false));
        assert!(!terminal_outcome_reached_or_finished(
            Some(&reached),
            /*handle_finished*/ false
        ));

        reached.store(true, Ordering::Release);
        assert!(terminal_outcome_reached_or_finished(
            Some(&reached),
            /*handle_finished*/ false
        ));
    }

    #[tokio::test]
    async fn cancellation_waiting_for_runtime_cleanup_emits_only_aborted_lifecycle()
    -> anyhow::Result<()> {
        let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
        let records = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut builder =
            codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
        builder.tool_lifecycle_contributor(Arc::new(FinishRecorder {
            records: Arc::clone(&records),
        }));
        session.services.extensions = Arc::new(builder.build());

        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context);
        let tool_name = codex_tools::ToolName::plain("cleanup_tool");
        let (started_tx, started_rx) = oneshot::channel();
        let (cleanup_started_tx, cleanup_started_rx) = oneshot::channel();
        let allow_cleanup = Arc::new(Notify::new());
        let handler = Arc::new(CancellationCleanupHandler {
            tool_name: tool_name.clone(),
            started: std::sync::Mutex::new(Some(started_tx)),
            cleanup_started: std::sync::Mutex::new(Some(cleanup_started_tx)),
            allow_cleanup: Arc::clone(&allow_cleanup),
        }) as Arc<dyn CoreToolRuntime>;
        let step_context = StepContext::for_test(Arc::clone(&turn_context));
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([handler]),
            Vec::new(),
        ));
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
        let runtime = ToolCallRuntime::new(router, session, step_context, tracker);
        let cancellation_token = CancellationToken::new();
        let call = ToolCall {
            tool_name,
            call_id: "call-1".to_string(),
            payload: ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        };

        let response_task =
            tokio::spawn(runtime.handle_tool_call(call, cancellation_token.clone()));
        started_rx.await.expect("handler should start");
        cancellation_token.cancel();
        cleanup_started_rx
            .await
            .expect("handler should start cleanup");
        tokio::time::sleep(Duration::from_millis(10)).await;
        allow_cleanup.notify_one();

        let response = tokio::time::timeout(Duration::from_secs(1), response_task)
            .await
            .expect("timed out waiting for tool response")
            .expect("tool response task should join")?;
        let ResponseInputItem::FunctionCallOutput { output, .. } = response else {
            anyhow::bail!("cancelled tool should return function output");
        };
        let FunctionCallOutputBody::Text(text) = output.body else {
            anyhow::bail!("cancelled tool output should be text");
        };
        assert!(text.contains("aborted by user"));

        let actual = records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect::<Vec<_>>();
        assert_eq!(vec![ToolCallOutcome::Aborted], actual);

        Ok(())
    }
=======
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
}
