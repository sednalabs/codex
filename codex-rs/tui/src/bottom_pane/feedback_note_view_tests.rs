use super::*;
use pretty_assertions::assert_eq;

fn render(view: &FeedbackNoteView, width: u16) -> String {
    let height = view.desired_height(width);
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    view.render(area, &mut buf);
    render_buffer(area, &buf)
}

fn render_buffer(area: Rect, buf: &Buffer) -> String {
    let mut lines: Vec<String> = (0..area.height)
        .map(|row| {
            let mut line = String::new();
            for col in 0..area.width {
                let symbol = buf[(area.x + col, area.y + row)].symbol();
                if symbol.is_empty() {
                    line.push(' ');
                } else {
                    line.push_str(symbol);
                }
            }
            line.trim_end().to_string()
        })
        .collect();

    while lines.first().is_some_and(|l| l.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

fn make_view(category: FeedbackCategory) -> FeedbackNoteView {
    let (tx_raw, _rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let tx = AppEventSender::new(tx_raw);
    FeedbackNoteView::new(
        category, /*turn_id*/ None, tx, /*include_logs*/ true,
    )
}

#[test]
fn feedback_view_bad_result() {
    let view = make_view(FeedbackCategory::BadResult);
    let rendered = render(&view, /*width*/ 60);
    insta::assert_snapshot!("feedback_view_bad_result", rendered);
}

#[test]
fn feedback_view_good_result() {
    let view = make_view(FeedbackCategory::GoodResult);
    let rendered = render(&view, /*width*/ 60);
    insta::assert_snapshot!("feedback_view_good_result", rendered);
}

#[test]
fn feedback_view_bug() {
    let view = make_view(FeedbackCategory::Bug);
    let rendered = render(&view, /*width*/ 60);
    insta::assert_snapshot!("feedback_view_bug", rendered);
}

#[test]
fn feedback_view_other() {
    let view = make_view(FeedbackCategory::Other);
    let rendered = render(&view, /*width*/ 60);
    insta::assert_snapshot!("feedback_view_other", rendered);
}

#[test]
fn feedback_view_safety_check() {
    let view = make_view(FeedbackCategory::SafetyCheck);
    let rendered = render(&view, /*width*/ 60);
    insta::assert_snapshot!("feedback_view_safety_check", rendered);
}

#[test]
fn feedback_view_with_connectivity_diagnostics() {
    let (tx_raw, _rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let tx = AppEventSender::new(tx_raw);
    let view = FeedbackNoteView::new(
        FeedbackCategory::Bug,
        /*turn_id*/ None,
        tx,
        /*include_logs*/ false,
    );
    let rendered = render(&view, /*width*/ 60);

    insta::assert_snapshot!("feedback_view_with_connectivity_diagnostics", rendered);
}

#[test]
fn submit_feedback_emits_submit_event_with_trimmed_note() {
    let (tx_raw, mut rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let tx = AppEventSender::new(tx_raw);
    let mut view = FeedbackNoteView::new(
        FeedbackCategory::Bug,
        Some("turn-123".to_string()),
        tx,
        /*include_logs*/ true,
    );
    view.textarea.insert_str("  something broke  ");

    view.submit();

    let event = rx.try_recv().expect("submit feedback event");
    assert!(matches!(
        event,
        AppEvent::SubmitFeedback {
            category: FeedbackCategory::Bug,
            reason: Some(reason),
            turn_id: Some(turn_id),
            include_logs: true,
        } if reason == "something broke" && turn_id == "turn-123"
    ));
    assert_eq!(view.is_complete(), true);
}

#[test]
fn submit_feedback_omits_empty_note() {
    let (tx_raw, mut rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    let tx = AppEventSender::new(tx_raw);
    let mut view = FeedbackNoteView::new(
        FeedbackCategory::GoodResult,
        /*turn_id*/ None,
        tx,
        /*include_logs*/ false,
    );

    view.submit();

    let event = rx.try_recv().expect("submit feedback event");
    assert!(matches!(
        event,
        AppEvent::SubmitFeedback {
            category: FeedbackCategory::GoodResult,
            reason: None,
            turn_id: None,
            include_logs: false,
        }
    ));
}
