use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::AcquireError;
use tokio::sync::Semaphore;
use tokio::sync::SemaphorePermit;

const MCP_LIVENESS_RETRY_INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MCP_LIVENESS_RETRY_MAX_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Default)]
struct McpLivenessProbeState {
    last_turn_id: Option<String>,
    consecutive_closed_results: u32,
    retry_not_before: Option<Instant>,
}

/// Owns MCP invalidation and the single gate used to publish runtime updates.
pub(super) struct McpRefresh {
    pending: AtomicBool,
    gate: Semaphore,
    liveness_probe: Mutex<McpLivenessProbeState>,
}

impl McpRefresh {
    pub(super) fn new() -> Self {
        Self {
            pending: AtomicBool::new(false),
            gate: Semaphore::new(/*permits*/ 1),
            liveness_probe: Mutex::new(McpLivenessProbeState::default()),
        }
    }

    pub(super) fn invalidate(&self) {
        self.pending.store(true, Ordering::Release);
    }

    pub(super) fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }

    pub(super) fn claim(&self) -> bool {
        self.pending.swap(false, Ordering::AcqRel)
    }

    pub(super) async fn acquire(&self) -> Result<SemaphorePermit<'_>, AcquireError> {
        self.gate.acquire().await
    }

    pub(super) fn close(&self) {
        self.gate.close();
    }

    /// Returns whether this is the first permitted MCP liveness probe for a turn.
    pub(super) fn should_probe_liveness(&self, turn_id: &str) -> bool {
        self.should_probe_liveness_at(turn_id, Instant::now())
    }

    fn should_probe_liveness_at(&self, turn_id: &str, now: Instant) -> bool {
        let mut state = self
            .liveness_probe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.last_turn_id.as_deref() == Some(turn_id) {
            return false;
        }
        state.last_turn_id = Some(turn_id.to_string());
        !state
            .retry_not_before
            .is_some_and(|retry_not_before| now < retry_not_before)
    }

    /// Records one liveness result and bounds repeated crash/restart attempts.
    pub(super) fn record_liveness_result(&self, has_closed_connections: bool) {
        self.record_liveness_result_at(has_closed_connections, Instant::now());
    }

    fn record_liveness_result_at(&self, has_closed_connections: bool, now: Instant) {
        let mut state = self
            .liveness_probe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if has_closed_connections {
            state.consecutive_closed_results = state.consecutive_closed_results.saturating_add(1);
            state.retry_not_before =
                Some(now + mcp_liveness_retry_backoff(state.consecutive_closed_results));
        } else {
            state.consecutive_closed_results = 0;
            state.retry_not_before = None;
        }
    }
}

fn mcp_liveness_retry_backoff(consecutive_closed_results: u32) -> Duration {
    let exponent = consecutive_closed_results.saturating_sub(1).min(5);
    MCP_LIVENESS_RETRY_INITIAL_BACKOFF
        .saturating_mul(1 << exponent)
        .min(MCP_LIVENESS_RETRY_MAX_BACKOFF)
}

/// Restores a claimed refresh when its task is cancelled before publication.
pub(super) struct McpRefreshInvalidationGuard<'a> {
    pub(super) refresh: &'a McpRefresh,
    pub(super) published: bool,
}

impl Drop for McpRefreshInvalidationGuard<'_> {
    fn drop(&mut self) {
        if !self.published {
            self.refresh.invalidate();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn liveness_probe_runs_once_per_turn_and_backs_off_repeated_closed_results() {
        let refresh = McpRefresh::new();
        let started_at = Instant::now();

        assert!(refresh.should_probe_liveness_at("turn-1", started_at));
        assert!(!refresh.should_probe_liveness_at("turn-1", started_at));
        refresh.record_liveness_result_at(/*has_closed_connections*/ true, started_at);

        assert!(
            !refresh.should_probe_liveness_at("turn-2", started_at + Duration::from_millis(999))
        );
        assert!(refresh.should_probe_liveness_at("turn-3", started_at + Duration::from_secs(1)));
        refresh.record_liveness_result_at(
            /*has_closed_connections*/ true,
            started_at + Duration::from_secs(1),
        );

        assert!(
            !refresh.should_probe_liveness_at("turn-4", started_at + Duration::from_millis(2999))
        );
        assert!(refresh.should_probe_liveness_at("turn-5", started_at + Duration::from_secs(3)));
    }

    #[test]
    fn healthy_liveness_result_clears_crash_backoff() {
        let refresh = McpRefresh::new();
        let started_at = Instant::now();

        assert!(refresh.should_probe_liveness_at("turn-1", started_at));
        refresh.record_liveness_result_at(/*has_closed_connections*/ true, started_at);
        refresh.record_liveness_result_at(
            /*has_closed_connections*/ false,
            started_at + Duration::from_millis(1),
        );

        assert!(refresh.should_probe_liveness_at("turn-2", started_at + Duration::from_millis(2)));
    }

    #[test]
    fn liveness_retry_backoff_is_capped() {
        assert_eq!(
            mcp_liveness_retry_backoff(/*consecutive_closed_results*/ 1),
            MCP_LIVENESS_RETRY_INITIAL_BACKOFF
        );
        assert_eq!(
            mcp_liveness_retry_backoff(/*consecutive_closed_results*/ 2),
            Duration::from_secs(2)
        );
        assert_eq!(
            mcp_liveness_retry_backoff(u32::MAX),
            MCP_LIVENESS_RETRY_MAX_BACKOFF
        );
    }
}
