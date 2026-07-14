use std::sync::Arc;
use std::thread;

use codex_protocol::ThreadId;
use codex_protocol::protocol::InferenceCallStatus;
use codex_protocol::protocol::InferenceCallTransport;
use codex_protocol::protocol::TokenUsage;

use crate::InferenceTraceAttempt;
use crate::InferenceTraceContext;

fn observed_attempt() -> InferenceTraceAttempt {
    InferenceTraceContext::disabled()
        .with_observations(
            ThreadId::new(),
            "turn-1".to_string(),
            "provider".to_string(),
            "configured-model".to_string(),
            Some("configured-tier".to_string()),
        )
        .start_observed_attempt(
            InferenceCallTransport::ResponsesHttp,
            "requested-model".to_string(),
            Some("requested-tier".to_string()),
        )
}

#[test]
fn duplicate_terminal_records_return_exactly_one_observation() {
    let attempt = observed_attempt();
    let started = attempt.started_observation().expect("started observation");
    let usage = TokenUsage {
        input_tokens: 17,
        cached_input_tokens: 3,
        output_tokens: 9,
        reasoning_output_tokens: 4,
        total_tokens: 26,
    };

    let completed = attempt
        .record_completed_observation(
            "response-1",
            Some("request-1"),
            &Some(usage.clone()),
            &[],
            Some("observed-model"),
            Some("observed-snapshot"),
            Some("observed-tier"),
        )
        .expect("first terminal observation");

    assert_eq!(completed.inference_call_id, started.inference_call_id);
    assert_eq!(completed.status, InferenceCallStatus::Completed);
    assert_eq!(completed.token_usage, Some(usage));
    assert!(
        attempt
            .record_completed_observation(
                "response-2",
                Some("request-2"),
                &None,
                &[],
                None,
                None,
                None,
            )
            .is_none()
    );
    assert!(attempt.record_failed("late failure", None, &[]).is_none());
    assert!(
        attempt
            .record_cancelled("late cancellation", None, &[])
            .is_none()
    );
}

#[test]
fn concurrent_terminal_race_returns_exactly_one_observation() {
    let attempt = Arc::new(observed_attempt());
    let started = attempt.started_observation().expect("started observation");
    let terminals = thread::scope(|scope| {
        let handles = (0..16)
            .map(|index| {
                let attempt = Arc::clone(&attempt);
                scope.spawn(move || {
                    if index % 2 == 0 {
                        attempt.record_failed("provider failure", Some("request-1"), &[])
                    } else {
                        attempt.record_cancelled("consumer dropped", Some("request-1"), &[])
                    }
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().expect("terminal worker"))
            .collect::<Vec<_>>()
    });

    assert_eq!(terminals.len(), 1);
    let terminal = &terminals[0];
    assert_eq!(terminal.inference_call_id, started.inference_call_id);
    assert!(matches!(
        terminal.status,
        InferenceCallStatus::Failed | InferenceCallStatus::Cancelled
    ));
    assert_eq!(terminal.upstream_request_id.as_deref(), Some("request-1"));
    assert!(terminal.response_id.is_none());
    assert!(terminal.observed_model.is_none());
    assert!(terminal.observed_model_snapshot.is_none());
    assert!(terminal.observed_service_tier.is_none());
    assert!(terminal.token_usage.is_none());
}
