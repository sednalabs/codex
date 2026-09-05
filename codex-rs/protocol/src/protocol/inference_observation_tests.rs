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
        source: None,
        status,
        transport: InferenceCallTransport::ResponsesHttp,
        configured_provider: "configured-provider".to_string(),
        configured_model: Some("configured-model".to_string()),
        requested_model: "requested-model".to_string(),
        effective_provider: "effective-provider".to_string(),
        effective_model: "effective-model".to_string(),
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
        outcome_detail: Some("detail".to_string()),
        truncated_fields: None,
        omitted_fields: None,
    }
}

fn expected_durable_event(status: InferenceCallStatus) -> InferenceCallEvent {
    let mut event = inference_call_event(status.clone());
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
            event.outcome_detail = None;
            event.omitted_fields = Some(vec![
                InferenceCallField::RequestCompletedAtMs,
                InferenceCallField::ResponseId,
                InferenceCallField::UpstreamRequestId,
                InferenceCallField::ObservedProvider,
                InferenceCallField::ObservedModel,
                InferenceCallField::ObservedModelSnapshot,
                InferenceCallField::ObservedServiceTier,
                InferenceCallField::TokenUsage,
                InferenceCallField::OutcomeDetail,
            ]);
        }
        InferenceCallStatus::Failed
        | InferenceCallStatus::Cancelled
        | InferenceCallStatus::UsageLimitReached
        | InferenceCallStatus::TransportUncertain
        | InferenceCallStatus::Unknown(_) => {
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
        InferenceCallStatus::LocalDenied => {
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
            "configured_model": "configured-model",
            "requested_model": "requested-model",
            "effective_provider": "effective-provider",
            "effective_model": "effective-model",
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
            },
            "outcome_detail": "detail"
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
    legacy_object.remove("configured_model");
    legacy_object.remove("effective_provider");
    legacy_object.remove("effective_model");
    legacy_object.remove("truncated_fields");
    legacy_object.remove("omitted_fields");
    assert_eq!(
        serde_json::from_value::<InferenceCallEvent>(legacy_wire)?,
        InferenceCallEvent {
            spawn_request_id: None,
            observed_provider: None,
            configured_model: None,
            effective_provider: "<unknown>".to_string(),
            effective_model: "<unknown>".to_string(),
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
        InferenceCallStatus::UsageLimitReached,
        InferenceCallStatus::LocalDenied,
        InferenceCallStatus::TransportUncertain,
        InferenceCallStatus::Unknown("future_status".to_string()),
    ] {
        assert_eq!(
            inference_call_event(status.clone())
                .into_durable()
                .expect("durable event"),
            expected_durable_event(status)
        );
    }
}

#[test]
fn inference_call_source_known_shapes_round_trip() -> Result<()> {
    let cases = [
        (InferenceCallSource::Direct, json!({"type": "direct"})),
        (
            InferenceCallSource::HostContinuityCheck,
            json!({"type": "host_continuity_check"}),
        ),
        (
            InferenceCallSource::CodeMode {
                cell_id: "cell-1".to_string(),
                runtime_tool_call_id: "runtime-call-1".to_string(),
            },
            json!({
                "type": "code_mode",
                "cell_id": "cell-1",
                "runtime_tool_call_id": "runtime-call-1",
            }),
        ),
    ];

    for (source, expected_wire) in cases {
        assert_eq!(serde_json::to_value(&source)?, expected_wire);
        assert_eq!(
            serde_json::from_value::<InferenceCallSource>(expected_wire)?,
            source
        );
    }
    Ok(())
}

#[test]
fn inference_call_source_preserves_unknown_objects_and_future_fields() -> Result<()> {
    let cases = [
        json!({
            "type": "future_source",
            "metadata": {"region": "melbourne", "attempt": 2},
            "enabled": true,
        }),
        json!({"type": "direct", "future_field": [1, {"nested": false}]}),
        json!({
            "type": "code_mode",
            "cell_id": "cell-1",
            "runtime_tool_call_id": "runtime-call-1",
            "future_field": "preserve-me",
        }),
    ];

    for expected_wire in cases {
        let source = serde_json::from_value::<InferenceCallSource>(expected_wire.clone())?;
        assert!(matches!(source, InferenceCallSource::Unknown { .. }));
        assert_eq!(serde_json::to_value(source)?, expected_wire);
    }
    Ok(())
}

#[test]
fn inference_call_source_rejects_malformed_objects() {
    for wire in [
        json!({}),
        json!({"type": 42}),
        json!({"type": null}),
        json!({"type": ""}),
        json!({"type": "code_mode"}),
        json!({"type": "code_mode", "cell_id": 42, "runtime_tool_call_id": "call"}),
        json!({"type": "code_mode", "cell_id": "cell", "runtime_tool_call_id": null}),
        json!("direct"),
        json!(null),
    ] {
        assert!(serde_json::from_value::<InferenceCallSource>(wire).is_err());
    }
}

#[test]
fn inference_call_open_scalars_preserve_unknown_wire_tokens() -> Result<()> {
    let status = serde_json::from_value::<InferenceCallStatus>(json!("Future.Status-v2"))?;
    let transport = serde_json::from_value::<InferenceCallTransport>(json!("ws+future"))?;
    let field = serde_json::from_value::<InferenceCallField>(json!("Future_Field"))?;
    assert_eq!(
        status,
        InferenceCallStatus::Unknown("Future.Status-v2".to_string())
    );
    assert_eq!(
        transport,
        InferenceCallTransport::Unknown("ws+future".to_string())
    );
    assert_eq!(
        field,
        InferenceCallField::Unknown("Future_Field".to_string())
    );
    assert_eq!(serde_json::to_value(status)?, json!("Future.Status-v2"));
    assert_eq!(serde_json::to_value(transport)?, json!("ws+future"));
    assert_eq!(serde_json::to_value(field)?, json!("Future_Field"));
    assert!(serde_json::from_value::<InferenceCallStatus>(json!("")).is_err());
    assert!(serde_json::from_value::<InferenceCallTransport>(json!("")).is_err());
    assert!(serde_json::from_value::<InferenceCallField>(json!("")).is_err());
    for value in [json!(false), json!(7), json!(null), json!({})] {
        assert!(serde_json::from_value::<InferenceCallStatus>(value.clone()).is_err());
        assert!(serde_json::from_value::<InferenceCallTransport>(value.clone()).is_err());
        assert!(serde_json::from_value::<InferenceCallField>(value).is_err());
    }
    Ok(())
}

#[test]
fn inference_call_unknown_status_is_conservative_terminal() {
    let mut event = inference_call_event(InferenceCallStatus::Unknown("future_status".to_string()));
    let durable = event.clone().into_durable().expect("durable event");
    assert_eq!(durable.status, event.status);
    assert_eq!(
        durable.request_completed_at_ms,
        event.request_completed_at_ms
    );
    assert_eq!(durable.upstream_request_id, event.upstream_request_id);
    assert_eq!(durable.outcome_detail, event.outcome_detail);
    assert_eq!(durable.response_id, None);
    assert_eq!(durable.observed_provider, None);
    assert_eq!(durable.observed_model, None);
    assert_eq!(durable.observed_model_snapshot, None);
    assert_eq!(durable.observed_service_tier, None);
    assert_eq!(durable.token_usage, None);

    event.status = InferenceCallStatus::Completed;
    let completed = event.into_durable().expect("durable event");
    assert_eq!(completed.response_id, Some("response-1".to_string()));
}

#[test]
fn inference_call_source_bounds_are_rejections() {
    let mut oversized_code_mode = inference_call_event(InferenceCallStatus::Started);
    oversized_code_mode.source = Some(InferenceCallSource::CodeMode {
        cell_id: "c".repeat(INFERENCE_CALL_CORRELATION_ID_MAX_BYTES + 1),
        runtime_tool_call_id: "runtime-call-1".to_string(),
    });
    assert!(oversized_code_mode.into_durable().is_none());

    let mut oversized_unknown = inference_call_event(InferenceCallStatus::Completed);
    oversized_unknown.source = Some(InferenceCallSource::Unknown {
        raw: serde_json::from_value(json!({
            "type": "future_source",
            "payload": "x".repeat(INFERENCE_CALL_EVENT_MAX_BYTES),
        }))
        .expect("source object"),
    });
    assert!(oversized_unknown.into_durable().is_none());
}

#[test]
fn inference_call_normalization_preserves_receipts_on_replay() {
    let oversized_required = "🦀".repeat(INFERENCE_CALL_STRING_MAX_BYTES / 4 + 1);
    let mut event = inference_call_event(InferenceCallStatus::Completed);
    event.configured_provider = oversized_required.clone();
    event.requested_model = oversized_required;
    let once = event.into_durable().expect("first durable event");
    let twice = once.clone().into_durable().expect("second durable event");
    assert_eq!(twice, once);
}

#[test]
fn inference_call_schema_and_typescript_describe_wire_shapes() -> Result<()> {
    let status_schema = serde_json::to_value(schemars::schema_for!(InferenceCallStatus))?;
    let transport_schema = serde_json::to_value(schemars::schema_for!(InferenceCallTransport))?;
    let field_schema = serde_json::to_value(schemars::schema_for!(InferenceCallField))?;
    let source_schema = serde_json::to_value(schemars::schema_for!(InferenceCallSource))?;
    assert_eq!(status_schema["type"], "string");
    assert_eq!(status_schema["minLength"], 1);
    assert_eq!(transport_schema["type"], "string");
    assert_eq!(transport_schema["minLength"], 1);
    assert_eq!(field_schema["type"], "string");
    assert_eq!(field_schema["minLength"], 1);
    assert!(source_schema["anyOf"].is_array());

    let source_branches = source_schema["anyOf"].as_array().expect("source branches");
    let direct_branch = source_branches
        .iter()
        .find(|branch| branch["properties"]["type"]["const"] == "direct")
        .expect("direct source branch");
    assert_eq!(direct_branch["required"], json!(["type"]));
    assert_eq!(direct_branch["additionalProperties"], true);

    let host_continuity_check_branch = source_branches
        .iter()
        .find(|branch| branch["properties"]["type"]["const"] == "host_continuity_check")
        .expect("host continuity check source branch");
    assert_eq!(host_continuity_check_branch["required"], json!(["type"]));
    assert_eq!(host_continuity_check_branch["additionalProperties"], true);

    let code_mode_branch = source_branches
        .iter()
        .find(|branch| branch["properties"]["type"]["const"] == "code_mode")
        .expect("code mode source branch");
    assert_eq!(
        code_mode_branch["required"],
        json!(["type", "cell_id", "runtime_tool_call_id"])
    );
    assert_eq!(code_mode_branch["properties"]["cell_id"]["type"], "string");
    assert_eq!(
        code_mode_branch["properties"]["runtime_tool_call_id"]["type"],
        "string"
    );
    assert_eq!(code_mode_branch["additionalProperties"], true);

    let unknown_branch = source_branches
        .iter()
        .find(|branch| branch["properties"]["type"]["minLength"] == 1)
        .expect("unknown source branch");
    assert_eq!(unknown_branch["additionalProperties"], true);
    assert_eq!(
        unknown_branch["not"]["properties"]["type"]["enum"],
        json!(["direct", "host_continuity_check", "code_mode"])
    );

    let status_decl = InferenceCallStatus::decl(&ts_rs::Config::default());
    let source_decl = InferenceCallSource::decl(&ts_rs::Config::default());
    assert!(status_decl.contains("string"));
    assert!(source_decl.contains("code_mode"));
    assert!(source_decl.contains("cell_id"));
    assert!(source_decl.contains("runtime_tool_call_id"));
    Ok(())
}

#[test]
fn inference_call_schema_source_cases_match_serde_contract() -> Result<()> {
    for wire in [
        json!({"type": "direct"}),
        json!({"type": "direct", "future_field": true}),
        json!({"type": "host_continuity_check", "future_field": 7}),
        json!({
            "type": "code_mode",
            "cell_id": "cell-1",
            "runtime_tool_call_id": "runtime-call-1",
            "future_field": "preserve-me",
        }),
        json!({"type": "future_source", "metadata": {"attempt": 2}}),
    ] {
        assert!(serde_json::from_value::<InferenceCallSource>(wire).is_ok());
    }

    for wire in [
        json!({"type": "code_mode"}),
        json!({"type": "code_mode", "cell_id": 42, "runtime_tool_call_id": "call"}),
        json!({
            "type": "code_mode",
            "cell_id": "cell-1",
            "runtime_tool_call_id": null,
        }),
        json!({"type": "code_mode", "cell_id": "cell-1"}),
    ] {
        assert!(serde_json::from_value::<InferenceCallSource>(wire).is_err());
    }
    Ok(())
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
