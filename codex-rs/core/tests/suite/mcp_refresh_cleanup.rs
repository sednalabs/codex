use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::collections::HashSet;
use std::fs;
use std::sync::Arc;
use std::time::Duration;

use codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID;
use codex_config::types::McpServerConfig;
use codex_config::types::McpServerTransportConfig;
use codex_protocol::protocol::Op;
use core_test_support::process::wait_for_pid_file;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::stdio_server_bin;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_mcp_server;

struct McpServerProcess {
    pid: String,
    // Retain the process object instead of reopening its PID after lifecycle actions.
    #[cfg(windows)]
    handle: std::os::windows::io::OwnedHandle,
}

impl McpServerProcess {
    fn observe_running(pid: String) -> anyhow::Result<Self> {
        let process = cfg_select! {
            unix => { Self { pid } }
            windows => {{
                use std::io;
                use std::num::NonZeroU32;
                use std::os::windows::io::FromRawHandle;
                use std::os::windows::io::OwnedHandle;

                use anyhow::Context;
                use windows_sys::Win32::System::Threading::OpenProcess;
                use windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE;

                let process_id = pid
                    .parse::<NonZeroU32>()
                    .with_context(|| format!("invalid MCP server PID {pid}"))?;
                // SAFETY: The PID is nonzero, and the returned handle is checked before use.
                let handle = unsafe {
                    OpenProcess(
                        PROCESS_SYNCHRONIZE,
                        /*binherithandle*/ 0,
                        process_id.get(),
                    )
                };
                if handle.is_null() {
                    return Err(io::Error::last_os_error())
                        .with_context(|| format!("failed to open MCP server process {pid}"));
                }
                // SAFETY: OpenProcess returned a non-null owned process handle.
                let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
                Self { pid, handle }
            }}
        };
        let alive = process.is_alive()?;
        anyhow::ensure!(alive, "MCP server process {} is not running", process.pid);
        Ok(process)
    }

    fn is_alive(&self) -> anyhow::Result<bool> {
        cfg_select! {
            unix => {{
                use core_test_support::process::process_is_alive;

                let Self { pid } = self;
                process_is_alive(pid)
            }}
            windows => {{
                use std::io;
                use std::os::windows::io::AsRawHandle;

                use anyhow::Context;
                use windows_sys::Win32::Foundation::WAIT_FAILED;
                use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
                use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
                use windows_sys::Win32::System::Threading::WaitForSingleObject;

                let Self { pid, handle } = self;
                // SAFETY: The owned handle stays open for this nonblocking wait.
                let wait_result = unsafe {
                    WaitForSingleObject(handle.as_raw_handle(), /*dwmilliseconds*/ 0)
                };
                match wait_result {
                    WAIT_TIMEOUT => Ok(true),
                    WAIT_OBJECT_0 => Ok(false),
                    WAIT_FAILED => Err(io::Error::last_os_error())
                        .with_context(|| format!("failed to wait for MCP server process {pid}")),
                    result => anyhow::bail!("unexpected wait result {result} for MCP server process {pid}"),
                }
            }}
        }
    }

    async fn wait_for_exit(&self) -> anyhow::Result<()> {
        cfg_select! {
            unix => {{
                use core_test_support::process::wait_for_process_exit;

                let Self { pid } = self;
                wait_for_process_exit(pid).await
            }}
            windows => {{
                use anyhow::Context;

                tokio::time::timeout(Duration::from_secs(2), async {
                    loop {
                        let alive = self.is_alive()?;
                        if !alive {
                            return Ok(());
                        }
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                })
                .await
                .context("timed out waiting for process to exit")?
            }}
        }
    }
}

fn stdio_server_config(
    command: String,
    env: HashMap<String, String>,
) -> McpServerConfig {
    McpServerConfig {
        auth: Default::default(),
        transport: McpServerTransportConfig::Stdio {
            command,
            args: Vec::new(),
            env: Some(env),
            env_vars: Vec::new(),
            cwd: None,
        },
        environment_id: DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
        enabled: true,
        required: false,
        startup_readiness: Default::default(),
        supports_parallel_tool_calls: false,
        tool_input_schema_max_bytes: None,
        omit_tools_from: None,
        disabled_reason: None,
        startup_timeout_sec: Some(Duration::from_secs(30)),
        tool_timeout_sec: None,
        default_tools_approval_mode: None,
        enabled_tools: None,
        disabled_tools: None,
        scopes: None,
        oauth: None,
        oauth_resource: None,
        tools: HashMap::new(),
    }
}

#[cfg(target_os = "linux")]
fn open_stdio_pipe_ids() -> anyhow::Result<HashSet<String>> {
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::fs::MetadataExt;

    let mut pipe_ids = HashSet::new();
    for entry in fs::read_dir("/proc/self/fd")? {
        let path = entry?.path();
        if let Ok(metadata) = fs::metadata(path)
            && metadata.file_type().is_fifo()
        {
            pipe_ids.insert(format!("{}:{}", metadata.dev(), metadata.ino()));
        }
    }
    Ok(pipe_ids)
}

#[cfg(target_os = "linux")]
fn shared_stdio_pipe_ids(child_pipe_ids: &HashSet<String>) -> anyhow::Result<HashSet<String>> {
    let parent_pipe_ids = open_stdio_pipe_ids()?;
    Ok(child_pipe_ids
        .intersection(&parent_pipe_ids)
        .cloned()
        .collect())
}

#[cfg(target_os = "linux")]
async fn wait_for_stdio_pipe_release(pipe_ids: &HashSet<String>) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if pipe_ids.is_disjoint(&open_stdio_pipe_ids()?) {
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
                    environment_id: DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
                    enabled: true,
                    required: false,
                    startup_readiness: Default::default(),
                    supports_parallel_tool_calls: false,
                    tool_input_schema_max_bytes: None,
                    omit_tools_from: None,
                    disabled_reason: None,
                    startup_timeout_sec: Some(Duration::from_secs(10)),
                    tool_timeout_sec: None,
                    default_tools_approval_mode: None,
                    enabled_tools: None,
                    disabled_tools: None,
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
    let superseded = McpServerProcess::observe_running(superseded_pid)?;
    #[cfg(target_os = "linux")]
    let superseded_pipe_ids = {
        let child_pipe_ids = fixture
            .codex
            .call_mcp_tool(
                "refresh_cleanup",
                "sync",
                Some(serde_json::json!({ "report_pipe_ids": true })),
                /*meta*/ None,
            )
            .await?
            .structured_content
            .and_then(|content| content.get("pipe_ids").cloned())
            .ok_or_else(|| anyhow::anyhow!("expected stdio pipe ids from the MCP server"))?;
        let child_pipe_ids: HashSet<String> = serde_json::from_value(child_pipe_ids)?;
        let pipe_ids = shared_stdio_pipe_ids(&child_pipe_ids)?;
        anyhow::ensure!(
            !pipe_ids.is_empty(),
            "expected stdio pipes shared with the MCP server"
        );
        pipe_ids
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
                        "sleep_after_ms": 10_000
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
    fixture.codex.submit(Op::RefreshMcpServers).await?;
    fixture.submit_turn("refresh MCP servers").await?;

    let replacement_pid = wait_for_pid_file(&pid_file).await?;
    let replacement = McpServerProcess::observe_running(replacement_pid)?;
    assert_ne!(replacement.pid, superseded.pid);
    let superseded_alive = superseded.is_alive()?;
    assert!(superseded_alive);
    long_call.await??;
    superseded.wait_for_exit().await?;
    #[cfg(target_os = "linux")]
    wait_for_stdio_pipe_release(&superseded_pipe_ids).await?;
    let replacement_alive = replacement.is_alive()?;
    assert!(replacement_alive);

    #[cfg(target_os = "linux")]
    let replacement_pipe_ids = {
        let child_pipe_ids = fixture
            .codex
            .call_mcp_tool(
                "refresh_cleanup",
                "sync",
                Some(serde_json::json!({ "report_pipe_ids": true })),
                /*meta*/ None,
            )
            .await?
            .structured_content
            .and_then(|content| content.get("pipe_ids").cloned())
            .ok_or_else(|| anyhow::anyhow!("expected stdio pipe ids from the MCP server"))?;
        let child_pipe_ids: HashSet<String> = serde_json::from_value(child_pipe_ids)?;
        let pipe_ids = shared_stdio_pipe_ids(&child_pipe_ids)?;
        anyhow::ensure!(
            !pipe_ids.is_empty(),
            "expected replacement stdio pipes shared with the MCP server"
        );
        pipe_ids
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
                        "sleep_after_ms": 10_000
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
    assert!(replacement.is_alive()?);
    tokio::time::timeout(Duration::from_secs(20), shutdown_call).await???;
    replacement.wait_for_exit().await?;
    #[cfg(target_os = "linux")]
    wait_for_stdio_pipe_release(&replacement_pipe_ids).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_cancels_pending_mcp_startup_while_in_flight_call_finishes() -> anyhow::Result<()>
{
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let temp_dir = tempfile::tempdir()?;
    let ready_pid_file = temp_dir.path().join("ready-mcp.pid");
    let pending_pid_file = temp_dir.path().join("pending-mcp.pid");
    let pending_initialize_barrier = temp_dir.path().join("allow-pending-initialize");
    let ready_pid_for_config = ready_pid_file.clone();
    let command = stdio_server_bin()?;
    let ready_command = command.clone();
    let fixture = test_codex()
        .with_config(move |config| {
            let mut servers = config.mcp_servers.get().clone();
            servers.insert(
                "ready_mcp".to_string(),
                stdio_server_config(
                    ready_command,
                    HashMap::from([(
                        "MCP_TEST_PID_FILE".to_string(),
                        ready_pid_for_config.to_string_lossy().into_owned(),
                    )]),
                ),
            );
            config
                .mcp_servers
                .set(servers)
                .expect("test MCP servers should accept any configuration");
        })
        .build(&server)
        .await?;

    wait_for_mcp_server(&fixture.codex, "ready_mcp").await?;
    let ready_pid = wait_for_pid_file(&ready_pid_file).await?;
    let ready = McpServerProcess::observe_running(ready_pid)?;
    let pending_pid = wait_for_pid_file(&pending_pid_file).await?;
    let pending = McpServerProcess::observe_running(pending_pid)?;
    anyhow::ensure!(
        !pending_initialize_barrier.exists(),
        "pending MCP initialize barrier must remain closed before shutdown"
    );

    #[cfg(target_os = "linux")]
    let ready_pipe_ids = {
        let child_pipe_ids = fixture
            .codex
            .call_mcp_tool(
                "ready_mcp",
                "sync",
                Some(serde_json::json!({ "report_pipe_ids": true })),
                /*meta*/ None,
            )
            .await?
            .structured_content
            .and_then(|content| content.get("pipe_ids").cloned())
            .ok_or_else(|| anyhow::anyhow!("expected stdio pipe ids from the ready MCP server"))?;
        let child_pipe_ids: HashSet<String> = serde_json::from_value(child_pipe_ids)?;
        let pipe_ids = shared_stdio_pipe_ids(&child_pipe_ids)?;
        anyhow::ensure!(!pipe_ids.is_empty(), "expected ready MCP stdio pipes");
        pipe_ids
    };

    // Add the blocked server only after the thread has initialized. Keeping it in the initial
    // fixture config makes `build()` wait for the deliberately blocked initialize request until
    // its startup timeout expires, before the test can exercise shutdown.
    let current_config = fixture.codex.config().await;
    let mut refreshed_config = current_config.as_ref().clone();
    let mut refreshed_servers = refreshed_config.mcp_servers.get().clone();
    refreshed_servers.insert(
        "pending_mcp".to_string(),
        stdio_server_config(
            command,
            HashMap::from([
                (
                    "MCP_TEST_INITIALIZE_BARRIER_FILE".to_string(),
                    pending_initialize_barrier.to_string_lossy().into_owned(),
                ),
                (
                    "MCP_TEST_PID_FILE".to_string(),
                    pending_pid_file.to_string_lossy().into_owned(),
                ),
            ]),
        ),
    );
    refreshed_config
        .mcp_servers
        .set(refreshed_servers)
        .expect("test MCP servers should accept any configuration");
    let refresh_outcome = fixture
        .codex
        .refresh_mcp_config(current_config, refreshed_config)
        .await;
    anyhow::ensure!(
        refresh_outcome == codex_core::ConfigRefreshOutcome::Published,
        "pending MCP config should publish before startup cancellation"
    );
    let pending_pid = wait_for_pid_file(&pending_pid_file).await?;
    let pending = McpServerProcess::observe_running(pending_pid)?;
    anyhow::ensure!(
        !pending_initialize_barrier.exists(),
        "pending MCP initialize barrier must remain closed before shutdown"
    );

    let barrier = serde_json::json!({
        "id": "mcp-shutdown-ready-call",
        "participants": 2,
        "timeout_ms": 1_000
    });
    let in_flight_call = tokio::spawn({
        let codex = Arc::clone(&fixture.codex);
        let barrier = barrier.clone();
        async move {
            codex
                .call_mcp_tool(
                    "ready_mcp",
                    "sync",
                    Some(serde_json::json!({
                        "barrier": barrier,
                        "sleep_after_ms": 5_000
                    })),
                    /*meta*/ None,
                )
                .await
        }
    });
    fixture
        .codex
        .call_mcp_tool(
            "ready_mcp",
            "sync",
            Some(serde_json::json!({ "barrier": barrier })),
            /*meta*/ None,
        )
        .await?;

    let shutdown = tokio::spawn({
        let codex = Arc::clone(&fixture.codex);
        async move { codex.shutdown_and_wait().await }
    });
    tokio::time::timeout(Duration::from_secs(2), shutdown).await???;
    tokio::time::timeout(Duration::from_secs(5), pending.wait_for_exit()).await??;
    anyhow::ensure!(
        ready.is_alive()?,
        "shutdown cancelled a ready in-flight MCP call"
    );
    anyhow::ensure!(
        !pending.is_alive()?,
        "shutdown left an optional MCP startup alive"
    );

    let in_flight_result =
        tokio::time::timeout(Duration::from_secs(20), in_flight_call).await???;
    anyhow::ensure!(
        in_flight_result.is_error == Some(false),
        "shutdown interrupted the ready in-flight MCP request"
    );
    ready.wait_for_exit().await?;
    #[cfg(target_os = "linux")]
    wait_for_stdio_pipe_release(&ready_pipe_ids).await?;
    Ok(())
}
