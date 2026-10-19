//! Turn lifecycle inputs and scheduling phases for host-owned contributors.

use codex_protocol::config_types::CollaborationMode;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ThreadGoalStatus;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnAbortReason;

use crate::ExtensionData;

/// Identifies a successfully completed goal turn awaiting its native idle attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoalTurnContinuation {
    pub goal_id: String,
    pub turn_id: String,
}

/// Actual result of the native idle continuation attempt for one goal turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GoalContinuationDisposition {
    /// The last persisted goal status observed during the idle attempt.
    Status(ThreadGoalStatus),
    /// No persisted goal was present when the idle attempt reached the goal read.
    NoGoal,
    /// The idle path could not confirm the current goal state.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GoalContinuationAttemptResult {
    Started {
        continuation_turn_id: String,
    },
    NotSubmitted {
        disposition: GoalContinuationDisposition,
    },
    Skipped {
        disposition: GoalContinuationDisposition,
    },
    Failed {
        disposition: GoalContinuationDisposition,
    },
}

/// Turn-scoped result; callers must match both identifiers before treating it as accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoalContinuationAttempt {
    pub goal_id: String,
    pub origin_turn_id: String,
    pub result: GoalContinuationAttemptResult,
}

impl GoalContinuationAttempt {
    pub fn started_for(&self, continuation: &GoalTurnContinuation) -> bool {
        self.goal_id == continuation.goal_id
            && self.origin_turn_id == continuation.turn_id
            && matches!(
                &self.result,
                GoalContinuationAttemptResult::Started {
                    continuation_turn_id
                } if !continuation_turn_id.is_empty()
                    && continuation_turn_id != &self.origin_turn_id
            )
    }
}

/// Runs before task registration or during cancellable regular-task startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnStartPhase {
    BeforeTaskRegistration,
    RegularTaskStart,
}

/// Input supplied when the host starts a turn.
pub struct TurnStartInput<'a> {
    /// Stable host-owned turn identifier.
    pub turn_id: &'a str,
    /// Effective collaboration mode for this turn.
    pub collaboration_mode: &'a CollaborationMode,
    /// Total token usage snapshot captured when the turn started.
    /// Present before task registration; absent during regular-task preparation.
    /// Token-accounting contributors must remain in `BeforeTaskRegistration`.
    pub token_usage_at_turn_start: Option<&'a TokenUsage>,
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
    /// Original error details, including backend metadata omitted from the public category.
    pub error_details: &'a CodexErrorDetails,
    /// Store scoped to the host session runtime.
    pub session_store: &'a ExtensionData,
    /// Store scoped to this thread runtime.
    pub thread_store: &'a ExtensionData,
    /// Store scoped to this turn runtime.
    pub turn_store: &'a ExtensionData,
}
