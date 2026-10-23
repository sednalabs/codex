use anyhow::Result;
use codex_features::Feature;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;
use wiremock::Match;
use wiremock::Mock;
use wiremock::Request;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

const NAMESPACE: &str = "agents";
const SCHEMA_DISABLED_PROMPT: &str = "inspect mailbox tools without the mailbox feature";
const SCHEMA_ENABLED_PROMPT: &str = "inspect mailbox tools with the mailbox feature";
const FLOW_PROMPT: &str = "run the mailbox integration flow";
const CHILD_TASK: &str = "finish quickly so the parent can join by immutable work reference";
const REQUEST_BODY: &str = "opaque mailbox request body";
const QUIET_REPLY_BODY: &str = "quiet final reply";

fn find_named_tool<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    match value {
        Value::Object(object) => {
            if object.get("name").and_then(Value::as_str) == Some(name)
                && object.get("parameters").is_some()
            {
                return Some(value);
            }
            object.values().find_map(|child| find_named_tool(child, name))
        }
        Value::Array(items) => items.iter().find_map(|child| find_named_tool(child, name)),
        _ => None,
    }
}

fn function_output_text<'a>(request: &'a Value, call_id: &str) -> Option<&'a str> {
    request
        .get("input")?
        .as_array()?
        .iter()
        .find(|item| {
            item.get("type").and_then(Value::as_str) == Some("function_call_output")
                && item.get("call_id").and_then(Value::as_str) == Some(call_id)
        })?
        .get("output")?
        .as_str()
}

fn has_function_output(request: &Value, call_id: &str) -> bool {
    function_output_text(request, call_id).is_some()
}

fn function_call_response(
    response_id: &str,
    call_id: &str,
    name: &str,
    args: Value,
) -> ResponseTemplate {
    let args = serde_json::to_string(&args).expect("tool arguments should serialize");
    sse_response(sse(vec![
        ev_response_created(response_id),
        ev_function_call_with_namespace(call_id, NAMESPACE, name, &args),
        ev_completed(response_id),
    ]))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mailbox_provider_schema_is_opt_in_and_exposes_the_v2_contract() -> Result<()> {
    let server = start_mock_server().await;
    let disabled_response = mount_sse_once_match(
        &server,
        |request: &Request| String::from_utf8_lossy(&request.body).contains(SCHEMA_DISABLED_PROMPT),
        sse(vec![
            ev_response_created("mailbox-schema-disabled"),
            ev_assistant_message("mailbox-schema-disabled-message", "checked"),
            ev_completed("mailbox-schema-disabled"),
        ]),
    )
    .await;
    let enabled_response = mount_sse_once_match(
        &server,
        |request: &Request| String::from_utf8_lossy(&request.body).contains(SCHEMA_ENABLED_PROMPT),
        sse(vec![
            ev_response_created("mailbox-schema-enabled"),
            ev_assistant_message("mailbox-schema-enabled-message", "checked"),
            ev_completed("mailbox-schema-enabled"),
        ]),
    )
    .await;

    let mut disabled_builder = test_codex().with_model("koffing").with_config(|config| {
        for feature in [Feature::Collab, Feature::MultiAgentV2] {
            config
                .features
                .enable(feature)
                .expect("test config should allow feature update");
        }
        config.multi_agent_v2.tool_namespace = Some(NAMESPACE.to_string());
    });
    let disabled = disabled_builder.build_with_auto_env(&server).await?;
    disabled.submit_turn(SCHEMA_DISABLED_PROMPT).await?;

    let mut enabled_builder = test_codex().with_model("koffing").with_config(|config| {
        for feature in [Feature::Collab, Feature::MultiAgentV2, Feature::AgentMailbox] {
            config
                .features
                .enable(feature)
                .expect("test config should allow feature update");
        }
        config.multi_agent_v2.tool_namespace = Some(NAMESPACE.to_string());
    });
    let enabled = enabled_builder.build_with_auto_env(&server).await?;
    enabled.submit_turn(SCHEMA_ENABLED_PROMPT).await?;

    let disabled_schema = disabled_response.single_request().body_json();
    let disabled_wait = find_named_tool(&disabled_schema["tools"], "wait_agent")
        .expect("legacy V2 wait tool should be present when mailbox is disabled");
    assert!(find_named_tool(&disabled_schema["tools"], "mailbox_read").is_none());
    assert!(disabled_wait["parameters"]["properties"].get("replies_to").is_none());

    let enabled_schema = enabled_response.single_request().body_json();
    let wait = find_named_tool(&enabled_schema["tools"], "wait_agent")
        .expect("mailbox V2 wait tool should be present");
    let wait_properties = &wait["parameters"]["properties"];
    assert!(wait_properties.get("targets").is_some());
    assert!(wait_properties.get("replies_to").is_some());
    assert!(wait_properties.get("timeout_ms").is_some());
    assert_eq!(wait_properties["return_when"]["enum"], json!(["any", "all"]));

    let send = find_named_tool(&enabled_schema["tools"], "send_message")
        .expect("mailbox send tool should be present");
    let send_properties = &send["parameters"]["properties"];
    for property in ["kind", "reply_to", "supersedes", "work_ref"] {
        assert!(send_properties.get(property).is_some(), "missing {property}");
    }
    assert!(send_properties.get("interrupt").is_none());

    let read = find_named_tool(&enabled_schema["tools"], "mailbox_read")
        .expect("canonical mailbox read tool should be present");
    let read_properties = &read["parameters"]["properties"];
    assert!(read_properties.get("cursor").is_some());
    assert!(read_properties.get("limit").is_some());
    assert!(read["description"]
        .as_str()
        .is_some_and(|description| description.contains("canonical conversation history")));

    Ok(())
}

#[derive(Clone)]
struct RootThreadMatcher {
    thread_id: String,
}

impl Match for RootThreadMatcher {
    fn matches(&self, request: &Request) -> bool {
        serde_json::from_slice::<Value>(&request.body)
            .ok()
            .and_then(|body| body.pointer("/client_metadata/thread_id").cloned())
            .and_then(|thread_id| thread_id.as_str().map(str::to_owned))
            .is_some_and(|thread_id| thread_id == self.thread_id)
    }
}

#[derive(Clone)]
struct ChildThreadMatcher {
    root_thread_id: String,
    task: String,
}

impl Match for ChildThreadMatcher {
    fn matches(&self, request: &Request) -> bool {
        serde_json::from_slice::<Value>(&request.body).ok().is_some_and(|body| {
            body.pointer("/client_metadata/thread_id")
                .and_then(Value::as_str)
                .is_some_and(|thread_id| thread_id != self.root_thread_id.as_str())
                && body.to_string().contains(self.task.as_str())
        })
    }
}

#[derive(Clone)]
struct RootMailboxFlowResponder {
    root_thread_id: String,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl Respond for RootMailboxFlowResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body)
            .expect("provider request should contain JSON");
        self.requests
            .lock()
            .expect("request capture lock should not be poisoned")
            .push(body.clone());

        if has_function_output(&body, "wait-call") {
            return sse_response(sse(vec![
                ev_response_created("mailbox-flow-final"),
                ev_assistant_message("mailbox-flow-final-message", "finished"),
                ev_completed("mailbox-flow-final"),
            ]));
        }

        if has_function_output(&body, "reply-call") {
            let spawn_output: Value = serde_json::from_str(
                function_output_text(&body, "spawn-call")
                    .expect("spawn output should remain in provider history"),
            )
            .expect("spawn output should be JSON");
            let send_output: Value = serde_json::from_str(
                function_output_text(&body, "request-call")
                    .expect("request receipt should remain in provider history"),
            )
            .expect("request receipt should be JSON");
            return function_call_response(
                "mailbox-wait-response",
                "wait-call",
                "wait_agent",
                json!({
                    "targets": [spawn_output["work_ref"]],
                    "replies_to": [send_output["message_id"]],
                    "return_when": "all",
                }),
            );
        }

        if has_function_output(&body, "read-call") {
            let send_output: Value = serde_json::from_str(
                function_output_text(&body, "request-call")
                    .expect("request receipt should remain in provider history"),
            )
            .expect("request receipt should be JSON");
            return function_call_response(
                "mailbox-reply-response",
                "reply-call",
                "send_message",
                json!({
                    "target": self.root_thread_id,
                    "kind": "note",
                    "reply_to": send_output["message_id"],
                    "message": QUIET_REPLY_BODY,
                }),
            );
        }

        if has_function_output(&body, "request-call") {
            return function_call_response(
                "mailbox-read-response",
                "read-call",
                "mailbox_read",
                json!({}),
            );
        }

        if has_function_output(&body, "spawn-call") {
            return function_call_response(
                "mailbox-request-response",
                "request-call",
                "send_message",
                json!({
                    "target": self.root_thread_id,
                    "kind": "request",
                    "message": REQUEST_BODY,
                }),
            );
        }

        function_call_response(
            "mailbox-spawn-response",
            "spawn-call",
            "spawn_agent",
            json!({
                "task_name": "quick-worker",
                "message": CHILD_TASK,
            }),
        )
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mailbox_read_keeps_encrypted_body_canonical_and_wait_joins_exact_ids() -> Result<()> {
    let server = start_mock_server().await;
    let mut builder = test_codex().with_model("koffing").with_config(|config| {
        for feature in [Feature::Collab, Feature::MultiAgentV2, Feature::AgentMailbox] {
            config
                .features
                .enable(feature)
                .expect("test config should allow feature update");
        }
        config.multi_agent_v2.tool_namespace = Some(NAMESPACE.to_string());
        config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    });
    let test = builder.build_with_auto_env(&server).await?;
    let root_thread_id = test.session_configured.thread_id.to_string();
    let captured = Arc::new(Mutex::new(Vec::new()));

    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .and(RootThreadMatcher {
            thread_id: root_thread_id.clone(),
        })
        .respond_with(RootMailboxFlowResponder {
            root_thread_id: root_thread_id.clone(),
            requests: Arc::clone(&captured),
        })
        .up_to_n_times(6)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .and(ChildThreadMatcher {
            root_thread_id: root_thread_id.clone(),
            task: CHILD_TASK.to_string(),
        })
        .respond_with(sse_response(sse(vec![
            ev_response_created("quick-worker-response"),
            ev_assistant_message("quick-worker-message", "done"),
            ev_completed("quick-worker-response"),
        ])))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    test.submit_turn(FLOW_PROMPT).await?;

    let requests = captured
        .lock()
        .expect("request capture lock should not be poisoned")
        .clone();
    assert_eq!(requests.len(), 6, "root should make one provider call per tool stage");

    let spawn: Value = serde_json::from_str(
        function_output_text(&requests[5], "spawn-call").expect("spawn output should be retained"),
    )?;
    let work_ref = spawn["work_ref"]
        .as_str()
        .expect("spawn should return an immutable mailbox work reference");
    assert!(work_ref.starts_with("work_"), "unexpected work reference: {work_ref}");

    let request_receipt: Value = serde_json::from_str(
        function_output_text(&requests[5], "request-call")
            .expect("request receipt should be retained"),
    )?;
    let request_id = request_receipt["message_id"]
        .as_str()
        .expect("request receipt should contain a native message ID");
    assert!(request_id.starts_with("amsg_"), "unexpected native ID: {request_id}");

    let read_output: Value = serde_json::from_str(
        function_output_text(&requests[3], "read-call")
            .expect("mailbox_read should return a receipt"),
    )?;
    assert_eq!(read_output["messages"][0]["message_id"], request_id);
    assert_eq!(read_output["messages"][0]["canonical_delivered"], true);
    assert!(read_output["messages"][0].get("message").is_none());
    assert!(read_output["messages"][0].get("body").is_none());
    assert!(!read_output.to_string().contains(REQUEST_BODY));

    let history = requests[3]["input"]
        .as_array()
        .expect("provider input should be an array")
        .iter()
        .find(|item| {
            item.get("type").and_then(Value::as_str) == Some("agent_message")
                && item.get("id").and_then(Value::as_str) == Some(request_id)
        })
        .expect("mailbox_read should append the request to canonical provider history");
    assert_eq!(history["content"][1]["type"], "encrypted_content");
    assert_eq!(history["content"][1]["encrypted_content"], REQUEST_BODY);

    let reply_receipt: Value = serde_json::from_str(
        function_output_text(&requests[5], "reply-call")
            .expect("final reply receipt should be retained"),
    )?;
    assert!(reply_receipt["message_id"].as_str().is_some_and(|id| id.starts_with("amsg_")));
    let wait_report: Value = serde_json::from_str(
        function_output_text(&requests[5], "wait-call")
            .expect("wait should return a selector report"),
    )?;
    assert_eq!(wait_report["requested_targets"], json!([work_ref]));
    assert_eq!(wait_report["requested_replies_to"], json!([request_id]));
    assert_eq!(wait_report["return_when"], "all");
    assert_eq!(wait_report["condition_satisfied"], true);
    assert!(wait_report["completed"].as_array().is_some_and(|completed| {
        completed.iter().any(|item| item["work_ref"] == work_ref && item["status"] == "completed")
            && completed
                .iter()
                .any(|item| item["message_id"] == request_id && item["status"] == "replied")
    }));
    assert_eq!(wait_report["pending"], json!([]));

    Ok(())
}

const QUIET_CHILD_TASK: &str = "send quiet mailbox updates then an addressed request";
const UNRELATED_CHILD_TASK: &str = "complete without being selected by the parent wait";
const QUIET_NOTE_BODY: &str = "non-attention note while the parent waits";
const QUIET_PROGRESS_BODY: &str = "non-attention progress while the parent waits";
const ADDRESSED_REQUEST_BODY: &str = "please inspect this request";

#[derive(Clone)]
struct WaitProbeResponder {
    root_thread_id: String,
    requests: Arc<Mutex<Vec<(Instant, Value)>>>,
    first_wait_started: Arc<Mutex<Option<Instant>>>,
    event_only_arguments: Arc<Mutex<Option<Value>>>,
}

impl Respond for WaitProbeResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body)
            .expect("provider request should contain JSON");
        let received_at = Instant::now();
        self.requests
            .lock()
            .expect("request capture lock should not be poisoned")
            .push((received_at, body.clone()));

        if has_function_output(&body, "wait-2-call") {
            return sse_response(sse(vec![
                ev_response_created("wait-probe-final"),
                ev_assistant_message("wait-probe-final-message", "finished"),
                ev_completed("wait-probe-final"),
            ]));
        }

        if has_function_output(&body, "wait-1-call") {
            let spawn_output: Value = serde_json::from_str(
                function_output_text(&body, "spawn-a-call")
                    .expect("watched worker output should remain in provider history"),
            )
            .expect("spawn output should be JSON");
            let arguments = json!({
                "targets": [spawn_output["work_ref"]],
                "return_when": "all",
            });
            *self
                .event_only_arguments
                .lock()
                .expect("event-only argument lock should not be poisoned") =
                Some(arguments.clone());
            return function_call_response(
                "wait-probe-event-only-response",
                "wait-2-call",
                "wait_agent",
                arguments,
            );
        }

        if has_function_output(&body, "spawn-b-call") {
            let spawn_output: Value = serde_json::from_str(
                function_output_text(&body, "spawn-a-call")
                    .expect("watched worker output should remain in provider history"),
            )
            .expect("spawn output should be JSON");
            *self
                .first_wait_started
                .lock()
                .expect("wait start lock should not be poisoned") = Some(Instant::now());
            return function_call_response(
                "wait-probe-deadline-response",
                "wait-1-call",
                "wait_agent",
                json!({
                    "targets": [spawn_output["work_ref"]],
                    "return_when": "all",
                    "timeout_ms": 1_000,
                }),
            );
        }

        if has_function_output(&body, "spawn-a-call") {
            return function_call_response(
                "wait-probe-unrelated-spawn-response",
                "spawn-b-call",
                "spawn_agent",
                json!({
                    "task_name": "unrelated-worker",
                    "message": UNRELATED_CHILD_TASK,
                }),
            );
        }

        function_call_response(
            "wait-probe-watched-spawn-response",
            "spawn-a-call",
            "spawn_agent",
            json!({
                "task_name": "watched-worker",
                "message": QUIET_CHILD_TASK,
            }),
        )
    }
}

#[derive(Clone)]
struct QuietThenRequestResponder {
    root_thread_id: String,
}

impl Respond for QuietThenRequestResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body)
            .expect("child provider request should contain JSON");

        let delayed = |response: ResponseTemplate| response.set_delay(Duration::from_millis(200));
        if has_function_output(&body, "addressed-request-call") {
            return sse_response(sse(vec![
                ev_response_created("quiet-child-final"),
                ev_assistant_message("quiet-child-final-message", "done"),
                ev_completed("quiet-child-final"),
            ])).set_delay(Duration::from_secs(3));
        }

        if has_function_output(&body, "quiet-progress-3-call") {
            return function_call_response(
                "quiet-child-request-response",
                "addressed-request-call",
                "send_message",
                json!({
                    "target": self.root_thread_id,
                    "kind": "request",
                    "message": ADDRESSED_REQUEST_BODY,
                }),
            )
            .set_delay(Duration::from_millis(400));
        }

        if has_function_output(&body, "quiet-progress-2-call") {
            return delayed(function_call_response(
                "quiet-child-progress-3-response",
                "quiet-progress-3-call",
                "send_message",
                json!({
                    "target": self.root_thread_id,
                    "kind": "progress",
                    "message": QUIET_PROGRESS_BODY,
                }),
            ));
        }

        if has_function_output(&body, "quiet-progress-1-call") {
            return delayed(function_call_response(
                "quiet-child-progress-2-response",
                "quiet-progress-2-call",
                "send_message",
                json!({
                    "target": self.root_thread_id,
                    "kind": "progress",
                    "message": QUIET_PROGRESS_BODY,
                }),
            ));
        }

        if has_function_output(&body, "quiet-note-call") {
            return delayed(function_call_response(
                "quiet-child-progress-1-response",
                "quiet-progress-1-call",
                "send_message",
                json!({
                    "target": self.root_thread_id,
                    "kind": "progress",
                    "message": QUIET_PROGRESS_BODY,
                }),
            ));
        }

        delayed(function_call_response(
            "quiet-child-note-response",
            "quiet-note-call",
            "send_message",
            json!({
                "target": self.root_thread_id,
                "kind": "note",
                "message": QUIET_NOTE_BODY,
            }),
        ))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quiet_events_keep_deadline_fixed_then_request_wakes_event_only_wait() -> Result<()> {
    let server = start_mock_server().await;
    let mut builder = test_codex().with_model("koffing").with_config(|config| {
        for feature in [Feature::Collab, Feature::MultiAgentV2, Feature::AgentMailbox] {
            config
                .features
                .enable(feature)
                .expect("test config should allow feature update");
        }
        config.multi_agent_v2.tool_namespace = Some(NAMESPACE.to_string());
        config.multi_agent_v2.max_concurrent_threads_per_session = 3;
    });
    let test = builder.build_with_auto_env(&server).await?;
    let root_thread_id = test.session_configured.thread_id.to_string();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let first_wait_started = Arc::new(Mutex::new(None));
    let event_only_arguments = Arc::new(Mutex::new(None));

    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .and(RootThreadMatcher {
            thread_id: root_thread_id.clone(),
        })
        .respond_with(WaitProbeResponder {
            root_thread_id: root_thread_id.clone(),
            requests: Arc::clone(&captured),
            first_wait_started: Arc::clone(&first_wait_started),
            event_only_arguments: Arc::clone(&event_only_arguments),
        })
        .up_to_n_times(5)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .and(ChildThreadMatcher {
            root_thread_id: root_thread_id.clone(),
            task: QUIET_CHILD_TASK.to_string(),
        })
        .respond_with(QuietThenRequestResponder {
            root_thread_id: root_thread_id.clone(),
        })
        .up_to_n_times(6)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .and(ChildThreadMatcher {
            root_thread_id: root_thread_id.clone(),
            task: UNRELATED_CHILD_TASK.to_string(),
        })
        .respond_with(sse_response(sse(vec![
            ev_response_created("unrelated-worker-response"),
            ev_assistant_message("unrelated-worker-message", "done"),
            ev_completed("unrelated-worker-response"),
        ])))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    test.submit_turn("verify quiet events, the deadline, and an addressed wake")
        .await?;

    let requests = captured
        .lock()
        .expect("request capture lock should not be poisoned")
        .clone();
    assert_eq!(requests.len(), 5, "quiet events must not add root provider calls");
    let wait_started = first_wait_started
        .lock()
        .expect("wait start lock should not be poisoned")
        .expect("first targeted wait should have started");
    let deadline_wait_elapsed = requests[3].0.duration_since(wait_started);
    assert!(
        (Duration::from_millis(850)..Duration::from_millis(1_500))
            .contains(&deadline_wait_elapsed),
        "quiet messages should not refresh the one-second deadline: {deadline_wait_elapsed:?}"
    );

    let watched_spawn: Value = serde_json::from_str(
        function_output_text(&requests[4].1, "spawn-a-call")
            .expect("watched spawn result should remain in history"),
    )?;
    let work_ref = watched_spawn["work_ref"]
        .as_str()
        .expect("watched spawn should return an immutable work reference");
    let unrelated_spawn: Value = serde_json::from_str(
        function_output_text(&requests[4].1, "spawn-b-call")
            .expect("unrelated spawn result should remain in history"),
    )?;
    let unrelated_work_ref = unrelated_spawn["work_ref"]
        .as_str()
        .expect("unrelated spawn should return an immutable work reference");
    assert_ne!(work_ref, unrelated_work_ref);

    let deadline_report: Value = serde_json::from_str(
        function_output_text(&requests[3].1, "wait-1-call")
            .expect("first wait should return its deadline report"),
    )?;
    assert_eq!(deadline_report["requested_targets"], json!([work_ref]));
    assert_eq!(deadline_report["completion_reason"], "deadline");
    assert_eq!(deadline_report["condition_satisfied"], false);
    assert!(deadline_report["quiet_count"].as_u64().unwrap_or_default() >= 4);
    assert_eq!(deadline_report["completed"], json!([]));
    assert_eq!(deadline_report["pending"][0]["work_ref"], work_ref);
    assert_eq!(deadline_report["pending"][0]["status"], "pending");
    assert!(deadline_report.to_string().contains(work_ref));
    assert!(!deadline_report.to_string().contains(unrelated_work_ref));

    let event_only_args = event_only_arguments
        .lock()
        .expect("event-only argument lock should not be poisoned")
        .clone()
        .expect("second wait call should follow the explicit deadline");
    assert!(event_only_args.get("timeout_ms").is_none());

    let request_report: Value = serde_json::from_str(
        function_output_text(&requests[4].1, "wait-2-call")
            .expect("event-only wait should return the addressed request report"),
    )?;
    assert_eq!(request_report["requested_targets"], json!([work_ref]));
    assert_eq!(request_report["completion_reason"], "request");
    assert_eq!(request_report["condition_satisfied"], false);
    assert_eq!(request_report["completed"], json!([]));
    assert_eq!(request_report["pending"][0]["work_ref"], work_ref);
    assert_eq!(request_report["pending"][0]["status"], "pending");
    let wake_ref = request_report["wake_ref"]
        .as_str()
        .expect("addressed request should identify the exact wake message");
    assert!(wake_ref.starts_with("amsg_"));

    let canonical_request = requests[4].1["input"]
        .as_array()
        .expect("provider input should be an array")
        .iter()
        .find(|item| {
            item.get("type").and_then(Value::as_str) == Some("agent_message")
                && item.get("id").and_then(Value::as_str) == Some(wake_ref)
        })
        .expect("wait should admit the addressed request into canonical history");
    assert_eq!(canonical_request["content"][1]["type"], "encrypted_content");
    assert_eq!(canonical_request["content"][1]["encrypted_content"], ADDRESSED_REQUEST_BODY);

    Ok(())
}
