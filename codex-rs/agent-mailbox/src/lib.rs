//! Durable, mailbox-owned capture and delivery state for native agent work.
//!
//! This crate deliberately has no dependency on `codex-core`. Adapters validate
//! trusted native-tree identity and perform canonical history writes; this crate
//! persists the accepted work, delivery intent, and exact observation state.

mod error;
mod activation;
mod messages;
mod message_delivery;
mod message_rules;
mod outcome;
mod schema;
mod snapshot;
mod store;
mod work;
mod types;

pub use error::{MailboxError, MailboxResult};
pub use store::Mailbox;
pub use snapshot::is_provably_closed_peer_cycle;
pub use types::{
    ActivationDecision, ActivationDisposition, AdmissionReceipt, Attention,
    CanonicalConfirmation, Continuation, ContinuationReceipt, ContinuationState, Dependency,
    DependencyMode, DispatchDecision,
    DispatchState, Envelope, GoalBinding, MessageDraft, MessageKind,
    NativeWork, ObservationTarget, Outcome, OutcomeReceipt,
    PendingPage, RequestDisposition, RequestSnapshot, ReturnWhen, Selection, Selector, Snapshot, SnapshotItem,
    SnapshotRequest, SnapshotState, TurnOutcomeReceipt, WorkRef, WorkSnapshot,
};

#[cfg(test)]
#[path = "mailbox_tests.rs"]
mod tests;
