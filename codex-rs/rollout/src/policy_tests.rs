use std::collections::HashMap;

use codex_protocol::ThreadId;
use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::CollabAgentToolCallItem;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_protocol::items::MailboxWaitReport;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::ThreadHistoryMode;

use crate::policy::is_persisted_rollout_item;

fn started_call(
    tool: CollabAgentTool,
    status: CollabAgentToolCallStatus,
    report: Option<MailboxWaitReport>,
) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ItemStarted(ItemStartedEvent {
        thread_id: ThreadId::new(),
        turn_id: "turn".to_string(),
        item: TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
            id: "wait-call".to_string(),
            tool,
            status,
            sender_thread_id: ThreadId::new(),
            receiver_thread_ids: Vec::new(),
            receiver_agents: Vec::new(),
            prompt: None,
            model: None,
            reasoning_effort: None,
            requested_model: None,
            requested_reasoning_effort: None,
            agents_states: HashMap::new(),
            wake_notifications: None,
            completion_reason: None,
            wake_cause: None,
            mailbox_wait_report: report,
        }),
        started_at_ms: 1,
    }))
}

fn started_wait(
    status: CollabAgentToolCallStatus,
    report: Option<MailboxWaitReport>,
) -> RolloutItem {
    started_call(CollabAgentTool::Wait, status, report)
}

fn in_progress_report() -> MailboxWaitReport {
    MailboxWaitReport {
        requested_targets: Vec::new(),
        requested_replies_to: Vec::new(),
        return_when: Default::default(),
        completed: Vec::new(),
        pending: Vec::new(),
        completion_reason: None,
        condition_satisfied: false,
        quiet_count: 0,
        wake_sender: None,
        wake_ref: None,
    }
}

#[test]
fn only_typed_in_progress_mailbox_wait_starts_are_persisted() {
    let valid = started_wait(
        CollabAgentToolCallStatus::InProgress,
        Some(in_progress_report()),
    );
    let no_report = started_wait(CollabAgentToolCallStatus::InProgress, None);
    let terminal = started_wait(
        CollabAgentToolCallStatus::Completed,
        Some(in_progress_report()),
    );
    let other_tool = started_call(
        CollabAgentTool::SendInput,
        CollabAgentToolCallStatus::InProgress,
        Some(in_progress_report()),
    );

    for mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
        assert!(is_persisted_rollout_item(&valid, mode));
        assert!(!is_persisted_rollout_item(&no_report, mode));
        assert!(!is_persisted_rollout_item(&terminal, mode));
        assert!(!is_persisted_rollout_item(&other_tool, mode));
    }
}
