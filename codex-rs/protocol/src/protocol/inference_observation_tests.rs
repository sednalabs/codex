use anyhow::Result;
use pretty_assertions::assert_eq;
use serde_json::json;
use ts_rs::TS as _;

use super::*;
use crate::protocol::EventMsg;

fn inference_call_event(status: InferenceCallStatus) -> InferenceCallEvent {
    InferenceCallEvent {
        inference_call_id: "call-1".to_string(),
        thread_id: ThreadId::from_string("01900000-0000-7000-8000-000000000001")
            .expect("thread id"),
        turn_id: "turn-1".to_string(),
        spawn_request_id: Some("spawn-1".to_string()),
        status,
        transport: InferenceCallTransport::ResponsesHttp,
        configured_provider: "configured-provider".to_string(),
        requested_model: "requested-model".to_string(),
        requested_service_tier: Some("requested-tier".to_string()),
        request_started_at_ms: 10,
        request_completed_at_ms: Some(20),
        response_id: Some("response-1".to_string()),
        upstream_request_id: Some("request-1".to_string()),
        observed_provider: Some("observed-provider".to_string()),
        observed_model: Some("observed-model".to_string()),
        observed_model_snapshot: Some("observed-snapshot".to_string()),
        observed_service_tier: Some("observed-tier".to_string()),
        token_usage: Some(TokenUsage {
            input_tokens: 11,
            cached_input_tokens: 2,
            cache_write_input_tokens: 1,
            output_tokens: 7,
            reasoning_output_tokens: 3,
            total_tokens: 18,
        }),
        truncated_fields: None,
        omitted_fields: None,
    }
}

fn expected_durable_event(status: InferenceCallStatus) -> InferenceCallEvent {
    let mut event = inference_call_event(status);
    match status {
        InferenceCallStatus::Started => {
            event.request_completed_at_ms = None;
            event.response_id = None;
            event.upstream_request_id = None;
            event.observed_provider = None;
            event.observed_model = None;
            event.observed_model_snapshot = None;
            event.observed_service_tier = None;
            event.token_usage = None;
            event.omitted_fields = Some(vec![
                InferenceCallField::RequestCompletedAtMs,
                InferenceCallField::ResponseId,
                InferenceCallField::UpstreamRequestId,
                InferenceCallField::ObservedProvider,
                InferenceCallField::ObservedModel,
                InferenceCallField::ObservedModelSnapshot,
                InferenceCallField::ObservedServiceTier,
                InferenceCallField::TokenUsage,
            ]);
        }
        InferenceCallStatus::Failed | InferenceCallStatus::Cancelled => {
            event.response_id = None;
            event.observed_provider = None;
            event.observed_model = None;
            event.observed_model_snapshot = None;
            event.observed_service_tier = None;
            event.token_usage = None;
            event.omitted_fields = Some(vec![
                InferenceCallField::ResponseId,
                InferenceCallField::ObservedProvider,
                InferenceCallField::ObservedModel,
                InferenceCallField::ObservedModelSnapshot,
                InferenceCallField::ObservedServiceTier,
                InferenceCallField::TokenUsage,
            ]);
        }
        InferenceCallStatus::Completed => {}
    }
    event
}

#[test]
fn inference_call_event_has_payload_free_wire_shape_and_legacy_defaults() -> Result<()> {
    let event = inference_call_event(InferenceCallStatus::Completed);
    let wire = serde_json::to_value(EventMsg::InferenceCall(event.clone()))?;
    assert_eq!(
        wire,
        json!({
            "type": "inference_call",
            "inference_call_id": "call-1",
            "thread_id": "01900000-0000-7000-8000-000000000001",
            "turn_id": "turn-1",
            "spawn_request_id": "spawn-1",
            "status": "completed",
            "transport": "responses_http",
            "configured_provider": "configured-provider",
            "requested_model": "requested-model",
            "requested_service_tier": "requested-tier",
            "request_started_at_ms": 10,
            "request_completed_at_ms": 20,
            "response_id": "response-1",
            "upstream_request_id": "request-1",
            "observed_provider": "observed-provider",
            "observed_model": "observed-model",
            "observed_model_snapshot": "observed-snapshot",
            "observed_service_tier": "observed-tier",
            "token_usage": {
                "input_tokens": 11,
                "cached_input_tokens": 2,
                "cache_write_input_tokens": 1,
                "output_tokens": 7,
                "reasoning_output_tokens": 3,
                "total_tokens": 18
            }
        })
    );
    let EventMsg::InferenceCall(decoded) = serde_json::from_value::<EventMsg>(wire)? else {
        panic!("expected inference_call");
    };
    assert_eq!(decoded, event);

    let mut legacy_wire = serde_json::to_value(event.clone())?;
    let legacy_object = legacy_wire.as_object_mut().expect("object");
    legacy_object.remove("spawn_request_id");
    legacy_object.remove("observed_provider");
    legacy_object.remove("truncated_fields");
    legacy_object.remove("omitted_fields");
    assert_eq!(
        serde_json::from_value::<InferenceCallEvent>(legacy_wire)?,
        InferenceCallEvent {
            spawn_request_id: None,
            observed_provider: None,
            ..event
        }
    );

    let declaration = InferenceCallEvent::decl(&ts_rs::Config::default());
    assert!(declaration.contains("truncated_fields?"));
    assert!(declaration.contains("omitted_fields?"));
    Ok(())
}

#[test]
fn inference_call_event_bounds_multibyte_fields_and_aggregate_size() -> Result<()> {
    let oversized_required = "🦀".repeat(INFERENCE_CALL_STRING_MAX_BYTES / 4 + 1);
    let bounded_optional = "🦀".repeat(INFERENCE_CALL_STRING_MAX_BYTES / 4);
    let mut event = inference_call_event(InferenceCallStatus::Completed);
    event.configured_provider.clone_from(&oversized_required);
    event.requested_model.clone_from(&oversized_required);
    event.requested_service_tier = Some(bounded_optional.clone());
    event.response_id = Some(bounded_optional.clone());
    event.upstream_request_id = Some(bounded_optional.clone());
    event.observed_provider = Some(bounded_optional.clone());
    event.observed_model = Some(bounded_optional.clone());
    event.observed_model_snapshot = Some(bounded_optional.clone());
    event.observed_service_tier = Some(bounded_optional);

    let bounded = event.into_durable().expect("bounded event");
    assert_eq!(
        (
            bounded.inference_call_id.as_str(),
            bounded.turn_id.as_str(),
            bounded.spawn_request_id.as_deref(),
        ),
        ("call-1", "turn-1", Some("spawn-1"))
    );
    assert_eq!(
        (
            bounded.configured_provider.len(),
            bounded.configured_provider.chars().count(),
            bounded.requested_model.len(),
            bounded.requested_model.chars().count(),
        ),
        (
            INFERENCE_CALL_STRING_MAX_BYTES,
            128,
            INFERENCE_CALL_STRING_MAX_BYTES,
            128,
        )
    );
    assert_eq!(
        bounded.truncated_fields,
        Some(vec![
            InferenceCallField::ConfiguredProvider,
            InferenceCallField::RequestedModel,
        ])
    );
    assert!(
        bounded
            .omitted_fields
            .as_ref()
            .is_some_and(|fields| !fields.is_empty())
    );
    assert!(
        serde_json::to_vec(&EventMsg::InferenceCall(bounded))?.len()
            <= INFERENCE_CALL_EVENT_MAX_BYTES
    );

    let mut oversized_optional = inference_call_event(InferenceCallStatus::Completed);
    oversized_optional.response_id = Some("🦀".repeat(INFERENCE_CALL_STRING_MAX_BYTES / 4 + 1));
    let bounded = oversized_optional.into_durable().expect("bounded event");
    assert!(bounded.response_id.is_none());
    assert_eq!(
        bounded.omitted_fields,
        Some(vec![InferenceCallField::ResponseId])
    );
    Ok(())
}

#[test]
fn inference_call_event_enforces_lifecycle_shapes() {
    for status in [
        InferenceCallStatus::Started,
        InferenceCallStatus::Completed,
        InferenceCallStatus::Failed,
        InferenceCallStatus::Cancelled,
    ] {
        assert_eq!(
            inference_call_event(status)
                .into_durable()
                .expect("durable event"),
            expected_durable_event(status)
        );
    }
}

#[test]
fn inference_call_event_never_truncates_correlation_identifiers() {
    let mut oversized_call = inference_call_event(InferenceCallStatus::Started);
    oversized_call.inference_call_id = "c".repeat(INFERENCE_CALL_ID_MAX_BYTES + 1);
    let mut oversized_turn = inference_call_event(InferenceCallStatus::Started);
    oversized_turn.turn_id = "t".repeat(INFERENCE_CALL_CORRELATION_ID_MAX_BYTES + 1);
    let mut oversized_spawn = inference_call_event(InferenceCallStatus::Started);
    oversized_spawn.spawn_request_id =
        Some("s".repeat(INFERENCE_CALL_CORRELATION_ID_MAX_BYTES + 1));

    assert!(oversized_call.into_durable().is_none());
    assert!(oversized_turn.into_durable().is_none());
    assert!(oversized_spawn.into_durable().is_none());
}

#[test]
fn event_msg_consumes_unknown_event_types() -> Result<()> {
    assert!(matches!(
        serde_json::from_value::<EventMsg>(json!({
            "type": "future_provider_observation",
            "payload": "ignored"
        }))?,
        EventMsg::Unknown
    ));
    Ok(())
}
