use super::*;
use crate::analytics_chats::LocalUsageThread;
use pretty_assertions::assert_eq;

#[test]
fn local_lineage_distinguishes_root_subagent_and_unknown_credit_coverage() {
    let mut items = Vec::new();
    append_local_usage(
        &mut items,
        Some(LocalUsage {
            threads: vec![
                LocalUsageThread {
                    kind: "Root".to_string(),
                    models: Some("model-a".to_string()),
                    estimated_credits: Some(1.25),
                    partial: false,
                },
                LocalUsageThread {
                    kind: "Subagent".to_string(),
                    models: None,
                    estimated_credits: None,
                    partial: true,
                },
            ],
            truncated: true,
        }),
    );
    assert_eq!(
        items
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        vec![
            "  Local Root · model-a",
            "  Local Subagent · model unavailable",
            "  Additional local threads omitted",
        ]
    );
    assert_eq!(
        items[0].description.as_deref(),
        Some("Estimated thread credits (may overcount): 1.250000")
    );
    assert_eq!(
        items[1].description.as_deref(),
        Some("Estimated thread credits (may overcount): unavailable · partial")
    );
}

#[test]
fn credit_millionths_are_rendered_without_float_rounding() {
    assert_eq!(format_micros(/*value*/ 1_250_000), "1.250000");
    assert_eq!(format_micros(/*value*/ -1), "-0.000001");
    assert_eq!(format_micros(i64::MIN), "-9223372036854.775808");
}
