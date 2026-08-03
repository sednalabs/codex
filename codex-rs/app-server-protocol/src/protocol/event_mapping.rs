use crate::protocol::common::ServerNotification;
use crate::protocol::item_builders::build_command_execution_begin_item;
use crate::protocol::item_builders::build_command_execution_end_item;
use crate::protocol::item_builders::convert_patch_changes;
use crate::protocol::spawn_provenance::normalize_legacy_failed_spawn_effective_identity;
use crate::protocol::spawn_provenance::normalize_required_legacy_spawn_requested_identity;
use crate::protocol::v2::AgentMessageDeltaNotification;
use crate::protocol::v2::CollabAgentState;
use crate::protocol::v2::CollabAgentTool;
use crate::protocol::v2::CollabAgentToolCallStatus;
use crate::protocol::v2::CommandExecutionOutputDeltaNotification;
use crate::protocol::v2::DynamicToolCallOutputContentItem;
use crate::protocol::v2::DynamicToolCallStatus;
use crate::protocol::v2::FileChangePatchUpdatedNotification;
use crate::protocol::v2::ItemCompletedNotification;
use crate::protocol::v2::ItemStartedNotification;
use crate::protocol::v2::PlanDeltaNotification;
use crate::protocol::v2::ReasoningSummaryPartAddedNotification;
use crate::protocol::v2::ReasoningSummaryTextDeltaNotification;
use crate::protocol::v2::ReasoningTextDeltaNotification;
use crate::protocol::v2::TerminalInteractionNotification;
use crate::protocol::v2::ThreadItem;
use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem as CoreDynamicToolCallOutputContentItem;
use codex_protocol::protocol::EventMsg;
use std::collections::HashMap;

/// Build the v2 app-server notification that directly corresponds to a single core event.
///
/// This only covers the stateless event-to-notification projections that have a one-to-one
/// mapping. Callers remain responsible for any surrounding state checks or side effects before
/// invoking this helper. Returns `None` for core events that do not have a direct v2 item
/// notification mapping so new or unrelated [`EventMsg`] variants cannot panic the app-server
/// hot path.
pub fn item_event_to_server_notification(
    msg: EventMsg,
    thread_id: &str,
    turn_id: &str,
) -> Option<ServerNotification> {
    let thread_id = thread_id.to_string();
    let turn_id = turn_id.to_string();
    let notification = match msg {
        EventMsg::DynamicToolCallResponse(response) => {
            let status = if response.success {
                DynamicToolCallStatus::Completed
            } else {
                DynamicToolCallStatus::Failed
            };
            let duration_ms = i64::try_from(response.duration.as_millis()).ok();
            let item = ThreadItem::DynamicToolCall {
                id: response.call_id,
                namespace: response.namespace,
                tool: response.tool,
                arguments: response.arguments,
                status,
                content_items: Some(
                    response
                        .content_items
                        .into_iter()
                        .map(|item| match item {
                            CoreDynamicToolCallOutputContentItem::InputText { text } => {
                                DynamicToolCallOutputContentItem::InputText { text }
                            }
                            CoreDynamicToolCallOutputContentItem::InputImage {
                                image_url,
                                detail,
                            } => DynamicToolCallOutputContentItem::InputImage { image_url, detail },
                            CoreDynamicToolCallOutputContentItem::InputAudio { audio_url } => {
                                DynamicToolCallOutputContentItem::InputAudio { audio_url }
                            }
                        })
                        .collect(),
                ),
                success: Some(response.success),
                duration_ms,
            };
            ServerNotification::ItemCompleted(ItemCompletedNotification {
                thread_id,
                turn_id: response.turn_id,
                item,
                completed_at_ms: response.completed_at_ms,
            })
        }
        EventMsg::CollabAgentSpawnBegin(begin_event) => {
            let (requested_model, requested_reasoning_effort) =
                normalize_required_legacy_spawn_requested_identity(
                    begin_event.model.clone(),
                    begin_event.reasoning_effort,
                );
            let item = ThreadItem::CollabAgentToolCall {
                id: begin_event.call_id,
                tool: CollabAgentTool::SpawnAgent,
                status: CollabAgentToolCallStatus::InProgress,
                sender_thread_id: begin_event.sender_thread_id.to_string(),
                receiver_thread_ids: Vec::new(),
                prompt: Some(begin_event.prompt),
                // A legacy begin event carries caller-requested values only. The effective
                // identity is only known once a terminal spawn event resolves it.
                model: None,
                reasoning_effort: None,
                requested_model,
                requested_reasoning_effort,
                agents_states: HashMap::new(),
            };
            ServerNotification::ItemStarted(ItemStartedNotification {
                thread_id,
                turn_id,
                item,
                started_at_ms: begin_event.started_at_ms,
            })
        }
        EventMsg::CollabAgentSpawnEnd(end_event) => {
            let has_receiver = end_event.new_thread_id.is_some();
            let agent_nickname = end_event.new_agent_nickname.clone();
            let agent_role = end_event.new_agent_role.clone();
            let status = match &end_event.status {
                codex_protocol::protocol::AgentStatus::Errored(_)
                | codex_protocol::protocol::AgentStatus::NotFound => {
                    CollabAgentToolCallStatus::Failed
                }
                _ if has_receiver => CollabAgentToolCallStatus::Completed,
                _ => CollabAgentToolCallStatus::Failed,
            };
            let (receiver_thread_ids, agents_states) = match end_event.new_thread_id {
                Some(id) => {
                    let receiver_id = id.to_string();
                    let received_status = CollabAgentState::from(end_event.status.clone())
                        .with_agent_identity(agent_nickname, agent_role);
                    (
                        vec![receiver_id.clone()],
                        [(receiver_id, received_status)].into_iter().collect(),
                    )
                }
                None => (Vec::new(), HashMap::new()),
            };
            let (model, reasoning_effort) = normalize_legacy_failed_spawn_effective_identity(
                has_receiver,
                Some(end_event.model),
                Some(end_event.reasoning_effort),
            );
            let item = ThreadItem::CollabAgentToolCall {
                id: end_event.call_id,
                tool: CollabAgentTool::SpawnAgent,
                status,
                sender_thread_id: end_event.sender_thread_id.to_string(),
                receiver_thread_ids,
                prompt: Some(end_event.prompt),
                model,
                reasoning_effort,
                requested_model: None,
                requested_reasoning_effort: None,
                agents_states,
            };
            ServerNotification::ItemCompleted(ItemCompletedNotification {
                thread_id,
                turn_id,
                item,
                completed_at_ms: end_event.completed_at_ms,
            })
        }
        EventMsg::CollabAgentInteractionBegin(begin_event) => {
            let receiver_thread_ids = vec![begin_event.receiver_thread_id.to_string()];
            let item = ThreadItem::CollabAgentToolCall {
                id: begin_event.call_id,
                tool: CollabAgentTool::SendInput,
                status: CollabAgentToolCallStatus::InProgress,
                sender_thread_id: begin_event.sender_thread_id.to_string(),
                receiver_thread_ids,
                prompt: Some(begin_event.prompt),
                model: None,
                reasoning_effort: None,
                requested_model: None,
                requested_reasoning_effort: None,
                agents_states: HashMap::new(),
            };
            ServerNotification::ItemStarted(ItemStartedNotification {
                thread_id,
                turn_id,
                item,
                started_at_ms: begin_event.started_at_ms,
            })
        }
        EventMsg::CollabAgentInteractionEnd(end_event) => {
            let status = match &end_event.status {
                codex_protocol::protocol::AgentStatus::Errored(_)
                | codex_protocol::protocol::AgentStatus::NotFound => {
                    CollabAgentToolCallStatus::Failed
                }
                _ => CollabAgentToolCallStatus::Completed,
            };
            let receiver_id = end_event.receiver_thread_id.to_string();
            let received_status = CollabAgentState::from(end_event.status).with_agent_identity(
                end_event.receiver_agent_nickname,
                end_event.receiver_agent_role,
            );
            let item = ThreadItem::CollabAgentToolCall {
                id: end_event.call_id,
                tool: CollabAgentTool::SendInput,
                status,
                sender_thread_id: end_event.sender_thread_id.to_string(),
                receiver_thread_ids: vec![receiver_id.clone()],
                prompt: Some(end_event.prompt),
                model: None,
                reasoning_effort: None,
                requested_model: None,
                requested_reasoning_effort: None,
                agents_states: [(receiver_id, received_status)].into_iter().collect(),
            };
            ServerNotification::ItemCompleted(ItemCompletedNotification {
                thread_id,
                turn_id,
                item,
                completed_at_ms: end_event.completed_at_ms,
            })
        }
        EventMsg::SubAgentActivity(activity) => {
            let item = ThreadItem::SubAgentActivity {
                id: activity.event_id,
                kind: activity.kind.into(),
                terminal_state: None,
                agent_thread_id: activity.agent_thread_id.to_string(),
                agent_path: String::from(activity.agent_path),
                model: activity.model,
                reasoning_effort: activity.reasoning_effort,
            };
            ServerNotification::ItemCompleted(ItemCompletedNotification {
                thread_id,
                turn_id,
                item,
                completed_at_ms: activity.occurred_at_ms,
            })
        }
        EventMsg::CollabWaitingBegin(begin_event) => {
            let receiver_thread_ids = begin_event
                .receiver_thread_ids
                .iter()
                .map(ToString::to_string)
                .collect();
            let item = ThreadItem::CollabAgentToolCall {
                id: begin_event.call_id,
                tool: CollabAgentTool::Wait,
                status: CollabAgentToolCallStatus::InProgress,
                sender_thread_id: begin_event.sender_thread_id.to_string(),
                receiver_thread_ids,
                prompt: None,
                model: None,
                reasoning_effort: None,
                requested_model: None,
                requested_reasoning_effort: None,
                agents_states: HashMap::new(),
            };
            ServerNotification::ItemStarted(ItemStartedNotification {
                thread_id,
                turn_id,
                item,
                started_at_ms: begin_event.started_at_ms,
            })
        }
        EventMsg::CollabWaitingEnd(end_event) => {
            let mut statuses = end_event.statuses;
            let mut agent_identities = HashMap::new();
            for agent_status in end_event.agent_statuses {
                agent_identities.insert(
                    agent_status.thread_id,
                    (agent_status.agent_nickname, agent_status.agent_role),
                );
                statuses.insert(agent_status.thread_id, agent_status.status);
            }
            let status = if statuses.values().any(|status| {
                matches!(
                    status,
                    codex_protocol::protocol::AgentStatus::Errored(_)
                        | codex_protocol::protocol::AgentStatus::NotFound
                )
            }) {
                CollabAgentToolCallStatus::Failed
            } else {
                CollabAgentToolCallStatus::Completed
            };
            let mut receiver_thread_ids = statuses
                .keys()
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            receiver_thread_ids.sort();
            let agents_states = statuses
                .iter()
                .map(|(id, status)| {
                    let (agent_nickname, agent_role) =
                        agent_identities.get(id).cloned().unwrap_or_default();
                    (
                        id.to_string(),
                        CollabAgentState::from(status.clone())
                            .with_agent_identity(agent_nickname, agent_role),
                    )
                })
                .collect();
            let item = ThreadItem::CollabAgentToolCall {
                id: end_event.call_id,
                tool: CollabAgentTool::Wait,
                status,
                sender_thread_id: end_event.sender_thread_id.to_string(),
                receiver_thread_ids,
                prompt: None,
                model: None,
                reasoning_effort: None,
                requested_model: None,
                requested_reasoning_effort: None,
                agents_states,
            };
            ServerNotification::ItemCompleted(ItemCompletedNotification {
                thread_id,
                turn_id,
                item,
                completed_at_ms: end_event.completed_at_ms,
            })
        }
        EventMsg::CollabCloseBegin(begin_event) => {
            let item = ThreadItem::CollabAgentToolCall {
                id: begin_event.call_id,
                tool: CollabAgentTool::CloseAgent,
                status: CollabAgentToolCallStatus::InProgress,
                sender_thread_id: begin_event.sender_thread_id.to_string(),
                receiver_thread_ids: vec![begin_event.receiver_thread_id.to_string()],
                prompt: None,
                model: None,
                reasoning_effort: None,
                requested_model: None,
                requested_reasoning_effort: None,
                agents_states: HashMap::new(),
            };
            ServerNotification::ItemStarted(ItemStartedNotification {
                thread_id,
                turn_id,
                item,
                started_at_ms: begin_event.started_at_ms,
            })
        }
        EventMsg::CollabCloseEnd(end_event) => {
            let status = match &end_event.status {
                codex_protocol::protocol::AgentStatus::Errored(_)
                | codex_protocol::protocol::AgentStatus::NotFound => {
                    CollabAgentToolCallStatus::Failed
                }
                _ => CollabAgentToolCallStatus::Completed,
            };
            let receiver_id = end_event.receiver_thread_id.to_string();
            let agents_states = [(
                receiver_id.clone(),
                CollabAgentState::from(end_event.status),
            )]
            .into_iter()
            .collect();
            let item = ThreadItem::CollabAgentToolCall {
                id: end_event.call_id,
                tool: CollabAgentTool::CloseAgent,
                status,
                sender_thread_id: end_event.sender_thread_id.to_string(),
                receiver_thread_ids: vec![receiver_id],
                prompt: None,
                model: None,
                reasoning_effort: None,
                requested_model: None,
                requested_reasoning_effort: None,
                agents_states,
            };
            ServerNotification::ItemCompleted(ItemCompletedNotification {
                thread_id,
                turn_id,
                item,
                completed_at_ms: end_event.completed_at_ms,
            })
        }
        EventMsg::CollabResumeBegin(begin_event) => {
            let item = ThreadItem::CollabAgentToolCall {
                id: begin_event.call_id,
                tool: CollabAgentTool::ResumeAgent,
                status: CollabAgentToolCallStatus::InProgress,
                sender_thread_id: begin_event.sender_thread_id.to_string(),
                receiver_thread_ids: vec![begin_event.receiver_thread_id.to_string()],
                prompt: None,
                model: None,
                reasoning_effort: None,
                requested_model: None,
                requested_reasoning_effort: None,
                agents_states: HashMap::new(),
            };
            ServerNotification::ItemStarted(ItemStartedNotification {
                thread_id,
                turn_id,
                item,
                started_at_ms: begin_event.started_at_ms,
            })
        }
        EventMsg::CollabResumeEnd(end_event) => {
            let status = match &end_event.status {
                codex_protocol::protocol::AgentStatus::Errored(_)
                | codex_protocol::protocol::AgentStatus::NotFound => {
                    CollabAgentToolCallStatus::Failed
                }
                _ => CollabAgentToolCallStatus::Completed,
            };
            let receiver_id = end_event.receiver_thread_id.to_string();
            let agents_states = [(
                receiver_id.clone(),
                CollabAgentState::from(end_event.status),
            )]
            .into_iter()
            .collect();
            let item = ThreadItem::CollabAgentToolCall {
                id: end_event.call_id,
                tool: CollabAgentTool::ResumeAgent,
                status,
                sender_thread_id: end_event.sender_thread_id.to_string(),
                receiver_thread_ids: vec![receiver_id],
                prompt: None,
                model: None,
                reasoning_effort: None,
                requested_model: None,
                requested_reasoning_effort: None,
                agents_states,
            };
            ServerNotification::ItemCompleted(ItemCompletedNotification {
                thread_id,
                turn_id,
                item,
                completed_at_ms: end_event.completed_at_ms,
            })
        }
        EventMsg::AgentMessageContentDelta(event) => {
            let codex_protocol::protocol::AgentMessageContentDeltaEvent { item_id, delta, .. } =
                event;
            ServerNotification::AgentMessageDelta(AgentMessageDeltaNotification {
                thread_id,
                turn_id,
                item_id,
                delta,
            })
        }
        EventMsg::PlanDelta(event) => ServerNotification::PlanDelta(PlanDeltaNotification {
            thread_id,
            turn_id,
            item_id: event.item_id,
            delta: event.delta,
        }),
        EventMsg::ReasoningContentDelta(event) => {
            ServerNotification::ReasoningSummaryTextDelta(ReasoningSummaryTextDeltaNotification {
                thread_id,
                turn_id,
                item_id: event.item_id,
                delta: event.delta,
                summary_index: event.summary_index,
            })
        }
        EventMsg::ReasoningRawContentDelta(event) => {
            ServerNotification::ReasoningTextDelta(ReasoningTextDeltaNotification {
                thread_id,
                turn_id,
                item_id: event.item_id,
                delta: event.delta,
                content_index: event.content_index,
            })
        }
        EventMsg::AgentReasoningSectionBreak(event) => {
            ServerNotification::ReasoningSummaryPartAdded(ReasoningSummaryPartAddedNotification {
                thread_id,
                turn_id,
                item_id: event.item_id,
                summary_index: event.summary_index,
            })
        }
        EventMsg::ItemStarted(item_started_event) => {
            ServerNotification::ItemStarted(ItemStartedNotification {
                thread_id,
                turn_id,
                item: item_started_event.item.into(),
                started_at_ms: item_started_event.started_at_ms,
            })
        }
        EventMsg::ItemCompleted(item_completed_event) => {
            ServerNotification::ItemCompleted(ItemCompletedNotification {
                thread_id,
                turn_id,
                item: item_completed_event.item.into(),
                completed_at_ms: item_completed_event.completed_at_ms,
            })
        }
        EventMsg::PatchApplyUpdated(event) => {
            ServerNotification::FileChangePatchUpdated(FileChangePatchUpdatedNotification {
                thread_id,
                turn_id,
                item_id: event.call_id,
                changes: convert_patch_changes(&event.changes),
            })
        }
        EventMsg::ExecCommandBegin(exec_command_begin_event) => {
            ServerNotification::ItemStarted(ItemStartedNotification {
                thread_id,
                turn_id,
                item: build_command_execution_begin_item(&exec_command_begin_event),
                started_at_ms: exec_command_begin_event.started_at_ms,
            })
        }
        EventMsg::ExecCommandOutputDelta(exec_command_output_delta_event) => {
            let item_id = exec_command_output_delta_event.call_id;
            let delta = String::from_utf8_lossy(&exec_command_output_delta_event.chunk).to_string();
            ServerNotification::CommandExecutionOutputDelta(
                CommandExecutionOutputDeltaNotification {
                    thread_id,
                    turn_id,
                    item_id,
                    delta,
                },
            )
        }
        EventMsg::TerminalInteraction(terminal_event) => {
            ServerNotification::TerminalInteraction(TerminalInteractionNotification {
                thread_id,
                turn_id,
                item_id: terminal_event.call_id,
                process_id: terminal_event.process_id,
                stdin: terminal_event.stdin,
                terminal_wait: terminal_event.terminal_wait.map(Into::into),
            })
        }
        EventMsg::ExecCommandEnd(exec_command_end_event) => {
            ServerNotification::ItemCompleted(ItemCompletedNotification {
                thread_id,
                turn_id,
                item: build_command_execution_end_item(&exec_command_end_event),
                completed_at_ms: exec_command_end_event.completed_at_ms,
            })
        }
        _ => return None,
    };
    Some(notification)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::spawn_provenance::normalize_legacy_spawn_requested_identity;
    use crate::protocol::v2::TerminalWaitInfo;
    use crate::protocol::v2::TerminalWaitPrimitive;
    use codex_protocol::ThreadId;
    use codex_protocol::protocol::CollabAgentInteractionEndEvent;
    use codex_protocol::protocol::CollabAgentSpawnBeginEvent;
    use codex_protocol::protocol::CollabAgentSpawnEndEvent;
    use codex_protocol::protocol::CollabAgentStatusEntry;
    use codex_protocol::protocol::CollabResumeBeginEvent;
    use codex_protocol::protocol::CollabResumeEndEvent;
    use codex_protocol::protocol::CollabWaitingEndEvent;
    use codex_protocol::protocol::ExecCommandOutputDeltaEvent;
    use codex_protocol::protocol::ExecOutputStream;
    use codex_protocol::protocol::TerminalInteractionEvent;
    use codex_protocol::protocol::TerminalWaitInfo as CoreTerminalWaitInfo;
    use codex_protocol::protocol::TerminalWaitPrimitive as CoreTerminalWaitPrimitive;
    use pretty_assertions::assert_eq;

    fn assert_item_started_server_notification(
        notification: Option<ServerNotification>,
        expected: ItemStartedNotification,
    ) {
        match notification.expect("supported event should map to notification") {
            ServerNotification::ItemStarted(payload) => assert_eq!(payload, expected),
            other => panic!("expected item started notification, got {other:?}"),
        }
    }

    fn assert_item_completed_server_notification(
        notification: Option<ServerNotification>,
        expected: ItemCompletedNotification,
    ) {
        match notification.expect("supported event should map to notification") {
            ServerNotification::ItemCompleted(payload) => assert_eq!(payload, expected),
            other => panic!("expected item completed notification, got {other:?}"),
        }
    }

    fn assert_command_execution_output_delta_server_notification(
        notification: Option<ServerNotification>,
        expected: CommandExecutionOutputDeltaNotification,
    ) {
        match notification.expect("supported event should map to notification") {
            ServerNotification::CommandExecutionOutputDelta(payload) => {
                assert_eq!(payload, expected)
            }
            other => panic!("expected command execution output delta, got {other:?}"),
        }
    }

    fn assert_terminal_interaction_server_notification(
        notification: Option<ServerNotification>,
        expected: TerminalInteractionNotification,
    ) {
        match notification.expect("supported event should map to notification") {
            ServerNotification::TerminalInteraction(payload) => assert_eq!(payload, expected),
            other => panic!("expected terminal interaction notification, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_event_returns_none_instead_of_panicking() {
        assert!(
            item_event_to_server_notification(EventMsg::ShutdownComplete, "thread-1", "turn-1",)
                .is_none()
        );
    }

    #[test]
    fn collab_spawn_begin_preserves_required_legacy_request_fields() {
        let event = CollabAgentSpawnBeginEvent {
            call_id: "call-spawn".to_string(),
            started_at_ms: 123,
            sender_thread_id: ThreadId::new(),
            prompt: "inspect the repository".to_string(),
            model: "gpt-caller".to_string(),
            reasoning_effort: Default::default(),
        };

        let notification = item_event_to_server_notification(
            EventMsg::CollabAgentSpawnBegin(event.clone()),
            "thread-1",
            "turn-1",
        );
        assert_item_started_server_notification(
            notification,
            ItemStartedNotification {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                started_at_ms: event.started_at_ms,
                item: ThreadItem::CollabAgentToolCall {
                    id: event.call_id,
                    tool: CollabAgentTool::SpawnAgent,
                    status: CollabAgentToolCallStatus::InProgress,
                    sender_thread_id: event.sender_thread_id.to_string(),
                    receiver_thread_ids: Vec::new(),
                    prompt: Some(event.prompt),
                    model: None,
                    reasoning_effort: None,
                    requested_model: Some("gpt-caller".to_string()),
                    requested_reasoning_effort: None,
                    agents_states: HashMap::new(),
                },
            },
        );
    }

    #[test]
    fn collab_spawn_begin_normalizes_the_v1_omitted_override_sentinel() {
        let event = CollabAgentSpawnBeginEvent {
            call_id: "call-spawn-v1-sentinel".to_string(),
            started_at_ms: 123,
            sender_thread_id: ThreadId::new(),
            prompt: "inspect the persisted history".to_string(),
            model: String::new(),
            reasoning_effort: codex_protocol::openai_models::ReasoningEffort::Medium,
        };

        let notification = item_event_to_server_notification(
            EventMsg::CollabAgentSpawnBegin(event.clone()),
            "thread-1",
            "turn-1",
        );
        assert_item_started_server_notification(
            notification,
            ItemStartedNotification {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                started_at_ms: event.started_at_ms,
                item: ThreadItem::CollabAgentToolCall {
                    id: event.call_id,
                    tool: CollabAgentTool::SpawnAgent,
                    status: CollabAgentToolCallStatus::InProgress,
                    sender_thread_id: event.sender_thread_id.to_string(),
                    receiver_thread_ids: Vec::new(),
                    prompt: Some(event.prompt),
                    model: None,
                    reasoning_effort: None,
                    requested_model: None,
                    requested_reasoning_effort: None,
                    agents_states: HashMap::new(),
                },
            },
        );
    }

    #[test]
    fn collab_spawn_begin_preserves_a_real_legacy_requested_identity() {
        let event = CollabAgentSpawnBeginEvent {
            call_id: "call-spawn-legacy-override".to_string(),
            started_at_ms: 123,
            sender_thread_id: ThreadId::new(),
            prompt: "use the requested identity".to_string(),
            model: "gpt-requested".to_string(),
            reasoning_effort: codex_protocol::openai_models::ReasoningEffort::Medium,
        };

        assert_eq!(
            normalize_legacy_spawn_requested_identity(
                Some(event.model.clone()),
                Some(event.reasoning_effort),
            ),
            (
                Some("gpt-requested".to_string()),
                Some(codex_protocol::openai_models::ReasoningEffort::Medium),
            )
        );
    }

    #[test]
    fn collab_spawn_end_preserves_v1_receiver_identity() {
        let receiver_thread_id = ThreadId::new();
        let notification = item_event_to_server_notification(
            EventMsg::CollabAgentSpawnEnd(CollabAgentSpawnEndEvent {
                call_id: "call-spawn-v1-identity".to_string(),
                completed_at_ms: 456,
                sender_thread_id: ThreadId::new(),
                new_thread_id: Some(receiver_thread_id),
                new_agent_nickname: Some("Scout".to_string()),
                new_agent_role: Some("explorer".to_string()),
                prompt: "inspect the repository".to_string(),
                model: "gpt-5.6-sol".to_string(),
                reasoning_effort: codex_protocol::openai_models::ReasoningEffort::XHigh,
                status: codex_protocol::protocol::AgentStatus::Running,
            }),
            "thread-1",
            "turn-1",
        );

        let ServerNotification::ItemCompleted(ItemCompletedNotification {
            item:
                ThreadItem::CollabAgentToolCall {
                    model,
                    reasoning_effort,
                    agents_states,
                    ..
                },
            ..
        }) = notification.expect("spawn terminal should map")
        else {
            panic!("expected completed spawn notification");
        };
        let state = agents_states
            .get(&receiver_thread_id.to_string())
            .expect("spawned child state");
        assert_eq!(state.agent_nickname.as_deref(), Some("Scout"));
        assert_eq!(state.agent_role.as_deref(), Some("explorer"));
        assert_eq!(model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(
            reasoning_effort,
            Some(codex_protocol::openai_models::ReasoningEffort::XHigh)
        );
    }

    #[test]
    fn collab_spawn_end_normalizes_required_legacy_absent_identity_sentinel() {
        let event = CollabAgentSpawnEndEvent {
            call_id: "call-spawn".to_string(),
            completed_at_ms: 456,
            sender_thread_id: ThreadId::new(),
            new_thread_id: None,
            new_agent_nickname: None,
            new_agent_role: None,
            prompt: "inspect the repository".to_string(),
            model: String::new(),
            reasoning_effort: Default::default(),
            status: codex_protocol::protocol::AgentStatus::NotFound,
        };

        let notification = item_event_to_server_notification(
            EventMsg::CollabAgentSpawnEnd(event.clone()),
            "thread-1",
            "turn-1",
        );
        assert_item_completed_server_notification(
            notification,
            ItemCompletedNotification {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                completed_at_ms: event.completed_at_ms,
                item: ThreadItem::CollabAgentToolCall {
                    id: event.call_id,
                    tool: CollabAgentTool::SpawnAgent,
                    status: CollabAgentToolCallStatus::Failed,
                    sender_thread_id: event.sender_thread_id.to_string(),
                    receiver_thread_ids: Vec::new(),
                    prompt: Some(event.prompt),
                    model: None,
                    reasoning_effort: None,
                    requested_model: None,
                    requested_reasoning_effort: None,
                    agents_states: HashMap::new(),
                },
            },
        );
    }

    #[test]
    fn collab_spawn_end_normalizes_the_legacy_sentinel_without_a_receiver() {
        let event = CollabAgentSpawnEndEvent {
            call_id: "call-failed-v1-sentinel".to_string(),
            completed_at_ms: 456,
            sender_thread_id: ThreadId::new(),
            new_thread_id: None,
            new_agent_nickname: None,
            new_agent_role: None,
            prompt: "inspect the repository".to_string(),
            model: String::new(),
            reasoning_effort: codex_protocol::openai_models::ReasoningEffort::Medium,
            status: codex_protocol::protocol::AgentStatus::NotFound,
        };

        let notification = item_event_to_server_notification(
            EventMsg::CollabAgentSpawnEnd(event),
            "thread-1",
            "turn-1",
        );
        let ServerNotification::ItemCompleted(ItemCompletedNotification {
            item:
                ThreadItem::CollabAgentToolCall {
                    status,
                    receiver_thread_ids,
                    model,
                    reasoning_effort,
                    ..
                },
            ..
        }) = notification.expect("spawn terminal should map")
        else {
            panic!("expected completed failed spawn notification");
        };
        assert_eq!(status, CollabAgentToolCallStatus::Failed);
        assert!(receiver_thread_ids.is_empty());
        assert_eq!(model, None);
        assert_eq!(reasoning_effort, None);
    }

    #[test]
    fn legacy_terminal_collab_events_preserve_agent_identity_and_status() {
        let sender_thread_id = ThreadId::new();
        let interaction_receiver_thread_id = ThreadId::new();
        let interaction = CollabAgentInteractionEndEvent {
            call_id: "send-v1-identity".to_string(),
            completed_at_ms: 456,
            sender_thread_id,
            receiver_thread_id: interaction_receiver_thread_id,
            receiver_agent_nickname: Some("Atlas".to_string()),
            receiver_agent_role: Some("worker".to_string()),
            prompt: "continue the implementation".to_string(),
            status: codex_protocol::protocol::AgentStatus::Completed(Some("done".to_string())),
        };
        let interaction_receiver_id = interaction.receiver_thread_id.to_string();
        assert_item_completed_server_notification(
            item_event_to_server_notification(
                EventMsg::CollabAgentInteractionEnd(interaction.clone()),
                "thread-1",
                "turn-1",
            ),
            ItemCompletedNotification {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                completed_at_ms: interaction.completed_at_ms,
                item: ThreadItem::CollabAgentToolCall {
                    id: interaction.call_id,
                    tool: CollabAgentTool::SendInput,
                    status: CollabAgentToolCallStatus::Completed,
                    sender_thread_id: interaction.sender_thread_id.to_string(),
                    receiver_thread_ids: vec![interaction_receiver_id.clone()],
                    prompt: Some(interaction.prompt),
                    model: None,
                    reasoning_effort: None,
                    requested_model: None,
                    requested_reasoning_effort: None,
                    agents_states: [(
                        interaction_receiver_id,
                        CollabAgentState::from(codex_protocol::protocol::AgentStatus::Completed(
                            Some("done".to_string()),
                        ))
                        .with_agent_identity(
                            Some("Atlas".to_string()),
                            Some("worker".to_string()),
                        ),
                    )]
                    .into_iter()
                    .collect(),
                },
            },
        );

        let completed_receiver_thread_id = ThreadId::new();
        let errored_receiver_thread_id = ThreadId::new();
        let wait = CollabWaitingEndEvent {
            sender_thread_id,
            call_id: "wait-v1-identity".to_string(),
            completed_at_ms: 789,
            agent_statuses: vec![
                CollabAgentStatusEntry {
                    thread_id: completed_receiver_thread_id,
                    agent_nickname: Some("Euclid".to_string()),
                    agent_role: Some("reviewer".to_string()),
                    status: codex_protocol::protocol::AgentStatus::Completed(Some(
                        "reviewed".to_string(),
                    )),
                },
                CollabAgentStatusEntry {
                    thread_id: errored_receiver_thread_id,
                    agent_nickname: Some("Noether".to_string()),
                    agent_role: Some("worker".to_string()),
                    status: codex_protocol::protocol::AgentStatus::Errored(
                        "connection lost".to_string(),
                    ),
                },
            ],
            // Older persisted records only populated `statuses`; enriched records carry paired
            // terminal status and identity in `agent_statuses`.
            statuses: HashMap::new(),
        };
        let completed_receiver_id = completed_receiver_thread_id.to_string();
        let errored_receiver_id = errored_receiver_thread_id.to_string();
        let mut receiver_thread_ids = vec![
            completed_receiver_id.clone(),
            errored_receiver_id.clone(),
        ];
        receiver_thread_ids.sort();
        assert_item_completed_server_notification(
            item_event_to_server_notification(
                EventMsg::CollabWaitingEnd(wait.clone()),
                "thread-1",
                "turn-1",
            ),
            ItemCompletedNotification {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                completed_at_ms: wait.completed_at_ms,
                item: ThreadItem::CollabAgentToolCall {
                    id: wait.call_id,
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::Failed,
                    sender_thread_id: wait.sender_thread_id.to_string(),
                    receiver_thread_ids,
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    requested_model: None,
                    requested_reasoning_effort: None,
                    agents_states: [
                        (
                            completed_receiver_id,
                            CollabAgentState::from(
                                codex_protocol::protocol::AgentStatus::Completed(Some(
                                    "reviewed".to_string(),
                                )),
                            )
                            .with_agent_identity(
                                Some("Euclid".to_string()),
                                Some("reviewer".to_string()),
                            ),
                        ),
                        (
                            errored_receiver_id,
                            CollabAgentState::from(
                                codex_protocol::protocol::AgentStatus::Errored(
                                    "connection lost".to_string(),
                                ),
                            )
                            .with_agent_identity(
                                Some("Noether".to_string()),
                                Some("worker".to_string()),
                            ),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                },
            },
        );
    }

    #[test]
    fn collab_resume_begin_maps_to_item_started_resume_agent() {
        let event = CollabResumeBeginEvent {
            call_id: "call-1".to_string(),
            started_at_ms: 123,
            sender_thread_id: ThreadId::new(),
            receiver_thread_id: ThreadId::new(),
            receiver_agent_nickname: None,
            receiver_agent_role: None,
        };

        let notification = item_event_to_server_notification(
            EventMsg::CollabResumeBegin(event.clone()),
            "thread-1",
            "turn-1",
        );
        assert_item_started_server_notification(
            notification,
            ItemStartedNotification {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                started_at_ms: event.started_at_ms,
                item: ThreadItem::CollabAgentToolCall {
                    id: event.call_id,
                    tool: CollabAgentTool::ResumeAgent,
                    status: CollabAgentToolCallStatus::InProgress,
                    sender_thread_id: event.sender_thread_id.to_string(),
                    receiver_thread_ids: vec![event.receiver_thread_id.to_string()],
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    requested_model: None,
                    requested_reasoning_effort: None,
                    agents_states: HashMap::new(),
                },
            },
        );
    }

    #[test]
    fn collab_resume_end_maps_to_item_completed_resume_agent() {
        let event = CollabResumeEndEvent {
            call_id: "call-2".to_string(),
            completed_at_ms: 456,
            sender_thread_id: ThreadId::new(),
            receiver_thread_id: ThreadId::new(),
            receiver_agent_nickname: None,
            receiver_agent_role: None,
            status: codex_protocol::protocol::AgentStatus::NotFound,
        };

        let receiver_id = event.receiver_thread_id.to_string();
        let notification = item_event_to_server_notification(
            EventMsg::CollabResumeEnd(event.clone()),
            "thread-2",
            "turn-2",
        );
        assert_item_completed_server_notification(
            notification,
            ItemCompletedNotification {
                thread_id: "thread-2".to_string(),
                turn_id: "turn-2".to_string(),
                completed_at_ms: event.completed_at_ms,
                item: ThreadItem::CollabAgentToolCall {
                    id: event.call_id,
                    tool: CollabAgentTool::ResumeAgent,
                    status: CollabAgentToolCallStatus::Failed,
                    sender_thread_id: event.sender_thread_id.to_string(),
                    receiver_thread_ids: vec![receiver_id.clone()],
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    requested_model: None,
                    requested_reasoning_effort: None,
                    agents_states: [(
                        receiver_id,
                        CollabAgentState::from(codex_protocol::protocol::AgentStatus::NotFound),
                    )]
                    .into_iter()
                    .collect(),
                },
            },
        );
    }

    #[test]
    fn exec_command_output_delta_maps_to_command_execution_output_delta() {
        let notification = item_event_to_server_notification(
            EventMsg::ExecCommandOutputDelta(ExecCommandOutputDeltaEvent {
                call_id: "call-1".to_string(),
                stream: ExecOutputStream::Stdout,
                chunk: b"hello".to_vec(),
            }),
            "thread-1",
            "turn-1",
        );

        assert_command_execution_output_delta_server_notification(
            notification,
            CommandExecutionOutputDeltaNotification {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item_id: "call-1".to_string(),
                delta: "hello".to_string(),
            },
        );
    }

    #[test]
    fn terminal_interaction_maps_terminal_wait_metadata() {
        let notification = item_event_to_server_notification(
            EventMsg::TerminalInteraction(TerminalInteractionEvent {
                call_id: "call-wait".to_string(),
                process_id: "123".to_string(),
                stdin: String::new(),
                terminal_wait: Some(CoreTerminalWaitInfo {
                    primitive: CoreTerminalWaitPrimitive::WriteStdinWaitUntilTerminal,
                    max_wait_ms: Some(10_000),
                    heartbeat_interval_ms: Some(1_000),
                }),
            }),
            "thread-1",
            "turn-1",
        );

        assert_terminal_interaction_server_notification(
            notification,
            TerminalInteractionNotification {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item_id: "call-wait".to_string(),
                process_id: "123".to_string(),
                stdin: String::new(),
                terminal_wait: Some(TerminalWaitInfo {
                    primitive: TerminalWaitPrimitive::WriteStdinWaitUntilTerminal,
                    max_wait_ms: Some(10_000),
                    heartbeat_interval_ms: Some(1_000),
                }),
            },
        );
    }
}
