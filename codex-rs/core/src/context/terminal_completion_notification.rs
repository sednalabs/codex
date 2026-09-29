use serde::Serialize;

use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

/// The terminal lifecycle outcome reported to the next model input.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TerminalCompletionStatus {
    Exited,
    Failed,
    Terminated,
    SessionShutdown,
    Pruned,
}

/// Metadata-only notification for an opted-in background unified-exec process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TerminalCompletionNotification {
    pub(crate) process_id: i32,
    pub(crate) instance_id: uuid::Uuid,
    pub(crate) status: TerminalCompletionStatus,
    pub(crate) exit_code: Option<i32>,
    pub(crate) coalesced_exited: u64,
    pub(crate) coalesced_failed: u64,
}

impl TerminalCompletionNotification {
    pub(crate) fn coalesce(&mut self, older: Self) {
        match older.status {
            TerminalCompletionStatus::Exited => self.coalesced_exited += 1,
            TerminalCompletionStatus::Failed
            | TerminalCompletionStatus::Terminated
            | TerminalCompletionStatus::SessionShutdown
            | TerminalCompletionStatus::Pruned => self.coalesced_failed += 1,
        }
        self.coalesced_exited += older.coalesced_exited;
        self.coalesced_failed += older.coalesced_failed;
    }
}

impl ContextualUserFragment for TerminalCompletionNotification {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("unified_exec.terminal_completion_notification".to_string())
    }

    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (
            "<terminal_completion_notification>",
            "</terminal_completion_notification>",
        )
    }

    fn body(&self) -> String {
        format!(
            "\n{}\n",
            serde_json::json!({
                "process_id": self.process_id,
                "instance_id": self.instance_id,
                "status": self.status,
                "exit_code": self.exit_code,
                "coalesced": {
                    "exited": self.coalesced_exited,
                    "failed": self.coalesced_failed,
                },
            })
        )
    }
}

#[cfg(test)]
#[path = "terminal_completion_notification_tests.rs"]
mod tests;
