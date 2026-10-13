use super::ResponsesStreamRequest;
use super::ResponsesStreamRetryState;
use super::capacity_backoff;
use super::handle_response_stream_error;
use super::is_transient_capacity_availability;
use super::log_retry;
use super::MAX_CAPACITY_RETRY_DELAY;
use crate::realtime_history::RealtimeHistoryState;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use codex_http_client::RetryAfter;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Http503SlowDown;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing_test::internal::MockWriter;

#[test]
fn capacity_retry_requires_positive_source_marker_and_expected_public_detail() {
    let positive = CodexErr::new(CodexErrorDetails::RateLimitExceeded("later".into()))
        .with_http_503_slow_down(Http503SlowDown::classified_http_503_slow_down());
    assert!(is_transient_capacity_availability(&positive));

    let generic_rate_limit = CodexErr::new(CodexErrorDetails::RateLimitExceeded("later".into()));
    assert!(!is_transient_capacity_availability(&generic_rate_limit));
    let marked_other_error = CodexErr::ServerOverloaded
        .with_http_503_slow_down(Http503SlowDown::classified_http_503_slow_down());
    assert!(!is_transient_capacity_availability(&marked_other_error));
}

#[test]
fn capacity_retry_backoff_stays_capped_for_unbounded_attempt_counts() {
    for attempt in [1, 10, 11, 100, u64::MAX] {
        let delay = capacity_backoff(attempt);
        assert!(delay >= Duration::from_millis(900));
        assert!(delay <= MAX_CAPACITY_RETRY_DELAY);
    }
}

#[test]
fn capacity_retry_first_backoff_respects_jitter_range() {
    let delay = capacity_backoff(1);
    assert!(delay >= Duration::from_millis(900));
    assert!(delay <= Duration::from_millis(1_100));
}

#[tokio::test]
async fn sampling_retry_logs_stream_error_context() {
    let (_session, turn_context) = make_session_and_context().await;
    let buffer: &'static std::sync::Mutex<Vec<u8>> =
        Box::leak(Box::new(std::sync::Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(MockWriter::new(buffer))
        .finish();
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    log_retry(
        ResponsesStreamRequest::Sampling,
        &turn_context,
        &CodexErr::Stream("websocket closed by server before response.completed".to_string()),
        /*retries*/ 2,
        /*max_retries*/ 5,
        Duration::from_secs(1),
    );

    let logs = String::from_utf8(
        buffer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
    )
    .expect("retry log should be valid utf-8");
    assert!(logs.contains("stream disconnected - retrying sampling request"));
    assert!(logs.contains(&format!("turn_id={}", turn_context.sub_id)));
    assert!(logs.contains("retries=2"));
    assert!(logs.contains("max_retries=5"));
    assert!(logs.contains(
        "sampling_error=stream disconnected before completion: websocket closed by server before response.completed"
    ));
}

/// Time spent reporting a retry must count toward the server's original deadline.
#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "test holds the event-delivery lock to delay notification while virtual time advances"
)]
async fn stream_retry_preserves_deadline_across_delayed_notification() {
    let (mut session, turn_context) = make_session_and_context().await;
    let step_context = StepContext::for_test(Arc::new(turn_context));
    session.realtime_history = Some(Mutex::new(RealtimeHistoryState::default()));
    let mut client_session = session.services.model_client.new_session();
    // The second retry emits a notification even when release builds hide the first.
    let mut retry_state = ResponsesStreamRetryState {
        retries: 1,
        ..Default::default()
    };

    tokio::time::pause();
    let advice = RetryAfter::from_delay(Duration::from_secs(10)).expect("retry deadline");
    tokio::time::advance(Duration::from_secs(4)).await;

    // Hold the event-delivery lock so reporting the error consumes part of the deadline.
    let history_guard = session.realtime_history.as_ref().unwrap().lock().await;
    let retry = handle_response_stream_error(
        &mut retry_state,
        /*max_retries*/ 2,
        CodexErr::InternalServerError.with_retry_after(advice),
        &mut client_session,
        &session,
        &step_context,
        ResponsesStreamRequest::Sampling,
        false,
    );
    tokio::pin!(retry);
    assert!(futures::poll!(&mut retry).is_pending());
    tokio::time::advance(Duration::from_secs(4)).await;
    drop(history_guard);

    assert!(futures::poll!(&mut retry).is_pending());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(futures::poll!(&mut retry).is_pending());
    retry.await.expect("retry should be allowed");
    // Tokio rounds timer deadlines up to the next millisecond.
    let resumed_at = Instant::now();
    assert!(
        (advice.deadline()..=advice.deadline() + Duration::from_millis(1)).contains(&resumed_at),
        "retry resumed at {resumed_at:?}, expected {advice:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn capacity_retry_respects_retry_after_as_a_minimum() {
    let (mut session, turn_context) = make_session_and_context().await;
    let step_context = StepContext::for_test(Arc::new(turn_context));
    session.realtime_history = Some(Mutex::new(RealtimeHistoryState::default()));
    let mut client_session = session.services.model_client.new_session();
    let mut retry_state = ResponsesStreamRetryState::default();
    let advice = RetryAfter::from_delay(Duration::from_secs(5)).expect("retry deadline");
    let retry = handle_response_stream_error(
        &mut retry_state,
        0,
        CodexErr::new(CodexErrorDetails::RateLimitExceeded("later".into()))
            .with_http_503_slow_down(Http503SlowDown::classified_http_503_slow_down())
            .with_retry_after(advice),
        &mut client_session,
        &session,
        &step_context,
        ResponsesStreamRequest::Sampling,
        true,
    );
    tokio::pin!(retry);
    assert!(futures::poll!(&mut retry).is_pending());
    tokio::time::advance(Duration::from_millis(4_999)).await;
    assert!(futures::poll!(&mut retry).is_pending());
    tokio::time::advance(Duration::from_millis(1)).await;
    retry.await.expect("capacity retry should proceed at provider deadline");
}
