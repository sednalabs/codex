use super::*;
use crate::session::InputQueueActivity;
use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
use crate::tools::handlers::multi_agents_spec::create_wait_agent_tool_v2;
use codex_protocol::AgentPath;
use codex_protocol::items::CollabAgentWaitInfo;
use codex_protocol::items::CollabAgentWaitOutcome;
use codex_tools::ToolSpec;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::Instant;
use tokio::time::timeout_at;

#[derive(Default)]
pub(crate) struct Handler {
    options: WaitAgentTimeoutOptions,
}

impl Handler {
    pub(crate) fn new(options: WaitAgentTimeoutOptions) -> Self {
        Self { options }
    }
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("wait_agent")
    }

    fn spec(&self) -> ToolSpec {
        create_wait_agent_tool_v2(self.options)
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl Handler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            payload,
            call_id,
            ..
        } = invocation;
        let arguments = function_arguments(payload)?;
        let args: WaitArgs = parse_arguments(&arguments)?;
        let min_timeout_ms = turn.config.multi_agent_v2.min_wait_timeout_ms;
        let max_timeout_ms = turn.config.multi_agent_v2.max_wait_timeout_ms;
        let default_timeout_ms = turn.config.multi_agent_v2.default_wait_timeout_ms;
        let requested_timeout_ms = args.timeout_ms;
        let timeout_ms = match requested_timeout_ms {
            Some(ms) if ms > max_timeout_ms => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "timeout_ms must be at most {max_timeout_ms}"
                )));
            }
            Some(ms) => ms.max(min_timeout_ms),
            None => default_timeout_ms,
        };

        let turn_state = session
            .input_queue
            .turn_state_for_sub_id(&session.active_turn, &turn.sub_id)
            .await;
        let (mut activity_rx, pending_activity) = session
            .input_queue
            .subscribe_activity(turn_state.as_deref())
            .await;

        session
            .emit_turn_item_started(
                &turn,
                &TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id.clone(),
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::InProgress,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: Vec::new(),
                    receiver_agents: Vec::new(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: Default::default(),
                    wait_info: None,
                }),
            )
            .await;

        let wait_started = Instant::now();
        let deadline = wait_started + Duration::from_millis(timeout_ms as u64);
        let outcome = wait_for_activity(&mut activity_rx, pending_activity, deadline).await;
        let agent_paths = if outcome == WaitOutcome::MailboxActivity {
            session
                .input_queue
                .pending_mailbox_agent_paths(turn_state.as_deref())
                .await
        } else {
            None
        };
        let wait_info = outcome.to_info(agent_paths);
        // A completed wait may wake for a message, user input, or its timeout.
        // Dropped waits do not have an observed outcome and are not included.
        turn.session_telemetry.record_duration(
            "codex.multi_agent.wait.duration_ms",
            wait_started.elapsed(),
            &[(
                "outcome",
                match outcome {
                    WaitOutcome::MailboxActivity => "mailbox",
                    WaitOutcome::Steered => "steered",
                    WaitOutcome::TimedOut => "timed_out",
                },
            )],
        );
        let result =
            WaitAgentResult::from_outcome(wait_info.clone(), requested_timeout_ms, timeout_ms);

        session
            .emit_turn_item_completed(
                &turn,
                TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id,
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::Completed,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: Vec::new(),
                    receiver_agents: Vec::new(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: HashMap::new(),
                    wait_info: Some(wait_info),
                }),
            )
            .await;

        Ok(boxed_tool_output(result))
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    timeout_ms: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct WaitAgentResult {
    pub(crate) message: String,
    pub(crate) timed_out: bool,
    pub(crate) wake: CollabAgentWaitInfo,
}

impl WaitAgentResult {
    fn from_outcome(
        wake: CollabAgentWaitInfo,
        requested_timeout_ms: Option<i64>,
        timeout_ms: i64,
    ) -> Self {
        let message = match wake.outcome {
            CollabAgentWaitOutcome::MailboxActivity => "Wait completed after mailbox activity.",
            CollabAgentWaitOutcome::SteeredInput => "Wait interrupted by new input.",
            CollabAgentWaitOutcome::TimedOut => "Wait timed out.",
        };
        let message = match requested_timeout_ms {
            Some(requested_timeout_ms) if requested_timeout_ms < timeout_ms => format!(
                "{message}\n\nRequested timeout of {requested_timeout_ms}ms was clamped to the minimum of {timeout_ms}ms."
            ),
            Some(_) | None => message.to_string(),
        };
        Self {
            message,
            timed_out: wake.outcome == CollabAgentWaitOutcome::TimedOut,
            wake,
        }
    }
}

impl ToolOutput for WaitAgentResult {
    fn log_output(&self) -> String {
        tool_output_json_text(self, "wait_agent")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, /*success*/ None, "wait_agent")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "wait_agent")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WaitOutcome {
    MailboxActivity,
    Steered,
    TimedOut,
}

impl WaitOutcome {
    fn to_info(self, agent_paths: Option<Vec<AgentPath>>) -> CollabAgentWaitInfo {
        let outcome = match self {
            Self::MailboxActivity => CollabAgentWaitOutcome::MailboxActivity,
            Self::Steered => CollabAgentWaitOutcome::SteeredInput,
            Self::TimedOut => CollabAgentWaitOutcome::TimedOut,
        };
        CollabAgentWaitInfo {
            outcome,
            agent_paths: if outcome == CollabAgentWaitOutcome::MailboxActivity {
                agent_paths
            } else {
                None
            },
        }
    }
}

async fn wait_for_activity(
    activity_rx: &mut tokio::sync::watch::Receiver<InputQueueActivity>,
    pending_activity: Option<InputQueueActivity>,
    deadline: Instant,
) -> WaitOutcome {
    if let Some(activity) = pending_activity {
        return match activity {
            InputQueueActivity::Mailbox => WaitOutcome::MailboxActivity,
            InputQueueActivity::Steer => WaitOutcome::Steered,
        };
    }
    match timeout_at(deadline, activity_rx.changed()).await {
        Ok(Ok(())) => match *activity_rx.borrow_and_update() {
            InputQueueActivity::Mailbox => WaitOutcome::MailboxActivity,
            InputQueueActivity::Steer => WaitOutcome::Steered,
        },
        Ok(Err(_)) | Err(_) => WaitOutcome::TimedOut,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tokio::sync::watch;

    #[tokio::test]
    async fn wait_reports_mailbox_steer_and_timeout_outcomes_separately() {
        let (mailbox_tx, mut mailbox_rx) = watch::channel(InputQueueActivity::Steer);
        mailbox_tx.send_replace(InputQueueActivity::Mailbox);
        assert_eq!(
            wait_for_activity(
                &mut mailbox_rx,
                /*pending_activity*/ None,
                Instant::now() + Duration::from_secs(1),
            )
            .await,
            WaitOutcome::MailboxActivity
        );

        let (steer_tx, mut steer_rx) = watch::channel(InputQueueActivity::Mailbox);
        steer_tx.send_replace(InputQueueActivity::Steer);
        assert_eq!(
            wait_for_activity(
                &mut steer_rx,
                /*pending_activity*/ None,
                Instant::now() + Duration::from_secs(1),
            )
            .await,
            WaitOutcome::Steered
        );

        let (_timeout_tx, mut timeout_rx) = watch::channel(InputQueueActivity::Mailbox);
        assert_eq!(
            wait_for_activity(
                &mut timeout_rx,
                /*pending_activity*/ None,
                Instant::now(),
            )
            .await,
            WaitOutcome::TimedOut
        );
    }

    #[test]
    fn wake_info_only_attributes_mailbox_activity() {
        let paths = vec![AgentPath::root().join("worker").expect("worker path")];
        assert_eq!(
            WaitOutcome::MailboxActivity.to_info(Some(paths.clone())),
            CollabAgentWaitInfo {
                outcome: CollabAgentWaitOutcome::MailboxActivity,
                agent_paths: Some(paths),
            }
        );
        assert_eq!(
            WaitOutcome::Steered.to_info(Some(vec![AgentPath::root()])),
            CollabAgentWaitInfo {
                outcome: CollabAgentWaitOutcome::SteeredInput,
                agent_paths: None,
            }
        );
        assert_eq!(
            WaitOutcome::TimedOut.to_info(Some(vec![AgentPath::root()])),
            CollabAgentWaitInfo {
                outcome: CollabAgentWaitOutcome::TimedOut,
                agent_paths: None,
            }
        );
    }
}
