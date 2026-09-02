//! Detached process launch and PID publication. Hold the reservation lock until
//! the record is published.

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
    pub(super) async fn start_inner(&self) -> Result<Option<u32>> {
        if let Some(parent) = self.pid_file.parent() {
            codex_uds::prepare_private_socket_directory(parent)
                .await
                .with_context(|| format!("failed to create pid directory {}", parent.display()))?;
        }
        let reservation_lock = self.acquire_reservation_lock().await?;
        loop {
            match fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&self.pid_file)
                .await
            {
                Ok(pid_file) => {
                    drop(pid_file);
                    break;
                }
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
        }
        // Pin the Windows image path across installer junction retargeting.
        #[cfg(windows)]
        let codex_bin = fs::canonicalize(&self.codex_bin)
            .await
            .unwrap_or_else(|_| self.codex_bin.clone());
        #[cfg(not(windows))]
        let codex_bin = &self.codex_bin;
        let mut command = Command::new(codex_bin);
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

        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Threading::CREATE_BREAKAWAY_FROM_JOB;
            use windows_sys::Win32::System::Threading::DETACHED_PROCESS;
            // Never retry inside the parent's Job Object: that would report a
            // successful launch that dies when the terminal/SSH session closes.
            command.creation_flags(DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB);
            let shutdown_file = self.pid_file.with_extension("shutdown");
            match fs::remove_file(&shutdown_file).await {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err).context("failed to clear daemon shutdown request"),
            }
            command.env(
                codex_app_server_transport::DAEMON_SHUTDOWN_FILE_ENV,
                shutdown_file,
            );
        }

        let child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                let _ = fs::remove_file(&self.pid_file).await;
                return Err(err).with_context(|| {
                    let job_hint = if cfg!(windows) {
                        " (the Windows host job must allow breakaway)"
                    } else {
                        ""
                    };
                    format!(
                        "failed to spawn detached app-server process using {}{job_hint}",
                        self.codex_bin.display()
                    )
                });
            }
        };
        let pid = child
            .id()
            .context("spawned app-server process has no pid")?;
        let record = match async {
            #[cfg(windows)]
            super::super::windows::Process::open(pid)?
                .context("daemon exited during launch")?
                .ensure_detached()?;
            read_process_start_time(pid).await
        }
        .await
        {
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
