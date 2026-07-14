use super::*;
use crate::context::ContextualUserFragment;
use crate::context::world_state::WorldState;
use anyhow::Result;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_protocol::models::ContentItem;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::permissions::NetworkSandboxPolicy;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn renders_full_environment_state() -> Result<()> {
    let context = EnvironmentsState {
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

    let mut world_state = WorldState::default();
    world_state.add_section(context);

    assert_eq!(
        vec![user_message(
            r#"<environment_context>
  <environments>
    <environment id="devbox">
      <cwd>/workspace</cwd>
      <shell>bash</shell>
    </environment>
    <environment id="laptop">
      <cwd>/repo</cwd>
      <shell>zsh</shell>
    </environment>
  </environments>
</environment_context>"#,
        )],
        render_fragments(world_state.render_full()),
    );
    Ok(())
}

#[test]
fn renders_only_changed_environments() -> Result<()> {
    let mut previous = WorldState::default();
    previous.add_section(EnvironmentsState {
        environments: [
            ("laptop".to_string(), available("file:///repo", "bash")?),
            ("devbox".to_string(), starting("file:///workspace")?),
            ("old".to_string(), available("file:///old", "sh")?),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    });
    let mut current = WorldState::default();
    current.add_section(EnvironmentsState {
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
    });

    assert_eq!(
        vec![user_message(
            r#"<environment_context>
  <environments>
    <environment id="devbox">
      <cwd>/workspace</cwd>
      <shell>powershell</shell>
    </environment>
    <environment id="laptop">
      <cwd>/repo</cwd>
      <shell>zsh</shell>
    </environment>
    <environment id="old" status="unavailable" />
    <environment id="remote">
      <cwd>/remote</cwd>
      <status>starting</status>
    </environment>
  </environments>
</environment_context>"#,
        )],
        render_fragments(current.render_diff(&previous.snapshot())),
    );
    Ok(())
}

#[test]
fn persisted_turn_context_values_render_a_diff() -> Result<()> {
    let environments = EnvironmentsState {
        environments: [(
            LOCAL_ENVIRONMENT_ID.to_string(),
            available("file:///repo", "zsh")?,
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let mut previous = WorldState::default();
    previous.add_section(EnvironmentsState {
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
    });
    let mut current = WorldState::default();
    current.add_section(EnvironmentsState {
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
    });

    assert_eq!(
        vec![user_message(
            r#"<environment_context>
  <current_date>2026-06-20</current_date>
  <timezone>America/Los_Angeles</timezone>
  <network enabled="true"><allowed>new.example.com</allowed><denied>blocked.example.com</denied></network>
  <filesystem><permission_profile type="external"><file_system type="external" /></permission_profile></filesystem>
</environment_context>"#,
        )],
        render_fragments(current.render_diff(&previous.snapshot())),
    );
    Ok(())
}

#[test]
fn persisted_snapshot_uses_model_visible_path_and_context_values() -> Result<()> {
    let mut world_state = WorldState::default();
    world_state.add_section(EnvironmentsState {
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
    });

    assert_eq!(
        serde_json::to_value(world_state.snapshot())?,
        json!({
            "environments": {
                "environments": {
                    "remote": {
                        "cwd": "C:\\windows",
                        "status": "available",
                        "shell": "powershell"
                    }
                },
                "filesystem": "<filesystem><permission_profile type=\"disabled\"><file_system type=\"unrestricted\" /></permission_profile></filesystem>"
            }
        }),
    );
    Ok(())
}

#[test]
fn single_environment_diff_ignores_unknown_shell() -> Result<()> {
    let previous = EnvironmentsState {
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
    let current = EnvironmentsState {
        environments: [(
            LOCAL_ENVIRONMENT_ID.to_string(),
            available("file:///repo", "zsh")?,
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let previous = WorldStateSection::snapshot(&previous);

    assert_eq!(
        None,
        render_fragment(WorldStateSection::render_diff(
            &current,
            PreviousSectionState::Known(&previous),
        ))
    );
    Ok(())
}

#[test]
fn removed_legacy_environment_renders_unavailable() -> Result<()> {
    let previous = EnvironmentsState {
        environments: [(
            LOCAL_ENVIRONMENT_ID.to_string(),
            available("file:///repo", "bash")?,
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let previous = WorldStateSection::snapshot(&previous);

    assert_eq!(
        Some(user_message(
            r#"<environment_context>
  <environments>
    <environment id="local" status="unavailable" />
  </environments>
</environment_context>"#,
        )),
        render_fragment(WorldStateSection::render_diff(
            &EnvironmentsState::default(),
            PreviousSectionState::Known(&previous),
        )),
    );
    Ok(())
}

#[test]
fn newly_visible_subagents_render_a_diff() {
    let previous = WorldStateSection::snapshot(&EnvironmentsState::default());
    let current = EnvironmentsState::default().with_subagents(subagent_context("model-a"));

    assert_eq!(
        Some(user_message(
            r#"<environment_context>
  <subagents>
    <agent name="worker" model="model-a" source="thread_config_snapshot" />
  </subagents>
</environment_context>"#,
        )),
        render_fragment(WorldStateSection::render_diff(
            &current,
            PreviousSectionState::Known(&previous),
        )),
    );
}

#[test]
fn changed_subagent_identity_renders_the_new_identity() {
    let previous = EnvironmentsState::default().with_subagents(subagent_context("model-a"));
    let previous = WorldStateSection::snapshot(&previous);
    let current = EnvironmentsState::default().with_subagents(subagent_context("model-b"));

    assert_eq!(
        Some(user_message(
            r#"<environment_context>
  <subagents>
    <agent name="worker" model="model-b" source="thread_config_snapshot" />
  </subagents>
</environment_context>"#,
        )),
        render_fragment(WorldStateSection::render_diff(
            &current,
            PreviousSectionState::Known(&previous),
        )),
    );
}

#[test]
fn removed_subagents_render_an_explicit_clear() {
    let previous = EnvironmentsState::default().with_subagents(subagent_context("model-a"));
    let previous = WorldStateSection::snapshot(&previous);

    assert_eq!(
        Some(user_message(
            r#"<environment_context>
  <subagents status="unavailable" />
</environment_context>"#,
        )),
        render_fragment(WorldStateSection::render_diff(
            &EnvironmentsState::default(),
            PreviousSectionState::Known(&previous),
        )),
    );
}

#[test]
fn unchanged_subagents_do_not_duplicate_context() {
    let current = EnvironmentsState::default().with_subagents(subagent_context("model-a"));
    let previous = WorldStateSection::snapshot(&current);

    assert_eq!(
        None,
        render_fragment(WorldStateSection::render_diff(
            &current,
            PreviousSectionState::Known(&previous),
        )),
    );
}

#[test]
fn subagent_context_enforces_row_field_and_total_caps() {
    let hostile = format!("  <escape attr=\"x\">\n{}", "<&\"' oversized ".repeat(400));
    let total_rows = 100;
    let mut builder = SubagentContextBuilder::default();
    let mut omitted = 0;
    for index in 0..total_rows {
        let row = SubagentContextRow::new(
            format!("worker-{index}-{hostile}").as_str(),
            Some(hostile.as_str()),
            Some(hostile.as_str()),
            Some(hostile.as_str()),
            Some(hostile.as_str()),
            Some(hostile.as_str()),
            hostile.as_str(),
        );
        if !builder.push(row) {
            omitted = total_rows - index;
            break;
        }
    }
    builder.note_omitted(omitted);
    let subagents = builder.finish();
    let row_count = subagents.as_str().matches("<agent ").count();

    assert!(row_count <= SUBAGENT_CONTEXT_MAX_ROWS);
    assert_eq!(omitted, total_rows - row_count);
    assert!(
        subagents
            .as_str()
            .contains(&format!("<omitted count=\"{omitted}\" />"))
    );
    assert!(subagents.as_str().contains("&lt;escape"));
    assert!(subagents.as_str().contains("&quot;"));
    assert!(!subagents.as_str().contains("\n<escape"));

    let rendered = EnvironmentsState::default()
        .with_subagents(subagents)
        .render();
    let start = rendered.find("  <subagents>").expect("subagents start");
    let end = rendered.find("  </subagents>").expect("subagents end") + "  </subagents>\n".len();
    assert!(
        end - start <= SUBAGENT_CONTEXT_MAX_RENDERED_BYTES,
        "subagent section exceeded hard cap: {} bytes",
        end - start
    );
}

fn subagent_context(model: &str) -> SubagentContext {
    let mut builder = SubagentContextBuilder::default();
    assert!(builder.push(SubagentContextRow::new(
        "worker",
        /*nickname*/ None,
        Some(model),
        /*effective_model_provider_id*/ None,
        /*effective_reasoning_effort*/ None,
        /*effective_service_tier*/ None,
        "thread_config_snapshot",
    )));
    builder.finish()
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

fn render_fragments(fragments: Vec<Box<dyn ContextualUserFragment>>) -> Vec<ResponseItem> {
    fragments
        .into_iter()
        .map(ContextualUserFragment::into_boxed_response_item)
        .collect()
}

fn render_fragment(fragment: Option<Box<dyn ContextualUserFragment>>) -> Option<ResponseItem> {
    fragment.map(ContextualUserFragment::into_boxed_response_item)
}

fn user_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}
