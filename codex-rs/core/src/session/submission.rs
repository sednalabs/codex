//! Core-owned queue metadata; forwarding transfers the residency guard with the operation.

use crate::client::ProviderAuthority;
use codex_extension_api::ExtensionDataInit;
use codex_protocol::automatic_turn::AutomaticTurnContinuationRef;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::W3cTraceContext;
use tokio::sync::OwnedRwLockReadGuard;

#[derive(Debug)]
pub(crate) struct Submission {
    pub id: String,
    pub op: Op,
    /// Accepted with settings, never separately from the request that supplied them.
    pub turn_extension_init: Option<ExtensionDataInit>,
    /// Optional W3C trace carrier propagated across async submission handoffs.
    pub trace: Option<W3cTraceContext>,
    pub parent_turn_id: Option<String>,
    pub root_turn_id: Option<String>,
    /// Internal authenticated app-server provenance, never serialized or written to rollout.
    pub automatic_turn: Option<AutomaticTurnContext>,
    /// Keeps a V2 recipient resident until this submission is handled or dropped.
    pub residency_guard: Option<OwnedRwLockReadGuard<()>>,
}

#[derive(Clone, Debug)]
pub(crate) struct AutomaticTurnContext {
    /// Stable, non-secret identity reconstructed from the currently authenticated provider state.
    /// It is persisted only as a server-side ticket binding; unavailable identity fails closed.
    pub principal_identity: Option<String>,
    pub client_user_message_id: Option<String>,
    pub provider_authority: Option<ProviderAuthority>,
    /// Durable State epochs captured when this submission was admitted.
    pub expected_epochs: Option<(i64, i64)>,
    /// Server-owned in-memory context for a redeemed one-time ticket; never serialized into the
    /// UserMessage or rollout history.
    pub continuation_ref: Option<AutomaticTurnContinuationRef>,
}
