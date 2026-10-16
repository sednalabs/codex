#![cfg(not(target_os = "windows"))]
#![allow(clippy::unwrap_used)]

use core_test_support::responses;
use core_test_support::test_codex_exec::test_codex_exec;

/// Verify that when the server reports an error, `codex-exec` exits with a
/// non-zero status code so automation can detect failures.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exits_non_zero_when_server_reports_error() -> anyhow::Result<()> {
    let test = test_codex_exec();

    // Mock a simple Responses API SSE stream that immediately reports a
    // `response.failed` event with an error message.
    let server = responses::start_mock_server().await;
    let body = responses::sse(vec![serde_json::json!({
        "type": "response.failed",
        "response": {
            "id": "resp_err_1",
            "error": {"code": "rate_limit_exceeded", "message": "synthetic server error"}
        }
    })]);
    responses::mount_sse_once(&server, body).await;

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("tell me something")
        .arg("--experimental-json")
        .assert()
        .code(1);

    Ok(())
}

/// Hold a real native provider beyond Ctrl-C: exec must interrupt the server
/// without waiting for the provider response or replaying the uncertain action.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupts_server_while_native_browser_provider_is_held() -> anyhow::Result<()> {
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    let test = test_codex_exec();
    let gate = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = gate.local_addr()?.port();
    let provider = r#"
import json, socket, sys
request = json.load(sys.stdin)
assert request["namespace"] == "codex_browser"
assert request["tool"] == "browser_observe"
with socket.create_connection(("127.0.0.1", int(sys.argv[1]))) as gate:
    gate.sendall(b"ready")
    gate.recv(1)
    json.dump({"contentItems": [{"type": "inputImage",
        "imageUrl": "data:image/png;base64,AAAA"}], "success": True}, sys.stdout)
"#;
    std::fs::write(
        test.home_path().join("browser-computer-use.json"),
        serde_json::to_vec(&serde_json::json!({
            "command": ["python3", "-c", provider, port.to_string()],
        }))?,
    )?;
    let server = responses::start_mock_server().await;
    let request = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("native-interrupt"),
            responses::ev_function_call_with_namespace(
                "native-call",
                "codex_browser",
                "browser_observe",
                "{}",
            ),
            responses::ev_completed("native-interrupt"),
        ]),
    )
    .await;
    let mock_provider = format!(
        "model_providers.mock_provider={{name=\"Mock provider for test\",base_url=\"{}/v1\",wire_api=\"responses\",supports_websockets=false}}",
        server.uri()
    );
    let child = tokio::process::Command::new(codex_utils_cargo_bin::cargo_bin("codex-exec")?)
        .current_dir(test.cwd_path())
        .env("CODEX_HOME", test.home_path())
        .env("CODEX_SQLITE_HOME", test.home_path())
        .env("CODEX_API_KEY", "dummy")
        .env("RUST_LOG", "codex_app_server=trace")
        .arg("--skip-git-repo-check")
        .arg("--json")
        .arg("-c")
        .arg(mock_provider)
        .args([
            "-c",
            "model_provider=\"mock_provider\"",
            "-c",
            "features.enable_request_compression=false",
        ])
        .arg("observe the browser")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let (mut provider_gate, _) =
        tokio::time::timeout(Duration::from_secs(30), gate.accept()).await??;
    let mut ready = [0; 5];
    tokio::time::timeout(
        Duration::from_secs(10),
        provider_gate.read_exact(&mut ready),
    )
    .await??;
    assert_eq!(&ready, b"ready");
    let pid = child.id().expect("live exec child");
    let signal = tokio::process::Command::new("kill")
        .args(["-INT", &pid.to_string()])
        .status()
        .await?;
    assert!(signal.success());

    // The provider is still held: it cannot produce its response before exec
    // exits. This times out on the old inline-await implementation.
    let output = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output()).await??;
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stdout = String::from_utf8(output.stdout)?;
    let events = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()?;
    assert!(
        events.iter().any(|event| event["type"] == "turn.started"),
        "expected a started turn before interruption: {events:?}"
    );
    // Interrupted turns initiate shutdown without a terminal JSON event. An
    // unrelated typed error or an ordinary completion must not satisfy this test.
    assert!(
        events.iter().all(|event| !matches!(
            event["type"].as_str(),
            Some("error" | "turn.failed" | "turn.completed")
        )),
        "expected an interrupted turn, not a failed or completed turn: {events:?}"
    );
    assert_eq!(request.requests().len(), 1, "no model request replay");
    let mut closed = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), provider_gate.read(&mut closed)).await??,
        0,
        "the pending provider process must be dropped, not released or replayed"
    );
    Ok(())
}
