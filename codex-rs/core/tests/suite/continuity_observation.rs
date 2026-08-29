//! Mock-provider evidence for the opt-in continuity observation probe.
//!
//! The fixture deliberately stops at the provider boundary: it proves that an
//! authoritative parent usage-limit response can admit one V2 child probe and
//! that the child reaches the mock `/responses` endpoint. It does not make any
//! claim about production credentials, quota domains, or provider capacity.

use anyhow::Result;
use codex_features::Feature;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_response_once_match;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use serial_test::serial;
use std::ffi::OsString;
use std::time::Duration;
use tokio::time::Instant;
use tokio::time::sleep;
use tracing_test::traced_test;
use wiremock::Request;
use wiremock::ResponseTemplate;

const ROOT_PROMPT: &str = "continuity observation parent";
const CHILD_PROMPT: &str = "Run one bounded diagnostic child step";
const CONTINUITY_OBSERVATION_ENV: &str = "CODEX_EXPERIMENTAL_CONTINUITY_OBSERVATION";
const POST_USAGE_LIMIT_SPAWN_ENV: &str = "CODEX_EXPERIMENTAL_CONTINUITY_V2_POST_USAGE_LIMIT_SPAWN";

fn decoded_body(request: &Request) -> Option<Vec<u8>> {
    let is_zstd = request
        .headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|entry| entry.trim().eq_ignore_ascii_case("zstd"))
        });
    if is_zstd {
        zstd::stream::decode_all(std::io::Cursor::new(&request.body)).ok()
    } else {
        Some(request.body.clone())
    }
}

fn body_contains(request: &Request, text: &str) -> bool {
    decoded_body(request)
        .and_then(|body| String::from_utf8(body).ok())
        .is_some_and(|body| body.contains(text))
}

fn request_has_input_type(request: &Request, input_type: &str) -> bool {
    decoded_body(request)
        .and_then(|body| serde_json::from_slice::<serde_json::Value>(&body).ok())
        .and_then(|body| {
            body.get("input")
                .and_then(serde_json::Value::as_array)
                .cloned()
        })
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type").and_then(serde_json::Value::as_str) == Some(input_type)
            })
        })
}

async fn wait_for_child_thread(test: &TestCodex) -> Result<codex_protocol::ThreadId> {
    let root_thread_id = test.session_configured.thread_id;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(thread_id) = test
            .thread_manager
            .list_thread_ids()
            .await
            .into_iter()
            .find(|thread_id| *thread_id != root_thread_id)
        {
            return Ok(thread_id);
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for post-limit V2 child");
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_child_completion(
    test: &TestCodex,
    child_id: codex_protocol::ThreadId,
) -> Result<()> {
    let child = test.thread_manager.get_thread(child_id).await?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(
            child.agent_status().await,
            codex_protocol::protocol::AgentStatus::Completed(_)
        ) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for post-limit V2 child completion");
        }
        sleep(Duration::from_millis(10)).await;
    }
}

fn submit_user_turn(prompt: &str) -> Op {
    Op::UserInput {
        items: vec![UserInput::Text {
            text: prompt.to_string(),
            text_elements: Vec::new(),
        }],
        final_output_json_schema: None,
        responsesapi_client_metadata: None,
        additional_context: Default::default(),
        thread_settings: Default::default(),
    }
}

struct EnvVarGuard {
    key: &'static str,
    original: Option<OsString>,
}

impl EnvVarGuard {
    fn enabled(key: &'static str) -> Self {
        let original = std::env::var_os(key);
        // Safety: this test is serialized because these controls are process-global.
        unsafe { std::env::set_var(key, "1") };
        Self { key, original }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        // Safety: the serialized test restores the process-global control it owns.
        unsafe {
            match &self.original {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
#[traced_test]
async fn usage_limit_v2_probe_reaches_child_provider_and_records_outcome() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let _observation = EnvVarGuard::enabled(CONTINUITY_OBSERVATION_ENV);
    let _post_limit_spawn = EnvVarGuard::enabled(POST_USAGE_LIMIT_SPAWN_ENV);
    let server = start_mock_server().await;

    let _parent_limit = mount_response_once_match(
        &server,
        |request: &Request| body_contains(request, ROOT_PROMPT),
        ResponseTemplate::new(429)
            .insert_header("content-type", "application/json")
            .set_body_json(serde_json::json!({
                "error": {
                    "type": "usage_limit_reached",
                    "message": "mock usage limit",
                    "resets_at": null,
                    "plan_type": null
                }
            })),
    )
    .await;
    let _child_response = mount_sse_once_match(
        &server,
        |request: &Request| {
            body_contains(request, CHILD_PROMPT) && request_has_input_type(request, "agent_message")
        },
        sse(vec![
            ev_response_created("continuity-child"),
            ev_assistant_message("continuity-child-message", "child completed"),
            ev_completed("continuity-child"),
        ]),
    )
    .await;

    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::Collab)
            .expect("collaboration feature should be enabled");
        config
            .features
            .enable(Feature::MultiAgentV2)
            .expect("Multi-Agent V2 feature should be enabled");
    });
    let test = builder.build(&server).await?;
    test.codex.submit(submit_user_turn(ROOT_PROMPT)).await?;

    let mut saw_limit_error = false;
    let mut saw_reroute = false;
    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::Error(error) => {
                saw_limit_error = true;
                assert_eq!(
                    error.codex_error_info,
                    Some(CodexErrorInfo::UsageLimitExceeded),
                    "the authoritative parent rejection must remain a usage-limit error"
                );
            }
            EventMsg::ModelReroute(_) => saw_reroute = true,
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }
    assert!(
        saw_limit_error,
        "parent 429 must be surfaced as a Codex error"
    );
    assert!(
        !saw_reroute,
        "a rejected parent request must not be rerouted"
    );
    let parent_requests = server
        .received_requests()
        .await
        .expect("mock server should expose received requests")
        .into_iter()
        .filter(|request| {
            request.url.path().ends_with("/responses") && body_contains(request, ROOT_PROMPT)
        })
        .count();
    assert_eq!(
        parent_requests, 1,
        "parent must not retry the rejected call"
    );

    let child_id = wait_for_child_thread(&test).await?;
    let child_request_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let child_request_seen = server
            .received_requests()
            .await
            .expect("mock server should expose received requests")
            .iter()
            .any(|request| {
                request.url.path().ends_with("/responses")
                    && body_contains(request, CHILD_PROMPT)
                    && request_has_input_type(request, "agent_message")
            });
        if child_request_seen {
            break;
        }
        if Instant::now() >= child_request_deadline {
            anyhow::bail!("timed out waiting for child provider request");
        }
        sleep(Duration::from_millis(10)).await;
    }
    wait_for_child_completion(&test, child_id).await?;

    let requests = server
        .received_requests()
        .await
        .expect("mock server should expose received requests");
    let response_requests = requests
        .iter()
        .filter(|request| request.url.path().ends_with("/responses"))
        .count();
    let child_requests = requests
        .iter()
        .filter(|request| {
            request.url.path().ends_with("/responses")
                && body_contains(request, CHILD_PROMPT)
                && request_has_input_type(request, "agent_message")
        })
        .count();
    assert_eq!(
        response_requests, 2,
        "only the rejected parent and one child call are allowed"
    );
    assert_eq!(
        child_requests, 1,
        "child initial work must reach the provider exactly once"
    );

    logs_assert(|lines: &[&str]| {
        let has_fields = |fields: &[(&str, &str)]| {
            lines.iter().any(|line| {
                fields.iter().all(|(key, value)| {
                    let bare = format!("{key}={value}");
                    let quoted = format!("{key}=\"{value}\"");
                    line.contains(&bare) || line.contains(&quoted)
                })
            })
        };
        if !has_fields(&[("continuity_stage", "post_usage_limit_spawn_attempt")]) {
            return Err("missing post-limit spawn attempt observation".to_string());
        }
        if !has_fields(&[("continuity_stage", "spawn_accepted")]) {
            return Err("missing accepted-spawn observation".to_string());
        }
        if !has_fields(&[("continuity_stage", "child_created")]) {
            return Err("missing child-created observation".to_string());
        }
        if !has_fields(&[("continuity_stage", "initial_work_published")]) {
            return Err("missing initial-work observation".to_string());
        }
        if !has_fields(&[
            ("continuity_actor", "child"),
            ("continuity_stage", "provider_request_begun"),
        ]) {
            return Err("missing child physical provider-request observation".to_string());
        }
        if !has_fields(&[
            ("continuity_actor", "child"),
            ("continuity_outcome", "completed"),
        ]) {
            return Err("missing child provider-outcome observation".to_string());
        }
        if !has_fields(&[
            ("continuity_actor", "parent"),
            ("continuity_outcome", "usage_limit"),
        ]) {
            return Err("missing rejected parent provider-outcome observation".to_string());
        }
        Ok(())
    });

    Ok(())
}
