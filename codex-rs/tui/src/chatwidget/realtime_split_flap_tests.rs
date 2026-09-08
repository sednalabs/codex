//! Regressions for bounded state used by realtime voice presentation.
//! Retained history must preserve ordering without growing beyond its limits.

use super::*;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

#[derive(Debug)]
struct PlainTranscriptCell(String);

impl HistoryCell for PlainTranscriptCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        vec![Line::from(vec![Span::raw("› "), Span::raw(self.0.clone())])]
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        vec![Line::from(self.0.clone())]
    }
}

fn cell(text: &str, motion_mode: MotionMode) -> SplitFlapTranscriptCell {
    SplitFlapTranscriptCell::new(
        Box::new(PlainTranscriptCell(text.to_string())),
        "user",
        text,
        /*previous*/ None,
        motion_mode,
        FrameRequester::test_dummy(),
    )
}

fn frame(cell: &SplitFlapTranscriptCell, milliseconds: u64) -> String {
    cell.animate_lines(
        cell.inner.display_hyperlink_lines(/*width*/ 16),
        /*width*/ 16,
        Duration::from_millis(milliseconds),
    )
    .into_iter()
    .map(|line| line.line.to_string().trim_end().to_string())
    .collect::<Vec<_>>()
    .join("\n")
}

#[test]
fn split_flap_frames_assemble_a_transcript_from_dark_tiles() {
    let board = cell("GATE 73", MotionMode::Animated);
    let frames = [0, 45, 90, 180, 285, 675]
        .into_iter()
        .map(|milliseconds| format!("{milliseconds:>3}ms {}", frame(&board, milliseconds)))
        .collect::<Vec<_>>()
        .join("\n");

    insta::assert_snapshot!(frames, @r"
      0ms ›
     45ms › T
     90ms › ATGG AG
    180ms › GGTT ET GAT
    285ms › GATE 73 TEG
    675ms › GATE 73
    ");

    let punctuated = cell("HELLO, WORLD?!", MotionMode::Animated);
    assert!(!frame(&punctuated, /*milliseconds*/ 90).contains(','));
    assert!(!frame(&punctuated, /*milliseconds*/ 180).contains(','));
    assert_eq!(frame(&punctuated, /*milliseconds*/ 285), "› HELLO, WORLD?!");
    assert_ne!(
        frame(&board, /*milliseconds*/ 285),
        frame(&board, /*milliseconds*/ 330)
    );

    let lowercase = cell("please keep going", MotionMode::Animated);
    assert!(lowercase.flap_sample.iter().all(u8::is_ascii_lowercase));
    for elapsed in [90, 285] {
        assert!(
            frame(&lowercase, elapsed)
                .chars()
                .all(|ch| !ch.is_ascii_uppercase())
        );
    }
}

#[test]
fn split_flap_paints_the_entire_transcript_row_black() {
    let cell = cell("GATE", MotionMode::Animated);
    let lines = cell.animate_lines(
        cell.inner.display_hyperlink_lines(/*width*/ 12),
        /*width*/ 12,
        Duration::ZERO,
    );
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 12, /*height*/ 1,
    );
    let mut buffer = Buffer::empty(area);
    Paragraph::new(visible_lines(lines)).render(area, &mut buffer);

    assert!(buffer.content.iter().all(|tile| tile.bg == Color::Black));
    assert_eq!(buffer[(0, 0)].fg, Color::Cyan);
}

#[test]
fn settled_tiles_briefly_glow_in_the_speakers_color() {
    for (role, motion_mode, expected) in [
        ("user", MotionMode::Animated, Some(Color::Cyan)),
        ("assistant", MotionMode::Animated, Some(Color::Magenta)),
        ("user", MotionMode::Reduced, None),
    ] {
        let cell = SplitFlapTranscriptCell::new(
            Box::new(PlainTranscriptCell("A".to_string())),
            role,
            "A",
            /*previous*/ None,
            motion_mode,
            FrameRequester::test_dummy(),
        );
        let tile_color = |elapsed| {
            cell.animate_lines(
                cell.inner.display_hyperlink_lines(/*width*/ 8),
                /*width*/ 8,
                elapsed,
            )[0]
            .line
            .spans
            .iter()
            .find(|span| span.content == "A")
            .and_then(|span| span.style.fg)
        };

        assert_eq!(tile_color(TILE_SETTLE_DURATION), expected);
        assert_eq!(
            tile_color(TILE_SETTLE_DURATION + TILE_AFTERGLOW_DURATION),
            (motion_mode == MotionMode::Animated).then_some(Color::Gray)
        );
    }
}

#[test]
fn reduced_motion_keeps_the_original_transcript_and_requests_no_frames() {
    let (frame_requester, mut requests) = FrameRequester::test_channel();
    let cell = SplitFlapTranscriptCell::new(
        Box::new(PlainTranscriptCell("GATE 73".to_string())),
        "user",
        "GATE 73",
        /*previous*/ None,
        MotionMode::Reduced,
        frame_requester,
    );

    assert_eq!(cell.display_lines(/*width*/ 16)[0].to_string(), "› GATE 73");
    assert_eq!(cell.transcript_animation_tick(), None);
    assert!(requests.try_recv().is_err());
}

#[test]
fn appended_transcript_keeps_the_previous_words_settled() {
    let mut first = cell("GATE", MotionMode::Animated);
    for arrival in &mut first.tile_arrivals {
        *arrival -= ANIMATION_DURATION;
    }
    let appended = SplitFlapTranscriptCell::new(
        Box::new(PlainTranscriptCell("GATE 73".to_string())),
        "user",
        "GATE 73",
        Some(&first),
        MotionMode::Animated,
        FrameRequester::test_dummy(),
    );

    assert!(frame(&appended, /*milliseconds*/ 0).starts_with("› GATE "));
    assert_eq!(appended.phase_started_at, first.phase_started_at);
    assert_eq!(appended.tile_arrivals[..4], first.tile_arrivals);
}

#[test]
fn unicode_graphemes_and_terminal_width_are_preserved() {
    let text = "a\u{301} 東京 👩‍💻 B";
    let cell = cell(text, MotionMode::Animated);
    let lines = cell.animate_lines(
        cell.inner.display_hyperlink_lines(/*width*/ 24),
        /*width*/ 24,
        Duration::ZERO,
    );

    assert!(lines[0].line.to_string().contains("a\u{301} 東京 👩‍💻"));
    assert_eq!(lines[0].width(), 24);
    assert_eq!(cell.raw_lines()[0].to_string(), text);
}

#[test]
fn settled_split_flap_stops_scheduling_animation_frames() {
    let (frame_requester, mut requests) = FrameRequester::test_channel();
    let mut cell = SplitFlapTranscriptCell::new(
        Box::new(PlainTranscriptCell("GATE".to_string())),
        "user",
        "GATE",
        /*previous*/ None,
        MotionMode::Animated,
        frame_requester,
    );
    assert!(requests.try_recv().is_ok());
    cell.started_at = Instant::now() - ANIMATION_DURATION;
    for arrival in &mut cell.tile_arrivals {
        *arrival -= ANIMATION_DURATION;
    }

    assert_eq!(cell.transcript_animation_tick(), None);
    assert_eq!(
        cell.display_lines(/*width*/ 12)[0].to_string().trim_end(),
        "› GATE"
    );
    assert!(requests.try_recv().is_err());
}
