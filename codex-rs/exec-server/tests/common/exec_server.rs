#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::anyhow;
use codex_exec_server::InitializeParams;
use codex_exec_server::InitializeResponse;
use codex_exec_server_protocol::JSONRPCError;
use codex_exec_server_protocol::JSONRPCMessage;
use codex_exec_server_protocol::JSONRPCNotification;
use codex_exec_server_protocol::JSONRPCRequest;
use codex_exec_server_protocol::JSONRPCResponse;
use codex_exec_server_protocol::RequestId;
use futures::SinkExt;
use futures::StreamExt;
use tempfile::TempDir;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::io::copy_bidirectional;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::process::Child;
use tokio::process::Command;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio::time::sleep;
use tokio::time::timeout;
use tokio::time::timeout_at;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(25);
const EVENT_TIMEOUT: Duration = Duration::from_secs(5);
const RESUME_RECOVERY_TIMEOUT: Duration = Duration::from_secs(5);
const SESSION_ALREADY_ATTACHED_ERROR_CODE: i64 = -32010;

pub(crate) struct ExecServerHarness {
    _codex_home: TempDir,
    _helper_paths: TestCodexHelperPaths,
    child: Child,
    websocket_url: String,
    websocket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    next_request_id: i64,
}

impl Drop for ExecServerHarness {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

pub(crate) struct TestCodexHelperPaths {
    pub(crate) codex_exe: PathBuf,
    pub(crate) codex_linux_sandbox_exe: Option<PathBuf>,
}

pub(crate) struct DisconnectableWebSocketProxy {
    websocket_url: String,
    pause_tx: Option<oneshot::Sender<()>>,
    blocked_connection_rx: Option<oneshot::Receiver<()>>,
    resume_tx: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl Drop for DisconnectableWebSocketProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(crate) fn test_codex_helper_paths() -> anyhow::Result<TestCodexHelperPaths> {
    let (helper_binary, codex_linux_sandbox_exe) = super::current_test_binary_helper_paths()?;
    Ok(TestCodexHelperPaths {
        codex_exe: helper_binary,
        codex_linux_sandbox_exe,
    })
}

pub(crate) async fn exec_server() -> anyhow::Result<ExecServerHarness> {
    exec_server_with_env(std::iter::empty::<(&str, &str)>()).await
}

pub(crate) async fn exec_server_with_env<I, K, V>(env: I) -> anyhow::Result<ExecServerHarness>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<std::ffi::OsStr>,
    V: AsRef<std::ffi::OsStr>,
{
    let helper_paths = test_codex_helper_paths()?;
    let codex_home = TempDir::new()?;
    let mut child = Command::new(&helper_paths.codex_exe);
    child.args(["exec-server", "--listen", "ws://127.0.0.1:0"]);
    child.stdin(Stdio::null());
    child.stdout(Stdio::piped());
    child.stderr(Stdio::inherit());
    child.kill_on_drop(true);
    child.env("CODEX_HOME", codex_home.path());
    child.envs(env);
    let mut child = child.spawn()?;

    let websocket_url = read_listen_url_from_stdout(&mut child).await?;
    let (websocket, _) = connect_websocket_when_ready(&websocket_url).await?;
    Ok(ExecServerHarness {
        _codex_home: codex_home,
        _helper_paths: helper_paths,
        child,
        websocket_url,
        websocket,
        next_request_id: 1,
    })
}

impl ExecServerHarness {
    pub(crate) fn websocket_url(&self) -> &str {
        &self.websocket_url
    }

    pub(crate) async fn disconnect_websocket(&mut self) -> anyhow::Result<()> {
        self.websocket.close(None).await?;
        Ok(())
    }

    pub(crate) async fn reconnect_websocket(&mut self) -> anyhow::Result<()> {
        let (websocket, _) = connect_websocket_when_ready(&self.websocket_url).await?;
        self.websocket = websocket;
        Ok(())
    }

    pub(crate) async fn disconnectable_websocket_proxy(
        &self,
    ) -> anyhow::Result<DisconnectableWebSocketProxy> {
        let upstream = self
            .websocket_url
            .strip_prefix("ws://")
            .ok_or_else(|| anyhow!("exec-server websocket URL must use ws://"))?
            .to_string();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let websocket_url = format!("ws://{}", listener.local_addr()?);
        let (pause_tx, pause_rx) = oneshot::channel();
        let (blocked_connection_tx, blocked_connection_rx) = oneshot::channel();
        let (resume_tx, resume_rx) = oneshot::channel();
        let task = tokio::spawn(run_disconnectable_proxy(
            listener,
            upstream,
            pause_rx,
            blocked_connection_tx,
            resume_rx,
        ));
        Ok(DisconnectableWebSocketProxy {
            websocket_url,
            pause_tx: Some(pause_tx),
            blocked_connection_rx: Some(blocked_connection_rx),
            resume_tx: Some(resume_tx),
            task,
        })
    }

    pub(crate) async fn send_request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<RequestId> {
        let id = RequestId::Integer(self.next_request_id);
        self.next_request_id += 1;
        self.send_message(JSONRPCMessage::Request(JSONRPCRequest {
            id: id.clone(),
            method: method.to_string(),
            params: Some(params),
            trace: None,
        }))
        .await?;
        Ok(id)
    }

    pub(crate) async fn send_notification(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<()> {
        self.send_message(JSONRPCMessage::Notification(JSONRPCNotification {
            method: method.to_string(),
            params: Some(params),
        }))
        .await
    }

    pub(crate) async fn resume_initialize(
        &mut self,
        session_id: String,
    ) -> anyhow::Result<InitializeResponse> {
        let params = serde_json::to_value(InitializeParams {
            client_name: "exec-server-test".to_string(),
            resume_session_id: Some(session_id),
        })?;
        let deadline = Instant::now() + RESUME_RECOVERY_TIMEOUT;

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(anyhow!(
                    "timed out recovering exec-server session resume after {RESUME_RECOVERY_TIMEOUT:?}"
                ));
            }

            let request_id = self.send_request("initialize", params.clone()).await?;
            let response = self
                .wait_for_resume_initialize_response(&request_id, deadline)
                .await?;
            match response {
                JSONRPCMessage::Response(JSONRPCResponse { result, .. }) => {
                    return Ok(serde_json::from_value(result)?);
                }
                JSONRPCMessage::Error(JSONRPCError { error, .. })
                    if error.code == SESSION_ALREADY_ATTACHED_ERROR_CODE =>
                {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(anyhow!(
                            "timed out recovering exec-server session resume after {RESUME_RECOVERY_TIMEOUT:?}"
                        ));
                    }
                    sleep(CONNECT_RETRY_INTERVAL.min(remaining)).await;
                }
                JSONRPCMessage::Error(JSONRPCError { error, .. }) => {
                    return Err(anyhow!(
                        "exec-server session resume initialize failed with error {}: {}",
                        error.code,
                        error.message
                    ));
                }
                _ => {
                    return Err(anyhow!(
                        "unexpected response while resuming exec-server session"
                    ));
                }
            }
        }
    }

    pub(crate) async fn send_raw_text(&mut self, text: &str) -> anyhow::Result<()> {
        self.websocket
            .send(Message::Text(text.to_string().into()))
            .await?;
        Ok(())
    }

    pub(crate) async fn send_raw_binary(&mut self, bytes: Vec<u8>) -> anyhow::Result<()> {
        self.websocket.send(Message::Binary(bytes.into())).await?;
        Ok(())
    }

    pub(crate) async fn next_event(&mut self) -> anyhow::Result<JSONRPCMessage> {
        self.next_event_with_timeout(EVENT_TIMEOUT).await
    }

    pub(crate) async fn wait_for_event<F>(
        &mut self,
        mut predicate: F,
    ) -> anyhow::Result<JSONRPCMessage>
    where
        F: FnMut(&JSONRPCMessage) -> bool,
    {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(anyhow!(
                    "timed out waiting for matching exec-server event after {EVENT_TIMEOUT:?}"
                ));
            }
            let event = self.next_event_until(deadline).await?;
            if predicate(&event) {
                return Ok(event);
            }
        }
    }

    pub(crate) async fn shutdown(&mut self) -> anyhow::Result<()> {
        self.child.start_kill()?;
        timeout(CONNECT_TIMEOUT, self.child.wait())
            .await
            .map_err(|_| anyhow!("timed out waiting for exec-server shutdown"))??;
        Ok(())
    }

    async fn send_message(&mut self, message: JSONRPCMessage) -> anyhow::Result<()> {
        let encoded = serde_json::to_string(&message)?;
        self.websocket.send(Message::Text(encoded.into())).await?;
        Ok(())
    }

    async fn next_event_with_timeout(
        &mut self,
        timeout_duration: Duration,
    ) -> anyhow::Result<JSONRPCMessage> {
        self.next_event_until(Instant::now() + timeout_duration)
            .await
    }

    async fn next_event_until(&mut self, deadline: Instant) -> anyhow::Result<JSONRPCMessage> {
        loop {
            let frame = timeout_at(deadline, self.websocket.next())
                .await
                .map_err(|_| anyhow!("timed out waiting for exec-server websocket event"))?
                .ok_or_else(|| anyhow!("exec-server websocket closed"))??;

            match frame {
                Message::Text(text) => {
                    return Ok(serde_json::from_str(text.as_ref())?);
                }
                Message::Binary(bytes) => {
                    return Ok(serde_json::from_slice(bytes.as_ref())?);
                }
                Message::Close(_) => return Err(anyhow!("exec-server websocket closed")),
                Message::Ping(_) | Message::Pong(_) => {}
                Message::Frame(_) => {
                    return Err(anyhow!("unexpected raw exec-server websocket frame"));
                }
            }
        }
    }

    async fn wait_for_resume_initialize_response(
        &mut self,
        request_id: &RequestId,
        deadline: Instant,
    ) -> anyhow::Result<JSONRPCMessage> {
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "timed out waiting for exec-server session resume initialize response"
            ));
        }

        loop {
            let event = self.next_event_until(deadline).await?;
            match event {
                JSONRPCMessage::Response(response) if response.id == *request_id => {
                    return Ok(JSONRPCMessage::Response(response));
                }
                JSONRPCMessage::Response(JSONRPCResponse { id, .. }) => {
                    return Err(anyhow!(
                        "unexpected exec-server response for request {request_id}: response id {id}"
                    ));
                }
                JSONRPCMessage::Error(JSONRPCError { id, error }) if id == *request_id => {
                    return Ok(JSONRPCMessage::Error(JSONRPCError { id, error }));
                }
                JSONRPCMessage::Error(JSONRPCError { id, error }) => {
                    return Err(anyhow!(
                        "unexpected exec-server error for request {id}: {}: {}",
                        error.code,
                        error.message
                    ));
                }
                JSONRPCMessage::Request(_) | JSONRPCMessage::Notification(_) => {}
            }
        }
    }
}

impl DisconnectableWebSocketProxy {
    pub(crate) fn websocket_url(&self) -> &str {
        &self.websocket_url
    }

    pub(crate) async fn pause_and_disconnect(&mut self) -> anyhow::Result<()> {
        self.pause_tx
            .take()
            .ok_or_else(|| anyhow!("disconnectable websocket proxy is already paused"))?
            .send(())
            .map_err(|_| anyhow!("disconnectable websocket proxy stopped"))?;
        let blocked_connection_rx = self
            .blocked_connection_rx
            .take()
            .ok_or_else(|| anyhow!("disconnectable websocket proxy is already paused"))?;
        timeout(CONNECT_TIMEOUT, blocked_connection_rx)
            .await
            .map_err(|_| anyhow!("timed out waiting for client reconnect attempt"))?
            .map_err(|_| anyhow!("disconnectable websocket proxy stopped"))?;
        Ok(())
    }

    pub(crate) fn resume(&mut self) -> anyhow::Result<()> {
        self.resume_tx
            .take()
            .ok_or_else(|| anyhow!("disconnectable websocket proxy is already resumed"))?
            .send(())
            .map_err(|_| anyhow!("disconnectable websocket proxy stopped"))?;
        Ok(())
    }
}

async fn run_disconnectable_proxy(
    listener: TcpListener,
    upstream: String,
    pause_rx: oneshot::Receiver<()>,
    blocked_connection_tx: oneshot::Sender<()>,
    mut resume_rx: oneshot::Receiver<()>,
) {
    let Ok((mut downstream, _)) = listener.accept().await else {
        return;
    };
    let Ok(mut upstream_stream) = TcpStream::connect(&upstream).await else {
        return;
    };
    tokio::select! {
        _ = copy_bidirectional(&mut downstream, &mut upstream_stream) => return,
        _ = pause_rx => {}
    }
    drop(downstream);
    drop(upstream_stream);

    let mut blocked_connection_tx = Some(blocked_connection_tx);
    loop {
        tokio::select! {
            _ = &mut resume_rx => break,
            accepted = listener.accept() => {
                let Ok((blocked, _)) = accepted else {
                    break;
                };
                drop(blocked);
                if let Some(blocked_connection_tx) = blocked_connection_tx.take() {
                    let _ = blocked_connection_tx.send(());
                }
            }
        }
    }

    loop {
        let Ok((mut downstream, _)) = listener.accept().await else {
            return;
        };
        let Ok(mut upstream_stream) = TcpStream::connect(&upstream).await else {
            continue;
        };
        let _ = copy_bidirectional(&mut downstream, &mut upstream_stream).await;
    }
}

async fn connect_websocket_when_ready(
    websocket_url: &str,
) -> anyhow::Result<(
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    tokio_tungstenite::tungstenite::handshake::client::Response,
)> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        match connect_async(websocket_url).await {
            Ok(websocket) => return Ok(websocket),
            Err(err)
                if Instant::now() < deadline
                    && matches!(
                        err,
                        tokio_tungstenite::tungstenite::Error::Io(ref io_err)
                            if io_err.kind() == std::io::ErrorKind::ConnectionRefused
                    ) =>
            {
                sleep(CONNECT_RETRY_INTERVAL).await;
            }
            Err(err) => return Err(err.into()),
        }
    }
}

async fn read_listen_url_from_stdout(child: &mut Child) -> anyhow::Result<String> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("failed to capture exec-server stdout"))?;
    let mut lines = BufReader::new(stdout).lines();
    let deadline = Instant::now() + CONNECT_TIMEOUT;

    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(anyhow!(
                "timed out waiting for exec-server listen URL on stdout after {CONNECT_TIMEOUT:?}"
            ));
        }
        let remaining = deadline.duration_since(now);
        let line = timeout(remaining, lines.next_line())
            .await
            .map_err(|_| anyhow!("timed out waiting for exec-server stdout"))??
            .ok_or_else(|| anyhow!("exec-server stdout closed before emitting listen URL"))?;
        let listen_url = line.trim();
        if listen_url.starts_with("ws://") {
            return Ok(listen_url.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::SinkExt;
    use futures::StreamExt;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    #[tokio::test]
    async fn resume_response_wait_ignores_interleaved_messages() -> anyhow::Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let websocket_url = format!("ws://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let mut websocket = accept_async(stream).await?;
            let Some(Ok(Message::Text(request))) = websocket.next().await else {
                anyhow::bail!("expected initialize request");
            };
            let JSONRPCMessage::Request(request) = serde_json::from_str(request.as_ref())? else {
                anyhow::bail!("expected JSON-RPC request");
            };

            for message in [
                JSONRPCMessage::Notification(JSONRPCNotification {
                    method: "process/outputDelta".to_string(),
                    params: Some(serde_json::json!({"processId": "proc-resume"})),
                }),
                JSONRPCMessage::Request(JSONRPCRequest {
                    id: RequestId::Integer(99),
                    method: "server/request".to_string(),
                    params: None,
                    trace: None,
                }),
                JSONRPCMessage::Response(JSONRPCResponse {
                    id: request.id,
                    result: serde_json::json!({"sessionId": "session-1"}),
                }),
            ] {
                websocket
                    .send(Message::Text(serde_json::to_string(&message)?.into()))
                    .await?;
            }
            Ok::<_, anyhow::Error>(())
        });

        let (websocket, _) = connect_async(&websocket_url).await?;
        let child = Command::new(std::env::current_exe()?)
            .arg("--help")
            .stdin(Stdio::null())
            .spawn()?;
        let mut harness = ExecServerHarness {
            _codex_home: TempDir::new()?,
            _helper_paths: TestCodexHelperPaths {
                codex_exe: PathBuf::new(),
                codex_linux_sandbox_exe: None,
            },
            child,
            websocket_url,
            websocket,
            next_request_id: 1,
        };

        let request_id = harness
            .send_request("initialize", serde_json::json!({}))
            .await?;
        let response = harness
            .wait_for_resume_initialize_response(
                &request_id,
                Instant::now() + Duration::from_secs(1),
            )
            .await?;
        assert!(matches!(
            response,
            JSONRPCMessage::Response(JSONRPCResponse { id, .. }) if id == request_id
        ));

        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn resume_response_wait_keeps_absolute_deadline_after_interleaved_messages()
    -> anyhow::Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let websocket_url = format!("ws://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let mut websocket = accept_async(stream).await?;
            let Some(Ok(Message::Text(request))) = websocket.next().await else {
                anyhow::bail!("expected initialize request");
            };
            let JSONRPCMessage::Request(request) = serde_json::from_str(request.as_ref())? else {
                anyhow::bail!("expected JSON-RPC request");
            };

            for message in [
                JSONRPCMessage::Notification(JSONRPCNotification {
                    method: "process/outputDelta".to_string(),
                    params: None,
                }),
                JSONRPCMessage::Request(JSONRPCRequest {
                    id: RequestId::Integer(99),
                    method: "server/request".to_string(),
                    params: None,
                    trace: None,
                }),
            ] {
                websocket
                    .send(Message::Text(serde_json::to_string(&message)?.into()))
                    .await?;
            }
            sleep(Duration::from_millis(100)).await;
            let response = JSONRPCMessage::Response(JSONRPCResponse {
                id: request.id,
                result: serde_json::json!({"sessionId": "session-1"}),
            });
            let _ = websocket
                .send(Message::Text(serde_json::to_string(&response)?.into()))
                .await;
            Ok::<_, anyhow::Error>(())
        });

        let (websocket, _) = connect_async(&websocket_url).await?;
        let child = Command::new(std::env::current_exe()?)
            .arg("--help")
            .stdin(Stdio::null())
            .spawn()?;
        let mut harness = ExecServerHarness {
            _codex_home: TempDir::new()?,
            _helper_paths: TestCodexHelperPaths {
                codex_exe: PathBuf::new(),
                codex_linux_sandbox_exe: None,
            },
            child,
            websocket_url,
            websocket,
            next_request_id: 1,
        };

        let request_id = harness
            .send_request("initialize", serde_json::json!({}))
            .await?;
        let error = harness
            .wait_for_resume_initialize_response(
                &request_id,
                Instant::now() + Duration::from_millis(25),
            )
            .await
            .expect_err("the original deadline must not reset after interleaved messages");
        assert!(error
            .to_string()
            .contains("timed out waiting for exec-server websocket event"));

        server.await??;
        Ok(())
    }
}
