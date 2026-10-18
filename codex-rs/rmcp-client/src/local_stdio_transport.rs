//! Protocol framing and child lifetime for locally spawned MCP servers.
//!
//! Process creation is platform-specific; protocol selection and shutdown are shared.

use std::future::Future;
use std::io;
use std::ops::Deref;
use std::ops::DerefMut;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use codex_utils_pty::Child;
use codex_utils_pty::Command;
use futures::FutureExt;
use rmcp::service::RoleClient;
use rmcp::service::RxJsonRpcMessage;
use rmcp::service::TxJsonRpcMessage;
use rmcp::transport::Transport;
use rmcp::transport::async_rw::AsyncRwTransport;
use tokio::process::ChildStderr;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;

use crate::bounded_stdio_transport::BoundedStdioTransport;
use crate::protocol_mode::McpProtocolMode;
use crate::stdio_server_launcher::COOPERATIVE_STDIO_CLOSE_TIMEOUT;
use crate::stdio_server_launcher::COOPERATIVE_STDIO_KILL_ERROR_WAIT;
use crate::stdio_server_launcher::StdioServerCloseOutcome;
use crate::stdio_server_launcher::cooperative_close_outcome_after_exit;

/// Serializes asynchronous child operations without suspending a mutex guard.
/// The semaphore protects the period while the child is moved out of its short
/// synchronous slot; the lease returns it even when an operation is cancelled.
pub(super) struct SharedChild {
    operation_gate: Arc<Semaphore>,
    child: StdMutex<Option<Child>>,
}

pub(super) struct SharedChildLease {
    permit: Option<OwnedSemaphorePermit>,
    owner: Arc<SharedChild>,
    child: Option<Child>,
}

impl SharedChild {
    fn new(child: Child) -> Arc<Self> {
        Arc::new(Self {
            operation_gate: Arc::new(Semaphore::new(1)),
            child: StdMutex::new(Some(child)),
        })
    }

    pub(super) async fn lock(self: &Arc<Self>) -> io::Result<SharedChildLease> {
        let permit = Arc::clone(&self.operation_gate)
            .acquire_owned()
            .await
            .map_err(|_| io::Error::other("MCP stdio child operation gate is closed"))?;
        let child = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .ok_or_else(|| io::Error::other("MCP stdio child ownership is unavailable"))?;
        Ok(SharedChildLease {
            permit: Some(permit),
            owner: Arc::clone(self),
            child: Some(child),
        })
    }
}

impl Deref for SharedChildLease {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        match self.child.as_ref() {
            Some(child) => child,
            None => unreachable!("leased child is present while borrowed"),
        }
    }
}

impl DerefMut for SharedChildLease {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self.child.as_mut() {
            Some(child) => child,
            None => unreachable!("leased child is present while borrowed"),
        }
    }
}

impl Drop for SharedChildLease {
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            let mut slot = self
                .owner
                .child
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            debug_assert!(slot.is_none(), "child lease is exclusive");
            *slot = Some(child);
        }
        // Release the operation gate only after the child has been restored.
        drop(self.permit.take());
    }
}

pub(super) struct LocalStdioTransport {
    child: Arc<SharedChild>,
    process_id: Option<u32>,
    transport: StdioTransport,
}

enum StdioTransport {
    /// Preserve rmcp's existing framing for servers using the initialize handshake.
    Legacy(AsyncRwTransport<RoleClient, ChildStdout, ChildStdin>),
    /// Bound frames and skip messages unknown to the client during 2026-07-28 discovery.
    V20260728(BoundedStdioTransport),
}

impl LocalStdioTransport {
    pub(super) fn spawn(
        command: Command,
        program_name: String,
        protocol_mode: McpProtocolMode,
    ) -> io::Result<(Self, Option<ChildStderr>)> {
        let mut child = command.spawn()?;
        let process_id = child.id();
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("MCP server stdin was not piped"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("MCP server stdout was not piped"))?;
        let stderr = child.stderr.take();
        let transport = match protocol_mode {
            McpProtocolMode::Legacy => StdioTransport::Legacy(AsyncRwTransport::new(stdout, stdin)),
            McpProtocolMode::V20260728 => {
                StdioTransport::V20260728(BoundedStdioTransport::new(stdin, stdout, program_name))
            }
        };
        Ok((
            Self {
                child: SharedChild::new(child),
                process_id,
                transport,
            },
            stderr,
        ))
    }

    pub(super) fn id(&self) -> Option<u32> {
        self.process_id
    }

    pub(super) fn process_child(&self) -> Arc<SharedChild> {
        Arc::clone(&self.child)
    }

    pub(super) async fn close_cooperatively(
        &mut self,
    ) -> (io::Result<()>, StdioServerCloseOutcome) {
        let close_deadline = tokio::time::Instant::now() + COOPERATIVE_STDIO_CLOSE_TIMEOUT;
        let transport_result = match tokio::time::timeout_at(close_deadline, async {
            match &mut self.transport {
                StdioTransport::Legacy(transport) => transport.close().await,
                StdioTransport::V20260728(transport) => transport.close().await,
            }
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out closing MCP stdio transport",
            )),
        };

        let mut child = match self.child.lock().await {
            Ok(child) => child,
            Err(error) => return (Err(error), StdioServerCloseOutcome::Failed),
        };
        let close_result = match tokio::time::timeout_at(close_deadline, child.wait()).await {
            Ok(Ok(status)) => cooperative_close_outcome_after_exit(status),
            Ok(Err(error)) => Err(error),
            Err(_) => match child.kill().await {
                Ok(()) => Ok(StdioServerCloseOutcome::Forced),
                Err(kill_error) => {
                    match tokio::time::timeout(COOPERATIVE_STDIO_KILL_ERROR_WAIT, child.wait())
                        .await
                    {
                        Ok(Ok(status)) => cooperative_close_outcome_after_exit(status),
                        Ok(Err(wait_error)) => Err(wait_error),
                        Err(_) => Err(kill_error),
                    }
                }
            },
        };

        match (transport_result, close_result) {
            (Err(error), Ok(StdioServerCloseOutcome::Forced)) => {
                (Err(error), StdioServerCloseOutcome::Forced)
            }
            (Err(error), _) | (Ok(()), Err(error)) => (Err(error), StdioServerCloseOutcome::Failed),
            (Ok(()), Ok(outcome)) => (Ok(()), outcome),
        }
    }
}

impl Transport<RoleClient> for LocalStdioTransport {
    type Error = io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = io::Result<()>> + Send + 'static {
        match &mut self.transport {
            StdioTransport::Legacy(transport) => transport.send(item).boxed(),
            StdioTransport::V20260728(transport) => transport.send(item).boxed(),
        }
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        match &mut self.transport {
            StdioTransport::Legacy(transport) => transport.receive().boxed(),
            StdioTransport::V20260728(transport) => transport.receive().boxed(),
        }
    }

    async fn close(&mut self) -> io::Result<()> {
        match &mut self.transport {
            StdioTransport::Legacy(transport) => transport.close().await?,
            StdioTransport::V20260728(transport) => transport.close().await?,
        }
        let mut child = self.child.lock().await?;
        match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
            Ok(status) => status.map(|_| ()),
            Err(_) => child.kill().await,
        }
    }
}
