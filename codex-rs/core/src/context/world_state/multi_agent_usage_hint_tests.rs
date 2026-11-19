use super::MultiAgentUsageHintState;
use super::WorldStateHash;
use super::WorldStateSection;
use super::PreviousSectionState;
use crate::context::ContextualUserFragment;
use crate::context::MultiAgentRoleInstructions;
use crate::context::world_state::test_support::FragmentSectionTestExt as _;

fn instructions() -> MultiAgentRoleInstructions {
    MultiAgentRoleInstructions::Configured("same instructions".to_string())
}

fn emitted_content_kind(
    current: &MultiAgentUsageHintState,
    previous: &WorldStateHash,
) -> String {
    current
        .render_fragment_diff(PreviousSectionState::Known(previous))
        .1
        .expect("ownership transition should re-emit the instructions")
        .content_kind()
        .0
}

#[test]
fn identical_text_ownership_transitions_reemit_the_new_classification() {
    let unowned = MultiAgentUsageHintState::new(instructions());
    let owned = MultiAgentUsageHintState::new(instructions()).with_client_ownership(true);

    assert_ne!(unowned.fingerprint, owned.fingerprint);
    assert_eq!(
        emitted_content_kind(&unowned, &owned.fingerprint),
        "multi_agent.usage_hint"
    );
    assert_eq!(
        emitted_content_kind(&owned, &unowned.fingerprint),
        "client.multi_agent.usage_hint"
    );
}

#[test]
fn origin_free_legacy_hash_reemits_for_both_ownership_values() {
    let instructions = instructions();
    let legacy_fingerprint = WorldStateHash::from_fragment(&instructions);

    for client_owned in [false, true] {
        let current = MultiAgentUsageHintState::new(instructions.clone())
            .with_client_ownership(client_owned);
        assert!(
            current
                .render_fragment_diff(PreviousSectionState::Known(&legacy_fingerprint))
                .1
                .is_some(),
            "legacy origin-free snapshot must not match client_owned={client_owned}"
        );
    }
}
