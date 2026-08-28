use std::sync::Arc;

use codex_protocol::ThreadId;
use codex_protocol::protocol::InferenceCallTransport;
use codex_protocol::protocol::SessionSource;

use super::InferenceAdmission;
use super::InferenceAttemptMetadata;
use super::InferenceCacheState;
use super::InferenceObservationError;
use super::InferenceObservationEvent;
use super::InferenceObservationRecorder;
use super::InferenceObservationSink;
use super::InferenceProviderIdentity;
use super::InferenceRequestKind;
use super::InMemoryInferenceObservationSink;
use super::ProviderTerminalResult;

fn metadata(admission: InferenceAdmission) -> InferenceAttemptMetadata {
    InferenceAttemptMetadata {
        inference_call_id: "call-1".to_string(),
        thread_id: ThreadId::new(),
        turn_id: "turn-1".to_string(),
        parent_thread_id: Some(ThreadId::new()),
        spawn_request_id: Some("spawn-1".to_string()),
        session_source: SessionSource::SubAgent(
            codex_protocol::protocol::SubAgentSource::Other("test".to_string()),
        ),
        request_kind: InferenceRequestKind::Turn,
        transport: InferenceCallTransport::ResponsesHttp,
        provider: InferenceProviderIdentity {
            configured_provider: "configured-provider".to_string(),
            configured_model: Some("configured-model".to_string()),
            requested_model: "requested-model".to_string(),
            effective_provider: "resolved-provider".to_string(),
            effective_model: "resolved-model".to_string(),
            requested_service_tier: Some("priority".to_string()),
        },
        cache_state: InferenceCacheState::Miss,
        admission,
        request_started_at_ms: 123,
    }
}

fn recorder(
    admission: InferenceAdmission,
) -> (InferenceObservationRecorder, InMemoryInferenceObservationSink) {
    let sink = InMemoryInferenceObservationSink::default();
    let recorder = InferenceObservationRecorder::new(metadata(admission), Arc::new(sink.clone()));
    (recorder, sink)
}

#[test]
fn physical_open_and_provider_terminal_preserve_exact_identity() {
    let (recorder, sink) = recorder(InferenceAdmission::Admitted);
    recorder
        .record_physical_request_opened()
        .expect("admitted request opens");
    recorder
        .record_provider_terminal(ProviderTerminalResult::UsageLimitReached {
            upstream_request_id: Some("request-1".to_string()),
            detail: Some("limit".to_string()),
        })
        .expect("provider terminal is recorded");

    let events = sink.events();
    assert_eq!(events.len(), 2);
    let (InferenceObservationEvent::PhysicalRequestOpened { attempt },
        InferenceObservationEvent::ProviderTerminal { attempt: terminal, result }) =
        (&events[0], &events[1])
    else {
        panic!("expected opened then provider terminal events");
    };
    let ProviderTerminalResult::UsageLimitReached {
        upstream_request_id,
        detail,
    } = result
    else {
        panic!("expected usage-limit terminal result");
    };
    assert_eq!(attempt, terminal);
    assert_eq!(upstream_request_id.as_deref(), Some("request-1"));
    assert_eq!(detail.as_deref(), Some("limit"));
    assert_eq!(attempt.provider.configured_provider, "configured-provider");
    assert_eq!(attempt.provider.effective_provider, "resolved-provider");
    assert_eq!(attempt.provider.requested_model, "requested-model");
    assert_eq!(attempt.provider.effective_model, "resolved-model");
    assert_eq!(attempt.parent_thread_id, recorder.attempt().parent_thread_id);
    assert_eq!(attempt.spawn_request_id.as_deref(), Some("spawn-1"));
    assert_eq!(attempt.cache_state, InferenceCacheState::Miss);
}

#[test]
fn local_denial_never_emits_physical_open_or_provider_result() {
    let (recorder, sink) = recorder(InferenceAdmission::Denied);
    recorder
        .record_local_denial("goal owner rejected")
        .expect("denial is recorded before I/O");

    assert!(matches!(
        sink.events().as_slice(),
        [InferenceObservationEvent::LocalDenial { .. }]
    ));
    assert_eq!(
        recorder.record_physical_request_opened(),
        Err(InferenceObservationError::AdmissionDenied)
    );
}

#[test]
fn transport_uncertainty_requires_open_and_is_terminal() {
    let (recorder, sink) = recorder(InferenceAdmission::Admitted);
    assert_eq!(
        recorder.record_transport_uncertain("connection dropped"),
        Err(InferenceObservationError::PhysicalRequestNotOpened)
    );
    recorder
        .record_physical_request_opened()
        .expect("request opens");
    recorder
        .record_transport_uncertain("connection dropped")
        .expect("uncertain transport is recorded");
    assert_eq!(
        recorder.record_provider_terminal(ProviderTerminalResult::Failed {
            upstream_request_id: None,
            error: "late result".to_string(),
        }),
        Err(InferenceObservationError::TerminalAlreadyRecorded)
    );
    assert!(matches!(
        sink.events().as_slice(),
        [InferenceObservationEvent::PhysicalRequestOpened { .. },
            InferenceObservationEvent::TransportUncertain { .. }]
    ));
}

#[test]
fn duplicate_open_is_rejected_and_denial_requires_denied_admission() {
    let (recorder, sink) = recorder(InferenceAdmission::Admitted);
    recorder
        .record_physical_request_opened()
        .expect("request opens");
    assert_eq!(
        recorder.record_physical_request_opened(),
        Err(InferenceObservationError::PhysicalRequestAlreadyOpened)
    );
    assert_eq!(
        recorder.record_local_denial("too late"),
        Err(InferenceObservationError::PhysicalRequestAlreadyOpened)
    );
    assert_eq!(sink.events().len(), 1);
}

#[test]
fn admission_state_blocks_incompatible_lifecycle_events() {
    let (denied, sink) = recorder(InferenceAdmission::Denied);
    assert_eq!(
        denied.record_physical_request_opened(),
        Err(InferenceObservationError::AdmissionDenied)
    );
    assert!(sink.events().is_empty());
    denied
        .record_local_denial("policy rejected")
        .expect("denied admission can record local denial");

    let (admitted, sink) = recorder(InferenceAdmission::Admitted);
    assert_eq!(
        admitted.record_local_denial("not a denial"),
        Err(InferenceObservationError::AdmissionNotDenied)
    );
    assert!(sink.events().is_empty());
}

#[test]
fn disabled_recorder_keeps_the_same_contract_without_sink_events() {
    let recorder = InferenceObservationRecorder::disabled(metadata(InferenceAdmission::Admitted));
    recorder
        .record_physical_request_opened()
        .expect("disabled recorder still tracks lifecycle");
    recorder
        .record_provider_terminal(ProviderTerminalResult::Completed {
            response_id: Some("response-1".to_string()),
            upstream_request_id: None,
            observed_model: None,
            observed_service_tier: None,
            token_usage: None,
        })
        .expect("disabled recorder accepts terminal");
}

#[test]
fn sink_trait_is_synchronous_and_receives_owned_events() {
    #[derive(Default)]
    struct CountingSink(std::sync::Mutex<usize>);

    impl InferenceObservationSink for CountingSink {
        fn record(&self, _event: InferenceObservationEvent) {
            *self.0.lock().expect("counter lock") += 1;
        }
    }

    let sink = Arc::new(CountingSink::default());
    let recorder = InferenceObservationRecorder::new(
        metadata(InferenceAdmission::Admitted),
        sink.clone(),
    );
    recorder
        .record_physical_request_opened()
        .expect("request opens");
    recorder
        .record_transport_uncertain("unknown")
        .expect("uncertain terminal");
    assert_eq!(*sink.0.lock().expect("counter lock"), 2);
}
