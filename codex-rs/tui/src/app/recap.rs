//! Tracks when an unfocused conversation becomes eligible for an automatic recap.

#![cfg_attr(not(test), allow(dead_code))]

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use crate::history_cell::AgentMarkdownCell;
use crate::history_cell::AgentMessageCell;
use crate::history_cell::HistoryCell;
use crate::history_cell::UserHistoryCell;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnStatus;

const MIN_COMPLETED_TURNS: usize = 3;
const MIN_TURNS_BETWEEN_RECAPS: usize = 2;
const RECAP_DELAY: Duration = Duration::from_secs(/*secs*/ 3 * 60);
const RECAP_HISTORY_MAX_TURNS: usize = 8;
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
    last_completed_turn_at: Option<Instant>,
    completed_turns: usize,
    last_recapped_turn_count: Option<usize>,
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
            self.last_completed_turn_at.get_or_insert(now);
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
        *self = Self {
            unfocused_since,
            ..Self::default()
        };
    }

    pub(super) fn note_focus_lost(&mut self, now: Instant) {
        self.unfocused_since.get_or_insert(now);
    }

    pub(super) fn note_focus_gained(&mut self) {
        self.unfocused_since = None;
    }

    pub(super) fn note_turn_completed(&mut self, now: Instant) {
        self.completed_turns += 1;
        self.last_completed_turn_at = Some(now);
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

        let last_completed_turn_at = self.last_completed_turn_at?;

        unfocused_since
            .max(last_completed_turn_at)
            .checked_add(RECAP_DELAY)
    }
}

#[cfg(test)]
#[path = "recap_tests.rs"]
mod tests;
