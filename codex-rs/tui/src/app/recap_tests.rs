use super::RECAP_DELAY;
use super::RECAP_HISTORY_MAX_TURNS;
use super::RECAP_PROMPT_MAX_BYTES;
use super::RecapProgress;
use super::RecapState;
use super::recap_history;
use super::recap_prompt;
use crate::history_cell::AgentMarkdownCell;
use crate::history_cell::AgentMessageCell;
use crate::history_cell::HistoryCell;
use crate::history_cell::PlainHistoryCell;
use crate::history_cell::ThreadRecapHistoryCell;
use crate::history_cell::UserHistoryCell;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnStatus;
use pretty_assertions::assert_eq;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

fn turn(status: TurnStatus) -> Turn {
    Turn {
        id: "turn".to_string(),
        items: Vec::new(),
        items_view: Default::default(),
        status,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    }
}

fn user_history_cell(message: &str) -> Arc<dyn HistoryCell> {
    Arc::new(UserHistoryCell {
        message: message.to_string(),
        text_elements: Vec::new(),
        local_image_paths: Vec::new(),
        remote_image_urls: Vec::new(),
    })
}

fn assistant_history_cell(message: &str) -> Arc<dyn HistoryCell> {
    Arc::new(AgentMarkdownCell::new(message.to_string(), Path::new(".")))
}

#[test]
fn recap_history_preserves_chronological_user_and_assistant_messages() {
    let cells = vec![
        user_history_cell("First request"),
        assistant_history_cell("First response"),
        user_history_cell("Second request"),
        Arc::new(AgentMessageCell::new(
            vec!["Streaming response".into()],
            /*is_first_line*/ true,
        )),
    ];

    assert_eq!(
        recap_history(&cells),
        "User: First request\n\nAssistant: First response\n\nUser: Second request\n\nAssistant: Streaming response"
    );
}

#[test]
fn recap_history_keeps_only_the_most_recent_eight_user_turns() {
    let total_turns = RECAP_HISTORY_MAX_TURNS + 2;
    let mut cells = Vec::new();

    for index in 0..total_turns {
        cells.push(user_history_cell(&format!("question-{index}")));
        cells.push(assistant_history_cell(&format!("answer-{index}")));
    }

    let expected = (2..total_turns)
        .map(|index| format!("User: question-{index}\n\nAssistant: answer-{index}"))
        .collect::<Vec<_>>()
        .join("\n\n");

    assert_eq!(recap_history(&cells), expected);
}

#[test]
fn recap_history_ignores_activity_previous_recaps_and_empty_messages() {
    let cells = vec![
        user_history_cell("  "),
        Arc::new(PlainHistoryCell::new(vec!["tool output".into()])),
        user_history_cell("Implement recap"),
        assistant_history_cell(" \n "),
        Arc::new(ThreadRecapHistoryCell::new("Previous recap".to_string())),
        assistant_history_cell("Done"),
    ];

    assert_eq!(
        recap_history(&cells),
        "User: Implement recap\n\nAssistant: Done"
    );
}

#[test]
fn recap_history_preserves_latest_user_turn_when_latest_response_is_oversized() {
    let cells = vec![
        user_history_cell("Keep this latest request"),
        assistant_history_cell(&"🦀".repeat(RECAP_PROMPT_MAX_BYTES * 2)),
    ];
    let prompt = recap_prompt(&recap_history(&cells));

    assert!(prompt.len() <= RECAP_PROMPT_MAX_BYTES);
    assert!(prompt.contains("User: Keep this latest request"));
    assert!(prompt.contains("Assistant: 🦀"));
}

#[test]
fn recap_history_caps_utf8_bytes_without_splitting_characters() {
    let cells = vec![user_history_cell(
        &"最新の進捗🦀".repeat(RECAP_PROMPT_MAX_BYTES),
    )];
    let history = recap_history(&cells);

    assert!(recap_prompt(&history).len() <= RECAP_PROMPT_MAX_BYTES);
    assert!(history.starts_with("User: 最新の進捗🦀"));
}

#[test]
fn recap_requires_focus_loss_and_three_completed_turns() {
    let now = Instant::now();
    let mut state = RecapState::default();

    state.note_turn_completed(now);
    state.note_turn_completed(now);
    assert!(!state.should_generate(now + RECAP_DELAY));

    state.note_focus_lost(now);
    assert!(!state.should_generate(now + RECAP_DELAY));

    state.note_turn_completed(now);
    assert!(state.should_generate(now + RECAP_DELAY));
}

#[test]
fn recap_waits_after_focus_loss_even_if_turn_completed_earlier() {
    let started = Instant::now();
    let mut state = RecapState::default();

    for _ in 0..3 {
        state.note_turn_completed(started);
    }

    let focus_lost = started + RECAP_DELAY;
    state.note_focus_lost(focus_lost);

    assert!(!state.should_generate(focus_lost));
    assert!(!state.should_generate(focus_lost + RECAP_DELAY - Duration::from_secs(/*secs*/ 1)));
    assert!(state.should_generate(focus_lost + RECAP_DELAY));
}

#[test]
fn completed_turn_resets_recap_deadline_while_unfocused() {
    let focus_lost = Instant::now();
    let mut state = RecapState::default();
    state.note_focus_lost(focus_lost);

    for _ in 0..3 {
        state.note_turn_completed(focus_lost);
    }

    let last_completed_turn_at = focus_lost + Duration::from_secs(/*secs*/ 30);
    state.note_turn_completed(last_completed_turn_at);

    assert!(!state.should_generate(focus_lost + RECAP_DELAY));
    assert!(state.should_generate(last_completed_turn_at + RECAP_DELAY));
}

#[test]
fn repeated_focus_loss_does_not_restart_recap_delay() {
    let focus_lost = Instant::now();
    let mut state = RecapState::default();
    state.note_focus_lost(focus_lost);

    for _ in 0..3 {
        state.note_turn_completed(focus_lost);
    }

    state.note_focus_lost(focus_lost + Duration::from_secs(/*secs*/ 30));

    assert!(state.should_generate(focus_lost + RECAP_DELAY));
}

#[test]
fn regaining_focus_prevents_recap_generation() {
    let now = Instant::now();
    let mut state = RecapState::default();
    state.note_focus_lost(now);

    for _ in 0..3 {
        state.note_turn_completed(now);
    }

    state.note_focus_gained();

    assert!(!state.should_generate(now + RECAP_DELAY));
}

#[test]
fn another_recap_requires_two_additional_completed_turns() {
    let now = Instant::now();
    let mut state = RecapState::default();
    state.note_focus_lost(now);
    state.seed_from_progress(
        RecapProgress {
            completed_turns: 3,
            last_recapped_turn_count: None,
        },
        now,
    );
    state.mark_recapped(/*completed_turn_count*/ 3);
    assert!(!state.should_generate(now + RECAP_DELAY));

    let fourth_turn = now + RECAP_DELAY;
    state.note_turn_completed(fourth_turn);
    assert!(!state.should_generate(fourth_turn + RECAP_DELAY));

    let fifth_turn = fourth_turn + Duration::from_secs(/*secs*/ 1);
    state.note_turn_completed(fifth_turn);

    assert!(state.should_generate(fifth_turn + RECAP_DELAY));
}

#[test]
fn replacing_the_primary_thread_resets_progress_but_preserves_focus() {
    let now = Instant::now();
    let mut state = RecapState::default();
    state.note_focus_lost(now);
    state.seed_from_progress(
        RecapProgress {
            completed_turns: 3,
            last_recapped_turn_count: Some(3),
        },
        now,
    );

    let replaced_at = now + Duration::from_secs(/*secs*/ 30);
    state.reset_for_new_thread(replaced_at);

    assert_eq!(state.progress(), RecapProgress::default());
    assert_eq!(state.unfocused_since, Some(replaced_at));
    assert!(!state.should_generate(replaced_at + RECAP_DELAY));
}

#[test]
fn restored_history_counts_only_completed_turns() {
    let now = Instant::now();
    let mut state = RecapState::default();
    state.note_focus_lost(now);
    state.seed_from_turns(
        &[
            turn(TurnStatus::Completed),
            turn(TurnStatus::Failed),
            turn(TurnStatus::Completed),
            turn(TurnStatus::Interrupted),
            turn(TurnStatus::InProgress),
            turn(TurnStatus::Completed),
        ],
        now,
    );

    assert_eq!(state.completed_turns, 3);
    assert!(state.should_generate(now + RECAP_DELAY));
}

#[test]
fn restored_history_never_reduces_observed_completed_turns() {
    let now = Instant::now();
    let mut state = RecapState::default();

    for _ in 0..4 {
        state.note_turn_completed(now);
    }

    state.seed_from_turns(
        &[
            turn(TurnStatus::Completed),
            turn(TurnStatus::Completed),
            turn(TurnStatus::Completed),
        ],
        now,
    );

    assert_eq!(state.completed_turns, 4);
}

#[test]
fn recap_history_cell_uses_labeled_checkpoint_layout() {
    let cell =
        ThreadRecapHistoryCell::new("Automatic recaps stay compact on wide terminals.".to_string());
    let rendered = cell
        .display_lines(/*width*/ 64)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    insta::assert_snapshot!(rendered, @r"
    ─ Conversation recap ───────────────────────────────────────────

      Automatic recaps stay compact on wide terminals.
    ");
}

#[test]
fn recap_history_cell_wraps_in_narrow_terminals() {
    let cell = ThreadRecapHistoryCell::new(
        "Keep conversation recaps readable in narrow terminals.".to_string(),
    );
    let rendered = cell
        .display_lines(/*width*/ 32)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    insta::assert_snapshot!(rendered, @r"
    ─ Conversation recap ───────────

      Keep conversation recaps
      readable in narrow terminals.
    ");
}

#[test]
fn recap_history_cell_preserves_heading_in_raw_history() {
    let cell = ThreadRecapHistoryCell::new("Resume this task.".to_string());
    let rendered = cell
        .raw_lines()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    insta::assert_snapshot!(rendered, @r"
    Conversation recap
    Resume this task.
    ");
}

#[test]
fn recap_history_cell_preserves_explicit_line_breaks() {
    let cell = ThreadRecapHistoryCell::new(
        "Finished the parser.\nNext: run the focused tests.".to_string(),
    );
    let displayed = cell
        .display_lines(/*width*/ 48)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let raw = cell
        .raw_lines()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    insta::assert_snapshot!(displayed, @r"
    ─ Conversation recap ───────────────────────────

      Finished the parser.
      Next: run the focused tests.
    ");
    insta::assert_snapshot!(raw, @r"
    Conversation recap
    Finished the parser.
    Next: run the focused tests.
    ");
}
