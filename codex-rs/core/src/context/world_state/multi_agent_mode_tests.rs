use super::super::test_support::render_section_cases;
use super::*;
use crate::context::MultiAgentRoleInstructions;
use crate::context::world_state::WorldState;
use crate::context::world_state::test_support::FragmentSectionTestExt as _;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ResponseItem;
use codex_protocol_test_support::ContentItemTestExt;
use codex_utils_output_truncation::approx_token_count;
use pretty_assertions::assert_eq;

fn state(mode: Option<MultiAgentMode>) -> MultiAgentModeState {
    MultiAgentModeState::new(mode)
}

fn client_owned_world_state(client_owned: bool) -> WorldState {
    let mut world_state = WorldState::default();
    world_state
        .add_section(state(Some(MultiAgentMode::Proactive)).with_client_ownership(client_owned));
    world_state
}

fn persisted_mode_snapshot_and_history(
    client_owned: bool,
) -> (
    crate::context::world_state::WorldStateSnapshot,
    Vec<ResponseItem>,
) {
    let world_state = client_owned_world_state(client_owned);
    let (snapshot, fragments) = world_state.render_full_fragments();
    let snapshot = serde_json::from_value(serde_json::to_value(snapshot).unwrap()).unwrap();
    let history = fragments
        .into_iter()
        .map(ContextualUserFragment::into_boxed_response_item)
        .collect();
    (snapshot, history)
}

fn assert_single_mode_fragment_ownership(
    fragments: &[Box<dyn ContextualUserFragment>],
    client_owned: bool,
) {
    assert_eq!(fragments.len(), 1);
    assert_eq!(
        fragments[0].content_kind(),
        ContentItemKind(
            if client_owned {
                "client.multi_agent.mode_instructions"
            } else {
                "multi_agent.mode_instructions"
            }
            .to_string()
        )
    );
}

#[test]
fn snapshots() {
    use PreviousSectionState::Absent;
    use PreviousSectionState::Known;
    use PreviousSectionState::Unknown;

    let inactive = state(/*mode*/ None);
    let explicit = state(Some(MultiAgentMode::ExplicitRequestOnly));
    let proactive = state(Some(MultiAgentMode::Proactive));
    let custom = state(Some(MultiAgentMode::Custom(
        "use a custom policy".to_string(),
    )));
    let empty = state(Some(MultiAgentMode::Custom(String::new())));

    insta::assert_snapshot!(render_section_cases(&[
        (Absent, Absent),
        (Absent, Known(&inactive)),
        (Absent, Known(&explicit)),
        (Known(&explicit), Known(&explicit)),
        (Known(&explicit), Known(&proactive)),
        (Known(&proactive), Known(&inactive)),
        (Known(&explicit), Known(&inactive)),
        (Known(&explicit), Known(&custom)),
        (Known(&custom), Known(&empty)),
        (Unknown, Known(&explicit)),
        (Unknown, Known(&inactive)),
    ]));
}

#[test]
fn persisted_mode_is_restored_only_when_missing_from_history() {
    let state = state(Some(MultiAgentMode::ExplicitRequestOnly));
    let retained: ResponseItem = ContextualUserFragment::into(
        MultiAgentModeInstructions::from_mode(MultiAgentMode::ExplicitRequestOnly)
            .expect("explicit mode should render"),
    );
    let mut world_state = WorldState::default();
    world_state.add_section(state);
    let snapshot = world_state.render_full().0;

    assert_eq!(
        world_state
            .render_history_fragment_diff(/*previous*/ None, std::slice::from_ref(&retained))
            .1
            .len(),
        1,
    );
    assert_eq!(
        world_state
            .render_history_fragment_diff(Some(&snapshot), &[])
            .1
            .len(),
        1
    );
    assert!(
        world_state
            .render_history_fragment_diff(Some(&snapshot), &[retained])
            .1
            .is_empty()
    );
}

#[test]
fn client_owned_mode_snapshot_roundtrip_does_not_duplicate_fragment() {
    let (previous_snapshot, history) =
        persisted_mode_snapshot_and_history(/*client_owned*/ true);
    let serialized = serde_json::to_value(&previous_snapshot).unwrap();
    assert_eq!(
        serialized["multi_agent_mode"]["client_owned"],
        serde_json::Value::Bool(true)
    );

    let (next_snapshot, updates) = client_owned_world_state(/*client_owned*/ true)
        .render_history_fragment_diff(Some(&previous_snapshot), &history);

    assert!(updates.is_empty());
    assert_eq!(next_snapshot, previous_snapshot);
}

#[test]
fn client_owned_mode_snapshot_transition_tracks_current_provenance() {
    for (previous_owned, current_owned) in [(false, true), (true, false)] {
        let (previous_snapshot, history) = persisted_mode_snapshot_and_history(previous_owned);
        let (_, updates) = client_owned_world_state(current_owned)
            .render_history_fragment_diff(Some(&previous_snapshot), &history);
        assert_single_mode_fragment_ownership(&updates, current_owned);
    }
}

#[test]
fn client_owned_mode_snapshot_renders_when_retained_fragment_is_missing() {
    let (previous_snapshot, _) = persisted_mode_snapshot_and_history(/*client_owned*/ true);
    let (_, updates) = client_owned_world_state(/*client_owned*/ true)
        .render_history_fragment_diff(Some(&previous_snapshot), &[]);

    assert_single_mode_fragment_ownership(&updates, /*client_owned*/ true);
}

#[test]
fn client_owned_mode_snapshot_without_field_defaults_and_rerenders() {
    let (previous_snapshot, history) =
        persisted_mode_snapshot_and_history(/*client_owned*/ true);
    let mut serialized = serde_json::to_value(previous_snapshot).unwrap();
    serialized["multi_agent_mode"]
        .as_object_mut()
        .expect("mode snapshot should be an object")
        .remove("client_owned");
    let previous_snapshot = serde_json::from_value(serialized).unwrap();

    let (_, updates) = client_owned_world_state(/*client_owned*/ true)
        .render_history_fragment_diff(Some(&previous_snapshot), &history);

    assert_single_mode_fragment_ownership(&updates, /*client_owned*/ true);
}

/// Active mode instructions must follow a newly migrated multi-agent usage hint.
#[test]
fn unchanged_mode_is_reemitted_after_usage_hint_migration() {
    let previous = state(Some(MultiAgentMode::Proactive));
    let current = MultiAgentModeState::new(Some(MultiAgentMode::Proactive)).with_usage_hint(
        &MultiAgentUsageHintState::new(MultiAgentRoleInstructions::Configured(
            "Current usage instructions.".to_string(),
        )),
    );

    let instructions = current
        .render_fragment_diff(PreviousSectionState::Known(&previous))
        .1
        .expect("unchanged mode should follow migrated usage instructions");

    assert_eq!(
        instructions.render().expect_text(),
        MultiAgentModeInstructions::from_mode(MultiAgentMode::Proactive)
            .expect("proactive mode should render")
            .render()
            .expect_text()
    );
}

#[test]
fn catalog_role_updates_remain_separate_from_active_mode() {
    let catalog_role = |base: &str| MultiAgentRoleInstructions::Composed {
        base: base.to_string(),
        marked: true,
        omit_update_plan_instructions: false,
        max_concurrency: 2,
        wait_agent_enabled: false,
        expose_model_overrides: false,
    };
    let previous_hint = MultiAgentUsageHintState::new(catalog_role("Previous role."));
    let previous_mode =
        MultiAgentModeState::new(Some(MultiAgentMode::Proactive)).with_usage_hint(&previous_hint);
    let mut previous = WorldState::default();
    previous.add_section(previous_hint);
    previous.add_section(previous_mode);
    let (previous_snapshot, previous_fragments) = previous.render_full_fragments();
    let history = previous_fragments
        .into_iter()
        .map(ContextualUserFragment::into_boxed_response_item)
        .collect::<Vec<_>>();

    let current_role = catalog_role("Current role.");
    let current_hint = MultiAgentUsageHintState::new(current_role.clone());
    let current_mode =
        MultiAgentModeState::new(Some(MultiAgentMode::Proactive)).with_usage_hint(&current_hint);
    let mut current = WorldState::default();
    current.add_section(current_hint);
    current.add_section(current_mode);

    let updates = crate::context_manager::updates::merge_world_state_updates(
        current
            .render_history_diff(Some(&previous_snapshot), &history)
            .1,
    );
    let expected_mode = MultiAgentModeInstructions::from_mode(MultiAgentMode::Proactive)
        .expect("proactive mode should render");
    assert_eq!(
        updates,
        vec![
            ContextualUserFragment::into(current_role.with_metadata(
                codex_protocol::models::ContentItemMetadata::tool(/*namespace*/ None)
            )),
            ContextualUserFragment::into(expected_mode.with_metadata(
                codex_protocol::models::ContentItemMetadata::tool(/*namespace*/ None)
            )),
        ],
    );
}

#[test]
fn custom_mode_is_bounded_before_snapshot_and_rendering() {
    let state = state(Some(MultiAgentMode::Custom("custom mode ".repeat(1_000))));
    let Some(MultiAgentMode::Custom(snapshot_mode)) = state
        .render_fragment_diff(PreviousSectionState::Absent)
        .0
        .unwrap()
        .mode
    else {
        panic!("expected custom multi-agent mode")
    };
    assert!(approx_token_count(&snapshot_mode) < 1_000);

    let rendered = state
        .render_fragment_diff(PreviousSectionState::Absent)
        .1
        .expect("custom mode should render")
        .render()
        .expect_text();
    assert!(approx_token_count(&rendered) < 1_000);
}
