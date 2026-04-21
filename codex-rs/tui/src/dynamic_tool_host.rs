use crate::extensions::tui_hooks;
use crate::legacy_core::config::Config;
use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::DynamicToolCallParams;
use codex_app_server_protocol::DynamicToolCallResponse;
use codex_app_server_protocol::DynamicToolSpec;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::warn;

const LOAD_DYNAMIC_TOOL_SPECS_TIMEOUT: Duration = Duration::from_secs(10);
const EXECUTE_DYNAMIC_TOOL_CALL_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) async fn load_dynamic_tool_specs(config: &Config) -> Vec<DynamicToolSpec> {
    let Some(command) = tui_hooks().dynamic_tool_command(config) else {
        return Vec::new();
    };

    match load_dynamic_tool_specs_for_command(&command.command).await {
        Ok(specs) => specs,
        Err(err) => {
            warn!("failed to load dynamic tool specs: {err}");
            Vec::new()
        }
    }
}

pub(crate) async fn execute_dynamic_tool_call(
    config: &Config,
    params: &DynamicToolCallParams,
) -> DynamicToolCallResponse {
    let Some(command) = tui_hooks().dynamic_tool_command(config) else {
        return failed_dynamic_tool_response("dynamic tool host is unavailable");
    };

    match execute_dynamic_tool_call_for_command(&command.command, params).await {
        Ok(response) => response,
        Err(err) => {
            warn!(
                tool = %params.tool,
                call_id = %params.call_id,
                "dynamic tool call failed: {err}"
            );
            failed_dynamic_tool_response(format!("Dynamic tool `{}` failed: {err}", params.tool))
        }
    }
}

async fn load_dynamic_tool_specs_for_command(
    command: &[String],
) -> Result<Vec<DynamicToolSpec>, String> {
    let stdout =
        run_dynamic_tool_command(command, "specs", None, LOAD_DYNAMIC_TOOL_SPECS_TIMEOUT).await?;
    serde_json::from_slice::<Vec<DynamicToolSpec>>(&stdout)
        .map_err(|err| format!("dynamic tool host returned invalid specs JSON: {err}"))
}

async fn execute_dynamic_tool_call_for_command(
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

fn failed_dynamic_tool_response(message: impl Into<String>) -> DynamicToolCallResponse {
    DynamicToolCallResponse {
        content_items: vec![DynamicToolCallOutputContentItem::InputText {
            text: message.into(),
        }],
        success: false,
    }
}

#[cfg(test)]
mod tests {
    use super::execute_dynamic_tool_call_for_command;
    use super::load_dynamic_tool_specs_for_command;
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
