use super::super::PreviousSectionState;
use super::super::test_support::render_section_cases;
use super::*;
use anyhow::Result;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::NetworkSandboxPolicy;

#[test]
fn subagent_context_preserves_ordinary_rows() {
    let mut builder = SubagentContextBuilder::default();
    assert!(builder.push(SubagentContextRow::new("agent-1", Some("atlas"))));
    assert!(builder.push(SubagentContextRow::new("agent-2", None)));

    assert_eq!(builder.finish().as_str(), "- agent-1: atlas\n- agent-2");
}

#[test]
fn subagent_context_replaces_xml_invalid_controls() {
    let mut builder = SubagentContextBuilder::default();
    assert!(builder.push(SubagentContextRow::new(
        "agent\0one\u{1}two",
        Some("nick\u{1}name\0end"),
    )));

    assert_eq!(
        builder.finish().as_str(),
        "- agent\u{FFFD}one\u{FFFD}two: nick\u{FFFD}name\u{FFFD}end"
    );
}

#[test]
fn subagent_context_normalizes_allowed_xml_whitespace() {
    let mut builder = SubagentContextBuilder::default();
    assert!(builder.push(SubagentContextRow::new(
        "agent\tone\ntwo",
        Some("nick\tname\nend"),
    )));

    assert_eq!(builder.finish().as_str(), "- agent one two: nick name end");
}

#[test]
fn subagent_context_escapes_normalizes_and_truncates_dynamic_fields() {
    let hostile = format!("  <agent attr=\"x\">\n{}", "<&\"' oversized ".repeat(100));
    let mut builder = SubagentContextBuilder::default();
    assert!(builder.push(SubagentContextRow::new(
        hostile.as_str(),
        Some(hostile.as_str()),
    )));
    let rendered = builder.finish();

    assert!(
        rendered
            .as_str()
            .contains("&lt;agent attr=&quot;x&quot;&gt;")
    );
    assert!(rendered.as_str().contains("&amp;"));
    assert!(rendered.as_str().contains("&apos;"));
    assert!(!rendered.as_str().contains('\n'));
    assert!(rendered.as_str().ends_with("..."));
    assert!(
        rendered.as_str().len()
            <= 2 + SUBAGENT_REFERENCE_MAX_ESCAPED_BYTES + 2 + SUBAGENT_NICKNAME_MAX_ESCAPED_BYTES
    );
}

#[test]
fn subagent_context_enforces_row_and_total_byte_caps_with_omitted_count() {
    let mut row_capped = SubagentContextBuilder::default();
    for index in 0..SUBAGENT_CONTEXT_MAX_ROWS {
        assert!(row_capped.push(SubagentContextRow::new(
            format!("worker-{index}").as_str(),
            None,
        )));
    }
    assert!(!row_capped.push(SubagentContextRow::new("worker-over-cap", None)));
    row_capped.note_omitted(1);
    let row_capped = row_capped.finish();
    assert_eq!(
        row_capped.as_str().matches("- worker-").count(),
        SUBAGENT_CONTEXT_MAX_ROWS
    );
    assert!(row_capped.as_str().ends_with("<omitted count=\"1\" />"));

    let total_rows = 100;
    let mut builder = SubagentContextBuilder::default();
    let mut omitted = 0;
    for index in 0..total_rows {
        let row = SubagentContextRow::new(
            format!("worker-{index}-{}", "x".repeat(400)).as_str(),
            Some("y".repeat(400).as_str()),
        );
        if !builder.push(row) {
            omitted = total_rows - index;
            break;
        }
    }
    builder.note_omitted(omitted);
    let subagents = builder.finish();
    let row_count = subagents.as_str().matches("- worker-").count();

    assert!(row_count < SUBAGENT_CONTEXT_MAX_ROWS);
    assert_eq!(omitted, total_rows - row_count);
    assert!(
        subagents
            .as_str()
            .contains(&format!("<omitted count=\"{omitted}\" />"))
    );
    assert!(
        subagent_context_rendered_bytes(subagents.as_str()) <= SUBAGENT_CONTEXT_MAX_RENDERED_BYTES
    );
}

#[test]
fn newly_visible_subagents_render_a_diff() {
    let before = EnvironmentsState::default();
    let after = environment_with_subagent("agent-1", Some("atlas"));

    assert_eq!(
        render_environment_diff(&before, &after).as_deref(),
        Some(
            "<environment_context>\n  <subagents>\n    - agent-1: atlas\n  </subagents>\n</environment_context>"
        )
    );
}

#[test]
fn changed_subagents_render_current_rows() {
    let before = environment_with_subagent("agent-1", Some("atlas"));
    let after = environment_with_subagent("agent-2", Some("borealis"));

    assert_eq!(
        render_environment_diff(&before, &after).as_deref(),
        Some(
            "<environment_context>\n  <subagents>\n    - agent-2: borealis\n  </subagents>\n</environment_context>"
        )
    );
}

#[test]
fn removed_subagents_render_an_explicit_clear() {
    let before = environment_with_subagent("agent-1", Some("atlas"));
    let after = EnvironmentsState::default();

    assert_eq!(
        render_environment_diff(&before, &after).as_deref(),
        Some(
            "<environment_context>\n  <subagents status=\"unavailable\" />\n</environment_context>"
        )
    );
}

#[test]
fn unchanged_subagents_do_not_render_a_diff() {
    let before = environment_with_subagent("agent-1", Some("atlas"));
    let after = environment_with_subagent("agent-1", Some("atlas"));

    assert_eq!(render_environment_diff(&before, &after), None);
}

#[test]
fn snapshots() -> Result<()> {
    use PreviousSectionState::Absent;
    use PreviousSectionState::Known;
    use PreviousSectionState::Unknown;

    let full = EnvironmentsState {
        environments: [
            ("laptop".to_string(), available("file:///repo", "zsh")?),
            (
                "devbox".to_string(),
                available("file:///workspace", "bash")?,
            ),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let before_environment_changes = EnvironmentsState {
        environments: [
            ("laptop".to_string(), available("file:///repo", "bash")?),
            ("devbox".to_string(), starting("file:///workspace")?),
            ("old".to_string(), available("file:///old", "sh")?),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let after_environment_changes = EnvironmentsState {
        environments: [
            ("laptop".to_string(), available("file:///repo", "zsh")?),
            (
                "devbox".to_string(),
                available("file:///workspace", "powershell")?,
            ),
            ("remote".to_string(), starting("file:///remote")?),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let environments = EnvironmentsState {
        environments: [(
            LOCAL_ENVIRONMENT_ID.to_string(),
            available("file:///repo", "zsh")?,
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let before_turn_context_changes = EnvironmentsState {
        current_date: Some("2026-06-19".to_string()),
        timezone: Some("UTC".to_string()),
        network: Some(NetworkContext::new(
            vec!["old.example.com".to_string()],
            vec![],
        )),
        filesystem: Some(FileSystemContext::from_permission_profile(
            &PermissionProfile::Disabled,
            &[],
        )),
        ..environments.clone()
    };
    let after_turn_context_changes = EnvironmentsState {
        current_date: Some("2026-06-20".to_string()),
        timezone: Some("America/Los_Angeles".to_string()),
        network: Some(NetworkContext::new(
            vec!["new.example.com".to_string()],
            vec!["blocked.example.com".to_string()],
        )),
        filesystem: Some(FileSystemContext::from_permission_profile(
            &PermissionProfile::External {
                network: NetworkSandboxPolicy::Restricted,
            },
            &[],
        )),
        ..environments
    };
    let foreign_windows = EnvironmentsState {
        environments: [(
            "remote".to_string(),
            available("file:///C:/windows", "powershell")?,
        )]
        .into_iter()
        .collect(),
        filesystem: Some(FileSystemContext::from_permission_profile(
            &PermissionProfile::Disabled,
            &[],
        )),
        ..Default::default()
    };
    let unknown_shell = EnvironmentsState {
        environments: [(
            LOCAL_ENVIRONMENT_ID.to_string(),
            EnvironmentState {
                cwd: PathUri::parse("file:///repo")?,
                status: EnvironmentStatus::Available,
                shell: None,
            },
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let known_shell = EnvironmentsState {
        environments: [(
            LOCAL_ENVIRONMENT_ID.to_string(),
            available("file:///repo", "zsh")?,
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let legacy_environment = EnvironmentsState {
        environments: [(
            LOCAL_ENVIRONMENT_ID.to_string(),
            available("file:///repo", "bash")?,
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let empty = EnvironmentsState::default();

    insta::assert_snapshot!(render_section_cases(&[
        (Absent, Absent),
        (Absent, Known(&full)),
        (Unknown, Known(&full)),
        (
            Known(&before_environment_changes),
            Known(&after_environment_changes),
        ),
        (
            Known(&before_turn_context_changes),
            Known(&after_turn_context_changes),
        ),
        (Absent, Known(&foreign_windows)),
        (Known(&unknown_shell), Known(&known_shell)),
        (Known(&legacy_environment), Known(&empty)),
    ]));
    Ok(())
}

fn available(cwd: &str, shell: &str) -> Result<EnvironmentState> {
    Ok(EnvironmentState {
        cwd: PathUri::parse(cwd)?,
        status: EnvironmentStatus::Available,
        shell: Some(shell.to_string()),
    })
}

fn starting(cwd: &str) -> Result<EnvironmentState> {
    Ok(EnvironmentState {
        cwd: PathUri::parse(cwd)?,
        status: EnvironmentStatus::Starting,
        shell: None,
    })
}

fn environment_with_subagent(reference: &str, nickname: Option<&str>) -> EnvironmentsState {
    let mut builder = SubagentContextBuilder::default();
    assert!(builder.push(SubagentContextRow::new(reference, nickname)));
    EnvironmentsState::default().with_subagents(builder.finish())
}

fn render_environment_diff(
    before: &EnvironmentsState,
    after: &EnvironmentsState,
) -> Option<String> {
    let previous = before.snapshot();
    after
        .render_diff(PreviousSectionState::Known(&previous))
        .map(|fragment| fragment.render())
}
