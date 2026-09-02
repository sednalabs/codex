//! Detached Unix process launch and PID publication under the reservation lock.

use super::PidBackend;
use super::PidFileState;
use super::PidRecord;
use super::read_process_start_time;
use anyhow::Context;
use anyhow::Result;
use std::process::Stdio;
use tokio::fs;
use tokio::process::Command;

impl PidBackend {
    pub(crate) async fn start(&self) -> Result<Option<u32>> {
        if let Some(parent) = self.pid_file.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed to create pid directory {}", parent.display()))?;
        }
        let reservation_lock = self.acquire_reservation_lock().await?;
        let _pid_file = loop {
            match fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&self.pid_file)
                .await
            {
                Ok(pid_file) => break pid_file,
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    match self.read_pid_file_state_with_lock_held().await? {
                        PidFileState::Missing => continue,
                        PidFileState::Running(record) => {
                            if self.record_is_active(&record).await? {
                                return Ok(None);
                            }
                            let _ = fs::remove_file(&self.pid_file).await;
                            continue;
                        }
                        PidFileState::Starting => {
                            unreachable!("lock holder cannot observe starting")
                        }
                    }
                }
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!("failed to reserve pid file {}", self.pid_file.display())
                    });
                }
            }
        };
        let mut command = Command::new(&self.codex_bin);
        let stderr_log = match self.open_stderr_log().await {
            Ok(stderr_log) => stderr_log,
            Err(err) => {
                let _ = fs::remove_file(&self.pid_file).await;
                return Err(err);
            }
        };
        command
            .args(self.command_args())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_log.into_std().await));
        if let Some((key, value)) = self.command_env() {
            command.env(key, value);
        }

        #[cfg(unix)]
        {
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }

        let child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                let _ = fs::remove_file(&self.pid_file).await;
                return Err(err).with_context(|| {
                    format!(
                        "failed to spawn detached app-server process using {}",
                        self.codex_bin.display()
                    )
                });
            }
        };
        let pid = child
            .id()
            .context("spawned app-server process has no pid")?;
        let record = match read_process_start_time(pid).await {
            Ok(process_start_time) => PidRecord {
                pid,
                process_start_time,
            },
            Err(err) => {
                let _ = self.terminate_process(pid);
                let mut context =
                    format!("failed to record pid-managed app-server process {pid} startup");
                super::super::append_stderr_log_tail_context(&self.pid_file, &mut context).await;
                let _ = fs::remove_file(&self.pid_file).await;
                return Err(err).context(context);
            }
        };
        let contents = serde_json::to_vec(&record).context("failed to serialize pid record")?;
        let temp_pid_file = self.pid_file.with_extension("pid.tmp");
        if let Err(err) = fs::write(&temp_pid_file, &contents).await {
            let _ = self.terminate_process(pid);
            let _ = fs::remove_file(&self.pid_file).await;
            return Err(err).with_context(|| {
                format!("failed to write pid temp file {}", temp_pid_file.display())
            });
        }
        if let Err(err) = fs::rename(&temp_pid_file, &self.pid_file).await {
            let _ = self.terminate_process(pid);
            let _ = fs::remove_file(&temp_pid_file).await;
            let _ = fs::remove_file(&self.pid_file).await;
            return Err(err).with_context(|| {
                format!("failed to publish pid file {}", self.pid_file.display())
            });
        }
        drop(reservation_lock);
        Ok(Some(pid))
    }
}
