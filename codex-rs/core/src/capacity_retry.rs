use std::time::Duration;

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::util::backoff;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use tokio_util::sync::CancellationToken;
use tracing::warn;

const CAPACITY_RETRY_MAX_DELAY: Duration = Duration::from_secs(60);
const CAPACITY_RETRY_BACKOFF_CAP_ATTEMPT: u64 = 10;

pub(crate) fn capacity_retry_delay(attempt: u64) -> Duration {
    backoff(attempt.min(CAPACITY_RETRY_BACKOFF_CAP_ATTEMPT)).min(CAPACITY_RETRY_MAX_DELAY)
}

pub(crate) async fn notify_and_wait_for_capacity_retry(
    sess: &Session,
    turn_context: &TurnContext,
    cancellation_token: &CancellationToken,
    attempt: u64,
    operation: &str,
    err: CodexErr,
) -> CodexResult<()> {
    let delay = capacity_retry_delay(attempt);
    warn!(
        "selected model is at capacity - retrying {operation} (attempt {attempt} in {delay:?})...",
    );
    sess.notify_stream_error(
        turn_context,
        format!(
            "Model at capacity; retrying in {}s (attempt {attempt})",
            delay.as_secs().max(1)
        ),
        err,
    )
    .await;
    tokio::select! {
        _ = cancellation_token.cancelled() => Err(CodexErr::TurnAborted),
        _ = tokio::time::sleep(delay) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_retry_delay_is_capped() {
        assert!(capacity_retry_delay(/*attempt*/ 1) <= CAPACITY_RETRY_MAX_DELAY);
        assert!(
            capacity_retry_delay(CAPACITY_RETRY_BACKOFF_CAP_ATTEMPT + 50)
                <= CAPACITY_RETRY_MAX_DELAY
        );
    }
}
