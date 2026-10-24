use std::collections::HashMap;

use codex_protocol::ThreadId;
use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::CollabAgentToolCallItem;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_protocol::items::MailboxWaitReport;
use codex_protocol::items::MailboxWakeCause;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
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
        item: collab_call_item(tool, status, report),
        started_at_ms: 1,
    }))
}

fn completed_call(
    tool: CollabAgentTool,
    status: CollabAgentToolCallStatus,
    report: Option<MailboxWaitReport>,
) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: ThreadId::new(),
        turn_id: "turn".to_string(),
        item: collab_call_item(tool, status, report),
        completed_at_ms: 2,
    }))
}

fn collab_call_item(
    tool: CollabAgentTool,
    status: CollabAgentToolCallStatus,
    report: Option<MailboxWaitReport>,
) -> TurnItem {
    TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
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
    })
}

fn started_wait(
    status: CollabAgentToolCallStatus,
    report: Option<MailboxWaitReport>,
) -> RolloutItem {
    started_call(CollabAgentTool::Wait, status, report)
}

fn completed_wait(
    status: CollabAgentToolCallStatus,
    report: Option<MailboxWaitReport>,
) -> RolloutItem {
    completed_call(CollabAgentTool::Wait, status, report)
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

fn terminal_report(cause: MailboxWakeCause, condition_satisfied: bool) -> MailboxWaitReport {
    MailboxWaitReport {
        completion_reason: Some(cause),
        condition_satisfied,
        ..in_progress_report()
    }
}

#[test]
fn typed_mailbox_wait_lifecycle_is_persisted_across_history_modes() {
    let start = started_wait(
        CollabAgentToolCallStatus::InProgress,
        Some(in_progress_report()),
    );
    let start_without_report =
        started_wait(CollabAgentToolCallStatus::InProgress, /*report*/ None);
    let non_in_progress_start = started_wait(
        CollabAgentToolCallStatus::Completed,
        Some(in_progress_report()),
    );
    let other_tool_start = started_call(
        CollabAgentTool::SendInput,
        CollabAgentToolCallStatus::InProgress,
        Some(in_progress_report()),
    );
    let completed = completed_wait(
        CollabAgentToolCallStatus::Completed,
        Some(terminal_report(
            MailboxWakeCause::Result,
            /*condition_satisfied*/ true,
        )),
    );
    let failed = completed_wait(
        CollabAgentToolCallStatus::Failed,
        Some(terminal_report(
            MailboxWakeCause::Interrupted,
            /*condition_satisfied*/ false,
        )),
    );
    let nonterminal_completion = completed_wait(
        CollabAgentToolCallStatus::InProgress,
        Some(in_progress_report()),
    );
    let terminal_without_report =
        completed_wait(CollabAgentToolCallStatus::Failed, /*report*/ None);
    let other_tool_terminal = completed_call(
        CollabAgentTool::SendInput,
        CollabAgentToolCallStatus::Failed,
        Some(in_progress_report()),
    );

    for mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
        assert!(is_persisted_rollout_item(&start, mode));
        assert!(!is_persisted_rollout_item(&start_without_report, mode));
        assert!(!is_persisted_rollout_item(&non_in_progress_start, mode));
        assert!(!is_persisted_rollout_item(&other_tool_start, mode));
        assert!(is_persisted_rollout_item(&completed, mode));
        assert!(is_persisted_rollout_item(&failed, mode));
    }

    assert!(!is_persisted_rollout_item(
        &other_tool_terminal,
        ThreadHistoryMode::Legacy
    ));
    assert!(!is_persisted_rollout_item(
        &nonterminal_completion,
        ThreadHistoryMode::Legacy
    ));
    assert!(is_persisted_rollout_item(
        &other_tool_terminal,
        ThreadHistoryMode::Paginated
    ));
    assert!(is_persisted_rollout_item(
        &nonterminal_completion,
        ThreadHistoryMode::Paginated
    ));
    assert!(!is_persisted_rollout_item(
        &terminal_without_report,
        ThreadHistoryMode::Legacy
    ));
    assert!(is_persisted_rollout_item(
        &terminal_without_report,
        ThreadHistoryMode::Paginated
    ));
}
