//! Rendering and live updates for mailbox wait history rows.

use super::collab_event;
use super::sanitize_notification_preview;
use super::title_text;
use crate::history_cell::HistoryCell;
use crate::history_cell::PlainHistoryCell;
use codex_protocol::items::MailboxWaitReport;
use codex_protocol::items::MailboxWaitReturnWhen;
use codex_protocol::items::MailboxWaitSelectionItem;
use codex_protocol::items::MailboxWakeCause;
use ratatui::text::Line;
use std::sync::Arc;
use std::sync::Mutex;

pub(crate) fn begin(report: &MailboxWaitReport) -> PlainHistoryCell {
    let selection_count = report.completed.len() + report.pending.len();
    let targetless = report.requested_targets.is_empty() && report.requested_replies_to.is_empty();
    let title = if targetless && selection_count == 0 {
        format!(
            "Waiting for mailbox attention ({})",
            return_when_label(report.return_when)
        )
    } else if targetless {
        format!("Waiting for mailbox attention or {} of {selection_count} work outcomes", return_when_label(report.return_when))
    } else {
        format!(
            "Waiting for {} of {selection_count} selected conditions",
            return_when_label(report.return_when)
        )
    };
    collab_event(title_text(title), mailbox_wait_report_lines(report, /*in_progress*/ true))
}

pub(crate) fn end(report: &MailboxWaitReport) -> PlainHistoryCell {
    let title = match report.completion_reason {
        Some(reason) => format!("Mailbox wait returned: {}", mailbox_wake_label(reason)),
        None => "Mailbox wait returned (cause unavailable)".to_string(),
    };
    collab_event(title_text(title), mailbox_wait_report_lines(report, /*in_progress*/ false))
}

#[derive(Clone, Debug)]
pub(crate) struct MailboxWaitProgressHandle(Arc<Mutex<MailboxWaitProgress>>);

impl MailboxWaitProgressHandle {
    pub(crate) fn update(&self, report: MailboxWaitReport, in_progress: bool) {
        match self.0.lock() {
            Ok(mut current) => {
                current.report = report;
                current.in_progress = in_progress;
            }
            Err(poisoned) => {
                let mut current = poisoned.into_inner();
                current.report = report;
                current.in_progress = in_progress;
            }
        }
    }

    pub(crate) fn cell(&self, call_id: impl Into<String>) -> MailboxWaitProgressCell {
        MailboxWaitProgressCell {
            call_id: call_id.into(),
            progress: Arc::clone(&self.0),
        }
    }

    pub(crate) fn finish(&self) {
        match self.0.lock() {
            Ok(mut current) => current.in_progress = false,
            Err(poisoned) => poisoned.into_inner().in_progress = false,
        }
    }
}

#[derive(Debug)]
struct MailboxWaitProgress {
    report: MailboxWaitReport,
    in_progress: bool,
}

#[derive(Debug)]
pub(crate) struct MailboxWaitProgressCell {
    call_id: String,
    progress: Arc<Mutex<MailboxWaitProgress>>,
}

impl MailboxWaitProgressCell {
    pub(crate) fn call_id(&self) -> &str {
        &self.call_id
    }

    pub(crate) fn snapshot_cell(&self) -> PlainHistoryCell {
        let progress = snapshot(&self.progress);
        if progress.in_progress {
            begin(&progress.report)
        } else {
            end(&progress.report)
        }
    }
}

pub(crate) fn progress_cell(
    call_id: impl Into<String>,
    report: MailboxWaitReport,
) -> (MailboxWaitProgressCell, MailboxWaitProgressHandle) {
    let progress = Arc::new(Mutex::new(MailboxWaitProgress {
        report,
        in_progress: true,
    }));
    (
        MailboxWaitProgressCell {
            call_id: call_id.into(),
            progress: Arc::clone(&progress),
        },
        MailboxWaitProgressHandle(progress),
    )
}

impl HistoryCell for MailboxWaitProgressCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let progress = snapshot(&self.progress);
        if progress.in_progress {
            begin(&progress.report).display_lines(width)
        } else {
            end(&progress.report).display_lines(width)
        }
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let progress = snapshot(&self.progress);
        if progress.in_progress {
            begin(&progress.report).raw_lines()
        } else {
            end(&progress.report).raw_lines()
        }
    }
}

fn snapshot(progress: &Mutex<MailboxWaitProgress>) -> MailboxWaitProgress {
    match progress.lock() {
        Ok(progress) => MailboxWaitProgress {
            report: progress.report.clone(),
            in_progress: progress.in_progress,
        },
        Err(poisoned) => {
            let progress = poisoned.into_inner();
            MailboxWaitProgress {
                report: progress.report.clone(),
                in_progress: progress.in_progress,
            }
        }
    }
}

fn mailbox_wait_report_lines(report: &MailboxWaitReport, in_progress: bool) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(format!(
        "Join condition: {}",
        return_when_label(report.return_when)
    ))];
    if !report.requested_targets.is_empty() {
        lines.push(Line::from(format!(
            "Targets: {}",
            report
                .requested_targets
                .iter()
                .map(|target| sanitize_notification_preview(target))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    if !report.requested_replies_to.is_empty() {
        lines.push(Line::from(format!(
            "Replies to: {}",
            report
                .requested_replies_to
                .iter()
                .map(|message_id| sanitize_notification_preview(message_id))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    for selection in &report.completed {
        lines.push(mailbox_selection_line("Completed", selection));
    }
    for selection in &report.pending {
        lines.push(mailbox_selection_line("Pending", selection));
    }
    if !in_progress {
        lines.push(Line::from(format!(
            "Condition satisfied: {}",
            if report.condition_satisfied { "yes" } else { "no" }
        )));
        if let Some(reason) = report.completion_reason {
            lines.push(Line::from(format!(
                "Completion reason: {}",
                mailbox_wake_label(reason)
            )));
        }
        if let Some(sender) = report.wake_sender.as_deref() {
            lines.push(Line::from(format!(
                "Wake sender: {}",
                sanitize_notification_preview(sender)
            )));
        }
        if let Some(wake_ref) = report.wake_ref.as_deref() {
            lines.push(Line::from(format!(
                "Wake reference: {}",
                sanitize_notification_preview(wake_ref)
            )));
        }
    }
    lines.push(Line::from(format!("Quiet count: {}", report.quiet_count)));
    lines
}

fn mailbox_selection_line(phase: &str, selection: &MailboxWaitSelectionItem) -> Line<'static> {
    let mut identity = Vec::new();
    if let Some(agent) = selection.agent.as_deref() {
        identity.push(sanitize_notification_preview(agent));
    }
    if let Some(work_ref) = selection.work_ref.as_deref() {
        identity.push(sanitize_notification_preview(work_ref));
    }
    if let Some(message_id) = selection.message_id.as_deref() {
        identity.push(sanitize_notification_preview(message_id));
    }
    if identity.is_empty() {
        identity.push("selected item".to_string());
    }
    Line::from(format!(
        "{phase} {}: {}",
        identity.join(" · "),
        sanitize_notification_preview(&selection.status)
    ))
}

fn return_when_label(return_when: MailboxWaitReturnWhen) -> &'static str {
    match return_when {
        MailboxWaitReturnWhen::Any => "any",
        MailboxWaitReturnWhen::All => "all",
    }
}

fn mailbox_wake_label(reason: MailboxWakeCause) -> &'static str {
    match reason {
        MailboxWakeCause::Result => "result",
        MailboxWakeCause::Reply => "reply",
        MailboxWakeCause::Request => "request",
        MailboxWakeCause::Human => "human",
        MailboxWakeCause::Tool => "tool",
        MailboxWakeCause::Deadline => "deadline",
        MailboxWakeCause::Cancel => "cancel",
        MailboxWakeCause::SubscriptionLoss => "subscription loss",
        MailboxWakeCause::RuntimeFault => "runtime fault",
        MailboxWakeCause::NoMatchingWork => "no matching work",
        MailboxWakeCause::AlreadySatisfied => "already satisfied",
        MailboxWakeCause::Cycle => "cycle",
        MailboxWakeCause::NeedsAttention => "needs attention",
    }
}
