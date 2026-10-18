use super::PreviousSectionState;
use super::SectionTransition;
use super::WorldStateHash;
use super::WorldStateSection;
use super::WorldStateUpdate;
use super::multi_agent_usage_hint::MultiAgentUsageHintState;
use crate::context::ContextualUserFragment;
use crate::context::multi_agent_mode_instructions::MultiAgentModeInstructions;
use codex_protocol::config_types::MultiAgentMode;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::truncate_text;
use serde::Deserialize;
use serde::Serialize;

const MULTI_AGENT_MODE_MAX_TOKENS: usize = 400;

/// Effective multi-agent mode currently visible to the model.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct MultiAgentModeState {
    mode: Option<MultiAgentMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    usage_hint_hash: Option<WorldStateHash>,
    #[serde(skip)]
    client_owned: bool,
}

impl MultiAgentModeState {
    pub(crate) fn new(mode: Option<MultiAgentMode>) -> Self {
        Self {
            mode: mode.map(|mode| match mode {
                MultiAgentMode::Custom(hint_text) => MultiAgentMode::Custom(truncate_text(
                    &hint_text,
                    TruncationPolicy::Tokens(MULTI_AGENT_MODE_MAX_TOKENS),
                )),
                mode @ (MultiAgentMode::ExplicitRequestOnly | MultiAgentMode::Proactive) => mode,
            }),
            usage_hint_hash: None,
            client_owned: false,
        }
    }

    pub(crate) fn with_client_ownership(mut self, client_owned: bool) -> Self {
        self.client_owned = client_owned;
        self
    }

    pub(crate) fn with_usage_hint(mut self, usage_hint: &MultiAgentUsageHintState) -> Self {
        self.usage_hint_hash = Some(usage_hint.fingerprint.clone());
        self
    }
}

impl WorldStateSection for MultiAgentModeState {
    const ID: &'static str = "multi_agent_mode";
    type Snapshot = Self;

    fn matches_legacy_fragment(role: &str, text: &str) -> bool {
        role == "developer" && MultiAgentModeInstructions::matches_text(text)
    }

    fn has_retained_fragment_matcher() -> bool {
        true
    }

    fn matches_retained_fragment(role: &str, text: &str) -> bool {
        Self::matches_legacy_fragment(role, text)
    }

    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Self::Snapshot>,
    ) -> SectionTransition<Self::Snapshot> {
        let mode = match (&self.mode, previous) {
            (Some(mode), PreviousSectionState::Known(previous))
                if previous.mode.as_ref() == Some(mode)
                    && previous.usage_hint_hash == self.usage_hint_hash =>
            {
                return (None, Vec::new());
            }
            (Some(mode), _) => mode.clone(),
            (None, PreviousSectionState::Known(previous))
                if previous.mode == Some(MultiAgentMode::Proactive) =>
            {
                MultiAgentMode::ExplicitRequestOnly
            }
            (None, PreviousSectionState::Unknown) => MultiAgentMode::ExplicitRequestOnly,
            (None, PreviousSectionState::Absent | PreviousSectionState::Known(_)) => {
                return (Some(self.clone()), Vec::new());
            }
        };

        let fragment = MultiAgentModeInstructions::from_mode(mode)
            .map(|instructions| instructions.with_client_ownership(self.client_owned))
            .map(|instructions| Box::new(instructions) as Box<dyn ContextualUserFragment>);
        (
            Some(self.clone()),
            WorldStateUpdate::optional_boxed_fragment(fragment),
        )
    }
}

#[cfg(test)]
#[path = "multi_agent_mode_tests.rs"]
mod tests;

#[cfg(test)]
mod instruction_overlay_origin_tests {
    use super::*;
    use crate::context::world_state::WorldStateUpdateContent;

    #[test]
    fn positive_mode_kind_does_not_change_snapshot_shape_or_repeat_after_snapshot_roundtrip() {
        let state = MultiAgentModeState::new(Some(MultiAgentMode::Custom("guidance".into())))
            .with_client_ownership(/*client_owned*/ true);
        let (snapshot, updates) = state.render_diff(PreviousSectionState::Absent);
        let WorldStateUpdateContent::Fragment(fragment) = &updates[0].content else {
            panic!("mode fragment expected");
        };
        assert_eq!(
            fragment.content_kind().as_str(),
            "client.multi_agent.mode_instructions"
        );
        let value = serde_json::to_value(snapshot.unwrap()).unwrap();
        assert!(value.get("client_owned").is_none());
        let previous = serde_json::from_value(value).unwrap();
        assert!(
            state
                .render_diff(PreviousSectionState::Known(&previous))
                .1
                .is_empty()
        );
        let unclassified =
            MultiAgentModeState::new(Some(MultiAgentMode::Custom("guidance".into())));
        let (_, updates) = unclassified.render_diff(PreviousSectionState::Absent);
        let WorldStateUpdateContent::Fragment(fragment) = &updates[0].content else {
            panic!("mode fragment expected");
        };
        assert_eq!(
            fragment.content_kind().as_str(),
            "multi_agent.mode_instructions"
        );
    }
}
