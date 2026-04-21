use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::DynamicToolCallParams;
use codex_app_server_protocol::DynamicToolCallResponse;
use codex_app_server_protocol::DynamicToolSpec;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

const CODEX_DYNAMIC_TOOL_COMMAND_ENV_VAR: &str = "CODEX_DYNAMIC_TOOL_COMMAND";
const LOAD_DYNAMIC_TOOL_SPECS_TIMEOUT: Duration = Duration::from_secs(10);
const EXECUTE_DYNAMIC_TOOL_CALL_TIMEOUT: Duration = Duration::from_secs(30);

pub fn dynamic_tool_host_command_from_env() -> Option<Vec<String>> {
    let command = std::env::var(CODEX_DYNAMIC_TOOL_COMMAND_ENV_VAR).ok()?;
    parse_dynamic_tool_host_command(command.trim())
}

pub async fn load_dynamic_tool_specs_for_command(
    command: &[String],
) -> Result<Vec<DynamicToolSpec>, String> {
    let stdout =
        run_dynamic_tool_command(command, "specs", None, LOAD_DYNAMIC_TOOL_SPECS_TIMEOUT).await?;
    serde_json::from_slice::<Vec<DynamicToolSpec>>(&stdout)
        .map_err(|err| format!("dynamic tool host returned invalid specs JSON: {err}"))
}

pub async fn execute_dynamic_tool_call_for_command(
    command: &[String],
    params: &DynamicToolCallParams,
) -> Result<DynamicToolCallResponse, String> {
    let stdin = serde_json::to_vec(params)
        .map_err(|err| format!("failed to encode dynamic tool request: {err}"))?;
    let stdout = run_dynamic_tool_command(
        command,
        "call",
        Some(stdin),
        EXECUTE_DYNAMIC_TOOL_CALL_TIMEOUT,
    )
    .await?;
    serde_json::from_slice::<DynamicToolCallResponse>(&stdout)
        .map_err(|err| format!("dynamic tool host returned invalid response JSON: {err}"))
}

pub fn failed_dynamic_tool_response(message: impl Into<String>) -> DynamicToolCallResponse {
    DynamicToolCallResponse {
        content_items: vec![DynamicToolCallOutputContentItem::InputText {
            text: message.into(),
        }],
        success: false,
    }
}

fn parse_dynamic_tool_host_command(command: &str) -> Option<Vec<String>> {
    let command = split_command_string(command);
    if command.is_empty() || command.first().is_some_and(String::is_empty) {
        return None;
    }
    Some(command)
}

fn split_command_string(command: &str) -> Vec<String> {
    let Some(parts) = shlex::split(command) else {
        return vec![command.to_string()];
    };
    match shlex::try_join(parts.iter().map(String::as_str)) {
        Ok(round_trip)
            if round_trip == command
                || (!command.contains(":\\")
                    && shlex::split(&round_trip).as_ref() == Some(&parts)) =>
        {
            parts
        }
        _ => vec![command.to_string()],
    }
}

async fn run_dynamic_tool_command(
    command: &[String],
    mode: &str,
    stdin: Option<Vec<u8>>,
    timeout_duration: Duration,
) -> Result<Vec<u8>, String> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| "dynamic tool command is empty".to_string())?;

    let mut process = Command::new(program);
    process
        .args(args)
        .arg(mode)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = process
        .spawn()
        .map_err(|err| format!("failed to spawn dynamic tool host `{program}`: {err}"))?;

    if let Some(stdin) = stdin
        && let Some(mut child_stdin) = child.stdin.take()
    {
        child_stdin
            .write_all(&stdin)
            .await
            .map_err(|err| format!("failed to write dynamic tool input: {err}"))?;
    }

    let output = timeout(timeout_duration, child.wait_with_output())
        .await
        .map_err(|_| {
            format!(
                "timed out waiting for dynamic tool host after {}s",
                timeout_duration.as_secs()
            )
        })?
        .map_err(|err| format!("failed waiting for dynamic tool host: {err}"))?;

    if output.status.success() {
        return Ok(output.stdout);
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        format!("exited with status {}", output.status)
    };
    Err(format!("dynamic tool host {mode} failed: {detail}"))
}

#[cfg(test)]
mod tests {
    use super::execute_dynamic_tool_call_for_command;
    use super::load_dynamic_tool_specs_for_command;
    use super::parse_dynamic_tool_host_command;
    use codex_app_server_protocol::DynamicToolCallOutputContentItem;
    use codex_app_server_protocol::DynamicToolCallParams;
    use codex_app_server_protocol::DynamicToolCallResponse;
    use codex_app_server_protocol::DynamicToolImageDetail;
    use pretty_assertions::assert_eq;

    #[cfg(unix)]
    fn shell_command(script: &str) -> Vec<String> {
        vec![
            "sh".to_string(),
            "-c".to_string(),
            script.to_string(),
            "dynamic-tool-host.sh".to_string(),
        ]
    }

    #[test]
    fn parse_dynamic_tool_host_command_round_trips_shell_wrappers() {
        let command =
            shlex::try_join(["/bin/zsh", "-lc", r#"python3 -c 'print("Hello, world!")'"#])
                .expect("round-trippable command");

        assert_eq!(
            parse_dynamic_tool_host_command(&command),
            Some(vec![
                "/bin/zsh".to_string(),
                "-lc".to_string(),
                r#"python3 -c 'print("Hello, world!")'"#.to_string(),
            ])
        );
    }

    #[test]
    fn parse_dynamic_tool_host_command_preserves_non_roundtrippable_windows_commands() {
        let command = r#"C:\Program Files\Git\bin\bash.exe -lc "echo hi""#;

        assert_eq!(
            parse_dynamic_tool_host_command(command),
            Some(vec![command.to_string()])
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn loads_dynamic_tool_specs_from_command_host() {
        let command = shell_command(
            r#"
set -eu
case "$1" in
  specs)
    cat <<'EOF'
[{"name":"android_observe","description":"Observe the live Android screen","inputSchema":{"type":"object","properties":{"prompt":{"type":"string"}},"additionalProperties":false},"deferLoading":false}]
EOF
    ;;
  call)
    cat >/dev/null
    cat <<'EOF'
{"contentItems":[{"type":"inputText","text":"Observed the Android screen."},{"type":"inputImage","imageUrl":"data:image/png;base64,AAA","detail":"original"}],"success":true}
EOF
    ;;
  *)
    echo "unexpected mode" >&2
    exit 1
    ;;
esac
"#,
        );

        let specs = load_dynamic_tool_specs_for_command(&command)
            .await
            .expect("load specs");

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "android_observe");
        assert_eq!(specs[0].description, "Observe the live Android screen");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn executes_dynamic_tool_call_with_original_image_detail() {
        let command = shell_command(
            r#"
set -eu
case "$1" in
  specs)
    echo '[]'
    ;;
  call)
    cat >/dev/null
    cat <<'EOF'
{"contentItems":[{"type":"inputText","text":"Observed the Android screen."},{"type":"inputImage","imageUrl":"data:image/png;base64,AAA","detail":"original"}],"success":true}
EOF
    ;;
  *)
    echo "unexpected mode" >&2
    exit 1
    ;;
esac
"#,
        );

        let response = execute_dynamic_tool_call_for_command(
            &command,
            &DynamicToolCallParams {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                call_id: "call-1".to_string(),
                tool: "android_observe".to_string(),
                arguments: serde_json::json!({
                    "prompt": "Summarize the current screen."
                }),
            },
        )
        .await
        .expect("execute call");

        assert_eq!(
            response,
            DynamicToolCallResponse {
                content_items: vec![
                    DynamicToolCallOutputContentItem::InputText {
                        text: "Observed the Android screen.".to_string(),
                    },
                    DynamicToolCallOutputContentItem::InputImage {
                        image_url: "data:image/png;base64,AAA".to_string(),
                        detail: Some(DynamicToolImageDetail::Original),
                    },
                ],
                success: true,
            }
        );
    }
}
