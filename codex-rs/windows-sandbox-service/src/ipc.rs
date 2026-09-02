//! Disabled provisioning IPC until authenticated request handling is installed.

use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub(crate) const PIPE_NAME: &str = r"\\.\pipe\OpenAI.CodexSandbox";

pub(crate) fn run(
    _shutdown: Arc<AtomicBool>,
    _on_ready: impl FnOnce() -> Result<()>,
) -> Result<()> {
    anyhow::bail!("sandbox provisioning IPC is disabled until authenticated handling is installed")
}

pub(crate) fn wake() {}
