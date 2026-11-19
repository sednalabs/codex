use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const MAX_DELAY: Duration = Duration::from_secs(60);
const MAX_BACKOFF_ATTEMPT: u64 = 10;

/// Keep selected-model retries patient while never retrying before server advice.
pub(crate) fn retry_deadline(error: &CodexErr, attempt: u64) -> Instant {
    let now = Instant::now();
    let local = now + crate::util::backoff(attempt.min(MAX_BACKOFF_ATTEMPT)).min(MAX_DELAY);
    error
        .retry_after()
        .map(codex_http_client::RetryAfter::deadline)
        .unwrap_or(local)
        .max(local)
}

pub(crate) async fn wait_for_capacity(
    retry_at: Instant,
    cancellation_token: &CancellationToken,
) -> CodexResult<()> {
    tokio::select! {
        biased;
        _ = cancellation_token.cancelled() => Err(CodexErr::TurnAborted),
        _ = tokio::time::sleep_until(retry_at) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_http_client::RetryAfter;
    use codex_protocol::error::CodexErrorDetails;

    #[tokio::test(start_paused = true)]
    async fn retry_deadline_honors_long_server_advice() {
        let advice = RetryAfter::from_delay(MAX_DELAY.saturating_mul(2)).expect("retry deadline");
        let server_deadline = advice.deadline();
        let error = CodexErr::ServerOverloaded.with_retry_after(advice);

        assert_eq!(retry_deadline(&error, /*attempt*/ 1), server_deadline);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_deadline_does_not_shorten_local_backoff_for_short_server_advice() {
        let advice = RetryAfter::from_delay(Duration::from_millis(1)).expect("retry deadline");
        let server_deadline = advice.deadline();
        let error = CodexErr::ServerOverloaded.with_retry_after(advice);
        let local_deadline = Instant::now() + crate::util::backoff(/*attempt*/ 1);
        let retry_at = retry_deadline(&error, /*attempt*/ 1);

        assert!(retry_at >= local_deadline);
        assert!(retry_at >= server_deadline);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_deadline_caps_local_backoff() {
        let now = Instant::now();
        let retry_at = retry_deadline(
            &CodexErr::ServerOverloaded,
            MAX_BACKOFF_ATTEMPT.saturating_add(50),
        );

        assert!(retry_at <= now + MAX_DELAY);
    }

    #[tokio::test(start_paused = true)]
    async fn capacity_wait_is_cancellable() {
        let cancellation_token = CancellationToken::new();
        let wait = wait_for_capacity(Instant::now() + MAX_DELAY, &cancellation_token);
        tokio::pin!(wait);
        assert!(futures::poll!(&mut wait).is_pending());

        cancellation_token.cancel();

        assert!(matches!(
            wait.await,
            Err(err) if matches!(err.details(), CodexErrorDetails::TurnAborted)
        ));
    }
}
