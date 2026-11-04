#![cfg(unix)]

use anyhow::Context;
use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_core::config::Config;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ToolCallOutcome;
use codex_extension_api::ToolFinishInput;
use codex_extension_api::ToolLifecycleContributor;
use codex_extension_api::ToolLifecycleFuture;
use codex_extension_api::ToolStartInput;
use codex_features::Feature;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use std::fs;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::oneshot;
use wiremock::Request;

// A yielded cell must be able to continue nested calls while a foreground
// mid-turn compaction request is in flight. This covers the listener heartbeat
// path rather than just proving that a cell id survives a later manual compact.
#[cfg(unix)]
#[test_case::test_case(true; "during_compaction")]
#[test_case::test_case(false; "without_compaction")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_mode_nested_callback_progresses_during_mid_turn_compaction(
    expect_compaction: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    struct CallbackObserver {
        compaction_held: Arc<AtomicBool>,
        wait_started: Arc<AtomicBool>,
        wait_finished: Arc<AtomicBool>,
        completed: Mutex<Option<oneshot::Sender<()>>>,
    }
    impl ToolLifecycleContributor for CallbackObserver {
        fn on_tool_start<'a>(&'a self, input: ToolStartInput<'a>) -> ToolLifecycleFuture<'a> {
            Box::pin(async move {
                if input.tool_name.name == "write_stdin"
                    && matches!(input.source, codex_extension_api::ToolCallSource::CodeMode { .. })
                {
                    self.wait_started.store(true, Ordering::Release);
                }
            })
        }

        fn on_tool_finish<'a>(&'a self, input: ToolFinishInput<'a>) -> ToolLifecycleFuture<'a> {
            Box::pin(async move {
                if input.tool_name.name == "write_stdin"
                    && matches!(input.source, codex_extension_api::ToolCallSource::CodeMode { .. })
                    && input.outcome == (ToolCallOutcome::Completed { success: true })
                {
                    self.wait_finished.store(true, Ordering::Release);
                }
                if input.tool_name.name == "test_sync_tool"
                    && matches!(input.source, codex_extension_api::ToolCallSource::CodeMode { .. })
                    && input.outcome == (ToolCallOutcome::Completed { success: true })
                    && self.compaction_held.load(Ordering::Acquire)
                    && let Some(sender) = self.completed.lock().unwrap().take()
                {
                    let _ = sender.send(());
                }
            })
        }
    }

    let (callback_tx, callback_rx) = oneshot::channel();
    let compaction_held = Arc::new(AtomicBool::new(false));
    let wait_started = Arc::new(AtomicBool::new(false));
    let wait_finished = Arc::new(AtomicBool::new(false));
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    extensions.tool_lifecycle_contributor(Arc::new(CallbackObserver {
        compaction_held: Arc::clone(&compaction_held),
        wait_started: Arc::clone(&wait_started),
        wait_finished: Arc::clone(&wait_finished),
        completed: Mutex::new(Some(callback_tx)),
    }));

    let server = responses::start_mock_server().await;
    let marker_dir = tempfile::tempdir()?;
    let marker = marker_dir.path().join("compaction-started");
    let wait_command = format!("while [ ! -e '{}' ]; do sleep 0.05; done", marker.display());
    let script = format!(
        "const gate = await tools.exec_command({{cmd: {}, yield_time_ms: 1000}}); if (!gate.session_id) throw Error('gate session missing'); text('gate-ready'); yield_control(); await tools.write_stdin({{session_id: gate.session_id, chars: '', yield_time_ms: 30000}}); await tools.test_sync_tool({{}});",
        serde_json::to_string(&wait_command)?
    );
    responses::mount_sse_once(
        &server,
        sse(vec![
            ev_custom_tool_call("call-background", "exec", &script),
            responses::ev_completed_with_tokens("initial", 96),
        ]),
    )
    .await;
    let compaction_started = Arc::new(tokio::sync::Notify::new());
    let notify = Arc::clone(&compaction_started);
    let gate_output = Arc::new(Mutex::new(None::<String>));
    let observed_output = Arc::clone(&gate_output);
    responses::mount_response_once_match(
        &server,
        move |request: &Request| {
            let body = String::from_utf8_lossy(&request.body);
            let output = serde_json::from_slice::<serde_json::Value>(&request.body)
                .ok()
                .and_then(|body| body.get("input")?.as_array()?.iter().find(|item| {
                    item.get("type").and_then(serde_json::Value::as_str)
                        == Some("custom_tool_call_output")
                        && item.get("call_id").and_then(serde_json::Value::as_str)
                            == Some("call-background")
                }).and_then(|item| item.get("output")).map(ToString::to_string));
            let target_request = output.is_some()
                && body.contains("nested-callback-midturn-compaction-prompt") == expect_compaction;
            if target_request {
                *observed_output.lock().unwrap() = output;
                notify.notify_one();
            }
            target_request
        },
        responses::sse_response(sse(vec![
            ev_assistant_message("summary", "summary"),
            responses::ev_completed_with_tokens("compact", 10),
        ]))
        .set_delay(Duration::from_secs(15)),
    )
    .await;
    // Keep the next sampling worker alive long enough to distinguish a
    // compaction-only stall from a broken callback or expired cell.
    responses::mount_response_once(
        &server,
        responses::sse_response(sse(vec![
            ev_assistant_message("done", "done"),
            ev_completed("done"),
        ]))
        .set_delay(Duration::from_secs(30)),
    )
    .await;
    let mut provider = codex_model_provider_info::built_in_model_providers(None)["openai"].clone();
    provider.name = "OpenAI-compatible test provider".to_string();
    provider.base_url = Some(format!("{}/v1", server.uri()));
    provider.supports_websockets = false;
    let test = test_codex()
        .with_model("test-gpt-5.1-codex")
        .with_extensions(Arc::new(extensions.build()))
        .with_code_mode_host_program(codex_utils_cargo_bin::cargo_bin("codex-code-mode-host")?)
        .with_config(move |config| {
            config.model_provider = provider;
            config.model_context_window = Some(if expect_compaction { 100 } else { 100_000 });
            config.model_auto_compact_token_limit =
                Some(if expect_compaction { 90 } else { 90_000 });
            config.compact_prompt = Some("nested-callback-midturn-compaction-prompt".to_string());
            config.features.enable(Feature::CodeMode).unwrap();
            config.features.enable(Feature::CodeModeHost).unwrap();
            config.code_mode.disable_in_process_fallback = true;
        })
        .build_with_auto_env(&server)
        .await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Start a background callback and compact".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let mut callback_rx = callback_rx;
    let result: Result<()> = async {
        tokio::time::timeout(Duration::from_secs(30), compaction_started.notified())
            .await
            .context("did not observe the target request with a cell output")?;
        let output = gate_output.lock().unwrap().clone().unwrap_or_default();
        anyhow::ensure!(output.contains("gate-ready"), "gate setup failed: {output}");
        compaction_held.store(true, Ordering::Release);
        fs::write(&marker, "continue")?;
        match tokio::time::timeout(Duration::from_secs(5), &mut callback_rx).await {
            Ok(callback) => callback.context("nested callback observer closed")?,
            Err(_) => {
                tokio::time::timeout(Duration::from_secs(30), &mut callback_rx)
                    .await
                    .with_context(|| {
                        format!(
                            "nested callback did not recover after compaction; wait started={}, finished={}",
                            wait_started.load(Ordering::Acquire),
                            wait_finished.load(Ordering::Acquire),
                        )
                    })?
                    .context("nested callback observer closed after compaction")?;
                anyhow::bail!("nested callback stalled during compaction and recovered afterwards");
            }
        }
        Ok(())
    }
    .await;
    // Always stop the intentionally held request and the yielded process.
    test.codex.submit(Op::Interrupt).await?;
    test.codex.shutdown_and_wait().await?;
    result
}
