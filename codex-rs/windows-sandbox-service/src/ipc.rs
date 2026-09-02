//! Disabled provisioning IPC until authenticated request handling is installed.

// Keep these private until the authenticated transport is connected.
#[allow(dead_code)]
mod home;
#[allow(dead_code)]
mod request;

use anyhow::Result;
#[cfg(test)]
use home::pin_existing_ancestors;
#[cfg(test)]
use request::ProvisioningRequest;
#[cfg(test)]
use request::validate_request;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub(crate) const PIPE_NAME: &str = r"\\.\pipe\OpenAI.CodexSandbox";
const MAX_REQUEST_BYTES: usize = 4096;

pub(crate) fn run(
    _shutdown: Arc<AtomicBool>,
    _on_ready: impl FnOnce() -> Result<()>,
) -> Result<()> {
    anyhow::bail!("sandbox provisioning IPC is disabled until authenticated handling is installed")
}

pub(crate) fn wake() {}

#[cfg(test)]
#[path = "ipc_tests.rs"]
mod tests;
