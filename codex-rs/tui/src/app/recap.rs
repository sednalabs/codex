//! Determines when an unfocused conversation is ready for an automatic recap.

// The stacked event-dispatch change activates the generation methods.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use super::App;
use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::app_server_session::AppServerSession;
use crate::history_cell::AgentMarkdownCell;
use crate::history_cell::AgentMessageCell;
use crate::history_cell::HistoryCell;
use crate::history_cell::ThreadRecapHistoryCell;
use crate::history_cell::UserHistoryCell;
use crate::temporary_structured_request::TemporaryStructuredThreadOptions;
use crate::temporary_structured_request::run_temporary_structured_turn;
use crate::temporary_structured_request::start_temporary_thread;
use crate::temporary_structured_request::unsubscribe_temporary_thread;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnStatus;
use codex_protocol::ThreadId;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

const MIN_COMPLETED_TURNS: usize = 3;
const MIN_TURNS_BETWEEN_RECAPS: usize = 2;
pub(super) const RECAP_DELAY: Duration = Duration::from_secs(/*secs*/ 3 * 60);
const RECAP_HISTORY_MAX_TURNS: usize = 8;
const RECAP_MAX_CHARS: usize = 320;
const RECAP_RETRY_DELAY: Duration = Duration::from_secs(/*secs*/ 30);
const RECAP_PROMPT_PREFIX: &str = concat!(
    "Write a brief catch-up for a user returning to this Codex task. ",
    "In at most 40 words and one or two plain-text sentences, explain the ",
    "objective, what was completed or learned, and the next step or blocker. ",
    "Mention changed files, tests, approvals, or requested decisions only ",
    "when relevant. Never claim changes were made or tests passed unless ",
    "the conversation confirms it. If the task is complete, say so instead ",
    "of inventing more work. Use the user's language; omit greetings, ",
    "markdown, lists, and tool chatter.\n\nRecent conversation:\n",
);
const RECAP_PROMPT_MAX_BYTES: usize = 900;

fn render_recap_message(role: &str, content: &str, max_bytes: usize) -> Option<String> {
    let prefix = format!("{role}: ");
    let content_budget = max_bytes.checked_sub(prefix.len())?;
    let end = content.floor_char_boundary(content_budget.min(content.len()));
    Some(format!("{prefix}{}", &content[..end]))
}

#[derive(Deserialize)]
struct GeneratedRecap {
    recap: String,
}

fn recap_history(cells: &[Arc<dyn HistoryCell>]) -> String {
    let mut messages = Vec::new();
    let mut user_turns = 0;

    for cell in cells.iter().rev() {
        let is_user = cell.as_any().is::<UserHistoryCell>();

        let role = if is_user {
            "User"
        } else if cell.as_any().is::<AgentMarkdownCell>() || cell.as_any().is::<AgentMessageCell>()
        {
            "Assistant"
        } else {
            continue;
        };

        let content = cell
            .raw_lines()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");

        let content = content.trim();
        if content.is_empty() {
            continue;
        }

        messages.push((role, content.to_string()));

        if is_user {
            user_turns += 1;
            if user_turns == RECAP_HISTORY_MAX_TURNS {
                break;
            }
        }
    }

    messages.reverse();
    if messages.is_empty() {
        return String::new();
    }
    let byte_budget = RECAP_PROMPT_MAX_BYTES.saturating_sub(RECAP_PROMPT_PREFIX.len());
    let latest = messages
        .iter()
        .rposition(|(r, _)| *r == "User")
        .unwrap_or(messages.len() - 1);
    // Reserve half the budget for the latest request, then fill from newest to oldest.
    let latest_user_budget = byte_budget / 2;
    let (role, content) = &messages[latest];
    let latest_user = render_recap_message(role, content, latest_user_budget).unwrap_or_default();
    let mut selected = vec![(latest, latest_user)];
    let mut remaining = byte_budget.saturating_sub(selected[0].1.len());
    for (index, (role, content)) in messages.iter().enumerate().rev() {
        if index == latest || remaining <= 2 {
            continue;
        }

        let Some(rendered) = render_recap_message(role, content, remaining - 2) else {
            continue;
        };
        remaining = remaining.saturating_sub(rendered.len() + 2);
        selected.push((index, rendered));
    }

    selected.sort_unstable_by_key(|(index, _)| *index);
    selected
        .into_iter()
        .map(|(_, message)| message)
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn recap_prompt(history: &str) -> String {
    format!("{RECAP_PROMPT_PREFIX}{}", history.trim())
}

fn recap_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "recap": {
                "type": "string",
                "minLength": 1,
                "maxLength": RECAP_MAX_CHARS,
            },
        },
        "required": ["recap"],
        "additionalProperties": false,
    })
}

fn parse_recap(response: &str) -> Option<String> {
    let recap = serde_json::from_str::<GeneratedRecap>(response).ok()?.recap;

    let recap = recap.trim();
    if recap.is_empty() {
        return None;
    }

    Some(recap.chars().take(RECAP_MAX_CHARS).collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RecapRequest {
    pub(super) thread_id: ThreadId,
    pub(super) request_id: Uuid,
    pub(super) completed_turn_count: usize,
    pub(super) turn_revision: usize,
}

impl App {
    pub(super) fn request_recap(&mut self, app_server: &AppServerSession, thread_id: ThreadId) {
        if self.recap.in_flight_request.is_some() {
            return;
        }

        let history = recap_history(&self.transcript_cells);
        if history.is_empty() {
            return;
        }

        let request = RecapRequest {
            thread_id,
            request_id: Uuid::new_v4(),
            completed_turn_count: self.recap.completed_turns,
            turn_revision: self.recap.turn_revision,
        };
        let request_handle = app_server.request_handle();
        let model = self.chat_widget.current_model().to_string();
        let config = self.chat_widget.config_ref();
        let cwd = if app_server.uses_remote_workspace() {
            app_server
                .remote_cwd_override()
                .map(|cwd| cwd.to_string_lossy().into_owned())
                .unwrap_or_else(|| config.cwd.display().to_string())
        } else {
            config.cwd.display().to_string()
        };
        let options = TemporaryStructuredThreadOptions {
            model,
            model_provider: config.model_provider_id.clone(),
            cwd,
            active_permission_profile: config
                .permissions
                .active_permission_profile()
                .map(|profile| profile.id),
            mcp_server_names: config.mcp_servers.get().keys().cloned().collect(),
        };
        let event_sender = self.app_event_tx.clone();
        let task = tokio::spawn(async move {
            let result = start_temporary_thread(&request_handle, options)
                .await
                .map(|thread| thread.thread.id)
                .map_err(|error| error.to_string());

            event_sender.send(AppEvent::RecapStarted {
                thread_id: request.thread_id,
                request_id: request.request_id,
                completed_turn_count: request.completed_turn_count,
                turn_revision: request.turn_revision,
                history,
                result,
            });
        });

        self.recap.in_flight_request_id = Some(request.request_id);
        self.recap.in_flight_request = Some(task);
    }

    pub(super) fn handle_recap_started(
        &mut self,
        app_server: &AppServerSession,
        request: RecapRequest,
        history: String,
        result: Result<String, String>,
    ) {
        let RecapRequest {
            thread_id,
            request_id,
            completed_turn_count,
            turn_revision,
        } = request;
        let is_current_request = self.recap.in_flight_request_id == Some(request_id);
        if is_current_request {
            self.recap.in_flight_request.take();
        }

        let temporary_thread_id_text = match result {
            Ok(temporary_thread_id) => temporary_thread_id,
            Err(error) => {
                if is_current_request {
                    self.recap.in_flight_request_id = None;
                    tracing::warn!(%thread_id, %error, "failed to start thread recap request");
                    self.recap
                        .schedule_retry(thread_id, self.app_event_tx.clone(), turn_revision);
                }
                return;
            }
        };

        let request_is_fresh = is_current_request
            && self.current_displayed_thread_id() == Some(thread_id)
            && !self.chat_widget.is_user_turn_pending_or_running()
            && self.recap.completed_turns == completed_turn_count
            && self.recap.turn_revision == turn_revision
            && self.recap.should_generate(Instant::now());
        let Ok(temporary_thread_id) = ThreadId::from_string(&temporary_thread_id_text) else {
            if is_current_request {
                self.recap.in_flight_request_id = None;
                tracing::warn!(%thread_id, "thread recap request returned an invalid thread ID");
                self.recap
                    .schedule_retry(thread_id, self.app_event_tx.clone(), turn_revision);
            }
            return;
        };

        if !request_is_fresh {
            if is_current_request {
                self.recap.in_flight_request_id = None;
            }
            let request_handle = app_server.request_handle();
            tokio::spawn(async move {
                unsubscribe_temporary_thread(&request_handle, temporary_thread_id_text).await;
            });
            return;
        }

        let (sender, receiver) = mpsc::unbounded_channel();
        self.temporary_structured_requests
            .insert(temporary_thread_id, sender);

        let request_handle = app_server.request_handle();
        let event_sender = self.app_event_tx.clone();
        let task = tokio::spawn(async move {
            let result = run_temporary_structured_turn(
                request_handle,
                temporary_thread_id_text,
                recap_prompt(&history),
                recap_output_schema(),
                /*effort*/ None,
                receiver,
            )
            .await
            .map_err(|error| error.to_string());

            event_sender.send(AppEvent::RecapGenerated {
                thread_id,
                request_id,
                temporary_thread_id,
                completed_turn_count,
                turn_revision,
                result,
            });
        });

        self.recap.in_flight_thread_id = Some(temporary_thread_id);
        self.recap.in_flight_request = Some(task);
    }

    pub(super) fn handle_generated_recap(
        &mut self,
        request: RecapRequest,
        temporary_thread_id: ThreadId,
        result: Result<String, String>,
    ) -> Option<ThreadRecapHistoryCell> {
        let RecapRequest {
            thread_id,
            request_id,
            completed_turn_count,
            turn_revision,
        } = request;
        if self.recap.in_flight_request_id != Some(request_id)
            || self.recap.in_flight_thread_id != Some(temporary_thread_id)
        {
            return None;
        }

        self.recap.in_flight_request_id = None;
        self.recap.in_flight_thread_id = None;
        self.recap.in_flight_request.take();

        if self.current_displayed_thread_id() != Some(thread_id)
            || self.chat_widget.is_user_turn_pending_or_running()
            || self.recap.completed_turns != completed_turn_count
            || self.recap.turn_revision != turn_revision
        {
            return None;
        }

        match result {
            Ok(response) => {
                let Some(recap) = parse_recap(&response) else {
                    tracing::warn!(%thread_id, "generated thread recap was invalid");
                    self.recap
                        .schedule_retry(thread_id, self.app_event_tx.clone(), turn_revision);
                    return None;
                };

                self.recap.mark_recapped(completed_turn_count);
                Some(ThreadRecapHistoryCell::new(recap))
            }
            Err(error) => {
                tracing::warn!(%thread_id, %error, "failed to generate thread recap");
                self.recap
                    .schedule_retry(thread_id, self.app_event_tx.clone(), turn_revision);
                None
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct RecapProgress {
    pub(super) completed_turns: usize,
    pub(super) last_recapped_turn_count: Option<usize>,
}

impl RecapProgress {
    pub(super) fn from_turns(turns: &[Turn]) -> Self {
        Self {
            completed_turns: turns
                .iter()
                .filter(|turn| matches!(turn.status, TurnStatus::Completed))
                .count(),
            last_recapped_turn_count: None,
        }
    }

    pub(super) fn merge(&mut self, other: Self) {
        self.completed_turns = self.completed_turns.max(other.completed_turns);
        self.last_recapped_turn_count = self
            .last_recapped_turn_count
            .max(other.last_recapped_turn_count);
    }
}

#[derive(Debug, Default)]
pub(super) struct RecapState {
    unfocused_since: Option<Instant>,
    last_turn_finished_at: Option<Instant>,
    completed_turns: usize,
    last_recapped_turn_count: Option<usize>,
    turn_revision: usize,
    scheduled_check: Option<JoinHandle<()>>,
    retry_revision: Option<usize>,
    in_flight_request_id: Option<Uuid>,
    in_flight_thread_id: Option<ThreadId>,
    in_flight_request: Option<JoinHandle<()>>,
}

impl RecapState {
    pub(super) fn seed_from_turns(&mut self, turns: &[Turn], now: Instant) {
        self.seed_from_progress(RecapProgress::from_turns(turns), now);
    }

    pub(super) fn seed_from_progress(&mut self, progress: RecapProgress, now: Instant) {
        self.completed_turns = self.completed_turns.max(progress.completed_turns);
        self.last_recapped_turn_count = self
            .last_recapped_turn_count
            .max(progress.last_recapped_turn_count);

        if progress.completed_turns > 0 {
            self.last_turn_finished_at.get_or_insert(now);
        }
    }

    pub(super) fn progress(&self) -> RecapProgress {
        RecapProgress {
            completed_turns: self.completed_turns,
            last_recapped_turn_count: self.last_recapped_turn_count,
        }
    }

    pub(super) fn reset_for_new_thread(&mut self, now: Instant) {
        let unfocused_since = self.unfocused_since.map(|_| now);
        let mut replacement = Self::default();
        replacement.unfocused_since = unfocused_since;
        std::mem::swap(self, &mut replacement);
    }

    pub(super) fn note_focus_lost(&mut self, now: Instant) {
        if self.unfocused_since.is_none() {
            self.retry_revision = None;
        }
        self.unfocused_since.get_or_insert(now);
    }

    pub(super) fn note_focus_gained(&mut self) {
        self.unfocused_since = None;

        if let Some(task) = self.scheduled_check.take() {
            task.abort();
        }

        self.retry_revision = None;
        self.in_flight_request_id = None;
        self.in_flight_thread_id = None;
        self.in_flight_request.take();
    }

    pub(super) fn note_turn_finished(&mut self, status: &TurnStatus, now: Instant) {
        if matches!(status, TurnStatus::Completed) {
            self.completed_turns += 1;
        }
        self.turn_revision += 1;
        self.last_turn_finished_at = Some(now);
    }

    pub(super) fn schedule_check(
        &mut self,
        thread_id: ThreadId,
        app_event_tx: AppEventSender,
        now: Instant,
    ) {
        if let Some(task) = self.scheduled_check.take() {
            task.abort();
        }

        let Some(deadline) = self.next_check_deadline() else {
            return;
        };

        let delay = deadline.saturating_duration_since(now);

        self.scheduled_check = Some(tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            app_event_tx.send(AppEvent::CheckRecap { thread_id });
        }));
    }

    pub(super) fn schedule_retry(
        &mut self,
        thread_id: ThreadId,
        app_event_tx: AppEventSender,
        turn_revision: usize,
    ) {
        if self.turn_revision != turn_revision || self.retry_revision == Some(turn_revision) {
            return;
        }
        self.retry_revision = Some(turn_revision);

        if let Some(task) = self.scheduled_check.take() {
            task.abort();
        }
        self.scheduled_check = Some(tokio::spawn(async move {
            tokio::time::sleep(RECAP_RETRY_DELAY).await;
            app_event_tx.send(AppEvent::CheckRecap { thread_id });
        }));
    }

    pub(super) fn should_generate(&self, now: Instant) -> bool {
        self.next_check_deadline()
            .is_some_and(|deadline| now >= deadline)
    }

    pub(super) fn mark_recapped(&mut self, completed_turn_count: usize) {
        self.last_recapped_turn_count = Some(completed_turn_count);
    }

    fn next_check_deadline(&self) -> Option<Instant> {
        let unfocused_since = self.unfocused_since?;

        if self.completed_turns < MIN_COMPLETED_TURNS {
            return None;
        }

        if self.last_recapped_turn_count.is_some_and(|previous| {
            self.completed_turns.saturating_sub(previous) < MIN_TURNS_BETWEEN_RECAPS
        }) {
            return None;
        }

        let last_turn_finished_at = self.last_turn_finished_at?;

        unfocused_since
            .max(last_turn_finished_at)
            .checked_add(RECAP_DELAY)
    }
}

impl Drop for RecapState {
    fn drop(&mut self) {
        if let Some(task) = self.scheduled_check.take() {
            task.abort();
        }

        // Let an in-flight request finish so it can unsubscribe its temporary thread.
    }
}

#[cfg(test)]
#[path = "recap_tests.rs"]
mod tests;
