//! Regression coverage for fragmented TLS detection and lossless prefix replay.

use pretty_assertions::assert_eq;
use rama_core::extensions::Extensions;
use rama_core::extensions::ExtensionsMut;
use rama_core::extensions::ExtensionsRef;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::DuplexStream;
use tokio::io::ReadBuf;
use tokio::time::Duration;

struct TestStream {
    inner: DuplexStream,
    extensions: Extensions,
}

impl TestStream {
    fn new(inner: DuplexStream) -> Self {
        Self {
            inner,
            extensions: Extensions::new(),
        }
    }
}

impl AsyncRead for TestStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for TestStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

impl ExtensionsRef for TestStream {
    fn extensions(&self) -> &Extensions {
        &self.extensions
    }
}

impl ExtensionsMut for TestStream {
    fn extensions_mut(&mut self) -> &mut Extensions {
        &mut self.extensions
    }
}

#[tokio::test]
async fn tls_prefix_detection_accumulates_fragmented_reads() {
    let tls_prefix = [0x16, 0x03, 0x03, 0x00, 0x80];
    let (mut writer, reader) = tokio::io::duplex(16);
    let writer_task = tokio::spawn(async move {
        writer.write_all(&tls_prefix[..1]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        writer.write_all(&tls_prefix[1..]).await.unwrap();
    });

    let (protocol, mut stream) = crate::brokered_tunnel::peek_protocol(
        TestStream::new(reader),
        crate::brokered_tunnel::BrokeredProtocols { tls: true },
    )
    .await
    .unwrap();
    let mut replayed = [0_u8; 5];
    stream.read_exact(&mut replayed).await.unwrap();

    assert_eq!(protocol, crate::brokered_tunnel::TunnelProtocol::Tls);
    assert_eq!(replayed, tls_prefix);
    writer_task.await.unwrap();
}
