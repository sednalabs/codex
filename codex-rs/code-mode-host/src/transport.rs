use std::net::SocketAddr;

use anyhow::Context;
use anyhow::Result;

use crate::grpc_transport;

/// The default transport retains the standalone host's original stdio behavior.
pub const DEFAULT_LISTEN_URL: &str = "stdio";

#[derive(Debug, Clone, Eq, PartialEq)]
enum ListenTransport {
    Stdio,
    Grpc(SocketAddr),
}

pub(crate) async fn run_transport(listen_url: &str) -> Result<()> {
    match parse_listen_url(listen_url)? {
        ListenTransport::Stdio => crate::run_stdio().await,
        ListenTransport::Grpc(bind_address) => grpc_transport::run_tcp_listener(bind_address).await,
    }
}

fn parse_listen_url(listen_url: &str) -> Result<ListenTransport> {
    if matches!(listen_url, "stdio" | "stdio://") {
        return Ok(ListenTransport::Stdio);
    }

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
    if let Some(socket_addr) = listen_url.strip_prefix("ws://") {
        let bind_address = socket_addr.parse::<SocketAddr>().with_context(|| {
            format!("invalid websocket --listen URL `{listen_url}`; expected `ws://IP:PORT`")
        });
        let bind_address = bind_address?;
        if !bind_address.ip().is_loopback() {
            anyhow::bail!(
                "invalid websocket --listen URL `{listen_url}`; code-mode host only supports loopback IP addresses"
            );
        }
        return Ok(ListenTransport::WebSocket(bind_address));
=======
    if let Some(socket_addr) = listen_url.strip_prefix("grpc://") {
        return socket_addr
            .parse::<SocketAddr>()
            .map(ListenTransport::Grpc)
            .with_context(|| {
                format!("invalid gRPC --listen URL `{listen_url}`; expected `grpc://IP:PORT`")
            });
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
    }

    anyhow::bail!(
        "unsupported --listen URL `{listen_url}`; expected `grpc://IP:PORT`, `stdio`, or `stdio://`"
    );
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
