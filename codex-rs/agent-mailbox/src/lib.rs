//! Durable, mailbox-owned capture and delivery state for native agent work.
//!
//! This crate deliberately has no dependency on `codex-core`. Adapters validate
//! trusted native-tree identity and perform canonical history writes; this crate
//! persists the accepted work, delivery intent, and exact observation state.

mod activation;
mod error;
mod message_delivery;
mod message_rules;
mod messages;
mod outcome;
mod outcome_messages;
mod outcome_store;
mod runtime_fault;
mod schema;
mod snapshot;
mod store;
mod types;
mod wait_graph;
mod work;

pub use error::{MailboxError, MailboxResult};
pub use snapshot::is_provably_closed_peer_cycle;
pub use store::Mailbox;
pub use types::{
    ActivationDecision, ActivationDisposition, AdmissionReceipt, Attention, CanonicalConfirmation,
    Continuation, ContinuationReceipt, ContinuationState, Dependency, DependencyMode,
    DispatchDecision, DispatchState, Envelope, GoalBinding, MessageDraft, MessageKind, NativeWork,
    ObservationTarget, Outcome, OutcomeReceipt, PendingPage, RecoveryEvidence, RequestDisposition,
    RequestSnapshot,
    ReturnWhen, Selection, Selector, Snapshot, SnapshotItem, SnapshotRequest, SnapshotState,
    TurnOutcomeReceipt, WorkRef, WorkSnapshot,
};
pub use wait_graph::InputArrival;
pub use wait_graph::WaitRegistration;

#[cfg(test)]
#[path = "mailbox_tests.rs"]
mod tests;
