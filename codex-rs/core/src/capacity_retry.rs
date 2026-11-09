//! Selected-model capacity retry timing and user notification.

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::util::backoff;
use codex_protocol::error::CodexErr;
use std::time::Duration;
use tokio::time::Instant;

const MAX_DELAY: Duration = Duration::from_secs(60);
const MAX_BACKOFF_ATTEMPT: u64 = 10;

pub(crate) fn retry_deadline(error: &CodexErr, attempt: u64) -> Instant {
    let now = Instant::now();
    let local = now + backoff(attempt.min(MAX_BACKOFF_ATTEMPT)).min(MAX_DELAY);
    error
        .retry_after()
        .map(|advice| advice.deadline())
        .unwrap_or(local)
        .max(local)
}

/// The caller owns cancellation of this wait along with its surrounding operation.
pub(crate) async fn wait_for_capacity(
    session: &Session,
    turn: &TurnContext,
    error: CodexErr,
    attempt: u64,
    operation: &str,
) {
    let retry_at = retry_deadline(&error, attempt);
    let delay = retry_at.saturating_duration_since(Instant::now());
    tracing::warn!(turn_id = %turn.sub_id, attempt, ?delay, operation, "model at capacity; retrying");
    session
        .notify_stream_error(
            turn,
            format!(
                "Model at capacity; retrying in {}s (attempt {attempt})",
                delay.as_secs().max(1)
            ),
            error,
        )
        .await;
    tokio::time::sleep_until(retry_at).await;
}
