use super::*;
use pretty_assertions::assert_eq;

#[test]
fn finalized_plan_reuses_lines_primed_by_transcript_height() {
    let cell = new_proposed_plan("1. Inspect **markdown**".to_string(), Path::new("/tmp"));
    let width = 48;

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
    assert!(cell.desired_transcript_height(width) > 0);
=======
    assert_eq!(cell.desired_transcript_height(width), 5);
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
    cell.rendered_lines
        .cached
        .lock()
        .expect("render cache lock")
        .as_mut()
        .expect("render cache should be populated")
        .1 = vec![HyperlinkLine::from("cached")];

    assert_eq!(
        visible_lines(cell.transcript_hyperlink_lines(width)),
        vec![Line::from("cached")]
    );
}

#[test]
fn finalized_plan_file_citation_renders_as_local_path_snapshot() {
    let cwd = std::env::temp_dir();
    let output = cwd.join("Quarterly Report.xlsx").display().to_string();
    let plan = new_proposed_plan(
        format!(
            "- :codex-file-citation{{path=\"{output}\" purpose=\"output\" artifact_kind=\"workbook\"}}\n"
        ),
        &cwd,
    );

    let rendered = ratatui::text::Text::from(plan.display_lines(/*width*/ 80));

    insta::assert_snapshot!(rendered, @"• Proposed Plan\n \n \n  - Quarterly Report.xlsx");
}
