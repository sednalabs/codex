use super::PreviousSectionState;
use super::SectionTransition;
use super::WorldStateHash;
use super::WorldStateSection;
use super::WorldStateUpdate;
use crate::context::ContextualUserFragment;
use crate::context::MultiAgentRoleInstructions;
use crate::context::MultiAgentUsageHint;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ContentItemMetadata;
use codex_protocol::models::ContentItemNamespace;

/// Configured or model-owned multi-agent instructions currently visible to the model.
#[derive(Clone, Debug)]
pub(crate) struct MultiAgentUsageHintState {
    instructions: MultiAgentRoleInstructions,
    pub(super) fingerprint: WorldStateHash,
    client_owned: bool,
    namespace: Option<ContentItemNamespace>,
}

impl MultiAgentUsageHintState {
    pub(crate) fn new(instructions: MultiAgentRoleInstructions) -> Self {
        let fingerprint = WorldStateHash::from_fragment(&instructions);
        Self {
            instructions,
            fingerprint,
            client_owned: false,
            namespace: None,
        }
    }

    pub(crate) fn with_namespace(mut self, namespace: ContentItemNamespace) -> Self {
        self.namespace = Some(namespace);
        self
    }

    pub(crate) fn with_client_ownership(mut self, client_owned: bool) -> Self {
        self.client_owned = client_owned;
        self.fingerprint = WorldStateHash::from_fragment(&self);
        self
    }
}

impl ContextualUserFragment for MultiAgentUsageHintState {
    fn role(&self) -> &'static str {
        self.instructions.role()
    }

    fn content_kind(&self) -> ContentItemKind {
        let kind = match (self.client_owned, self.instructions.markers().0.is_empty()) {
            (true, true) => "client.multi_agent.usage_hint",
            (true, false) => "client.multi_agent.role_instructions",
            (false, true) => "multi_agent.usage_hint",
            (false, false) => "multi_agent.role_instructions",
        };
        ContentItemKind(kind.to_owned())
    }

    fn markers(&self) -> (&'static str, &'static str) {
        self.instructions.markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        MultiAgentRoleInstructions::type_markers()
    }

    fn body(&self) -> ContentItem {
        self.instructions.body()
    }
}

impl WorldStateSection for MultiAgentUsageHintState {
    const ID: &'static str = "multi_agent_usage_hint";
    type Snapshot = WorldStateHash;

    fn matches_current_legacy_fragment(&self, role: &str, text: &str) -> bool {
        role == self.instructions.role()
            && text == self.instructions.render().into_text().unwrap_or_default()
    }

    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Self::Snapshot>,
    ) -> SectionTransition<Self::Snapshot> {
        let fragment: Option<Box<dyn ContextualUserFragment>> = match previous {
            PreviousSectionState::Known(previous) if previous == &self.fingerprint => {
                return (None, Vec::new());
            }
            PreviousSectionState::Unknown => None,
            PreviousSectionState::Known(_) | PreviousSectionState::Absent => {
                if self.instructions.markers().0.is_empty() {
                    Some(Box::new(
                        MultiAgentUsageHint::new(
                            &self.instructions.body().into_text().unwrap_or_default(),
                        )
                        .with_client_ownership(self.client_owned)
                        .with_metadata(ContentItemMetadata::tool(self.namespace.clone())),
                    ))
                } else {
                    Some(Box::new(self.clone().with_metadata(
                        ContentItemMetadata::tool(self.namespace.clone()),
                    )))
                }
            }
        };
        (
            Some(self.fingerprint.clone()),
            WorldStateUpdate::optional_standalone_boxed_fragment(fragment),
        )
    }
}
