#![cfg(unix)]

use std::process::Stdio;
use std::time::Duration;

use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCRequest;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_exec_server::InitializeParams;
use codex_exec_server::InitializeResponse;
use codex_utils_cargo_bin::cargo_bin;
use pretty_assertions::assert_eq;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Command;
use tokio::time::timeout;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_server_accepts_initialize_over_stdio() -> anyhow::Result<()> {
    let binary = cargo_bin("codex-exec-server")?;
    let mut child = Command::new(binary);
    child.args(["--listen", "stdio://"]);
    child.stdin(Stdio::piped());
    child.stdout(Stdio::piped());
    child.stderr(Stdio::inherit());
    let mut child = child.spawn()?;

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout).lines();

    let initialize = JSONRPCMessage::Request(JSONRPCRequest {
        id: RequestId::Integer(1),
        method: "initialize".to_string(),
        params: Some(serde_json::to_value(InitializeParams {
            client_name: "exec-server-test".to_string(),
        })?),
        trace: None,
    });
    stdin
        .write_all(format!("{}\n", serde_json::to_string(&initialize)?).as_bytes())
        .await?;

    let response_line = timeout(Duration::from_secs(5), stdout.next_line()).await??;
    let response_line = response_line.expect("response line");
    let response: JSONRPCMessage = serde_json::from_str(&response_line)?;
    let JSONRPCMessage::Response(JSONRPCResponse { id, result }) = response else {
        panic!("expected initialize response");
    };
    assert_eq!(id, RequestId::Integer(1));
    let initialize_response: InitializeResponse = serde_json::from_value(result)?;
    assert_eq!(initialize_response, InitializeResponse {});

    let initialized = JSONRPCMessage::Notification(JSONRPCNotification {
        method: "initialized".to_string(),
        params: Some(serde_json::json!({})),
    });
    stdin
        .write_all(format!("{}\n", serde_json::to_string(&initialized)?).as_bytes())
        .await?;

    child.start_kill()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_server_stubs_process_start_over_stdio() -> anyhow::Result<()> {
    let binary = cargo_bin("codex-exec-server")?;
    let mut child = Command::new(binary);
    child.args(["--listen", "stdio://"]);
    child.stdin(Stdio::piped());
    child.stdout(Stdio::piped());
    child.stderr(Stdio::inherit());
    let mut child = child.spawn()?;

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout).lines();

    let initialize = JSONRPCMessage::Request(JSONRPCRequest {
        id: RequestId::Integer(1),
        method: "initialize".to_string(),
        params: Some(serde_json::to_value(InitializeParams {
            client_name: "exec-server-test".to_string(),
        })?),
        trace: None,
    });
    stdin
        .write_all(format!("{}\n", serde_json::to_string(&initialize)?).as_bytes())
        .await?;
    let _ = timeout(Duration::from_secs(5), stdout.next_line()).await??;

    let exec = JSONRPCMessage::Request(JSONRPCRequest {
        id: RequestId::Integer(2),
        method: "process/start".to_string(),
        params: Some(serde_json::json!({
            "processId": "proc-1",
            "argv": ["true"],
            "cwd": std::env::current_dir()?,
            "env": {},
            "tty": false,
            "arg0": null
        })),
        trace: None,
    });
    stdin
        .write_all(format!("{}\n", serde_json::to_string(&exec)?).as_bytes())
        .await?;

    let response_line = timeout(Duration::from_secs(5), stdout.next_line()).await??;
    let response_line = response_line.expect("exec response line");
    let response: JSONRPCMessage = serde_json::from_str(&response_line)?;
    let JSONRPCMessage::Error(codex_app_server_protocol::JSONRPCError { id, error }) = response
    else {
        panic!("expected process/start stub error");
    };
    assert_eq!(id, RequestId::Integer(2));
    assert_eq!(error.code, -32601);
    assert_eq!(
        error.message,
        "exec-server stub does not implement `process/start` yet"
    );

    child.start_kill()?;
    Ok(())
}
