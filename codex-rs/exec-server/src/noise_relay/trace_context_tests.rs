use codex_exec_server_protocol::EXEC_CLOSED_METHOD;
use codex_exec_server_protocol::EXEC_METHOD;
use codex_exec_server_protocol::JSONRPCMessage;
use codex_exec_server_protocol::JSONRPCNotification;
use codex_exec_server_protocol::JSONRPCRequest;
use codex_exec_server_protocol::JSONRPCResponse;
use codex_exec_server_protocol::RequestId;
use codex_protocol::protocol::W3cTraceContext;

use super::MAX_TRACE_CONTEXT_ENTRIES;
use super::NoiseTraceContext;

fn trace_context() -> W3cTraceContext {
    W3cTraceContext {
        traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".to_string(),
        tracestate: Some("vendor=value".to_string()),
    }
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
