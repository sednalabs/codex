use std::sync::Arc;

use crate::client::ModelClientSession;
use tokio_util::sync::CancellationToken;

use crate::session::TurnInput;
use crate::session::turn::run_hooks_and_record_inputs;
use crate::session::turn::run_turn;
use crate::session::turn_context::TurnContext;
use crate::session_startup_prewarm::SessionStartupPrewarmResolution;
use crate::state::TaskIdentity;
use crate::state::TaskKind;
use crate::state::TurnState;
use codex_extension_api::OwnerContinuationDeferred;
use codex_extension_api::OwnerContinuationPending;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnStartedEvent;
use tracing::Instrument;
use tracing::trace_span;

use super::SessionTask;
use super::SessionTaskContext;
use super::SessionTaskContinuationResult;
use super::SessionTaskResult;
use super::TaskContinuationContext;

#[derive(Default)]
pub(crate) struct RegularTask {
    client_session: tokio::sync::Mutex<Option<ModelClientSession>>,
}

impl RegularTask {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    async fn run_turn_local_continuation(
        &self,
        sess: Arc<crate::session::session::Session>,
        ctx: Arc<TurnContext>,
        turn_extension_data: Arc<codex_extension_api::ExtensionData>,
        task_identity: TaskIdentity,
        turn_state: Arc<tokio::sync::Mutex<TurnState>>,
        input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> SessionTaskContinuationResult {
        if !sess
            .requeue_turn_local_continuation_input(task_identity, &ctx, &turn_state, input)
            .await
        {
            return SessionTaskContinuationResult::retained(Ok(None));
        }
        let result = self
            .run_turn_loop(
                sess,
                ctx,
                turn_extension_data,
                Vec::new(),
                cancellation_token,
                None,
                Some(TaskContinuationContext {
                    task_identity,
                    turn_state: Arc::clone(&turn_state),
                }),
            )
            .await;
        if turn_state
            .lock()
            .await
            .turn_local_continuation_input_was_consumed()
        {
            SessionTaskContinuationResult::consumed(result)
        } else {
            SessionTaskContinuationResult::retained(result)
        }
    }

    async fn run_turn_loop(
        &self,
        sess: Arc<crate::session::session::Session>,
        ctx: Arc<TurnContext>,
        turn_extension_data: Arc<codex_extension_api::ExtensionData>,
        input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
        prewarmed_client_session: Option<ModelClientSession>,
        continuation: Option<TaskContinuationContext>,
    ) -> SessionTaskResult {
        let run_turn_span = trace_span!("run_turn");
        let mut next_input = input;
        let mut client_session = self.client_session.lock().await;
        if client_session.is_none() {
            *client_session = Some(
                prewarmed_client_session
                    .unwrap_or_else(|| sess.services.model_client.new_session()),
            );
        }
        let client_session = client_session
            .as_mut()
            .expect("regular task client session initialized");
        loop {
            let last_agent_message = run_turn(
                Arc::clone(&sess),
                Arc::clone(&ctx),
                Arc::clone(&turn_extension_data),
                next_input,
                client_session,
                cancellation_token.child_token(),
                continuation.as_ref(),
            )
            .instrument(run_turn_span.clone())
            .await?;
            if let Some(turn_state) = continuation
                .as_ref()
                .map(|continuation| &continuation.turn_state)
                && turn_state
                    .lock()
                    .await
                    .turn_local_continuation_input_was_requeued()
            {
                return Ok(last_agent_message);
            }
            if ctx.terminal_error.lock().await.is_some() {
                return Ok(last_agent_message);
            }
            if ctx
                .extension_data
                .get::<OwnerContinuationDeferred>()
                .is_some()
                || ctx
                    .extension_data
                    .get::<OwnerContinuationPending>()
                    .is_some()
            {
                // Keep queued steer/input work in custody, but never let the same task make a
                // second provider request after a dormant or exhausted admission decision.
                return Ok(last_agent_message);
            }
            if let Some(continuation) = continuation.as_ref() {
                match sess
                    .input_queue
                    .has_pending_input_for_continuation(
                        &sess.active_turn,
                        continuation,
                        &cancellation_token,
                    )
                    .await
                {
                    Some(true) => {}
                    Some(false) | None => return Ok(last_agent_message),
                }
            } else if !sess.input_queue.has_pending_input(&sess.active_turn).await {
                return Ok(last_agent_message);
            }
            next_input = Vec::new();
        }
    }
}

impl SessionTask for RegularTask {
    fn rejected_initial_input_disposition(&self) -> super::RejectedInitialInputDisposition {
        super::RejectedInitialInputDisposition::RecordAsRegularTurn
    }

    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.turn"
    }

    fn supports_turn_local_continuation(&self) -> bool {
        true
    }

    fn run_pending_input_continuation(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        task_identity: TaskIdentity,
        turn_state: Arc<tokio::sync::Mutex<TurnState>>,
        input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> impl std::future::Future<Output = SessionTaskContinuationResult> + Send {
        async move {
            self.run_turn_local_continuation(
                session.clone_session(),
                ctx,
                session.turn_extension_data(),
                task_identity,
                turn_state,
                input,
                cancellation_token,
            )
            .await
        }
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        let sess = session.clone_session();
        let turn_extension_data = session.turn_extension_data();
        ctx.reset_provider_usage().await;
        // Regular turns emit `TurnStarted` inline so first-turn lifecycle does
        // not wait on startup prewarm resolution.
        let prewarmed_client_session = async {
            let event = EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: ctx.sub_id.clone(),
                trace_id: ctx.trace_id.clone(),
                started_at: ctx.turn_timing_state.started_at_unix_secs().await,
                model_context_window: ctx.model_context_window(),
                collaboration_mode_kind: ctx.mode,
            });
            sess.send_event(ctx.as_ref(), event).await;
            sess.set_server_reasoning_included(/*included*/ false).await;
            sess.consume_startup_prewarm_for_regular_turn(&cancellation_token)
                .await
        }
        .instrument(trace_span!("regular_task.prepare_run_turn"))
        .await;
        let prewarmed_client_session = match prewarmed_client_session {
            SessionStartupPrewarmResolution::Cancelled => {
                run_hooks_and_record_inputs(&sess, &ctx, &input).await;
                return Ok(None);
            }
            SessionStartupPrewarmResolution::Unavailable { .. } => None,
            SessionStartupPrewarmResolution::Ready(prewarmed_client_session) => {
                Some(*prewarmed_client_session)
            }
        };
        self.run_turn_loop(
            sess,
            ctx,
            turn_extension_data,
            input,
            cancellation_token,
            prewarmed_client_session,
            None,
        )
        .await
    }
}
