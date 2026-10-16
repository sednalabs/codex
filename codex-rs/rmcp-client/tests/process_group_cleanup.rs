#![cfg(unix)]

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use codex_rmcp_client::ElicitationAction;
use codex_rmcp_client::ElicitationResponse;
use codex_rmcp_client::LocalStdioServerLauncher;
use codex_rmcp_client::RmcpClient;
use futures::FutureExt as _;
use rmcp::model::ClientCapabilities;
use rmcp::model::Implementation;
use rmcp::model::InitializeRequestParams;
use rmcp::model::ProtocolVersion;
use serde_json::json;

fn stdio_server_bin() -> Result<std::path::PathBuf> {
    codex_utils_cargo_bin::cargo_bin("test_stdio_server").map_err(Into::into)
}

fn init_params() -> InitializeRequestParams {
    InitializeRequestParams::new(
        ClientCapabilities::default(),
        Implementation::new("codex-test", "0.0.0-test").with_title("Codex rmcp shutdown test"),
    )
    .with_protocol_version(ProtocolVersion::V_2025_06_18)
}

fn process_exists(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

async fn wait_for_pid_file(path: &Path) -> Result<u32> {
    for _ in 0..50 {
        match fs::read_to_string(path) {
            Ok(content) => {
                let trimmed = content.trim();
                if trimmed.is_empty() {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }

                let pid = trimmed
                    .parse::<u32>()
                    .with_context(|| format!("failed to parse pid from {}", path.display()))?;
                return Ok(pid);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()));
            }
        }
    }

    anyhow::bail!("timed out waiting for child pid file at {}", path.display());
}

async fn wait_for_process_exit(pid: u32) -> Result<()> {
    for _ in 0..50 {
        if !process_exists(pid) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    anyhow::bail!("process {pid} still running after timeout");
}

async fn wait_for_file(path: &Path) -> Result<()> {
    for _ in 0..50 {
        if path.is_file() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    anyhow::bail!("timed out waiting for server close acknowledgment");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn drop_kills_wrapper_process_group() -> Result<()> {
    let temp_dir = tempfile::tempdir()?;
    let child_pid_file = temp_dir.path().join("child.pid");
    let child_pid_file_str = child_pid_file.to_string_lossy().into_owned();
    let wrapper = temp_dir.path().join("wrapper");
    codex_utils_cargo_bin::write_executable(
        &wrapper,
        "#!/bin/sh\nsleep 300 & child_pid=$!; echo \"$child_pid\" > \"$CHILD_PID_FILE\"; cat >/dev/null\n",
    )?;

    let client = RmcpClient::new_stdio_client(
        OsString::from("./wrapper"),
        vec![],
        Some(HashMap::from([(
            OsString::from("CHILD_PID_FILE"),
            OsString::from(child_pid_file_str),
        )])),
        &[],
        Some(temp_dir.path().to_string_lossy().into_owned()),
        Arc::new(LocalStdioServerLauncher::new(std::env::current_dir()?)),
    )
    .await?;

    let grandchild_pid = wait_for_pid_file(&child_pid_file).await?;
    assert!(
        process_exists(grandchild_pid),
        "expected grandchild process {grandchild_pid} to be running before dropping client"
    );

    drop(client);

    wait_for_process_exit(grandchild_pid).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_kills_initialized_stdio_server_with_in_flight_operation() -> Result<()> {
    let temp_dir = tempfile::tempdir()?;
    let server_pid_file = temp_dir.path().join("server.pid");
    let server_pid_file_str = server_pid_file.to_string_lossy().into_owned();

    let client = Arc::new(
        RmcpClient::new_stdio_client(
            stdio_server_bin()?.into(),
            Vec::<OsString>::new(),
            Some(HashMap::from([(
                OsString::from("MCP_TEST_PID_FILE"),
                OsString::from(server_pid_file_str),
            )])),
            &[],
            /*cwd*/ None,
            Arc::new(LocalStdioServerLauncher::new(std::env::current_dir()?)),
        )
        .await?,
    );

    client
        .initialize(
            init_params(),
            Some(Duration::from_secs(5)),
            Box::new(|_, _| {
                async {
                    Ok(ElicitationResponse {
                        action: ElicitationAction::Accept,
                        content: Some(json!({})),
                        meta: None,
                    })
                }
                .boxed()
            }),
        )
        .await?;

    let server_pid = wait_for_pid_file(&server_pid_file).await?;
    assert!(
        process_exists(server_pid),
        "expected MCP server process {server_pid} to be running before shutdown"
    );

    let call_client = Arc::clone(&client);
    let call_task = tokio::spawn(async move {
        call_client
            .call_tool(
                "sync".to_string(),
                Some(json!({ "sleep_after_ms": 300_000 })),
                /*meta*/ None,
                Some(Duration::from_secs(300)),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    client.shutdown().await;

    wait_for_process_exit(server_pid).await?;
    let _ = tokio::time::timeout(Duration::from_secs(5), call_task).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn cooperative_close_waits_for_local_server_ack_and_reaps_owned_child() -> Result<()> {
    let temp_dir = tempfile::tempdir()?;
    let server_pid_file = temp_dir.path().join("server.pid");
    let wrapper_pid_file = temp_dir.path().join("wrapper.pid");
    let close_ack_file = temp_dir.path().join("close.ack");
    let policy_leak_file = temp_dir.path().join("policy-leaked");
    let wrapper = temp_dir.path().join("wrapper");
    codex_utils_cargo_bin::write_executable(
        &wrapper,
        "#!/bin/sh\n\
         printf '%s\\n' \"$$\" > \"$WRAPPER_PID_FILE\"\n\
         if [ \"${CODEX_INTERNAL_MCP_STDIO_CLOSE_POLICY+x}\" = x ]; then printf leaked > \"$POLICY_LEAK_FILE\"; fi\n\
         \"$MCP_SERVER\" &\n\
         server_pid=$!\n\
         wait \"$server_pid\"\n\
         sleep 3.2\n\
         printf acknowledged > \"$CLOSE_ACK_FILE\"\n",
    )?;

    let server = stdio_server_bin()?;
    let client = RmcpClient::new_stdio_client(
        OsString::from("./wrapper"),
        Vec::<OsString>::new(),
        Some(HashMap::from([
            (
                OsString::from("CODEX_INTERNAL_MCP_STDIO_CLOSE_POLICY"),
                OsString::from("cooperative-local-v1"),
            ),
            (
                OsString::from("WRAPPER_PID_FILE"),
                OsString::from(wrapper_pid_file.to_string_lossy().into_owned()),
            ),
            (
                OsString::from("MCP_TEST_PID_FILE"),
                OsString::from(server_pid_file.to_string_lossy().into_owned()),
            ),
            (
                OsString::from("MCP_SERVER"),
                server.into_os_string(),
            ),
            (
                OsString::from("CLOSE_ACK_FILE"),
                OsString::from(close_ack_file.to_string_lossy().into_owned()),
            ),
            (
                OsString::from("POLICY_LEAK_FILE"),
                OsString::from(policy_leak_file.to_string_lossy().into_owned()),
            ),
        ])),
        &[],
        Some(temp_dir.path().to_string_lossy().into_owned()),
        Arc::new(LocalStdioServerLauncher::new(std::env::current_dir()?)),
    )
    .await?;

    client
        .initialize(
            init_params(),
            Some(Duration::from_secs(5)),
            Box::new(|_, _| {
                async {
                    Ok(ElicitationResponse {
                        action: ElicitationAction::Accept,
                        content: Some(json!({})),
                        meta: None,
                    })
                }
                .boxed()
            }),
        )
        .await?;

    let wrapper_pid = wait_for_pid_file(&wrapper_pid_file).await?;
    let server_pid = wait_for_pid_file(&server_pid_file).await?;
    assert!(process_exists(wrapper_pid));
    assert!(process_exists(server_pid));
    let started_at = Instant::now();
    let ((), ()) = tokio::join!(client.shutdown(), client.shutdown());
    assert!(
        started_at.elapsed() >= Duration::from_secs(3),
        "cooperative close must wait for the server's delayed close acknowledgment"
    );
    assert!(close_ack_file.is_file(), "server completed its close path");
    assert!(!policy_leak_file.exists(), "host selector is not passed to the child");
    wait_for_process_exit(wrapper_pid).await?;
    wait_for_process_exit(server_pid).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn cooperative_drop_closes_stdin_before_owned_child_fallback() -> Result<()> {
    let temp_dir = tempfile::tempdir()?;
    let server_pid_file = temp_dir.path().join("server.pid");
    let close_ack_file = temp_dir.path().join("close.ack");
    let wrapper = temp_dir.path().join("wrapper");
    codex_utils_cargo_bin::write_executable(
        &wrapper,
        "#!/bin/sh\n\
         printf '%s\\n' \"$$\" > \"$SERVER_PID_FILE\"\n\
         cat >/dev/null\n\
         sleep 3.2\n\
         printf acknowledged > \"$CLOSE_ACK_FILE\"\n",
    )?;

    let client = RmcpClient::new_stdio_client(
        OsString::from("./wrapper"),
        Vec::<OsString>::new(),
        Some(HashMap::from([
            (
                OsString::from("CODEX_INTERNAL_MCP_STDIO_CLOSE_POLICY"),
                OsString::from("cooperative-local-v1"),
            ),
            (
                OsString::from("SERVER_PID_FILE"),
                OsString::from(server_pid_file.to_string_lossy().into_owned()),
            ),
            (
                OsString::from("CLOSE_ACK_FILE"),
                OsString::from(close_ack_file.to_string_lossy().into_owned()),
            ),
        ])),
        &[],
        Some(temp_dir.path().to_string_lossy().into_owned()),
        Arc::new(LocalStdioServerLauncher::new(std::env::current_dir()?)),
    )
    .await?;

    let server_pid = wait_for_pid_file(&server_pid_file).await?;
    assert!(process_exists(server_pid));
    drop(client);
    wait_for_file(&close_ack_file).await?;
    wait_for_process_exit(server_pid).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn cooperative_close_times_out_reaps_owned_child_and_prevents_reuse() -> Result<()> {
    let temp_dir = tempfile::tempdir()?;
    let server_pid_file = temp_dir.path().join("server.pid");
    let server = temp_dir.path().join("unresponsive-server");
    codex_utils_cargo_bin::write_executable(
        &server,
        "#!/usr/bin/env python3\n\
         import json\n\
         import os\n\
         import sys\n\
         import time\n\
         with open(os.environ['SERVER_PID_FILE'], 'w') as pid_file:\n\
             pid_file.write(str(os.getpid()))\n\
         request = json.loads(sys.stdin.readline())\n\
         print(json.dumps({\n\
             'jsonrpc': '2.0',\n\
             'id': request['id'],\n\
             'result': {\n\
                 'protocolVersion': '2025-06-18',\n\
                 'capabilities': {},\n\
                 'serverInfo': {'name': 'unresponsive-test-server', 'version': '1'},\n\
             },\n\
         }), flush=True)\n\
         time.sleep(300)\n",
    )?;

    let client = RmcpClient::new_stdio_client(
        OsString::from("./unresponsive-server"),
        Vec::<OsString>::new(),
        Some(HashMap::from([
            (
                OsString::from("CODEX_INTERNAL_MCP_STDIO_CLOSE_POLICY"),
                OsString::from("cooperative-local-v1"),
            ),
            (
                OsString::from("SERVER_PID_FILE"),
                OsString::from(server_pid_file.to_string_lossy().into_owned()),
            ),
        ])),
        &[],
        Some(temp_dir.path().to_string_lossy().into_owned()),
        Arc::new(LocalStdioServerLauncher::new(std::env::current_dir()?)),
    )
    .await?;

    client
        .initialize(
            init_params(),
            Some(Duration::from_secs(5)),
            Box::new(|_, _| {
                async {
                    Ok(ElicitationResponse {
                        action: ElicitationAction::Accept,
                        content: Some(json!({})),
                        meta: None,
                    })
                }
                .boxed()
            }),
        )
        .await?;

    let server_pid = wait_for_pid_file(&server_pid_file).await?;
    assert!(process_exists(server_pid));
    let started_at = Instant::now();
    client.shutdown().await;
    assert!(
        started_at.elapsed() >= Duration::from_secs(25),
        "unresponsive local stdio server should use the bounded close fallback"
    );
    wait_for_process_exit(server_pid).await?;
    assert!(client.is_closed().await);
    assert!(
        client
            .list_tools(None, Some(Duration::from_secs(1)))
            .await
            .is_err(),
        "a client with an unconfirmed cooperative close must not be reused"
    );
    Ok(())
}
