#![cfg(windows)]

use std::fs;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use codex_utils_pty::TerminalSize;
use codex_utils_pty::spawn_pty_process;
use tokio::time::timeout;

struct PreviewFixture(PathBuf);

// The empty-chat composer chooses one prompt at random; any one proves that the TUI rendered.
const VISIBLE_COMPOSER_MARKERS: [&[u8]; 8] = [
    b"Explain this codebase",
    b"Summarize recent commits",
    b"Implement {feature}",
    b"Find and fix a bug in @filename",
    b"Write tests for @filename",
    b"Improve documentation in @filename",
    b"Run /review on my current changes",
    b"Use /skills to list available skills",
];

fn visible_terminal_bytes(output: &[u8]) -> Vec<u8> {
    let mut visible = Vec::with_capacity(output.len());
    let mut index = 0;
    while index < output.len() {
        match output[index] {
            0x1b => {
                index += 1;
                if index == output.len() {
                    break;
                }
                match output[index] {
                    b'[' => {
                        index += 1;
                        while index < output.len() {
                            let byte = output[index];
                            index += 1;
                            if (0x40..=0x7e).contains(&byte) {
                                break;
                            }
                        }
                    }
                    b']' => {
                        index += 1;
                        while index < output.len() {
                            let byte = output[index];
                            index += 1;
                            if byte == 0x07 {
                                break;
                            }
                            if byte == 0x1b && output.get(index) == Some(&b'\\') {
                                index += 1;
                                break;
                            }
                        }
                    }
                    _ => index += 1,
                }
            }
            b'\r' | b'\n' | b'\t' | 0x20..=0x7e => {
                visible.push(output[index]);
                index += 1;
            }
            _ => index += 1,
        }
    }
    visible
}

impl Drop for PreviewFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
#[ignore = "run only against the extracted native Windows preview ZIP"]
async fn preview_cli_renders_tui_in_conpty_and_exits_on_ctrl_c() -> Result<()> {
    let cli = std::env::var_os("SEDNA_PREVIEW_CLI")
        .map(PathBuf::from)
        .context("SEDNA_PREVIEW_CLI must point to the codex.exe extracted from the preview ZIP")?;
    anyhow::ensure!(
        cli.is_file(),
        "preview CLI does not exist: {}",
        cli.display()
    );

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_nanos();
    let fixture_root =
        std::env::temp_dir().join(format!("sedna-preview-tui-{}-{unique}", std::process::id()));
    let _fixture = PreviewFixture(fixture_root.clone());
    let home = fixture_root.join("home");
    let cwd = fixture_root.join("workspace");
    let log_dir = fixture_root.join("logs");
    fs::create_dir_all(&home)?;
    fs::create_dir_all(&cwd)?;
    fs::create_dir_all(&log_dir)?;

    let mut env: std::collections::HashMap<String, String> = [
        "APPDATA",
        "COMSPEC",
        "HOMEDRIVE",
        "HOMEPATH",
        "LOCALAPPDATA",
        "PATH",
        "PROGRAMFILES",
        "PROGRAMFILES(X86)",
        "SYSTEMROOT",
        "TEMP",
        "TMP",
        "USERPROFILE",
        "WINDIR",
    ]
    .into_iter()
    .filter_map(|key| {
        std::env::var_os(key).map(|value| (key.to_string(), value.to_string_lossy().into_owned()))
    })
    .collect();
    env.extend([
        (
            "CODEX_HOME".to_string(),
            home.to_string_lossy().into_owned(),
        ),
        ("NO_COLOR".to_string(), "1".to_string()),
        ("RUST_LOG".to_string(), "trace".to_string()),
        ("TERM".to_string(), "xterm-256color".to_string()),
    ]);

    let log_dir_config = format!("log_dir={:?}", log_dir.to_string_lossy());
    let args = vec!["-c".to_string(), log_dir_config];
    let mut process = spawn_pty_process(
        cli.to_str()
            .context("preview CLI path is not valid UTF-8")?,
        &args,
        &cwd,
        &env,
        &None,
        TerminalSize {
            rows: 30,
            cols: 100,
        },
        &[],
    )
    .await
    .context("failed to start preview CLI in native ConPTY")?;

    let mut terminal_output = Vec::new();
    let rendered_ui = timeout(Duration::from_secs(30), async {
        loop {
            let Some(chunk) = process.stdout_rx.recv().await else {
                anyhow::bail!("preview TUI exited before rendering its composer")
            };
            terminal_output.extend_from_slice(&chunk);
            let visible_output = visible_terminal_bytes(&terminal_output);
            if VISIBLE_COMPOSER_MARKERS.iter().any(|marker| {
                visible_output
                    .windows(marker.len())
                    .any(|window| window == *marker)
            }) {
                return Ok::<(), anyhow::Error>(());
            }
            anyhow::ensure!(
                terminal_output.len() <= 1024 * 1024,
                "preview TUI produced more than 1 MiB before rendering its composer"
            );
        }
    })
    .await;
    if rendered_ui.is_err() {
        let log_path = log_dir.join("codex-tui.log");
        let log_bytes = fs::metadata(&log_path)
            .map(|metadata| metadata.len())
            .unwrap_or_default();
        let fixture_path = fixture_root.to_string_lossy().into_owned();
        let fixture_path_forward_slashes = fixture_path.replace('\\', "/");
        let workspace_path = cwd.to_string_lossy().into_owned();
        let workspace_path_forward_slashes = workspace_path.replace('\\', "/");
        let visible_output = String::from_utf8_lossy(&visible_terminal_bytes(&terminal_output))
            .replace(&fixture_path, "<fixture>")
            .replace(&fixture_path_forward_slashes, "<fixture>")
            .replace(&workspace_path, "<workspace>")
            .replace(&workspace_path_forward_slashes, "<workspace>");
        let terminal_excerpt = visible_output
            .chars()
            .rev()
            .take(2048)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        let diagnostics = fs::read_to_string(log_path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                let message = if line.contains(" ERROR ") {
                    line.split_once(" ERROR ")
                } else if line.contains(" WARN ")
                    && !line.contains("path=")
                    && !line.contains("://")
                {
                    line.split_once(" WARN ")
                } else {
                    None
                };
                message.map(|(_, message)| message.trim())
            })
            .take(20)
            .map(|message| {
                message
                    .replace(&fixture_path, "<fixture>")
                    .replace(&fixture_path_forward_slashes, "<fixture>")
                    .replace(&workspace_path, "<workspace>")
                    .replace(&workspace_path_forward_slashes, "<workspace>")
            })
            .collect::<Vec<_>>()
            .join("\n");
        anyhow::bail!(
            "preview TUI did not render a composer placeholder within 30 seconds (terminal output: {} bytes, TUI log: {log_bytes} bytes, process exited: {}); visible terminal excerpt:\n{terminal_excerpt}\nTUI warnings/errors:\n{diagnostics}",
            terminal_output.len(),
            process.session.has_exited(),
        );
    }
    rendered_ui
        .context("preview TUI did not render a composer placeholder within 30 seconds")??;
    anyhow::ensure!(
        !process.session.has_exited(),
        "preview TUI exited before Ctrl+C was sent"
    );

    process
        .session
        .writer_sender()
        .send(vec![0x03])
        .await
        .context("failed to send Ctrl+C to preview TUI")?;
    timeout(Duration::from_secs(15), &mut process.exit_rx)
        .await
        .context("preview TUI did not exit after Ctrl+C")?
        .context("preview TUI exit status was lost")?;

    drop(process);
    Ok(())
}
