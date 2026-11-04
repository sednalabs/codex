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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_mode_nested_callback_progresses_during_mid_turn_compaction() -> Result<()> {
    skip_if_no_network!(Ok(()));

    struct CallbackObserver {
        compaction_held: Arc<AtomicBool>,
        completed: Mutex<Option<oneshot::Sender<()>>>,
    }
    impl ToolLifecycleContributor for CallbackObserver {
        fn on_tool_finish<'a>(&'a self, input: ToolFinishInput<'a>) -> ToolLifecycleFuture<'a> {
            Box::pin(async move {
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
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    extensions.tool_lifecycle_contributor(Arc::new(CallbackObserver {
        compaction_held: Arc::clone(&compaction_held),
        completed: Mutex::new(Some(callback_tx)),
    }));

    let server = responses::start_mock_server().await;
    let marker_dir = tempfile::tempdir()?;
    let marker = marker_dir.path().join("compaction-started");
    let wait_command = format!("while [ ! -e '{}' ]; do sleep 0.05; done", marker.display());
    let script = format!(
        "const gate = tools.exec_command({{cmd: {}, yield_time_ms: 30000}}); yield_control(); await gate; await tools.test_sync_tool({{}});",
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
    responses::mount_response_once_match(
        &server,
        move |request: &Request| {
            let is_compaction = String::from_utf8_lossy(&request.body)
                .contains("nested-callback-midturn-compaction-prompt");
            if is_compaction {
                notify.notify_one();
            }
            is_compaction
        },
        responses::sse_response(sse(vec![
            ev_assistant_message("summary", "summary"),
            responses::ev_completed_with_tokens("compact", 10),
        ]))
        .set_delay(Duration::from_secs(15)),
    )
    .await;
    responses::mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("done", "done"),
            ev_completed("done"),
        ]),
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
            config.model_context_window = Some(100);
            config.model_auto_compact_token_limit = Some(90);
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
    let result: Result<()> = async {
        tokio::time::timeout(Duration::from_secs(30), compaction_started.notified())
            .await
            .context("did not observe the mid-turn compaction request")?;
        compaction_held.store(true, Ordering::Release);
        fs::write(&marker, "continue")?;
        tokio::time::timeout(Duration::from_secs(5), callback_rx)
            .await
            .context("nested callback did not finish while compaction was held")?
            .context("nested callback observer closed")?;
        Ok(())
    }
    .await;
    // Always stop the intentionally held request and the yielded process.
    test.codex.submit(Op::Interrupt).await?;
    test.codex.shutdown_and_wait().await?;
    result
}
