//! Detect broker-required TLS without consuming bytes needed for opaque forwarding.

use anyhow::Context;
use anyhow::Result;
use rama_core::extensions::ExtensionsMut;
use rama_core::stream::PeekStream;
use rama_core::stream::Stream;
use std::io::Cursor;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::time::timeout;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BrokeredProtocols {
    pub(crate) tls: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TunnelProtocol {
    Tls,
    Opaque,
}

const PREFIX_TIMEOUT: Duration = Duration::from_millis(250);

/// Detect only configured protocols, replaying every byte for opaque forwarding.
pub(crate) async fn peek_protocol<S>(
    mut stream: S,
    protocols: BrokeredProtocols,
) -> Result<(TunnelProtocol, PeekStream<Cursor<Vec<u8>>, S>)>
where
    S: Stream + Unpin + ExtensionsMut,
{
    let mut prefix = Vec::new();
    let mut buf = [0_u8; 5];
    if let Ok(result) = timeout(PREFIX_TIMEOUT, stream.read(&mut buf)).await {
        prefix.extend_from_slice(&buf[..result.context("read tunnel prefix")?])
    }
    let protocol = if prefix.first() == Some(&0x16) && protocols.tls {
        // Do not time out a fragmented TLS record once the client has started its handshake.
        while prefix.len() < 5
            && matches!(
                prefix.as_slice(),
                [0x16] | [0x16, 0x03] | [0x16, 0x03, 0x00..=0x04, ..]
            )
        {
            let read = stream.read(&mut buf[..5 - prefix.len()]).await?;
            if read == 0 {
                break;
            }
            prefix.extend_from_slice(&buf[..read]);
        }
        if matches!(prefix.as_slice(), [0x16, 0x03, 0x00..=0x04, _, _]) {
            TunnelProtocol::Tls
        } else {
            TunnelProtocol::Opaque
        }
    } else {
        TunnelProtocol::Opaque
    };
    Ok((protocol, PeekStream::new(Cursor::new(prefix), stream)))
}

#[cfg(test)]
#[path = "brokered_tunnel_tests.rs"]
mod tests;
