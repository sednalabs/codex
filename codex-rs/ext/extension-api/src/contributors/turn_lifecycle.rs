use codex_protocol::ThreadId;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::error::CodexErrKind;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnAbortReason;
use std::time::Duration;

use crate::ExtensionData;

/// Marks one host turn whose terminal owner reduction is deferred while an
/// extension preserves the same owner for provider-scoped continuation.
///
/// The host continues to persist and emit the original terminal protocol
/// events. It only defers the derived agent-status update and parent result
/// notification for this exact turn.
#[derive(Debug)]
pub struct OwnerContinuationPending;

/// Marks a provider-limited turn whose queued input must remain pending until a new
/// authoritative provider admission exists. Unlike [`OwnerContinuationPending`], this does not
/// preserve terminal status; it only prevents the same regular task from sampling again.
#[derive(Debug)]
pub struct OwnerContinuationDeferred;

/// Marks the next automatic goal continuation turn for the bounded V2 health check.
///
/// This thread-scoped activation is consumed once when Core creates the turn; prompt text is
/// never used as a control marker.
#[derive(Debug)]
pub struct GoalContinuationHealthCheck;

/// Provider-declared rate-limit evidence for one exact thread and turn.
///
/// `None` means unavailable; Core must not infer account identity, shared quota, or
/// independence from parentage when a provider does not report it.
#[derive(Clone, Debug, PartialEq)]
pub struct RateLimitDomain {
    pub thread_id: ThreadId,
    pub provider_id: Option<String>,
    pub requested_model: Option<String>,
    pub effective_model: Option<String>,
    pub account_context_key: Option<String>,
    pub shared_quota_key: Option<String>,
    pub snapshot: Option<RateLimitSnapshot>,
    pub reset_at: Option<String>,
    pub retry_after: Option<Duration>,
}

/// Input supplied when the host starts a turn.
pub struct TurnStartInput<'a> {
    /// Stable host-owned turn identifier.
    pub turn_id: &'a str,
    /// Effective collaboration mode for this turn.
    pub collaboration_mode: &'a CollaborationMode,
    /// Total token usage snapshot captured when the turn started.
    pub token_usage_at_turn_start: &'a TokenUsage,
    /// Store scoped to the host session runtime.
    pub session_store: &'a ExtensionData,
    /// Store scoped to this thread runtime.
    pub thread_store: &'a ExtensionData,
    /// Store scoped to this turn runtime.
    pub turn_store: &'a ExtensionData,
}

/// Input supplied when the host completes a turn.
pub struct TurnStopInput<'a> {
    /// Store scoped to the host session runtime.
    pub session_store: &'a ExtensionData,
    /// Store scoped to this thread runtime.
    pub thread_store: &'a ExtensionData,
    /// Store scoped to this turn runtime.
    pub turn_store: &'a ExtensionData,
}

/// Input supplied when the host aborts a turn.
pub struct TurnAbortInput<'a> {
    /// Reason the host aborted the turn.
    pub reason: TurnAbortReason,
    /// Store scoped to the host session runtime.
    pub session_store: &'a ExtensionData,
    /// Store scoped to this thread runtime.
    pub thread_store: &'a ExtensionData,
    /// Store scoped to this turn runtime.
    pub turn_store: &'a ExtensionData,
}

/// Input supplied when the host observes an error for a turn.
pub struct TurnErrorInput<'a> {
    /// Stable host-owned turn identifier.
    pub turn_id: &'a str,
    /// Error surfaced by the host for this turn.
    pub error: CodexErrorInfo,
    /// Exact semantic error kind when the host still has the original provider error.
    /// `None` means the protocol-facing error was synthesized without enough source context;
    /// extensions must fail closed rather than infer a retryable provider denial from it.
    pub error_kind: Option<CodexErrKind>,
    /// Provider-specified delay before a rate-limited request may be retried.
    /// `None` means the provider did not establish an eligible retry time.
    pub rate_limit_retry_after: Option<Duration>,
    /// Exact-thread provider evidence associated with this error. Unknown scope remains `None`.
    pub rate_limit_domain: RateLimitDomain,
    /// Store scoped to the host session runtime.
    pub session_store: &'a ExtensionData,
    /// Store scoped to this thread runtime.
    pub thread_store: &'a ExtensionData,
    /// Store scoped to this turn runtime.
    pub turn_store: &'a ExtensionData,
}
