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

/// Host-local identity for the request that produced a lifecycle event.
///
/// These values come from local configuration and resolution. They are not claims about
/// provider-observed identity, account scope, quota scope, or model handling.
#[derive(Clone, Debug, PartialEq)]
pub struct LocalRequestIdentity {
    pub thread_id: ThreadId,
    pub configured_provider_key: Option<String>,
    pub requested_model: Option<String>,
    pub resolved_model: Option<String>,
}

/// Authority for rate-limit facts carried by [`ProviderLimitEvidence`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderEvidenceAuthority {
    UnknownUnsupportedTransport,
    UnknownLostProvenance,
    /// The API bridge decoded an HTTP 429 with `error.type == "usage_limit_reached"`.
    ///
    /// This identifies only the recognized parser path. It does not establish provider,
    /// account, quota, model, or retry authority.
    RecognizedHttpUsageLimit,
}

/// Provider-limit facts associated with one exact request.
///
/// Reset, snapshot, and retry-denial values are evidence only when paired with an explicit
/// authority. They do not establish account identity, shared quota scope, or provider/model
/// identity.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderLimitEvidence {
    pub authority: ProviderEvidenceAuthority,
    pub snapshot: Option<RateLimitSnapshot>,
    pub reset_at: Option<String>,
    pub retry_after: Option<Duration>,
}

/// Local request identity and separately-authorized provider-limit evidence.
#[derive(Clone, Debug, PartialEq)]
pub struct RateLimitDomain {
    pub local_request_identity: LocalRequestIdentity,
    pub provider_limit_evidence: ProviderLimitEvidence,
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
    /// Local request correlation plus authority-tagged provider-limit evidence associated with
    /// this error. Provider-limit evidence may be explicitly unknown; this is not a claim of
    /// exact-thread provider evidence.
    pub rate_limit_domain: RateLimitDomain,
    /// Store scoped to the host session runtime.
    pub session_store: &'a ExtensionData,
    /// Store scoped to this thread runtime.
    pub thread_store: &'a ExtensionData,
    /// Store scoped to this turn runtime.
    pub turn_store: &'a ExtensionData,
}
