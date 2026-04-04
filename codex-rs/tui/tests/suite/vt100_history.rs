#![expect(clippy::expect_used)]

use crate::test_backend::VT100Backend;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;

// Small helper macro to assert a collection contains an item with a clearer
// failure message.
macro_rules! assert_contains {
    ($collection:expr, $item:expr $(,)?) => {
        assert!(
            $collection.contains(&$item),
            "Expected {:?} to contain {:?}",
            $collection,
            $item
        );
    };
    ($collection:expr, $item:expr, $($arg:tt)+) => {
        assert!($collection.contains(&$item), $($arg)+);
    };
}

struct TestScenario {
    term: codex_tui::custom_terminal::Terminal<VT100Backend>,
}

impl TestScenario {
    fn new(width: u16, height: u16, viewport: Rect) -> Self {
        let backend = VT100Backend::new(width, height);
        let mut term = codex_tui::custom_terminal::Terminal::with_options(backend)
            .expect("failed to construct terminal");
        term.set_viewport_area(viewport);
        Self { term }
    }

    fn run_insert(&mut self, lines: Vec<Line<'static>>) {
        codex_tui::insert_history::insert_history_lines(&mut self.term, lines)
            .expect("Failed to insert history lines in test");
    }

    fn redraw_viewport(&mut self, content: Line<'static>) {
        self.term
            .draw(|frame| frame.render_widget_ref(content, frame.area()))
            .expect("failed to draw viewport content in test");
    }
}

#[test]
fn basic_insertion_no_wrap() {
    // Screen of 20x6; viewport is the last row (height=1 at y=5)
    let area = Rect::new(
        /*x*/ 0, /*y*/ 5, /*width*/ 20, /*height*/ 1,
    );
    let mut scenario = TestScenario::new(/*width*/ 20, /*height*/ 6, area);

    let lines = vec!["first".into(), "second".into()];
    scenario.run_insert(lines);
    let rows = scenario.term.backend().vt100().screen().contents();
    assert_contains!(rows, String::from("first"));
    assert_contains!(rows, String::from("second"));
}

#[test]
fn long_token_wraps() {
    let area = Rect::new(
        /*x*/ 0, /*y*/ 5, /*width*/ 20, /*height*/ 1,
    );
    let mut scenario = TestScenario::new(/*width*/ 20, /*height*/ 6, area);

    let long = "A".repeat(45); // > 2 lines at width 20
    let lines = vec![long.clone().into()];
    scenario.run_insert(lines);
    let screen = scenario.term.backend().vt100().screen();

    // Count total A's on the screen
    let mut count_a = 0usize;
    for row in 0..6 {
        for col in 0..20 {
            if let Some(cell) = screen.cell(row, col)
                && let Some(ch) = cell.contents().chars().next()
                && ch == 'A'
            {
                count_a += 1;
            }
        }
    }

    assert_eq!(
        count_a,
        long.len(),
        "wrapped content did not preserve all characters"
    );
}

#[test]
fn emoji_and_cjk() {
    let area = Rect::new(
        /*x*/ 0, /*y*/ 5, /*width*/ 20, /*height*/ 1,
    );
    let mut scenario = TestScenario::new(/*width*/ 20, /*height*/ 6, area);

    let text = String::from("😀😀😀😀😀 你好世界");
    let lines = vec![text.clone().into()];
    scenario.run_insert(lines);
    let rows = scenario.term.backend().vt100().screen().contents();
    for ch in text.chars().filter(|c| !c.is_whitespace()) {
        assert!(
            rows.contains(ch),
            "missing character {ch:?} in reconstructed screen"
        );
    }
}

#[test]
fn mixed_ansi_spans() {
    let area = Rect::new(
        /*x*/ 0, /*y*/ 5, /*width*/ 20, /*height*/ 1,
    );
    let mut scenario = TestScenario::new(/*width*/ 20, /*height*/ 6, area);

    let line = vec!["red".red(), "+plain".into()].into();
    scenario.run_insert(vec![line]);
    let rows = scenario.term.backend().vt100().screen().contents();
    assert_contains!(rows, String::from("red+plain"));
}

#[test]
fn cursor_restoration() {
    let area = Rect::new(
        /*x*/ 0, /*y*/ 5, /*width*/ 20, /*height*/ 1,
    );
    let mut scenario = TestScenario::new(/*width*/ 20, /*height*/ 6, area);

    let lines = vec!["x".into()];
    scenario.run_insert(lines);
    assert_eq!(scenario.term.last_known_cursor_pos, (0, 0).into());
}

#[test]
fn word_wrap_no_mid_word_split() {
    // Screen of 40x10; viewport is the last row
    let area = Rect::new(
        /*x*/ 0, /*y*/ 9, /*width*/ 40, /*height*/ 1,
    );
    let mut scenario = TestScenario::new(/*width*/ 40, /*height*/ 10, area);

    let sample = "Years passed, and Willowmere thrived in peace and friendship. Mira’s herb garden flourished with both ordinary and enchanted plants, and travelers spoke of the kindness of the woman who tended them.";
    scenario.run_insert(vec![sample.into()]);
    let joined = scenario.term.backend().vt100().screen().contents();
    assert!(
        !joined.contains("bo\nth"),
        "word 'both' should not be split across lines:\n{joined}"
    );
}

#[test]
fn em_dash_and_space_word_wrap() {
    // Repro from report: ensure we break before "inside", not mid-word.
    let area = Rect::new(
        /*x*/ 0, /*y*/ 9, /*width*/ 40, /*height*/ 1,
    );
    let mut scenario = TestScenario::new(/*width*/ 40, /*height*/ 10, area);

    let sample = "Mara found an old key on the shore. Curious, she opened a tarnished box half-buried in sand—and inside lay a single, glowing seed.";
    scenario.run_insert(vec![sample.into()]);
    let joined = scenario.term.backend().vt100().screen().contents();
    assert!(
        !joined.contains("insi\nde"),
        "word 'inside' should not be split across lines:\n{joined}"
    );
}

#[test]
fn tmux_like_viewport_preserves_preexisting_history_content() {
    let area = Rect::new(
        /*x*/ 0, /*y*/ 4, /*width*/ 36, /*height*/ 2,
    );
    let mut scenario = TestScenario::new(/*width*/ 36, /*height*/ 16, area);

    let base_lines: Vec<Line<'static>> = vec![
        "tmux preseed row: hello".into(),
        "tmux preseed row: world".into(),
    ];
    scenario.run_insert(base_lines);

    // Simulate tmux-like viewport bookkeeping by reapplying the same area after insert.
    scenario.term.set_viewport_area(area);

    let long = "tmux regression: this content is intentionally long to exercise wrapping and avoid clipping";
    scenario.run_insert(vec![Line::from(long)]);

    let rows = scenario
        .term
        .backend()
        .vt100()
        .screen()
        .rows(0, 36)
        .collect::<Vec<String>>();

    assert!(
        rows.iter()
            .any(|row| row.contains("tmux preseed row: hello")),
        "expected first preseed row to remain visible after wrapped insert, rows: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("tmux preseed row: world")),
        "expected second preseed row to remain visible after wrapped insert, rows: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("tmux regression:")),
        "expected wrapped regression text to remain visible, rows: {rows:?}"
    );
}

#[test]
fn android_style_narrow_viewport_keeps_url_content_from_being_clipped() {
    let area = Rect::new(
        /*x*/ 0, /*y*/ 11, /*width*/ 14, /*height*/ 1,
    );
    let mut scenario = TestScenario::new(/*width*/ 14, /*height*/ 24, area);

    let prompt = "base row";
    scenario.run_insert(vec![Line::from(prompt)]);

    let long_url = format!(
        "https://example.test/api/v1/projects/alpha-team/releases/2026-02-17/builds/{}",
        "very-long-segment-".repeat(12),
    );
    let url_line = Line::from(vec!["• ".into(), long_url.into()]);
    scenario.run_insert(vec![url_line]);

    let rows = scenario
        .term
        .backend()
        .vt100()
        .screen()
        .rows(0, 14)
        .collect::<Vec<String>>();
    let prompt_row = rows
        .iter()
        .position(|row| row.contains(prompt))
        .unwrap_or_else(|| panic!("expected prompt row in screen rows: {rows:?}"));
    let url_row = rows
        .iter()
        .position(|row| row.contains("https://ex"))
        .unwrap_or_else(|| panic!("expected URL first row in screen rows: {rows:?}"));

    assert!(
        url_row <= prompt_row + 2,
        "expected URL content to appear immediately after prompt (allowing at most one spacer row), got prompt_row={prompt_row}, url_row={url_row}, rows={rows:?}"
    );
}

#[test]
fn committed_rows_survive_redraw_and_viewport_pressure() {
    // Keep the viewport above the screen bottom so the first history insert has
    // to move the viewport down, then verify redraws do not wipe prior rows.
    let area = Rect::new(
        /*x*/ 0, /*y*/ 6, /*width*/ 30, /*height*/ 2,
    );
    let mut scenario = TestScenario::new(/*width*/ 30, /*height*/ 10, area);

    scenario.run_insert(vec![
        "A0-visible-before-redraw".into(),
        "A1-visible-before-redraw".into(),
    ]);

    scenario.redraw_viewport("live redraw #1".into());
    scenario.redraw_viewport("live redraw #2".into());

    scenario.run_insert(vec![
        "B0-pressure-wrap-wrap-wrap-wrap".into(),
        "B1-pressure-wrap-wrap-wrap-wrap".into(),
    ]);

    scenario.redraw_viewport("live redraw #3".into());
    scenario.run_insert(vec!["C0-tail-after-pressure".into()]);

    let rows = scenario
        .term
        .backend()
        .vt100()
        .screen()
        .rows(0, 30)
        .collect::<Vec<String>>();

    for marker in [
        "A0-visible-before-redraw",
        "A1-visible-before-redraw",
        "C0-tail-after-pressure",
        "live redraw #3",
    ] {
        assert!(
            rows.iter().any(|row| row.contains(marker)),
            "expected marker {marker:?} to remain visible after redraw pressure, rows: {rows:?}"
        );
    }

    let a0 = rows
        .iter()
        .position(|row| row.contains("A0-visible-before-redraw"))
        .expect("missing A0 marker");
    let a1 = rows
        .iter()
        .position(|row| row.contains("A1-visible-before-redraw"))
        .expect("missing A1 marker");
    let b0 = rows
        .iter()
        .position(|row| row.contains("B0-pressure-wrap-wrap-wrap-"))
        .expect("missing B0 marker prefix");
    let b0_tail = rows
        .iter()
        .enumerate()
        .skip(b0 + 1)
        .find_map(|(idx, row)| row.contains("wrap").then_some(idx))
        .expect("missing B0 wrap continuation");
    let b1 = rows
        .iter()
        .position(|row| row.contains("B1-pressure-wrap-wrap-wrap-"))
        .expect("missing B1 marker prefix");
    let b1_tail = rows
        .iter()
        .enumerate()
        .skip(b1 + 1)
        .find_map(|(idx, row)| row.contains("wrap").then_some(idx))
        .expect("missing B1 wrap continuation");
    let c0 = rows
        .iter()
        .position(|row| row.contains("C0-tail-after-pressure"))
        .expect("missing C0 marker");
    assert!(
        a0 < a1 && a1 < b0 && b0 < b0_tail && b0_tail < b1 && b1 < b1_tail && b1_tail < c0,
        "expected markers to keep transcript order without overwrite, rows: {rows:?}"
    );
}
