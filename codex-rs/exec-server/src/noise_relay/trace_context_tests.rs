use codex_exec_server_protocol::EXEC_CLOSED_METHOD;
use codex_exec_server_protocol::EXEC_METHOD;
use codex_exec_server_protocol::JSONRPCMessage;
use codex_exec_server_protocol::JSONRPCNotification;
use codex_exec_server_protocol::JSONRPCRequest;
use codex_exec_server_protocol::JSONRPCResponse;
use codex_exec_server_protocol::RequestId;
use codex_protocol::protocol::W3cTraceContext;

use super::MAX_PROCESS_ID_BYTES;
use super::MAX_REQUEST_ID_BYTES;
use super::MAX_TRACE_CONTEXT_BYTES_PER_MAP;
use super::MAX_TRACE_CONTEXT_ENTRIES;
use super::MAX_TRACEPARENT_BYTES;
use super::MAX_TRACESTATE_BYTES;
use super::NoiseTraceContext;

fn trace_context() -> W3cTraceContext {
    W3cTraceContext {
        traceparent: Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".to_string()),
        tracestate: Some("vendor=value".to_string()),
    }
}

#[test]
fn oversized_peer_carriers_cannot_inflate_retained_trace_state() {
    let oversized_request_id = "r".repeat(MAX_REQUEST_ID_BYTES + 1);
    let oversized_process_id = "p".repeat(MAX_PROCESS_ID_BYTES + 1);
    let oversized_traceparent = "t".repeat(MAX_TRACEPARENT_BYTES + 1);
    let oversized_tracestate = "s".repeat(MAX_TRACESTATE_BYTES + 1);
    let mut context = NoiseTraceContext::default();

    context.observe_request(&JSONRPCMessage::Request(JSONRPCRequest {
        id: RequestId::String(oversized_request_id),
        method: EXEC_METHOD.to_string(),
        params: Some(serde_json::json!({"processId": oversized_process_id})),
        trace: Some(trace_context()),
    }));
    context.observe_request(&JSONRPCMessage::Request(JSONRPCRequest {
        id: RequestId::Integer(1),
        method: EXEC_METHOD.to_string(),
        params: Some(serde_json::json!({"processId": "process-traceparent"})),
        trace: Some(W3cTraceContext {
            traceparent: Some(oversized_traceparent),
            tracestate: None,
        }),
    }));
    context.observe_request(&JSONRPCMessage::Request(JSONRPCRequest {
        id: RequestId::Integer(2),
        method: EXEC_METHOD.to_string(),
        params: Some(serde_json::json!({"processId": "process-tracestate"})),
        trace: Some(W3cTraceContext {
            traceparent: None,
            tracestate: Some(oversized_tracestate),
        }),
    }));

    assert_eq!(context.state_sizes(), (0, 0));
    assert_eq!(context.retained_bytes(), 0);

    for index in 0..(MAX_TRACE_CONTEXT_ENTRIES * 2) {
        context.observe_request(&JSONRPCMessage::Request(JSONRPCRequest {
            id: RequestId::String(format!(
                "{index:04}{}",
                "r".repeat(MAX_REQUEST_ID_BYTES - 4)
            )),
            method: EXEC_METHOD.to_string(),
            params: Some(serde_json::json!({
                "processId": format!("{index:04}{}", "p".repeat(MAX_PROCESS_ID_BYTES - 4)),
            })),
            trace: Some(W3cTraceContext {
                traceparent: Some("t".repeat(MAX_TRACEPARENT_BYTES)),
                tracestate: Some("s".repeat(MAX_TRACESTATE_BYTES)),
            }),
        }));
    }

    assert!(context.state_sizes().0 <= MAX_TRACE_CONTEXT_ENTRIES);
    assert!(context.state_sizes().1 <= MAX_TRACE_CONTEXT_ENTRIES);
    assert!(context.retained_bytes() <= MAX_TRACE_CONTEXT_BYTES_PER_MAP * 2);
}

fn process_start_request(trace: W3cTraceContext) -> JSONRPCMessage {
    JSONRPCMessage::Request(JSONRPCRequest {
        id: RequestId::Integer(7),
        method: EXEC_METHOD.to_string(),
        params: Some(serde_json::json!({"processId": "process-1"})),
        trace: Some(trace),
    })
}

#[test]
fn correlates_response_and_terminal_notification_with_request_trace() {
    let trace = trace_context();
    let mut context = NoiseTraceContext::default();
    context.observe_request(&process_start_request(trace.clone()));

    let response = JSONRPCMessage::Response(JSONRPCResponse {
        id: RequestId::Integer(7),
        result: serde_json::Value::Null,
    });
    assert_eq!(context.return_trace(&response), Some(trace.clone()));
    assert_eq!(context.return_trace(&response), None);

    let closed = JSONRPCMessage::Notification(JSONRPCNotification {
        method: EXEC_CLOSED_METHOD.to_string(),
        params: Some(serde_json::json!({"processId": "process-1"})),
    });
    assert_eq!(context.return_trace(&closed), Some(trace));
    assert_eq!(context.return_trace(&closed), None);
}

#[test]
fn unfinished_peer_ids_cannot_grow_trace_state_without_bound() {
    let trace = trace_context();
    let mut context = NoiseTraceContext::default();
    for index in 0..(MAX_TRACE_CONTEXT_ENTRIES + 64) {
        context.observe_request(&JSONRPCMessage::Request(JSONRPCRequest {
            id: RequestId::Integer(index as i64),
            method: EXEC_METHOD.to_string(),
            params: Some(serde_json::json!({"processId": format!("process-{index}")})),
            trace: Some(trace.clone()),
        }));
    }

    assert_eq!(
        context.state_sizes(),
        (MAX_TRACE_CONTEXT_ENTRIES, MAX_TRACE_CONTEXT_ENTRIES)
    );
    let evicted_response = JSONRPCMessage::Response(JSONRPCResponse {
        id: RequestId::Integer(0),
        result: serde_json::Value::Null,
    });
    assert_eq!(context.return_trace(&evicted_response), None);
    let newest_response = JSONRPCMessage::Response(JSONRPCResponse {
        id: RequestId::Integer((MAX_TRACE_CONTEXT_ENTRIES + 63) as i64),
        result: serde_json::Value::Null,
    });
    assert_eq!(context.return_trace(&newest_response), Some(trace));
}
