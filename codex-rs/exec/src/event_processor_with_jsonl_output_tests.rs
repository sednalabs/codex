use super::*;
use codex_app_server_protocol::CodexErrorInfo;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::tempdir;

#[test]
fn failed_turn_does_not_overwrite_output_last_message_file() {
    let tempdir = tempdir().expect("create tempdir");
    let output_path = tempdir.path().join("last-message.txt");
    std::fs::write(&output_path, "keep existing contents").expect("seed output file");

    let mut processor = EventProcessorWithJsonOutput::new(Some(output_path.clone()));

    let collected = processor.collect_thread_events(ServerNotification::ItemCompleted(
        codex_app_server_protocol::ItemCompletedNotification {
            item: ThreadItem::AgentMessage {
                id: "msg-1".to_string(),
                text: "partial answer".to_string(),
                phase: None,
                memory_citation: None,
            },
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 0,
        },
    ));

    assert_eq!(collected.status, CodexStatus::Running);
    assert_eq!(processor.final_message(), Some("partial answer"));

    let status = processor.process_server_notification(ServerNotification::TurnCompleted(
        codex_app_server_protocol::TurnCompletedNotification {
            final_model: None,
            model_snapshot: None,
            thread_id: "thread-1".to_string(),
            turn: codex_app_server_protocol::Turn {
                id: "turn-1".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: Vec::new(),
                status: TurnStatus::Failed,
                error: Some(codex_app_server_protocol::TurnError {
                    message: "turn failed".to_string(),
                    additional_details: None,
                    codex_error_info: None,
                }),
                started_at: None,
                completed_at: Some(0),
                duration_ms: None,
            },
        },
    ));

    assert_eq!(status, CodexStatus::InitiateShutdown);
    assert_eq!(processor.final_message(), None);

    EventProcessor::print_final_output(&mut processor);

    assert_eq!(
        std::fs::read_to_string(&output_path).expect("read output file"),
        "keep existing contents"
    );
}

#[test]
fn runtime_warning_emits_a_non_fatal_error_item() {
    let mut processor = EventProcessorWithJsonOutput::new(/*last_message_path*/ None);

    let collected = processor.collect_thread_events(ServerNotification::Warning(
        codex_app_server_protocol::WarningNotification {
            thread_id: Some("thread-1".to_string()),
            message: "invalid global instructions".to_string(),
        },
    ));

    assert_eq!(
        collected,
        CollectedThreadEvents {
            events: vec![ThreadEvent::ItemCompleted(ItemCompletedEvent {
                thread_id: None,
                turn_id: None,
                item: ExecThreadItem {
                    id: "item_0".to_string(),
                    details: ThreadItemDetails::Error(ErrorItem {
                        message: "invalid global instructions".to_string(),
                    }),
                },
            })],
            status: CodexStatus::Running,
        }
    );
}

#[test]
fn mcp_tool_call_result_preserves_meta_in_jsonl_event() {
    let mut processor = EventProcessorWithJsonOutput::new(/*last_message_path*/ None);

    let collected = processor.collect_thread_events(ServerNotification::ItemCompleted(
        codex_app_server_protocol::ItemCompletedNotification {
            item: ThreadItem::McpToolCall {
                id: "mcp-1".to_string(),
                server: "search service".to_string(),
                tool: "web_run".to_string(),
                status: McpToolCallStatus::Completed,
                arguments: json!({"search_query": [{"q": "OpenAI Codex CLI documentation"}]}),
                app_context: None,
                mcp_app_resource_uri: None,
                plugin_id: None,
                result: Some(Box::new(codex_app_server_protocol::McpToolCallResult {
                    content: vec![json!({"type": "text", "text": "search result"})],
                    structured_content: None,
                    meta: Some(json!({"raw_messages": [{"ref_id": "turn0search0"}]})),
                })),
                error: None,
                duration_ms: Some(42),
            },
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 0,
        },
    ));

    assert_eq!(collected.status, CodexStatus::Running);
    assert_eq!(collected.events.len(), 1);

    let ThreadEvent::ItemCompleted(ItemCompletedEvent { item, .. }) = &collected.events[0] else {
        panic!("expected item.completed event");
    };
    let ThreadItemDetails::McpToolCall(item) = &item.details else {
        panic!("expected MCP tool call item");
    };
    let result = item.result.as_ref().expect("expected MCP tool result");
    assert_eq!(
        result.meta,
        Some(json!({"raw_messages": [{"ref_id": "turn0search0"}]}))
    );

    let serialized = serde_json::to_value(&collected.events[0]).expect("serialize event");
    assert_eq!(
        serialized["item"]["result"]["_meta"],
        json!({"raw_messages": [{"ref_id": "turn0search0"}]})
    );
    assert!(serialized["item"]["result"].get("meta").is_none());
}

#[test]
fn thread_error_event_maps_only_allowlisted_codex_error_codes() {
    let cases = [
        (
            CodexErrorInfo::ContextWindowExceeded,
            "context_window_exceeded",
        ),
        (
            CodexErrorInfo::SessionBudgetExceeded,
            "session_budget_exceeded",
        ),
        (CodexErrorInfo::UsageLimitExceeded, "usage_limit_exceeded"),
        (CodexErrorInfo::ServerOverloaded, "server_overloaded"),
        (CodexErrorInfo::CyberPolicy, "cyber_policy"),
        (
            CodexErrorInfo::HttpConnectionFailed {
                http_status_code: None,
            },
            "http_connection_failed",
        ),
        (
            CodexErrorInfo::ResponseStreamConnectionFailed {
                http_status_code: None,
            },
            "response_stream_connection_failed",
        ),
        (CodexErrorInfo::InternalServerError, "internal_server_error"),
        (CodexErrorInfo::Unauthorized, "unauthorized"),
        (CodexErrorInfo::BadRequest, "bad_request"),
        (CodexErrorInfo::SandboxError, "sandbox_error"),
        (
            CodexErrorInfo::ResponseStreamDisconnected {
                http_status_code: None,
            },
            "response_stream_disconnected",
        ),
        (
            CodexErrorInfo::ResponseTooManyFailedAttempts {
                http_status_code: None,
            },
            "response_too_many_failed_attempts",
        ),
        (
            CodexErrorInfo::ActiveTurnNotSteerable {
                turn_kind: codex_app_server_protocol::NonSteerableTurnKind::Review,
            },
            "active_turn_not_steerable",
        ),
        (
            CodexErrorInfo::ThreadRollbackFailed,
            "thread_rollback_failed",
        ),
        (CodexErrorInfo::Other, "other"),
    ];

    for (error_info, expected_code) in cases {
        let event = ThreadErrorEvent::new(
            "private provider message and token sentinel".to_string(),
            Some("thread-1".to_string()),
            Some("turn-1".to_string()),
            Some(&error_info),
        );
        let serialized = serde_json::to_value(event).expect("serialize error event");
        assert_eq!(serialized["codex_error_code"], expected_code);
        assert!(serialized.get("http_status_code").is_none());
    }
}

#[test]
fn thread_error_event_bounds_only_structured_http_status_codes() {
    for (status, expected) in [
        (Some(99), None),
        (Some(100), Some(100)),
        (Some(599), Some(599)),
        (Some(600), None),
        (None, None),
    ] {
        let error_info = CodexErrorInfo::HttpConnectionFailed {
            http_status_code: status,
        };
        let event = ThreadErrorEvent::new(
            "private provider message".to_string(),
            None,
            None,
            Some(&error_info),
        );
        assert_eq!(event.http_status_code, expected);
    }
}

#[test]
fn streamed_error_jsonl_adds_typed_code_and_bounded_status_fields() {
    let mut processor = EventProcessorWithJsonOutput::new(/*last_message_path*/ None);
    let collected = processor.collect_thread_events(ServerNotification::Error(
        codex_app_server_protocol::ErrorNotification {
            error: codex_app_server_protocol::TurnError {
                message: "provider response body private-sentinel".to_string(),
                codex_error_info: Some(CodexErrorInfo::ResponseStreamDisconnected {
                    http_status_code: Some(503),
                }),
                additional_details: Some("arbitrary private details".to_string()),
            },
            will_retry: false,
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
        },
    ));

    assert_eq!(collected.events.len(), 1);
    let serialized = serde_json::to_value(&collected.events[0]).expect("serialize error event");
    assert_eq!(
        serialized["codex_error_code"],
        "response_stream_disconnected"
    );
    assert_eq!(serialized["http_status_code"], 503);
    assert!(serialized.get("response_body").is_none());
    assert!(serialized.get("additional_details").is_none());
}

#[test]
fn failed_turn_jsonl_carries_typed_error_code_and_omits_unbounded_status() {
    let mut processor = EventProcessorWithJsonOutput::new(/*last_message_path*/ None);
    let collected = processor.collect_thread_events(ServerNotification::TurnCompleted(
        codex_app_server_protocol::TurnCompletedNotification {
            final_model: None,
            model_snapshot: None,
            thread_id: "thread-1".to_string(),
            turn: codex_app_server_protocol::Turn {
                id: "turn-1".to_string(),
                items_view: codex_app_server_protocol::TurnItemsView::Full,
                items: Vec::new(),
                status: TurnStatus::Failed,
                error: Some(codex_app_server_protocol::TurnError {
                    message: "provider error private-sentinel".to_string(),
                    codex_error_info: Some(CodexErrorInfo::HttpConnectionFailed {
                        http_status_code: Some(700),
                    }),
                    additional_details: Some("private details".to_string()),
                }),
                started_at: None,
                completed_at: Some(0),
                duration_ms: None,
            },
        },
    ));

    assert_eq!(collected.events.len(), 1);
    let serialized = serde_json::to_value(&collected.events[0]).expect("serialize turn failure");
    assert_eq!(
        serialized["error"]["codex_error_code"],
        "http_connection_failed"
    );
    assert!(serialized["error"].get("http_status_code").is_none());
}
