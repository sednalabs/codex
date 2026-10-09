use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::DynamicToolCallParams;
use codex_app_server_protocol::DynamicToolCallResponse;
use codex_app_server_protocol::DynamicToolNamespaceSpec;
use codex_app_server_protocol::DynamicToolNamespaceTool;
use codex_app_server_protocol::DynamicToolSpec;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadStartParams;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::io;
use std::io::Write;
use std::path::Path;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::process::Command;
use tokio::time::timeout;

pub(crate) const NAMESPACE: &str = "codex_desktop";
const OBSERVE_TOOL_NAME: &str = "desktop_observe";
const STEP_TOOL_NAME: &str = "desktop_step";
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_REQUEST_BYTES: usize = 65_536;
const MAX_STDOUT_BYTES: usize = 50_331_648;
const MAX_STDERR_CAPTURE_BYTES: usize = 16_384;
const IO_CHUNK_BYTES: usize = 8_192;
const ENV_COMMAND: &str = "CODEX_DESKTOP_COMPUTER_USE_COMMAND";
const ENV_PROVIDER: &str = "CODEX_DESKTOP_COMPUTER_USE_PROVIDER";
const ENV_TIMEOUT_SECS: &str = "CODEX_DESKTOP_COMPUTER_USE_TIMEOUT_SECS";
const PROVIDER_COMMAND: &str = "command";

pub(crate) fn is_desktop_call(params: &DynamicToolCallParams) -> bool {
    params.namespace.as_deref() == Some(NAMESPACE)
}

pub(crate) fn completed_event(
    request_id: RequestId,
    response: DynamicToolCallResponse,
) -> crate::app_event::AppEvent {
    crate::app_event::AppEvent::DynamicToolCallCompleted {
        request_id,
        response,
    }
}

pub(crate) fn validate_no_reserved_name_conflict(params: &ThreadStartParams) -> Result<(), String> {
    if let Some(specs) = &params.dynamic_tools {
        for spec in specs {
            match spec {
                DynamicToolSpec::Namespace(namespace) if namespace.name == NAMESPACE => {
                    return Err(format!(
                        "Desktop dynamic namespace `{NAMESPACE}` is reserved; remove the configured namespace before starting this thread."
                    ));
                }
                DynamicToolSpec::Function(function) if function.name == NAMESPACE => {
                    return Err(format!(
                        "Desktop dynamic function `{NAMESPACE}` is reserved; remove the configured function before starting this thread."
                    ));
                }
                _ => {}
            }
        }
    }
    if let Some(config) = &params.config
        && (config.contains_key(&format!("mcp_servers.{NAMESPACE}"))
            || config
                .get("mcp_servers")
                .and_then(Value::as_object)
                .is_some_and(|servers| servers.contains_key(NAMESPACE)))
    {
        return Err(format!(
            "MCP server key `{NAMESPACE}` is reserved by Desktop dynamic tools; rename that server before starting this thread."
        ));
    }
    Ok(())
}

pub(crate) fn specs_for_codex_home(codex_home: &Path) -> Vec<DynamicToolSpec> {
    match DesktopRuntimeConfig::load(codex_home) {
        Ok(Some(_)) => vec![DynamicToolSpec::Namespace(DynamicToolNamespaceSpec {
            name: NAMESPACE.to_string(),
            description: "Opt-in desktop computer-use command provider".to_string(),
            tools: vec![
                desktop_dynamic_tool(
                    OBSERVE_TOOL_NAME,
                    "Capture the current desktop app state as a model-visible screenshot.",
                ),
                desktop_dynamic_tool(
                    STEP_TOOL_NAME,
                    "Perform configured desktop UI actions, then return a fresh screenshot.",
                ),
            ],
        })],
        Ok(None) | Err(_) => Vec::new(),
    }
}

fn desktop_dynamic_tool(name: &str, description: &str) -> DynamicToolNamespaceTool {
    DynamicToolNamespaceTool::Function(DynamicToolFunctionSpec {
        name: name.to_string(),
        description: description.to_string(),
        input_schema: json!({
            "type": "object",
            "additionalProperties": true
        }),
        defer_loading: false,
    })
}

pub(crate) async fn handle_for_codex_home(
    params: &DynamicToolCallParams,
    codex_home: &Path,
) -> Option<DynamicToolCallResponse> {
    if !is_desktop_call(params)
        || !matches!(params.tool.as_str(), OBSERVE_TOOL_NAME | STEP_TOOL_NAME)
    {
        return None;
    }

    let config = match DesktopRuntimeConfig::load(codex_home) {
        Ok(Some(config)) => config,
        Ok(None) => return None,
        Err(error) => return Some(failed_response(error)),
    };

    let response = match run_provider(params, config).await {
        Ok(mut response) => {
            require_native_image_for_visual_response(
                &mut response,
                "Desktop visual result is missing native image output.",
            );
            response
        }
        Err(error) => failed_response(error),
    };
    Some(response)
}

async fn run_provider(
    params: &DynamicToolCallParams,
    config: DesktopRuntimeConfig,
) -> Result<DynamicToolCallResponse, String> {
    let request = serialize_request_bounded(params)?;

    let (stdout, stderr, status) = run_provider_process(
        &config.argv,
        &request,
        config.timeout,
        MAX_STDOUT_BYTES,
        MAX_STDERR_CAPTURE_BYTES,
    )
    .await?;
    let stderr_was_truncated = stderr.truncated;
    let stderr_text = String::from_utf8_lossy(&stderr.bytes);
    if !status.success() {
        let truncation = if stderr_was_truncated {
            format!("; stderr capture truncated after {MAX_STDERR_CAPTURE_BYTES} bytes")
        } else {
            String::new()
        };
        return Err(format!(
            "Desktop provider exited with status {status}: {}{}. Its step result may be uncertain; do not replay automatically. Recover with desktop_observe.",
            compact_process_output(&stderr_text),
            truncation
        ));
    }

    let mut response = parse_provider_response(&stdout)?;
    if stderr_was_truncated {
        append_text(
            &mut response.content_items,
            &format!(
                "\n\nDesktop provider stderr was truncated after {MAX_STDERR_CAPTURE_BYTES} bytes; excess output was drained and discarded."
            ),
        );
    }
    Ok(response)
}

struct BoundedRequestWriter {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl Write for BoundedRequestWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(buffer.len()) > self.limit {
            self.exceeded = true;
            return Err(io::Error::other("serialized request limit exceeded"));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialize_request_bounded<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    let mut writer = BoundedRequestWriter {
        bytes: Vec::with_capacity(MAX_REQUEST_BYTES),
        limit: MAX_REQUEST_BYTES,
        exceeded: false,
    };
    if let Err(error) = serde_json::to_writer(&mut writer, value) {
        if writer.exceeded {
            return Err(format!(
                "Desktop provider request exceeds the {MAX_REQUEST_BYTES}-byte JSON limit. The command was not started."
            ));
        }
        return Err(format!(
            "failed to serialize desktop provider request: {error}"
        ));
    }
    Ok(writer.bytes)
}

#[derive(Debug)]
struct CapturedStderr {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn run_provider_process(
    argv: &[String],
    request: &[u8],
    deadline: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> Result<(Vec<u8>, CapturedStderr, ExitStatus), String> {
    let (program, args) = argv
        .split_first()
        .filter(|(program, _)| !program.trim().is_empty())
        .ok_or_else(|| "Desktop provider command is empty.".to_string())?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("failed to start desktop provider `{program}`: {error}"))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "failed to open desktop provider stdin".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to open desktop provider stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "failed to open desktop provider stderr".to_string())?;

    let process_io = async {
        let write = write_request(stdin, request);
        let read_stdout = read_stdout_bounded(stdout, stdout_limit);
        let read_stderr = read_stderr_bounded(stderr, stderr_limit);
        let wait = async {
            child
                .wait()
                .await
                .map_err(|error| {
                    format!(
                        "failed to wait for desktop provider: {error}. The command may have executed; do not replay a step automatically. Recover with desktop_observe."
                    )
                })
        };
        tokio::try_join!(write, read_stdout, read_stderr, wait)
    };

    match timeout(deadline, process_io).await {
        Ok(Ok(((), stdout, stderr, status))) => Ok((stdout, stderr, status)),
        Ok(Err(error)) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            Err(error)
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            Err(format!(
                "Desktop provider timed out after {} seconds. The command may have executed; do not replay a step automatically. Recover with desktop_observe.",
                deadline.as_secs()
            ))
        }
    }
}

async fn write_request(mut stdin: ChildStdin, request: &[u8]) -> Result<(), String> {
    stdin
        .write_all(request)
        .await
        .map_err(|error| {
            format!(
                "failed to write desktop provider request: {error}. The command may have received a partial request; do not replay a step automatically. Recover with desktop_observe."
            )
        })?;
    stdin
        .shutdown()
        .await
        .map_err(|error| {
            format!(
                "failed to close desktop provider stdin: {error}. The command may have executed; do not replay a step automatically. Recover with desktop_observe."
            )
        })
}

async fn read_stdout_bounded(stdout: ChildStdout, limit: usize) -> Result<Vec<u8>, String> {
    match read_to_end_bounded(stdout, limit).await {
        Ok(output) => Ok(output),
        Err(BoundedReadError::Limit) => Err(format!(
            "Desktop provider stdout exceeded the {limit}-byte JSON limit. Its action result is unknown; output was not truncated into success. Recover with desktop_observe before deciding whether to retry."
        )),
        Err(BoundedReadError::Io(error)) => Err(format!(
            "Failed while reading desktop provider stdout: {error}. The command may have executed; do not replay a step automatically. Recover with desktop_observe."
        )),
    }
}

async fn read_stderr_bounded<R: AsyncRead + Unpin>(
    stderr: R,
    limit: usize,
) -> Result<CapturedStderr, String> {
    let mut reader = stderr;
    let mut captured = Vec::with_capacity(limit.min(IO_CHUNK_BYTES));
    let mut truncated = false;
    let mut buffer = [0_u8; IO_CHUNK_BYTES];
    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .map_err(|error| {
                format!(
                    "failed to drain desktop provider stderr: {error}. The command may have executed; do not replay a step automatically. Recover with desktop_observe."
                )
            })?;
        if count == 0 {
            break;
        }
        let remaining = limit.saturating_sub(captured.len());
        let keep = remaining.min(count);
        captured.extend_from_slice(&buffer[..keep]);
        truncated |= keep < count;
    }
    Ok(CapturedStderr {
        bytes: captured,
        truncated,
    })
}

async fn read_to_end_bounded<R: AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
) -> Result<Vec<u8>, BoundedReadError> {
    let mut output = Vec::with_capacity(limit.min(IO_CHUNK_BYTES));
    let mut buffer = [0_u8; IO_CHUNK_BYTES];
    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .map_err(|error| BoundedReadError::Io(error.to_string()))?;
        if count == 0 {
            return Ok(output);
        }
        let remaining = limit.saturating_sub(output.len());
        if count > remaining {
            return Err(BoundedReadError::Limit);
        }
        output.extend_from_slice(&buffer[..count]);
    }
}

#[derive(Debug)]
enum BoundedReadError {
    Limit,
    Io(String),
}

fn parse_provider_response(bytes: &[u8]) -> Result<DynamicToolCallResponse, String> {
    serde_json::from_slice(bytes).map_err(|error| {
        let snippet = compact_process_output(&String::from_utf8_lossy(bytes));
        format!(
            "failed to parse desktop provider response: {error}; stdout: {snippet}. The command may have executed; do not replay a step automatically. Recover with desktop_observe."
        )
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DesktopRuntimeConfig {
    argv: Vec<String>,
    timeout: Duration,
}

impl DesktopRuntimeConfig {
    fn load(codex_home: &Path) -> Result<Option<Self>, String> {
        Self::from_sources(
            DesktopRuntimeConfigFile::load(codex_home),
            DesktopRuntimeEnv::read(),
        )
    }

    fn from_sources(
        file: Option<DesktopRuntimeConfigFile>,
        env: DesktopRuntimeEnv,
    ) -> Result<Option<Self>, String> {
        if let Some(provider) = env.provider.as_deref()
            && !provider_is_command(provider)
        {
            return Ok(None);
        }
        if let Some(error) = env.timeout_error {
            return Err(error);
        }

        let timeout_secs = env
            .timeout_secs
            .or_else(|| file.as_ref().and_then(|config| config.timeout_secs));
        let timeout = timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT);
        if timeout > MAX_REQUEST_TIMEOUT {
            return Err(format!(
                "Configured desktop provider timeout of {} seconds exceeds the {}-second maximum; the command was not started.",
                timeout.as_secs(),
                MAX_REQUEST_TIMEOUT.as_secs()
            ));
        }

        if let Some(command) = env.command {
            let Some(argv) = command_spec_to_argv(CommandSpec::String(command)) else {
                return Ok(None);
            };
            return Ok(Some(Self { argv, timeout }));
        }

        let Some(file) = file else {
            return Ok(None);
        };
        if file
            .provider
            .as_deref()
            .is_some_and(|provider| !provider_is_command(provider))
        {
            return Ok(None);
        }
        let command = file.command;
        let Some(argv) = command.and_then(command_spec_to_argv) else {
            return Ok(None);
        };
        Ok(Some(Self { argv, timeout }))
    }
}

#[derive(Deserialize)]
struct DesktopRuntimeConfigFile {
    provider: Option<String>,
    command: Option<CommandSpec>,
    timeout_secs: Option<u64>,
}

impl DesktopRuntimeConfigFile {
    fn load(codex_home: &Path) -> Option<Self> {
        [
            codex_home.join("desktop-computer-use.json"),
            codex_home.join("desktop-dynamic-tools.json"),
        ]
        .into_iter()
        .find_map(|path| {
            std::fs::read_to_string(path)
                .ok()
                .and_then(|contents| serde_json::from_str(&contents).ok())
        })
    }
}

#[derive(Default)]
struct DesktopRuntimeEnv {
    provider: Option<String>,
    command: Option<String>,
    timeout_secs: Option<u64>,
    timeout_error: Option<String>,
}

impl DesktopRuntimeEnv {
    fn read() -> Self {
        let timeout_setting = read_env(ENV_TIMEOUT_SECS);
        let (timeout_secs, timeout_error) = match timeout_setting {
            Some(value) => match value.parse() {
                Ok(timeout) => (Some(timeout), None),
                Err(_) => (
                    None,
                    Some(format!(
                        "Invalid {ENV_TIMEOUT_SECS}; provide a whole number of seconds. The command was not started."
                    )),
                ),
            },
            None => (None, None),
        };
        Self {
            provider: read_env(ENV_PROVIDER),
            command: read_env(ENV_COMMAND),
            timeout_secs,
            timeout_error,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(untagged)]
enum CommandSpec {
    String(String),
    Array(Vec<String>),
}

fn command_spec_to_argv(command: CommandSpec) -> Option<Vec<String>> {
    let argv = match command {
        CommandSpec::String(command) => shlex::split(&command)?,
        CommandSpec::Array(argv) => argv,
    };
    argv.first()
        .is_some_and(|program| !program.trim().is_empty())
        .then_some(argv)
}

fn read_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn provider_is_command(provider: &str) -> bool {
    provider.trim().eq_ignore_ascii_case(PROVIDER_COMMAND)
}

fn append_text(items: &mut Vec<DynamicToolCallOutputContentItem>, extra: &str) {
    if let Some(DynamicToolCallOutputContentItem::InputText { text }) = items.first_mut() {
        text.push_str(extra);
    } else {
        items.insert(
            0,
            DynamicToolCallOutputContentItem::InputText {
                text: extra.trim().to_string(),
            },
        );
    }
}

fn require_native_image_for_visual_response(
    response: &mut DynamicToolCallResponse,
    missing_image_message: &str,
) {
    if !response.success
        || response
            .content_items
            .iter()
            .any(|item| matches!(item, DynamicToolCallOutputContentItem::InputImage { .. }))
    {
        return;
    }
    append_text(
        &mut response.content_items,
        &format!(
            "\n\n{missing_image_message} The desktop provider must return screenshots as native image content items, not text-only summaries or artifact paths."
        ),
    );
    response.success = false;
}

fn failed_response(error: String) -> DynamicToolCallResponse {
    DynamicToolCallResponse {
        content_items: vec![DynamicToolCallOutputContentItem::InputText { text: error }],
        success: false,
    }
}

fn compact_process_output(output: &str) -> String {
    const LIMIT: usize = 500;
    let compact = output.trim();
    if compact.chars().count() <= LIMIT {
        return compact.to_string();
    }
    let mut truncated = compact.chars().take(LIMIT - 3).collect::<String>();
    truncated.push_str("...");
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::DynamicToolCallOutputContentItem;

    #[test]
    fn configured_command_requires_explicit_command_provider_and_valid_argv() {
        assert_eq!(
            command_spec_to_argv(CommandSpec::String("desktop-provider --stdio".to_string())),
            Some(vec!["desktop-provider".to_string(), "--stdio".to_string()])
        );
        assert_eq!(
            command_spec_to_argv(CommandSpec::Array(vec![
                "desktop-provider".to_string(),
                "--stdio".to_string()
            ])),
            Some(vec!["desktop-provider".to_string(), "--stdio".to_string()])
        );
        assert!(command_spec_to_argv(CommandSpec::String("   ".to_string())).is_none());
        assert!(shlex::split("unterminated '").is_none());
    }

    #[test]
    fn configured_timeout_above_policy_is_rejected_without_clamping() {
        let config = DesktopRuntimeConfigFile {
            provider: Some(PROVIDER_COMMAND.to_string()),
            command: Some(CommandSpec::Array(vec!["fake".to_string()])),
            timeout_secs: Some(301),
        };
        let result = DesktopRuntimeConfig::from_sources(Some(config), DesktopRuntimeEnv::default());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("exceeds"));
    }

    #[tokio::test]
    async fn oversized_request_is_rejected_before_spawn() {
        let params = DynamicToolCallParams {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            call_id: "call".to_string(),
            namespace: Some(NAMESPACE.to_string()),
            tool: STEP_TOOL_NAME.to_string(),
            arguments: json!({"payload": "x".repeat(MAX_REQUEST_BYTES)}),
        };
        let config = DesktopRuntimeConfig {
            argv: vec!["this-executable-must-not-be-started".to_string()],
            timeout: DEFAULT_REQUEST_TIMEOUT,
        };
        let error = run_provider(&params, config).await.unwrap_err();
        assert!(error.contains("The command was not started"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exact_request_limit_is_accepted_and_sent_to_fake_provider() {
        let mut params = DynamicToolCallParams {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            call_id: "call".to_string(),
            namespace: Some(NAMESPACE.to_string()),
            tool: OBSERVE_TOOL_NAME.to_string(),
            arguments: json!(""),
        };
        let baseline = serialize_request_bounded(&params).unwrap().len();
        params.arguments = json!("x".repeat(MAX_REQUEST_BYTES - baseline));
        assert_eq!(
            serialize_request_bounded(&params).unwrap().len(),
            MAX_REQUEST_BYTES
        );

        let expected = DynamicToolCallResponse {
            content_items: vec![DynamicToolCallOutputContentItem::InputText {
                text: "ok".to_string(),
            }],
            success: true,
        };
        let response_json = serde_json::to_string(&expected).unwrap();
        let config = DesktopRuntimeConfig {
            argv: vec![
                "python3".to_string(),
                "-c".to_string(),
                "import sys; data = sys.stdin.buffer.read(); sys.exit(7) if len(data) != 65536 else None; sys.stdout.write(sys.argv[1])"
                    .to_string(),
                response_json,
            ],
            timeout: Duration::from_secs(3),
        };
        let response = run_provider(&params, config).await.unwrap();
        assert_eq!(response, expected);
    }

    #[test]
    fn explicit_none_and_unsupported_provider_remain_unavailable() {
        for provider in ["none", "unknown"] {
            let env = DesktopRuntimeEnv {
                provider: Some(provider.to_string()),
                ..DesktopRuntimeEnv::default()
            };
            assert!(
                DesktopRuntimeConfig::from_sources(/*file*/ None, env)
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn stdout_ceiling_rejects_instead_of_returning_truncated_json() {
        let (mut writer, reader) = tokio::io::duplex(32);
        let reader_task = tokio::spawn(read_to_end_bounded(reader, /*limit*/ 4));
        writer.write_all(b"12345").await.unwrap();
        drop(writer);
        assert!(reader_task.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn stderr_is_drained_beyond_capture_limit_and_marks_truncation() {
        let (mut writer, reader) = tokio::io::duplex(32);
        let reader_task = tokio::spawn(read_stderr_bounded(reader, /*limit*/ 4));
        writer.write_all(b"123456789").await.unwrap();
        drop(writer);
        let captured = reader_task.await.unwrap().unwrap();
        assert_eq!(captured.bytes.as_slice(), b"1234");
        assert!(captured.truncated);
    }

    #[tokio::test]
    async fn exact_and_over_configured_stdout_limits_are_distinguished() {
        fn json_response_with_text_len(text_len: usize) -> Vec<u8> {
            serde_json::to_vec(&DynamicToolCallResponse {
                content_items: vec![DynamicToolCallOutputContentItem::InputText {
                    text: "x".repeat(text_len),
                }],
                success: true,
            })
            .unwrap()
        }

        async fn limited_output(bytes: Vec<u8>) -> Result<Vec<u8>, BoundedReadError> {
            let (mut writer, reader) = tokio::io::duplex(IO_CHUNK_BYTES * 2);
            let write_task = tokio::spawn(async move {
                for chunk in bytes.chunks(IO_CHUNK_BYTES) {
                    if writer.write_all(chunk).await.is_err() {
                        break;
                    }
                }
            });
            let output = read_to_end_bounded(reader, MAX_STDOUT_BYTES).await;
            write_task.await.unwrap();
            output
        }

        let empty_response = json_response_with_text_len(/*text_len*/ 0);
        let exact_text_len = MAX_STDOUT_BYTES - empty_response.len();
        let exact_json = json_response_with_text_len(exact_text_len);
        assert_eq!(exact_json.len(), MAX_STDOUT_BYTES);
        let exact = limited_output(exact_json).await.unwrap();
        let parsed = parse_provider_response(&exact).unwrap();
        assert!(parsed.success);
        assert!(matches!(
            limited_output(json_response_with_text_len(exact_text_len + 1)).await,
            Err(BoundedReadError::Limit)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_provider_drains_large_output_before_reading_full_request() {
        let argv = vec![
            "python3".to_string(),
            "-c".to_string(),
            "import sys; sys.stdout.buffer.write(b'o' * 1048576); sys.stdout.flush(); sys.stderr.buffer.write(b'e' * 1048576); sys.stderr.flush(); data = sys.stdin.buffer.read(); sys.exit(0 if len(data) == 65536 else 7)"
                .to_string(),
        ];
        let request = vec![b'r'; MAX_REQUEST_BYTES];
        let (stdout, stderr, status) = run_provider_process(
            &argv,
            &request,
            Duration::from_secs(5),
            MAX_STDOUT_BYTES,
            MAX_STDERR_CAPTURE_BYTES,
        )
        .await
        .unwrap();
        assert!(status.success());
        assert_eq!(stdout.len(), 1_048_576);
        assert_eq!(stderr.bytes.len(), MAX_STDERR_CAPTURE_BYTES);
        assert!(stderr.truncated);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_provider_full_duplex_preserves_typed_image_and_marks_stderr_truncation() {
        let expected = DynamicToolCallResponse {
            content_items: vec![DynamicToolCallOutputContentItem::InputImage {
                image_url: "data:image/png;base64,AAAA".to_string(),
            }],
            success: true,
        };
        let response_json = serde_json::to_string(&expected).unwrap();
        let request = vec![b'r'; 32_768];
        let argv = vec![
            "python3".to_string(),
            "-c".to_string(),
            "import sys; sys.stdin.buffer.read(); sys.stderr.buffer.write(b'x' * 40000); sys.stdout.write(sys.argv[1])"
                .to_string(),
            response_json,
        ];
        let (stdout, stderr, status) = run_provider_process(
            &argv,
            &request,
            Duration::from_secs(3),
            MAX_STDOUT_BYTES,
            MAX_STDERR_CAPTURE_BYTES,
        )
        .await
        .unwrap();
        assert!(status.success());
        assert_eq!(stderr.bytes.len(), MAX_STDERR_CAPTURE_BYTES);
        assert!(stderr.truncated);

        let mut response = parse_provider_response(&stdout).unwrap();
        require_native_image_for_visual_response(&mut response, "Missing native image.");
        assert!(response.success);
        append_text(
            &mut response.content_items,
            "\nDesktop provider stderr was truncated.",
        );
        assert!(matches!(
            response.content_items.as_slice(),
            [DynamicToolCallOutputContentItem::InputText { text },
             DynamicToolCallOutputContentItem::InputImage { image_url }]
                if text.contains("stderr was truncated")
                    && image_url == "data:image/png;base64,AAAA"
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdout_over_limit_terminates_fake_provider_without_truncated_success() {
        let argv = vec![
            "python3".to_string(),
            "-c".to_string(),
            "import sys,time; sys.stdout.buffer.write(b'12345'); sys.stdout.flush(); time.sleep(5)"
                .to_string(),
        ];
        let started = std::time::Instant::now();
        let result = run_provider_process(
            &argv,
            b"{}",
            Duration::from_secs(3),
            /*stdout_limit*/ 4,
            /*stderr_limit*/ 32,
        )
        .await;
        assert!(
            result
                .unwrap_err()
                .contains("stdout exceeded the 4-byte JSON limit")
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn successful_visual_result_requires_native_inline_image() {
        let mut response = DynamicToolCallResponse {
            content_items: vec![DynamicToolCallOutputContentItem::InputText {
                text: "desktop state".to_string(),
            }],
            success: true,
        };
        require_native_image_for_visual_response(&mut response, "Missing native image.");
        assert!(!response.success);
        assert!(matches!(
            response.content_items.first(),
            Some(DynamicToolCallOutputContentItem::InputText { text }) if text.contains("Missing native image")
        ));

        let with_image = DynamicToolCallResponse {
            content_items: vec![DynamicToolCallOutputContentItem::InputImage {
                image_url: "data:image/png;base64,AAAA".to_string(),
            }],
            success: true,
        };
        let json = serde_json::to_vec(&with_image).unwrap();
        let parsed = parse_provider_response(&json).unwrap();
        assert!(parsed.success);
        assert!(matches!(
            parsed.content_items.as_slice(),
            [DynamicToolCallOutputContentItem::InputImage { image_url }]
                if image_url == "data:image/png;base64,AAAA"
        ));

        let event = completed_event(RequestId::String("desktop-turn-call".to_string()), parsed);
        let crate::app_event::AppEvent::DynamicToolCallCompleted {
            request_id: RequestId::String(request_id),
            response,
        } = event
        else {
            panic!("expected correlated Desktop dynamic-tool completion");
        };
        assert_eq!(request_id, "desktop-turn-call");
        assert!(response.success);
        assert!(matches!(
            response.content_items.as_slice(),
            [DynamicToolCallOutputContentItem::InputImage { image_url }]
                if image_url == "data:image/png;base64,AAAA"
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_owned_fake_child_and_returns_unknown_result() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("started");
        let finished = dir.path().join("finished");
        let argv = vec![
            "python3".to_string(),
            "-c".to_string(),
            format!(
                "from pathlib import Path; import time; Path({:?}).write_text('started'); time.sleep(1.5); Path({:?}).write_text('finished')",
                marker.display().to_string(),
                finished.display().to_string()
            ),
        ];

        let result = run_provider_process(
            &argv,
            b"{{}}",
            Duration::from_millis(500),
            /*stdout_limit*/ 1024,
            /*stderr_limit*/ 32,
        )
        .await;
        assert!(
            result
                .unwrap_err()
                .contains("The command may have executed; do not replay")
        );
        assert!(marker.exists());
        tokio::time::sleep(Duration::from_millis(1_600)).await;
        assert!(
            !finished.exists(),
            "timed-out child continued after cleanup"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_provider_future_drops_and_kills_owned_child() {
        let dir = tempfile::tempdir().unwrap();
        let started = dir.path().join("started");
        let finished = dir.path().join("finished");
        let argv = vec![
            "python3".to_string(),
            "-c".to_string(),
            format!(
                "from pathlib import Path; import time; Path({:?}).write_text('started'); time.sleep(1); Path({:?}).write_text('finished')",
                started.display().to_string(),
                finished.display().to_string()
            ),
        ];
        let task_argv = argv.clone();
        let task = tokio::spawn(async move {
            run_provider_process(
                &task_argv,
                b"{}",
                Duration::from_secs(5),
                /*stdout_limit*/ 1024,
                /*stderr_limit*/ 32,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !started.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        let _ = task.await;
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        assert!(!finished.exists());
    }
}
