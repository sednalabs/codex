//! Helpers for rendering and navigating multi-agent state in the TUI.
//!
//! This module owns the shared presentation contracts for multi-agent history rows, `/agent` picker
//! entries, and the fast-switch keyboard shortcuts. Higher-level coordination, such as deciding
//! which thread becomes active or when a thread closes, stays in [`crate::app::App`].

use crate::history_cell::PlainHistoryCell;
use crate::render::line_utils::prefix_lines;
use crate::status::format_tokens_compact;
use crate::text_formatting::truncate_text;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::CollabAgentInteractionEndEvent;
use codex_protocol::protocol::CollabAgentRef;
use codex_protocol::protocol::CollabAgentSpawnEndEvent;
use codex_protocol::protocol::CollabAgentStatusEntry;
use codex_protocol::protocol::CollabCloseEndEvent;
use codex_protocol::protocol::CollabResumeBeginEvent;
use codex_protocol::protocol::CollabResumeEndEvent;
use codex_protocol::protocol::CollabWaitingBeginEvent;
use codex_protocol::protocol::CollabWaitingCompletionReason;
use codex_protocol::protocol::CollabWaitingEndEvent;
use codex_protocol::protocol::TokenUsage;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
#[cfg(target_os = "macos")]
use crossterm::event::KeyEventKind;
#[cfg(target_os = "macos")]
use crossterm::event::KeyModifiers;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use std::collections::HashMap;
use std::collections::HashSet;

const COLLAB_PROMPT_PREVIEW_GRAPHEMES: usize = 160;
const COLLAB_AGENT_ERROR_PREVIEW_GRAPHEMES: usize = 160;
const COLLAB_AGENT_RESPONSE_PREVIEW_GRAPHEMES: usize = 240;
pub(crate) const SUBAGENT_LABEL: &str = "Subagent";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentPickerThreadEntry {
    /// Human-friendly nickname shown in picker rows and footer labels.
    pub(crate) agent_nickname: Option<String>,
    /// Agent type shown in brackets when present, for example `worker`.
    pub(crate) agent_role: Option<String>,
    /// Whether the thread has emitted a close event and should render dimmed.
    pub(crate) is_closed: bool,
    /// Unix timestamp (seconds) when the thread was created, if known.
    pub(crate) created_at: Option<i64>,
    /// Unix timestamp (seconds) when the thread was last updated, if known.
    pub(crate) updated_at: Option<i64>,
}

#[derive(Clone, Copy)]
struct AgentLabel<'a> {
    thread_id: Option<ThreadId>,
    nickname: Option<&'a str>,
    role: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpawnRequestSummary {
    pub(crate) model: String,
    pub(crate) reasoning_effort: ReasoningEffortConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AgentPickerThreadUsage {
    pub(crate) token_usage: TokenUsage,
    pub(crate) model_context_window: Option<i64>,
}

pub(crate) fn agent_picker_status_dot_spans(is_closed: bool) -> Vec<Span<'static>> {
    let dot = if is_closed {
        "•".into()
    } else {
        "•".green()
    };
    vec![dot, " ".into()]
}

pub(crate) fn format_agent_picker_item_name(
    agent_nickname: Option<&str>,
    agent_role: Option<&str>,
    is_primary: bool,
) -> String {
    if is_primary {
        return "Main [default]".to_string();
    }

    let agent_nickname = agent_nickname
        .map(str::trim)
        .filter(|nickname| !nickname.is_empty());
    let agent_role = agent_role.map(str::trim).filter(|role| !role.is_empty());
    match (agent_nickname, agent_role) {
        (Some(agent_nickname), Some(agent_role)) => {
            format!("{SUBAGENT_LABEL}: {agent_nickname} [{agent_role}]")
        }
        (Some(agent_nickname), None) => format!("{SUBAGENT_LABEL}: {agent_nickname}"),
        (None, Some(agent_role)) => format!("{SUBAGENT_LABEL} [{agent_role}]"),
        (None, None) => SUBAGENT_LABEL.to_string(),
    }
}

pub(crate) fn format_agent_picker_item_description(
    thread_id: ThreadId,
    token_usage: &TokenUsage,
    model_context_window: Option<i64>,
    updated_at: Option<i64>,
    created_at: Option<i64>,
) -> String {
    format_agent_picker_item_description_at(
        thread_id,
        token_usage,
        model_context_window,
        updated_at,
        created_at,
        Utc::now().timestamp(),
    )
}

fn format_agent_picker_item_description_at(
    thread_id: ThreadId,
    token_usage: &TokenUsage,
    model_context_window: Option<i64>,
    updated_at: Option<i64>,
    created_at: Option<i64>,
    now_ts: i64,
) -> String {
    let uuid = thread_id.to_string();
    let mut parts = vec![uuid];
    if token_usage.total_tokens > 0 {
        parts.push(format!(
            "{} used",
            format_tokens_compact(token_usage.total_tokens)
        ));
        if let Some(context_window) = model_context_window {
            parts.push(format!(
                "{}% left",
                token_usage.percent_of_context_window_remaining(context_window)
            ));
        }
    }
    if let Some(age) = format_agent_picker_age(updated_at, created_at, now_ts) {
        parts.push(age);
    }
    parts.join(" • ")
}

fn format_agent_picker_age(
    updated_at: Option<i64>,
    created_at: Option<i64>,
    now_ts: i64,
) -> Option<String> {
    let timestamp = updated_at.or(created_at)?;
    let age_secs = now_ts.saturating_sub(timestamp).max(0);
    let label = if age_secs < 60 {
        format!("{age_secs}s ago")
    } else if age_secs < 60 * 60 {
        format!("{}m ago", age_secs / 60)
    } else if age_secs < 60 * 60 * 24 {
        format!("{}h ago", age_secs / (60 * 60))
    } else {
        format!("{}d ago", age_secs / (60 * 60 * 24))
    };
    Some(label)
}

pub(crate) fn previous_agent_shortcut() -> crate::key_hint::KeyBinding {
    crate::key_hint::alt(KeyCode::Left)
}

pub(crate) fn next_agent_shortcut() -> crate::key_hint::KeyBinding {
    crate::key_hint::alt(KeyCode::Right)
}

/// Matches the canonical "previous agent" binding plus platform-specific fallbacks that keep agent
/// navigation working when enhanced key reporting is unavailable.
pub(crate) fn previous_agent_shortcut_matches(
    key_event: KeyEvent,
    allow_word_motion_fallback: bool,
) -> bool {
    previous_agent_shortcut().is_press(key_event)
        || previous_agent_word_motion_fallback(key_event, allow_word_motion_fallback)
}

/// Matches the canonical "next agent" binding plus platform-specific fallbacks that keep agent
/// navigation working when enhanced key reporting is unavailable.
pub(crate) fn next_agent_shortcut_matches(
    key_event: KeyEvent,
    allow_word_motion_fallback: bool,
) -> bool {
    next_agent_shortcut().is_press(key_event)
        || next_agent_word_motion_fallback(key_event, allow_word_motion_fallback)
}

#[cfg(target_os = "macos")]
fn previous_agent_word_motion_fallback(
    key_event: KeyEvent,
    allow_word_motion_fallback: bool,
) -> bool {
    // Some terminals, especially on macOS, send Option+b/f as word-motion keys instead of
    // Option+arrow events unless enhanced keyboard reporting is enabled. Callers should only
    // enable this fallback when the composer is empty so draft editing retains the expected
    // word-wise motion behavior.
    allow_word_motion_fallback
        && matches!(
            key_event,
            KeyEvent {
                code: KeyCode::Char('b'),
                modifiers: KeyModifiers::ALT,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            }
        )
}

#[cfg(not(target_os = "macos"))]
fn previous_agent_word_motion_fallback(
    _key_event: KeyEvent,
    _allow_word_motion_fallback: bool,
) -> bool {
    false
}

#[cfg(target_os = "macos")]
fn next_agent_word_motion_fallback(key_event: KeyEvent, allow_word_motion_fallback: bool) -> bool {
    // Some terminals, especially on macOS, send Option+b/f as word-motion keys instead of
    // Option+arrow events unless enhanced keyboard reporting is enabled. Callers should only
    // enable this fallback when the composer is empty so draft editing retains the expected
    // word-wise motion behavior.
    allow_word_motion_fallback
        && matches!(
            key_event,
            KeyEvent {
                code: KeyCode::Char('f'),
                modifiers: KeyModifiers::ALT,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            }
        )
}

#[cfg(not(target_os = "macos"))]
fn next_agent_word_motion_fallback(
    _key_event: KeyEvent,
    _allow_word_motion_fallback: bool,
) -> bool {
    false
}

pub(crate) fn spawn_end(
    ev: CollabAgentSpawnEndEvent,
    spawn_request: Option<&SpawnRequestSummary>,
) -> PlainHistoryCell {
    let CollabAgentSpawnEndEvent {
        call_id: _,
        sender_thread_id: _,
        new_thread_id,
        new_agent_nickname,
        new_agent_role,
        prompt,
        status: _,
        ..
    } = ev;

    let title = match new_thread_id {
        Some(thread_id) => title_with_agent(
            "Spawned",
            AgentLabel {
                thread_id: Some(thread_id),
                nickname: new_agent_nickname.as_deref(),
                role: new_agent_role.as_deref(),
            },
            spawn_request,
        ),
        None => title_text("Agent spawn failed"),
    };

    let mut details = Vec::new();
    if let Some(line) = prompt_line(&prompt) {
        details.push(line);
    }
    collab_event(title, details)
}

pub(crate) fn interaction_end(ev: CollabAgentInteractionEndEvent) -> PlainHistoryCell {
    let CollabAgentInteractionEndEvent {
        call_id: _,
        sender_thread_id: _,
        receiver_thread_id,
        receiver_agent_nickname,
        receiver_agent_role,
        prompt,
        status: _,
    } = ev;

    let title = title_with_agent(
        "Sent input to",
        AgentLabel {
            thread_id: Some(receiver_thread_id),
            nickname: receiver_agent_nickname.as_deref(),
            role: receiver_agent_role.as_deref(),
        },
        /*spawn_request*/ None,
    );

    let mut details = Vec::new();
    if let Some(line) = prompt_line(&prompt) {
        details.push(line);
    }
    collab_event(title, details)
}

pub(crate) fn waiting_begin(ev: CollabWaitingBeginEvent) -> PlainHistoryCell {
    let CollabWaitingBeginEvent {
        sender_thread_id: _,
        receiver_thread_ids,
        receiver_agents,
        call_id: _,
    } = ev;
    let receiver_agents = merge_wait_receivers(&receiver_thread_ids, receiver_agents);

    let title = match receiver_agents.as_slice() {
        [receiver] => title_with_agent(
            "Waiting for",
            agent_label_from_ref(receiver),
            /*spawn_request*/ None,
        ),
        [] => title_text("Waiting for agents"),
        _ => title_text(format!("Waiting for {} agents", receiver_agents.len())),
    };

    let details = if receiver_agents.len() > 1 {
        receiver_agents
            .iter()
            .map(|receiver| agent_label_line(agent_label_from_ref(receiver)))
            .collect()
    } else {
        Vec::new()
    };

    collab_event(title, details)
}

pub(crate) fn waiting_end(ev: CollabWaitingEndEvent) -> PlainHistoryCell {
    let CollabWaitingEndEvent {
        call_id: _,
        sender_thread_id: _,
        agent_statuses,
        statuses,
        receiver_thread_ids: _,
        pending_thread_ids,
        completion_reason,
        timed_out,
    } = ev;
    let is_timed_out =
        timed_out || matches!(completion_reason, CollabWaitingCompletionReason::Timeout);
    let title = if is_timed_out {
        if statuses.is_empty() {
            title_text("Waiting timed out")
        } else {
            title_text("Waiting partially timed out")
        }
    } else {
        title_text("Finished waiting")
    };
    let mut details = wait_complete_lines(&statuses, &agent_statuses);
    if is_timed_out && !pending_thread_ids.is_empty() {
        #[allow(clippy::disallowed_methods)]
        details.push(Line::from(vec![
            Span::from("Pending: ").yellow(),
            Span::from(format_thread_id_list(&pending_thread_ids)),
        ]));
    }
    collab_event(title, details)
}

pub(crate) fn close_end(ev: CollabCloseEndEvent) -> PlainHistoryCell {
    let CollabCloseEndEvent {
        call_id: _,
        sender_thread_id,
        receiver_thread_id,
        receiver_agent_nickname,
        receiver_agent_role,
        status: _,
    } = ev;

    collab_event(
        title_with_agent(
            "Closed",
            AgentLabel {
                thread_id: Some(receiver_thread_id),
                nickname: receiver_agent_nickname.as_deref(),
                role: receiver_agent_role.as_deref(),
            },
            /*spawn_request*/ None,
        ),
        vec![
            resume_target_line("Resume subagent: ", receiver_thread_id),
            resume_target_line("Return to parent: ", sender_thread_id),
        ],
    )
}

pub(crate) fn resume_begin(ev: CollabResumeBeginEvent) -> PlainHistoryCell {
    let CollabResumeBeginEvent {
        call_id: _,
        sender_thread_id: _,
        receiver_thread_id,
        receiver_agent_nickname,
        receiver_agent_role,
    } = ev;

    collab_event(
        title_with_agent(
            "Resuming",
            AgentLabel {
                thread_id: Some(receiver_thread_id),
                nickname: receiver_agent_nickname.as_deref(),
                role: receiver_agent_role.as_deref(),
            },
            /*spawn_request*/ None,
        ),
        Vec::new(),
    )
}

pub(crate) fn resume_end(ev: CollabResumeEndEvent) -> PlainHistoryCell {
    let CollabResumeEndEvent {
        call_id: _,
        sender_thread_id: _,
        receiver_thread_id,
        receiver_agent_nickname,
        receiver_agent_role,
        status,
    } = ev;

    collab_event(
        title_with_agent(
            "Resumed",
            AgentLabel {
                thread_id: Some(receiver_thread_id),
                nickname: receiver_agent_nickname.as_deref(),
                role: receiver_agent_role.as_deref(),
            },
            /*spawn_request*/ None,
        ),
        vec![status_summary_line(&status)],
    )
}

#[cfg_attr(debug_assertions, allow(dead_code))]
pub(crate) fn subagent_notification(agent_id: &str, status: &AgentStatus) -> PlainHistoryCell {
    let mut spans = vec![Span::from("Subagent update ").bold()];
    if let Ok(thread_id) = ThreadId::from_string(agent_id) {
        spans.extend(agent_label_spans(AgentLabel {
            thread_id: Some(thread_id),
            nickname: None,
            role: None,
        }));
    } else {
        spans.push(Span::from(agent_id.to_string()).cyan());
    }

    collab_event(title_spans_line(spans), vec![status_summary_line(status)])
}

fn collab_event(title: Line<'static>, details: Vec<Line<'static>>) -> PlainHistoryCell {
    let mut lines: Vec<Line<'static>> = vec![title];
    if !details.is_empty() {
        lines.extend(prefix_lines(details, "  └ ".dim(), "    ".into()));
    }
    PlainHistoryCell::new(lines)
}

fn title_text(title: impl Into<String>) -> Line<'static> {
    title_spans_line(vec![Span::from(title.into()).bold()])
}

fn title_with_agent(
    prefix: &str,
    agent: AgentLabel<'_>,
    spawn_request: Option<&SpawnRequestSummary>,
) -> Line<'static> {
    let mut spans = vec![Span::from(format!("{prefix} ")).bold()];
    spans.extend(agent_label_spans(agent));
    spans.extend(spawn_request_spans(spawn_request));
    title_spans_line(spans)
}

fn title_spans_line(mut spans: Vec<Span<'static>>) -> Line<'static> {
    let mut title = Vec::with_capacity(spans.len() + 1);
    title.push(Span::from("• ").dim());
    title.append(&mut spans);
    title.into()
}

fn agent_label_from_ref(agent: &CollabAgentRef) -> AgentLabel<'_> {
    AgentLabel {
        thread_id: Some(agent.thread_id),
        nickname: agent.agent_nickname.as_deref(),
        role: agent.agent_role.as_deref(),
    }
}

fn agent_label_line(agent: AgentLabel<'_>) -> Line<'static> {
    agent_label_spans(agent).into()
}

fn agent_label_spans(agent: AgentLabel<'_>) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let nickname = agent
        .nickname
        .map(str::trim)
        .filter(|nickname| !nickname.is_empty());
    let role = agent.role.map(str::trim).filter(|role| !role.is_empty());

    if let Some(nickname) = nickname {
        spans.push(Span::from(nickname.to_string()).cyan().bold());
    } else if let Some(thread_id) = agent.thread_id {
        spans.push(Span::from(thread_id.to_string()).cyan());
    } else {
        spans.push(Span::from("agent").cyan());
    }

    if let Some(role) = role {
        spans.push(Span::from(" ").dim());
        spans.push(Span::from(format!("[{role}]")));
    }

    spans
}

fn spawn_request_spans(spawn_request: Option<&SpawnRequestSummary>) -> Vec<Span<'static>> {
    let Some(spawn_request) = spawn_request else {
        return Vec::new();
    };

    let model = spawn_request.model.trim();
    if model.is_empty() && spawn_request.reasoning_effort == ReasoningEffortConfig::default() {
        return Vec::new();
    }

    let details = if model.is_empty() {
        format!("({})", spawn_request.reasoning_effort)
    } else {
        format!("({model} {})", spawn_request.reasoning_effort)
    };

    vec![Span::from(" ").dim(), Span::from(details).magenta()]
}

fn prompt_line(prompt: &str) -> Option<Line<'static>> {
    let trimmed = prompt.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(Line::from(Span::from(truncate_text(
            trimmed,
            COLLAB_PROMPT_PREVIEW_GRAPHEMES,
        ))))
    }
}

fn resume_target_line(label: &'static str, thread_id: ThreadId) -> Line<'static> {
    Line::from(vec![
        Span::from(label).dim(),
        Span::from(thread_id.to_string()).cyan(),
    ])
}

fn format_thread_id_list(ids: &[ThreadId]) -> String {
    if ids.is_empty() {
        return "none".to_string();
    }
    ids.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn merge_wait_receivers(
    receiver_thread_ids: &[ThreadId],
    mut receiver_agents: Vec<CollabAgentRef>,
) -> Vec<CollabAgentRef> {
    if receiver_agents.is_empty() {
        return receiver_thread_ids
            .iter()
            .map(|thread_id| CollabAgentRef {
                thread_id: *thread_id,
                agent_nickname: None,
                agent_role: None,
            })
            .collect();
    }

    let mut seen = receiver_agents
        .iter()
        .map(|agent| agent.thread_id)
        .collect::<HashSet<_>>();
    for thread_id in receiver_thread_ids {
        if seen.insert(*thread_id) {
            receiver_agents.push(CollabAgentRef {
                thread_id: *thread_id,
                agent_nickname: None,
                agent_role: None,
            });
        }
    }
    receiver_agents
}

fn wait_complete_lines(
    statuses: &HashMap<ThreadId, AgentStatus>,
    agent_statuses: &[CollabAgentStatusEntry],
) -> Vec<Line<'static>> {
    if statuses.is_empty() && agent_statuses.is_empty() {
        return vec![Line::from(Span::from("No agents completed yet"))];
    }

    let entries = if agent_statuses.is_empty() {
        let mut entries = statuses
            .iter()
            .map(|(thread_id, status)| CollabAgentStatusEntry {
                thread_id: *thread_id,
                agent_nickname: None,
                agent_role: None,
                status: status.clone(),
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.thread_id.to_string().cmp(&right.thread_id.to_string()));
        entries
    } else {
        let mut entries = agent_statuses.to_vec();
        let seen = entries
            .iter()
            .map(|entry| entry.thread_id)
            .collect::<HashSet<_>>();
        let mut extras = statuses
            .iter()
            .filter(|(thread_id, _)| !seen.contains(thread_id))
            .map(|(thread_id, status)| CollabAgentStatusEntry {
                thread_id: *thread_id,
                agent_nickname: None,
                agent_role: None,
                status: status.clone(),
            })
            .collect::<Vec<_>>();
        extras.sort_by(|left, right| left.thread_id.to_string().cmp(&right.thread_id.to_string()));
        entries.extend(extras);
        entries
    };

    entries
        .into_iter()
        .map(|entry| {
            let CollabAgentStatusEntry {
                thread_id,
                agent_nickname,
                agent_role,
                status,
            } = entry;
            let mut spans = agent_label_spans(AgentLabel {
                thread_id: Some(thread_id),
                nickname: agent_nickname.as_deref(),
                role: agent_role.as_deref(),
            });
            spans.push(Span::from(": ").dim());
            spans.extend(status_summary_spans(&status));
            spans.into()
        })
        .collect()
}

fn status_summary_line(status: &AgentStatus) -> Line<'static> {
    status_summary_spans(status).into()
}

fn status_summary_spans(status: &AgentStatus) -> Vec<Span<'static>> {
    match status {
        AgentStatus::PendingInit => vec![Span::from("Pending init").cyan()],
        AgentStatus::Running => vec![Span::from("Running").cyan().bold()],
        // Allow `.yellow()`
        #[allow(clippy::disallowed_methods)]
        AgentStatus::Interrupted => vec![Span::from("Interrupted").yellow()],
        AgentStatus::Completed(message) => {
            let mut spans = vec![Span::from("Completed").green()];
            if let Some(message) = message.as_ref() {
                let message_preview = truncate_text(
                    &message.split_whitespace().collect::<Vec<_>>().join(" "),
                    COLLAB_AGENT_RESPONSE_PREVIEW_GRAPHEMES,
                );
                if !message_preview.is_empty() {
                    spans.push(Span::from(" - ").dim());
                    spans.push(Span::from(message_preview));
                }
            }
            spans
        }
        AgentStatus::Errored(error) => {
            let mut spans = vec![Span::from("Error").red()];
            let error_preview = truncate_text(
                &error.split_whitespace().collect::<Vec<_>>().join(" "),
                COLLAB_AGENT_ERROR_PREVIEW_GRAPHEMES,
            );
            if !error_preview.is_empty() {
                spans.push(Span::from(" - ").dim());
                spans.push(Span::from(error_preview));
            }
            spans
        }
        AgentStatus::Shutdown => vec![Span::from("Shutdown")],
        AgentStatus::NotFound => vec![Span::from("Not found").red()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_cell::HistoryCell;
    #[cfg(target_os = "macos")]
    use crossterm::event::KeyEvent;
    #[cfg(target_os = "macos")]
    use crossterm::event::KeyModifiers;
    use insta::assert_snapshot;
    use pretty_assertions::assert_eq;
    use ratatui::style::Color;
    use ratatui::style::Modifier;

    #[test]
    fn picker_description_falls_back_to_thread_id_without_usage() {
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000111").expect("valid thread");

        assert_eq!(
            format_agent_picker_item_description(
                thread_id,
                &TokenUsage::default(),
                /*model_context_window*/ None,
                /*updated_at*/ None,
                /*created_at*/ None,
            ),
            "00000000-0000-0000-0000-000000000111"
        );
    }

    #[test]
    fn picker_description_includes_compact_token_usage_when_present() {
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000112").expect("valid thread");
        let usage = TokenUsage {
            input_tokens: 9_800,
            cached_input_tokens: 300,
            output_tokens: 2_200,
            total_tokens: 12_300,
            ..Default::default()
        };
        assert_eq!(
            format_agent_picker_item_description(
                thread_id,
                &usage,
                /*model_context_window*/ None,
                /*updated_at*/ None,
                /*created_at*/ None,
            ),
            "00000000-0000-0000-0000-000000000112 • 12.3K used"
        );
    }

    #[test]
    fn picker_description_includes_remaining_context_when_known() {
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000113").expect("valid thread");
        let usage = TokenUsage {
            total_tokens: 12_300,
            ..Default::default()
        };

        assert_eq!(
            format_agent_picker_item_description_at(
                thread_id,
                &usage,
                /*model_context_window*/ Some(24_000),
                /*updated_at*/ None,
                /*created_at*/ None,
                /*now_ts*/ 1_000,
            ),
            "00000000-0000-0000-0000-000000000113 • 12.3K used • 98% left"
        );
    }

    #[test]
    fn picker_description_includes_compact_age_when_known() {
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000113").expect("valid thread");
        let usage = TokenUsage {
            total_tokens: 12_300,
            ..Default::default()
        };

        let snapshot = [
            format_agent_picker_item_description_at(
                thread_id,
                &usage,
                /*model_context_window*/ None,
                /*updated_at*/ Some(958),
                /*created_at*/ Some(900),
                /*now_ts*/ 1_000,
            ),
            format_agent_picker_item_description_at(
                thread_id,
                &usage,
                /*model_context_window*/ None,
                /*updated_at*/ Some(400),
                /*created_at*/ Some(300),
                /*now_ts*/ 1_000,
            ),
            format_agent_picker_item_description_at(
                thread_id,
                &TokenUsage::default(),
                /*model_context_window*/ None,
                /*updated_at*/ None,
                /*created_at*/ Some(1_000 - 3 * 60 * 60),
                /*now_ts*/ 1_000,
            ),
        ]
        .join("\n");

        assert_snapshot!("agent_picker_item_description_age", snapshot);
    }

    #[test]
    fn collab_events_snapshot() {
        let sender_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000001")
            .expect("valid sender thread id");
        let robie_id = ThreadId::from_string("00000000-0000-0000-0000-000000000002")
            .expect("valid robie thread id");
        let bob_id = ThreadId::from_string("00000000-0000-0000-0000-000000000003")
            .expect("valid bob thread id");

        let spawn = spawn_end(
            CollabAgentSpawnEndEvent {
                call_id: "call-spawn".to_string(),
                sender_thread_id,
                new_thread_id: Some(robie_id),
                new_agent_nickname: Some("Robie".to_string()),
                new_agent_role: Some("explorer".to_string()),
                prompt: "Compute 11! and reply with just the integer result.".to_string(),
                model: "gpt-5".to_string(),
                reasoning_effort: ReasoningEffortConfig::High,
                status: AgentStatus::PendingInit,
            },
            Some(&SpawnRequestSummary {
                model: "gpt-5".to_string(),
                reasoning_effort: ReasoningEffortConfig::High,
            }),
        );

        let send = interaction_end(CollabAgentInteractionEndEvent {
            call_id: "call-send".to_string(),
            sender_thread_id,
            receiver_thread_id: robie_id,
            receiver_agent_nickname: Some("Robie".to_string()),
            receiver_agent_role: Some("explorer".to_string()),
            prompt: "Please continue and return the answer only.".to_string(),
            status: AgentStatus::Running,
        });

        let waiting = waiting_begin(CollabWaitingBeginEvent {
            sender_thread_id,
            receiver_thread_ids: vec![robie_id],
            receiver_agents: vec![CollabAgentRef {
                thread_id: robie_id,
                agent_nickname: Some("Robie".to_string()),
                agent_role: Some("explorer".to_string()),
            }],
            call_id: "call-wait".to_string(),
        });

        let mut statuses = HashMap::new();
        statuses.insert(
            robie_id,
            AgentStatus::Completed(Some("39916800".to_string())),
        );
        statuses.insert(bob_id, AgentStatus::Errored("tool timeout".to_string()));
        let finished = waiting_end(CollabWaitingEndEvent {
            sender_thread_id,
            call_id: "call-wait".to_string(),
            agent_statuses: vec![
                CollabAgentStatusEntry {
                    thread_id: robie_id,
                    agent_nickname: Some("Robie".to_string()),
                    agent_role: Some("explorer".to_string()),
                    status: AgentStatus::Completed(Some("39916800".to_string())),
                },
                CollabAgentStatusEntry {
                    thread_id: bob_id,
                    agent_nickname: Some("Bob".to_string()),
                    agent_role: Some("worker".to_string()),
                    status: AgentStatus::Errored("tool timeout".to_string()),
                },
            ],
            statuses,
            receiver_thread_ids: Vec::new(),
            pending_thread_ids: vec![],
            completion_reason: CollabWaitingCompletionReason::Terminal,
            timed_out: false,
        });

        let close = close_end(CollabCloseEndEvent {
            call_id: "call-close".to_string(),
            sender_thread_id,
            receiver_thread_id: robie_id,
            receiver_agent_nickname: Some("Robie".to_string()),
            receiver_agent_role: Some("explorer".to_string()),
            status: AgentStatus::Completed(Some("39916800".to_string())),
        });

        let snapshot = [spawn, send, waiting, finished, close]
            .iter()
            .map(cell_to_text)
            .collect::<Vec<_>>()
            .join("\n\n");
        assert_snapshot!("collab_agent_transcript", snapshot);
    }

    #[test]
    fn collab_wait_timeout_snapshot() {
        let sender_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000001")
            .expect("valid sender thread id");
        let robie_id = ThreadId::from_string("00000000-0000-0000-0000-000000000002")
            .expect("valid robie thread id");
        let bob_id = ThreadId::from_string("00000000-0000-0000-0000-000000000003")
            .expect("valid bob thread id");

        let waiting = waiting_begin(CollabWaitingBeginEvent {
            sender_thread_id,
            receiver_thread_ids: vec![robie_id, bob_id],
            receiver_agents: vec![
                CollabAgentRef {
                    thread_id: robie_id,
                    agent_nickname: Some("Robie".to_string()),
                    agent_role: Some("explorer".to_string()),
                },
                CollabAgentRef {
                    thread_id: bob_id,
                    agent_nickname: Some("Bob".to_string()),
                    agent_role: Some("worker".to_string()),
                },
            ],
            call_id: "call-wait-timeout".to_string(),
        });

        let finished = waiting_end(CollabWaitingEndEvent {
            sender_thread_id,
            call_id: "call-wait-timeout".to_string(),
            agent_statuses: Vec::new(),
            statuses: HashMap::new(),
            receiver_thread_ids: vec![robie_id, bob_id],
            pending_thread_ids: vec![robie_id, bob_id],
            completion_reason: CollabWaitingCompletionReason::Timeout,
            timed_out: true,
        });

        let snapshot = [waiting, finished]
            .iter()
            .map(cell_to_text)
            .collect::<Vec<_>>()
            .join("\n\n");
        assert_snapshot!("collab_wait_timeout", snapshot);
    }

    #[test]
    fn collab_close_end_includes_resume_targets() {
        let sender_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000001")
            .expect("valid sender thread id");
        let receiver_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000002")
            .expect("valid receiver thread id");

        let close = close_end(CollabCloseEndEvent {
            call_id: "call-close".to_string(),
            sender_thread_id,
            receiver_thread_id,
            receiver_agent_nickname: Some("Robie".to_string()),
            receiver_agent_role: Some("explorer".to_string()),
            status: AgentStatus::Completed(Some("39916800".to_string())),
        });
        let rendered = cell_to_text(&close);

        assert!(
            rendered.contains("Resume subagent: 00000000-0000-0000-0000-000000000002"),
            "expected rendered close message to include subagent resume target, got: {rendered}"
        );
        assert!(
            rendered.contains("Return to parent: 00000000-0000-0000-0000-000000000001"),
            "expected rendered close message to include parent resume target, got: {rendered}"
        );
    }

    #[test]
    fn collab_wait_partial_timeout_snapshot() {
        let sender_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000001")
            .expect("valid sender thread id");
        let robie_id = ThreadId::from_string("00000000-0000-0000-0000-000000000002")
            .expect("valid robie thread id");
        let bob_id = ThreadId::from_string("00000000-0000-0000-0000-000000000003")
            .expect("valid bob thread id");

        let waiting = waiting_begin(CollabWaitingBeginEvent {
            sender_thread_id,
            receiver_thread_ids: vec![robie_id, bob_id],
            receiver_agents: vec![
                CollabAgentRef {
                    thread_id: robie_id,
                    agent_nickname: Some("Robie".to_string()),
                    agent_role: Some("explorer".to_string()),
                },
                CollabAgentRef {
                    thread_id: bob_id,
                    agent_nickname: Some("Bob".to_string()),
                    agent_role: Some("worker".to_string()),
                },
            ],
            call_id: "call-wait-partial".to_string(),
        });

        let mut statuses = std::collections::HashMap::new();
        statuses.insert(
            robie_id,
            AgentStatus::Completed(Some("39916800".to_string())),
        );
        let finished = waiting_end(CollabWaitingEndEvent {
            sender_thread_id,
            call_id: "call-wait-partial".to_string(),
            agent_statuses: vec![CollabAgentStatusEntry {
                thread_id: robie_id,
                agent_nickname: Some("Robie".to_string()),
                agent_role: Some("explorer".to_string()),
                status: AgentStatus::Completed(Some("39916800".to_string())),
            }],
            statuses,
            receiver_thread_ids: vec![robie_id, bob_id],
            pending_thread_ids: vec![bob_id],
            completion_reason: CollabWaitingCompletionReason::Timeout,
            timed_out: true,
        });

        let snapshot = [waiting, finished]
            .iter()
            .map(cell_to_text)
            .collect::<Vec<_>>()
            .join("\n\n");
        assert_snapshot!("collab_wait_partial_timeout", snapshot);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn agent_shortcut_matches_option_arrow_word_motion_fallbacks_only_when_allowed() {
        assert!(previous_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Left, KeyModifiers::ALT),
            /*allow_word_motion_fallback*/ false,
        ));
        assert!(next_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Right, KeyModifiers::ALT),
            /*allow_word_motion_fallback*/ false,
        ));
        assert!(previous_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT),
            /*allow_word_motion_fallback*/ true,
        ));
        assert!(next_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT),
            /*allow_word_motion_fallback*/ true,
        ));
        assert!(!previous_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT),
            /*allow_word_motion_fallback*/ false,
        ));
        assert!(!next_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT),
            /*allow_word_motion_fallback*/ false,
        ));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn agent_shortcut_matches_option_arrows_only() {
        assert!(previous_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Left, crossterm::event::KeyModifiers::ALT,),
            /*allow_word_motion_fallback*/ false
        ));
        assert!(next_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Right, crossterm::event::KeyModifiers::ALT,),
            /*allow_word_motion_fallback*/ false
        ));
        assert!(!previous_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Char('b'), crossterm::event::KeyModifiers::ALT,),
            /*allow_word_motion_fallback*/ false
        ));
        assert!(!next_agent_shortcut_matches(
            KeyEvent::new(KeyCode::Char('f'), crossterm::event::KeyModifiers::ALT,),
            /*allow_word_motion_fallback*/ false
        ));
    }

    #[test]
    fn title_styles_nickname_and_role() {
        let sender_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000001")
            .expect("valid sender thread id");
        let robie_id = ThreadId::from_string("00000000-0000-0000-0000-000000000002")
            .expect("valid robie thread id");
        let cell = spawn_end(
            CollabAgentSpawnEndEvent {
                call_id: "call-spawn".to_string(),
                sender_thread_id,
                new_thread_id: Some(robie_id),
                new_agent_nickname: Some("Robie".to_string()),
                new_agent_role: Some("explorer".to_string()),
                prompt: String::new(),
                model: "gpt-5".to_string(),
                reasoning_effort: ReasoningEffortConfig::High,
                status: AgentStatus::PendingInit,
            },
            Some(&SpawnRequestSummary {
                model: "gpt-5".to_string(),
                reasoning_effort: ReasoningEffortConfig::High,
            }),
        );

        let lines = cell.display_lines(/*width*/ 200);
        let title = &lines[0];
        assert_eq!(title.spans[2].content.as_ref(), "Robie");
        assert_eq!(title.spans[2].style.fg, Some(Color::Cyan));
        assert!(title.spans[2].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(title.spans[4].content.as_ref(), "[explorer]");
        assert_eq!(title.spans[4].style.fg, None);
        assert!(!title.spans[4].style.add_modifier.contains(Modifier::DIM));
        assert_eq!(title.spans[6].content.as_ref(), "(gpt-5 high)");
        assert_eq!(title.spans[6].style.fg, Some(Color::Magenta));
    }

    #[test]
    fn collab_resume_interrupted_snapshot() {
        let sender_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000001")
            .expect("valid sender thread id");
        let robie_id = ThreadId::from_string("00000000-0000-0000-0000-000000000002")
            .expect("valid robie thread id");

        let cell = resume_end(CollabResumeEndEvent {
            call_id: "call-resume".to_string(),
            sender_thread_id,
            receiver_thread_id: robie_id,
            receiver_agent_nickname: Some("Robie".to_string()),
            receiver_agent_role: Some("explorer".to_string()),
            status: AgentStatus::Interrupted,
        });

        assert_snapshot!("collab_resume_interrupted", cell_to_text(&cell));
    }

    fn cell_to_text(cell: &PlainHistoryCell) -> String {
        cell.display_lines(/*width*/ 200)
            .iter()
            .map(line_to_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn line_to_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .join("")
    }
}
