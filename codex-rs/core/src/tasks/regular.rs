use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::turn::McpStartupRequirements;
use crate::session::turn::run_hooks_and_record_inputs;
use crate::session::turn::run_turn;
use crate::session::turn_context::TurnContext;
use crate::session_startup_prewarm::SessionStartupPrewarmResolution;
use crate::state::TaskKind;
use codex_thread_store::PersistContext;
use tracing::Instrument;
use tracing::trace_span;

use super::SessionTask;
use super::SessionTaskResult;

#[derive(Default)]
pub(crate) struct RegularTask;

impl RegularTask {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl SessionTask for RegularTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.turn"
    }

    async fn run(
        self: Arc<Self>,
        sess: Arc<Session>,
        ctx: Arc<TurnContext>,
        input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        let run_turn_span = trace_span!("run_turn");
        ctx.reset_provider_usage().await;
        // Regular turns emit `TurnStarted` inline so first-turn lifecycle does
        // not wait on startup prewarm resolution.
        let prewarmed_client_session = async {
            sess.emit_turn_started(&ctx).await;
            sess.set_server_reasoning_included(/*included*/ false).await;
            sess.consume_startup_prewarm_for_regular_turn(&cancellation_token)
                .await
        }
        .instrument(trace_span!("regular_task.prepare_run_turn"))
        .await;
        let prewarmed_client_session = match prewarmed_client_session {
            SessionStartupPrewarmResolution::Cancelled => {
                run_hooks_and_record_inputs(
                    &sess,
                    &ctx,
                    &ctx.capture_current_model_info(),
                    &input,
                    PersistContext::Standard,
                )
                .await;
                return Ok(None);
            }
            SessionStartupPrewarmResolution::Unavailable { .. } => None,
            SessionStartupPrewarmResolution::Ready(prewarmed_client_session) => {
                Some(*prewarmed_client_session)
            }
        };
        let mut next_input = input;
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
        let provider_authority = sess
            .services
            .automatic_turn_provider_authority(&ctx.sub_id)
            .await;
        let mut client_session = match prewarmed_client_session {
            Some(mut client_session) => {
                client_session.set_expected_authority(provider_authority);
                client_session
            }
            None => sess
                .services
                .model_client
                .new_session_with_authority(provider_authority),
        };
=======
        let mut prewarmed_client_session = prewarmed_client_session;
        let mut mcp_startup_requirements = McpStartupRequirements::default();
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
        loop {
            let last_agent_message = run_turn(
                Arc::clone(&sess),
                Arc::clone(&ctx),
                next_input,
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
                &mut client_session,
=======
                &mut mcp_startup_requirements,
                prewarmed_client_session.take(),
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
                cancellation_token.child_token(),
            )
            .instrument(run_turn_span.clone())
            .await?;
            // Terminal errors are already reported. Let task completion preserve pending
            // input instead of restarting the failed turn for that same input.
            if ctx.terminal_error.lock().await.is_some() {
                return Ok(last_agent_message);
            }
            if !sess.input_queue.has_pending_input(&sess.active_turn).await {
                return Ok(last_agent_message);
            }
            next_input = Vec::new();
        }
    }
}
