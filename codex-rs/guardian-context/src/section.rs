//! Section identities survive consumer-specific transcript selection and rendering.

use crate::ConversationTranscriptEntry;
use crate::PlannedAction;
use crate::PreviousReviews;
use crate::TrustedTool;

/// Ordered evidence with a stable section identity and source-specific content.
///
/// Variants preserve provenance: transcript entries carry their original roles,
/// root messages remain line-role-labeled, and answers are host-verified fragments.
/// Conversation, authorization and action evidence retain user-role delivery.
/// Host-attested reviews and tool identities use separate developer messages.
/// Review actions/rationales and remote tool descriptions remain untrusted.
#[derive(Clone, Debug, PartialEq)]
pub enum ContextSection<T = ConversationTranscriptEntry> {
    ConversationTranscript { items: Vec<T> },
    RootConversation { items: Vec<String> },
    TrustedUserAnswers { items: Vec<String> },
    RetainedUserInstructions { items: Vec<String> },
    PlannedAction(PlannedAction),
    PreviousReviews(PreviousReviews),
    TrustedTool(TrustedTool),
    PermissionContext { items: Vec<String> },
}
