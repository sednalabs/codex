use super::super::PreviousSectionState;
use super::super::test_support::render_section_cases;
use super::*;
use anyhow::Result;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::NetworkSandboxPolicy;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn subagent_context_preserves_ordinary_rows() {
    let mut builder = SubagentContextBuilder::default();
    assert!(builder.push(SubagentContextRow::new("agent-1", Some("atlas"))));
    assert!(builder.push(SubagentContextRow::new("agent-2", /*nickname*/ None,)));

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
            /*nickname*/ None,
        )));
    }
    assert!(!row_capped.push(SubagentContextRow::new(
        "worker-over-cap",
        /*nickname*/ None,
    )));
    row_capped.note_omitted(/*count*/ 1);
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
            ("laptop".to_string(), primary("file:///repo", "zsh")?),
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
            ("laptop".to_string(), primary("file:///repo", "bash")?),
            ("devbox".to_string(), starting("file:///workspace")?),
            ("old".to_string(), available("file:///old", "sh")?),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let after_environment_changes = EnvironmentsState {
        environments: [
            ("laptop".to_string(), primary("file:///repo", "zsh")?),
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
                error: None,
                shell: None,
                is_primary: false,
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

#[test]
fn changing_primary_environment_updates_model_context_and_persisted_state() -> Result<()> {
    let before = EnvironmentsState {
        environments: [
            ("local".to_string(), primary("file:///local", "bash")?),
            ("remote".to_string(), available("file:///remote", "zsh")?),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let after = EnvironmentsState {
        environments: [
            ("local".to_string(), available("file:///local", "bash")?),
            ("remote".to_string(), primary("file:///remote", "zsh")?),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let previous = before.snapshot();
    let rendered = after
        .render_diff(PreviousSectionState::Known(&previous))
        .expect("primary change should update the model")
        .render();

    assert_eq!(
        rendered,
        format!(
            "<environment_context>\n  <environments>\n    <environment id=\"local\" primary=\"false\">\n      <cwd>{}</cwd>\n      <shell>bash</shell>\n    </environment>\n    <environment id=\"remote\" primary=\"true\">\n      <cwd>{}</cwd>\n      <shell>zsh</shell>\n    </environment>\n  </environments>\n</environment_context>",
            PathUri::parse("file:///local")?.inferred_native_path_string(),
            PathUri::parse("file:///remote")?.inferred_native_path_string(),
        )
    );

    let mut previous_world_state = super::super::WorldState::default();
    previous_world_state.add_section(before);
    let mut current_world_state = super::super::WorldState::default();
    current_world_state.add_section(after);
    assert_eq!(
        current_world_state
            .snapshot()
            .merge_patch_from(&previous_world_state.snapshot())
            .map(serde_json::Value::Object),
        Some(json!({
            "environments": {
                "environments": {
                    "local": { "is_primary": null },
                    "remote": { "is_primary": true },
                },
            },
        }))
    );

    Ok(())
}

#[test]
fn legacy_single_environment_snapshot_does_not_change() -> Result<()> {
    let environment = EnvironmentsState {
        environments: [("local".to_string(), primary("file:///repo", "bash")?)]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let legacy_snapshot = serde_json::from_value::<EnvironmentsSnapshot>(json!({
        "environments": {
            "local": {
                "cwd": PathUri::parse("file:///repo")?.inferred_native_path_string(),
                "status": "available",
                "shell": "bash",
            },
        },
    }))?;

    assert!(
        environment
            .render_diff(PreviousSectionState::Known(&legacy_snapshot))
            .is_none()
    );
    assert_eq!(
        serde_json::to_value(environment.snapshot())?["environments"]["local"],
        json!({
            "cwd": PathUri::parse("file:///repo")?.inferred_native_path_string(),
            "status": "available",
            "shell": "bash",
        })
    );

    Ok(())
}

#[test]
fn crossing_single_environment_boundary_restates_current_environments() -> Result<()> {
    let single = EnvironmentsState {
        environments: [("local".to_string(), primary("file:///local", "bash")?)]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let local_cwd = PathUri::parse("file:///local")?.inferred_native_path_string();
    let remote_cwd = PathUri::parse("file:///remote")?.inferred_native_path_string();

    for (local_is_primary, remote_is_primary) in [(true, false), (false, true)] {
        let multiple = EnvironmentsState {
            environments: [
                (
                    "local".to_string(),
                    EnvironmentState {
                        is_primary: local_is_primary,
                        ..available("file:///local", "bash")?
                    },
                ),
                (
                    "remote".to_string(),
                    EnvironmentState {
                        is_primary: remote_is_primary,
                        ..available("file:///remote", "zsh")?
                    },
                ),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };

        let expanded = multiple
            .render_diff(PreviousSectionState::Known(&single.snapshot()))
            .expect("adding an environment should update the model")
            .render();
        assert_eq!(
            expanded,
            format!(
                "<environment_context>\n  <environments>\n    <environment id=\"local\" primary=\"{local_is_primary}\">\n      <cwd>{local_cwd}</cwd>\n      <shell>bash</shell>\n    </environment>\n    <environment id=\"remote\" primary=\"{remote_is_primary}\">\n      <cwd>{remote_cwd}</cwd>\n      <shell>zsh</shell>\n    </environment>\n  </environments>\n</environment_context>"
            )
        );

        let reduced = single
            .render_diff(PreviousSectionState::Known(&multiple.snapshot()))
            .expect("removing an environment should update the model")
            .render();
        assert_eq!(
            reduced,
            format!(
                "<environment_context>\n  <environments>\n    <environment id=\"local\" primary=\"true\">\n      <cwd>{local_cwd}</cwd>\n      <shell>bash</shell>\n    </environment>\n    <environment id=\"remote\" status=\"unavailable\" />\n  </environments>\n</environment_context>"
            )
        );
    }

    Ok(())
}

fn available(cwd: &str, shell: &str) -> Result<EnvironmentState> {
    Ok(EnvironmentState {
        cwd: PathUri::parse(cwd)?,
        status: EnvironmentStatus::Available,
        error: None,
        shell: Some(shell.to_string()),
        is_primary: false,
    })
}

fn primary(cwd: &str, shell: &str) -> Result<EnvironmentState> {
    Ok(EnvironmentState {
        is_primary: true,
        ..available(cwd, shell)?
    })
}

fn starting(cwd: &str) -> Result<EnvironmentState> {
    Ok(EnvironmentState {
        cwd: PathUri::parse(cwd)?,
        status: EnvironmentStatus::Starting,
        error: None,
        shell: None,
        is_primary: false,
    })
}

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
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
=======
#[test]
fn failure_context_is_escaped_incremental_and_cleared_on_recovery() -> Result<()> {
    let failed = EnvironmentsState {
        environments: [(
            "remote".to_string(),
            EnvironmentState {
                cwd: PathUri::parse("file:///workspace")?,
                status: EnvironmentStatus::Failed,
                error: Some("Repository <empty> & unavailable".to_string()),
                shell: None,
                is_primary: false,
            },
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    insta::assert_snapshot!(failed.body(), @r#"

      <environments>
        <environment id="remote">
          <cwd>/workspace</cwd>
          <status>failed</status>
          <error>Repository &lt;empty&gt; &amp; unavailable</error>
        </environment>
      </environments>
    "#);
    assert!(
        failed
            .render_diff(PreviousSectionState::Known(&failed.snapshot()))
            .is_none()
    );
    let recovered = EnvironmentsState {
        environments: [(
            "remote".to_string(),
            available("file:///workspace", "bash")?,
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let update = recovered
        .render_diff(PreviousSectionState::Known(&failed.snapshot()))
        .expect("recovery must be visible to the model");
    assert!(!update.body().contains("<error>"));
    assert!(update.body().contains("<shell>bash</shell>"));
    Ok(())
}

#[test]
fn failure_context_limits_total_detail_bytes_at_utf8_boundaries() {
    use codex_protocol::protocol::EnvironmentConfigState;
    use codex_protocol::protocol::TurnEnvironmentSelection;
    let snapshot = TurnEnvironmentSnapshot {
        environments: (0..4)
            .map(|index| TurnEnvironmentState::Failed {
                selection: TurnEnvironmentSelection {
                    environment_id: format!("remote-{index}"),
                    cwd: PathUri::parse("file:///workspace").unwrap(),
                    workspace_roots: Vec::new(),
                    config: EnvironmentConfigState::FromThread,
                },
                error: "界".repeat(300),
            })
            .collect(),
    };
    let states = environment_states(&snapshot);
    let details: Vec<_> = states
        .values()
        .map(|state| state.error.as_deref())
        .collect();
    let truncated = "界".repeat(85);
    assert_eq!(
        details,
        vec![
            Some(truncated.as_str()),
            Some(truncated.as_str()),
            None,
            None
        ]
    );
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
}
