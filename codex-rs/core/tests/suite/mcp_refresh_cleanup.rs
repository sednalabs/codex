use std::collections::HashMap;
use std::fs;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

#[cfg(target_os = "linux")]
use std::collections::HashSet;

use codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID;
use codex_config::types::McpServerConfig;
use codex_config::types::McpServerTransportConfig;
use core_test_support::process::process_is_alive;
use core_test_support::process::wait_for_pid_file;
use core_test_support::process::wait_for_process_exit;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::stdio_server_bin;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_mcp_server;

const IN_FLIGHT_CALL_DELAY_MS: u64 = 15_000;

#[cfg(target_os = "linux")]
fn pipe_targets() -> anyhow::Result<HashSet<String>> {
    let mut targets = HashSet::new();
    for entry in fs::read_dir("/proc/self/fd")? {
        let entry = entry?;
        let target = match fs::read_link(entry.path()) {
            Ok(target) => target,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let target = target.to_string_lossy();
        if target.starts_with("pipe:[") {
            targets.insert(target.into_owned());
        }
    }
    Ok(targets)
}

#[cfg(target_os = "linux")]
fn shared_stdio_pipe_targets(child_targets: &HashSet<String>) -> anyhow::Result<HashSet<String>> {
    let parent_targets = pipe_targets()?;
    Ok(parent_targets
        .intersection(child_targets)
        .cloned()
        .collect())
}

#[cfg(target_os = "linux")]
async fn wait_for_stdio_pipe_release(targets: &HashSet<String>) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let open_targets = pipe_targets()?;
            if targets.is_disjoint(&open_targets) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("timed out waiting for MCP stdio pipe descriptors to close"))?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_reaps_superseded_mcp_server_after_in_flight_call_finishes() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let temp_dir = tempfile::tempdir()?;
    let pid_file = temp_dir.path().join("mcp.pid");
    let pid_file_for_config = pid_file.clone();
    let command = stdio_server_bin()?;
    let fixture = test_codex()
        .with_config(move |config| {
            let mut servers = config.mcp_servers.get().clone();
            servers.insert(
                "refresh_cleanup".to_string(),
                McpServerConfig {
                    auth: Default::default(),
                    transport: McpServerTransportConfig::Stdio {
                        command,
                        args: Vec::new(),
                        env: Some(HashMap::from([(
                            "MCP_TEST_PID_FILE".to_string(),
                            pid_file_for_config.to_string_lossy().into_owned(),
                        )])),
                        env_vars: Vec::new(),
                        cwd: None,
                    },
                    protocol_mode: None,
                    environment_id: DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
                    enabled: true,
                    required: false,
                    supports_parallel_tool_calls: false,
                    disabled_reason: None,
                    startup_timeout_sec: Some(Duration::from_secs(10)),
                    tool_timeout_sec: None,
                    default_tools_approval_mode: None,
                    enabled_tools: None,
                    disabled_tools: None,
                    enable_elicitation: false,
                    read_only: false,
                    strict_tool_classification: false,
                    require_approval_for_mutating: false,
                    scopes: None,
                    oauth: None,
                    oauth_resource: None,
                    tools: HashMap::new(),
                },
            );
            config
                .mcp_servers
                .set(servers)
                .expect("test MCP servers should accept any configuration");
        })
        .build(&server)
        .await?;
    wait_for_mcp_server(&fixture.codex, "refresh_cleanup").await?;

    let superseded_pid = wait_for_pid_file(&pid_file).await?;
    assert!(process_is_alive(&superseded_pid)?);
    #[cfg(target_os = "linux")]
    let superseded_pipe_targets = {
        let child_targets = fixture
            .codex
            .call_mcp_tool(
                "refresh_cleanup",
                "sync",
                Some(serde_json::json!({ "report_pipe_targets": true })),
                /*meta*/ None,
            )
            .await?
            .structured_content
            .and_then(|content| content.get("pipe_targets").cloned())
            .ok_or_else(|| anyhow::anyhow!("expected stdio pipe targets from the MCP server"))?;
        let child_targets: HashSet<String> = serde_json::from_value(child_targets)?;
        let targets = shared_stdio_pipe_targets(&child_targets)?;
        anyhow::ensure!(
            !targets.is_empty(),
            "expected to observe stdio pipe descriptors shared with the MCP server"
        );
        targets
    };

    let barrier = serde_json::json!({
        "id": "mcp-refresh-cleanup",
        "participants": 2,
        "timeout_ms": 1_000
    });
    let long_call = tokio::spawn({
        let codex = Arc::clone(&fixture.codex);
        let barrier = barrier.clone();
        async move {
            codex
                .call_mcp_tool(
                    "refresh_cleanup",
                    "sync",
                    Some(serde_json::json!({
                        "barrier": barrier,
                        "sleep_after_ms": IN_FLIGHT_CALL_DELAY_MS
                    })),
                    /*meta*/ None,
                )
                .await
        }
    });
    fixture
        .codex
        .call_mcp_tool(
            "refresh_cleanup",
            "sync",
            Some(serde_json::json!({ "barrier": barrier })),
            /*meta*/ None,
        )
        .await?;
    fs::remove_file(&pid_file)?;

    responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_assistant_message("msg-1", "done"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    fixture
        .codex
        .set_openai_form_elicitation_support(/*supported*/ true)
        .await?;
    fixture.submit_turn("refresh MCP servers").await?;

    let replacement_pid = wait_for_pid_file(&pid_file).await?;
    assert_ne!(replacement_pid, superseded_pid);
    assert!(process_is_alive(&superseded_pid)?);
    tokio::time::timeout(Duration::from_secs(20), long_call).await???;
    wait_for_process_exit(&superseded_pid).await?;
    #[cfg(target_os = "linux")]
    wait_for_stdio_pipe_release(&superseded_pipe_targets).await?;
    assert!(process_is_alive(&replacement_pid)?);

    #[cfg(target_os = "linux")]
    let replacement_pipe_targets = {
        let child_targets = fixture
            .codex
            .call_mcp_tool(
                "refresh_cleanup",
                "sync",
                Some(serde_json::json!({ "report_pipe_targets": true })),
                /*meta*/ None,
            )
            .await?
            .structured_content
            .and_then(|content| content.get("pipe_targets").cloned())
            .ok_or_else(|| anyhow::anyhow!("expected stdio pipe targets from the MCP server"))?;
        let child_targets: HashSet<String> = serde_json::from_value(child_targets)?;
        let targets = shared_stdio_pipe_targets(&child_targets)?;
        anyhow::ensure!(
            !targets.is_empty(),
            "expected to observe replacement stdio pipe descriptors shared with the MCP server"
        );
        targets
    };

    let shutdown_barrier = serde_json::json!({
        "id": "mcp-shutdown-cleanup",
        "participants": 2,
        "timeout_ms": 1_000
    });
    let shutdown_call = tokio::spawn({
        let codex = Arc::clone(&fixture.codex);
        let barrier = shutdown_barrier.clone();
        async move {
            codex
                .call_mcp_tool(
                    "refresh_cleanup",
                    "sync",
                    Some(serde_json::json!({
                        "barrier": barrier,
                        "sleep_after_ms": IN_FLIGHT_CALL_DELAY_MS
                    })),
                    /*meta*/ None,
                )
                .await
        }
    });
    fixture
        .codex
        .call_mcp_tool(
            "refresh_cleanup",
            "sync",
            Some(serde_json::json!({ "barrier": shutdown_barrier })),
            /*meta*/ None,
        )
        .await?;

    let shutdown = tokio::spawn({
        let codex = Arc::clone(&fixture.codex);
        async move { codex.shutdown_and_wait().await }
    });
    tokio::time::timeout(Duration::from_secs(1), shutdown).await???;
    assert!(process_is_alive(&replacement_pid)?);
    tokio::time::timeout(Duration::from_secs(20), shutdown_call).await???;
    wait_for_process_exit(&replacement_pid).await?;
    #[cfg(target_os = "linux")]
    wait_for_stdio_pipe_release(&replacement_pipe_targets).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_turn_replaces_closed_mcp_server_before_provider_catalogue() -> anyhow::Result<()>
{
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let temp_dir = tempfile::tempdir()?;
    let pid_file = temp_dir.path().join("mcp.pid");
    let pid_file_for_config = pid_file.clone();
    let command = stdio_server_bin()?;
    let fixture = test_codex()
        .with_model_info_override("gpt-5.4", |model| model.supports_search_tool = false)
        .with_config(move |config| {
            let mut servers = config.mcp_servers.get().clone();
            servers.insert(
                "ordinary_liveness".to_string(),
                McpServerConfig {
                    auth: Default::default(),
                    transport: McpServerTransportConfig::Stdio {
                        command,
                        args: Vec::new(),
                        env: Some(HashMap::from([
                            (
                                "MCP_TEST_DYNAMIC_SERVER_METADATA".to_string(),
                                "1".to_string(),
                            ),
                            (
                                "MCP_TEST_PID_FILE".to_string(),
                                pid_file_for_config.to_string_lossy().into_owned(),
                            ),
                        ])),
                        env_vars: Vec::new(),
                        cwd: None,
                    },
                    protocol_mode: None,
                    environment_id: DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
                    enabled: true,
                    required: false,
                    supports_parallel_tool_calls: false,
                    disabled_reason: None,
                    startup_timeout_sec: Some(Duration::from_secs(10)),
                    tool_timeout_sec: None,
                    default_tools_approval_mode: None,
                    enabled_tools: None,
                    disabled_tools: None,
                    enable_elicitation: false,
                    read_only: false,
                    strict_tool_classification: false,
                    require_approval_for_mutating: false,
                    scopes: None,
                    oauth: None,
                    oauth_resource: None,
                    tools: HashMap::new(),
                },
            );
            config
                .mcp_servers
                .set(servers)
                .expect("test MCP servers should accept any configuration");
        })
        .build(&server)
        .await?;
    wait_for_mcp_server(&fixture.codex, "ordinary_liveness").await?;

    let first_pid = wait_for_pid_file(&pid_file).await?;
    assert!(process_is_alive(&first_pid)?);
    let first_response = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("ordinary-first"),
            responses::ev_assistant_message("ordinary-first-message", "done"),
            responses::ev_completed("ordinary-first"),
        ]),
    )
    .await;
    fixture.submit_turn("ordinary first turn").await?;
    assert!(
        first_response
            .single_request()
            .body_contains_text(&format!("Echo from rmcp-test-process-{first_pid}."))
    );

    let status = Command::new("kill").args(["-9", &first_pid]).status()?;
    anyhow::ensure!(status.success(), "failed to kill MCP process {first_pid}");
    wait_for_process_exit(&first_pid).await?;
    fs::remove_file(&pid_file)?;

    let replacement_response = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("ordinary-replacement"),
            responses::ev_assistant_message("ordinary-replacement-message", "done"),
            responses::ev_completed("ordinary-replacement"),
        ]),
    )
    .await;
    fixture.submit_turn("ordinary replacement turn").await?;

    let replacement_pid = wait_for_pid_file(&pid_file).await?;
    assert_ne!(replacement_pid, first_pid);
    assert!(process_is_alive(&replacement_pid)?);
    assert!(
        replacement_response
            .single_request()
            .body_contains_text(&format!("Echo from rmcp-test-process-{replacement_pid}."))
    );

    fixture.codex.shutdown_and_wait().await?;
    wait_for_process_exit(&replacement_pid).await
}
