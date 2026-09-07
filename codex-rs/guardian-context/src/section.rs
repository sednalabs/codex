//! Section identities survive consumer-specific transcript selection and rendering.

use crate::ConversationTranscriptEntry;
use crate::PlannedAction;
use crate::PreviousReviews;

/// Ordered evidence with a stable section identity and source-specific content.
///
/// Variants preserve provenance: transcript entries carry their original roles,
/// root messages remain line-role-labeled, and answers are host-verified fragments.
/// Conversation, authorization and action evidence retain user-role delivery.
/// Host-attested previous reviews use a separate developer message; their source
/// actions and rationales are explicitly not instructions or authorization.
#[derive(Clone, Debug, PartialEq)]
pub enum ContextSection<T = ConversationTranscriptEntry> {
    ConversationTranscript { items: Vec<T> },
    RootConversation { items: Vec<String> },
    TrustedUserAnswers { items: Vec<String> },
    RetainedUserInstructions { items: Vec<String> },
    PlannedAction(PlannedAction),
    PreviousReviews(PreviousReviews),
    PermissionContext { items: Vec<String> },
}
