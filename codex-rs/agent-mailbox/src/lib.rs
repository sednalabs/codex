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

pub use error::MailboxError;
pub use error::MailboxResult;
pub use snapshot::is_provably_closed_peer_cycle;
pub use store::Mailbox;
pub use types::ActivationDecision;
pub use types::ActivationDisposition;
pub use types::AdmissionReceipt;
pub use types::Attention;
pub use types::CanonicalConfirmation;
pub use types::Continuation;
pub use types::ContinuationReceipt;
pub use types::ContinuationState;
pub use types::Dependency;
pub use types::DependencyMode;
pub use types::DispatchDecision;
pub use types::DispatchState;
pub use types::Envelope;
pub use types::GoalBinding;
pub use types::MessageDraft;
pub use types::MessageKind;
pub use types::NativeWork;
pub use types::ObservationTarget;
pub use types::Outcome;
pub use types::OutcomeReceipt;
pub use types::PendingPage;
pub use types::RecoveryEvidence;
pub use types::RequestDisposition;
pub use types::RequestSnapshot;
pub use types::ReturnWhen;
pub use types::Selection;
pub use types::Selector;
pub use types::Snapshot;
pub use types::SnapshotItem;
pub use types::SnapshotRequest;
pub use types::SnapshotState;
pub use types::TurnOutcomeReceipt;
pub use types::WorkRef;
pub use types::WorkSnapshot;
pub use wait_graph::InputArrival;
pub use wait_graph::WaitRegistration;

#[cfg(test)]
#[path = "mailbox_tests.rs"]
mod tests;
