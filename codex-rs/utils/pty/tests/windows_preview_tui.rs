#![cfg(windows)]

use std::collections::HashMap;
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

impl Drop for PreviewFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
#[ignore = "run only against the extracted native Windows preview ZIP"]
async fn preview_cli_enters_tui_in_conpty_and_exits_on_ctrl_c() -> Result<()> {
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
    fs::create_dir_all(&home)?;
    fs::create_dir_all(&cwd)?;

    let mut env = HashMap::new();
    for key in [
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
    ] {
        if let Some(value) = std::env::var_os(key) {
            env.insert(key.to_string(), value.to_string_lossy().into_owned());
        }
    }
    env.insert(
        "CODEX_HOME".to_string(),
        home.to_string_lossy().into_owned(),
    );
    env.insert("NO_COLOR".to_string(), "1".to_string());
    env.insert("TERM".to_string(), "xterm-256color".to_string());

    let args = [];
    let arg0 = None;
    let mut process = spawn_pty_process(
        cli.to_str()
            .context("preview CLI path is not valid UTF-8")?,
        &args,
        &cwd,
        &env,
        &arg0,
        TerminalSize {
            rows: 30,
            cols: 100,
        },
        &[],
    )
    .await
    .context("failed to start preview CLI in native ConPTY")?;

    let mut terminal_output = Vec::new();
    timeout(Duration::from_secs(30), async {
        loop {
            let Some(chunk) = process.stdout_rx.recv().await else {
                anyhow::bail!("preview TUI exited before entering the alternate screen")
            };
            terminal_output.extend_from_slice(&chunk);
            if terminal_output
                .windows(b"\x1b[?1049h".len())
                .any(|window| window == b"\x1b[?1049h")
            {
                return Ok::<(), anyhow::Error>(());
            }
            anyhow::ensure!(
                terminal_output.len() <= 1024 * 1024,
                "preview TUI produced more than 1 MiB before entering the alternate screen"
            );
        }
    })
    .await
    .context("preview TUI did not enter the alternate screen within 30 seconds")??;
    anyhow::ensure!(
        !process.session.has_exited(),
        "preview TUI exited immediately after entering the alternate screen"
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
