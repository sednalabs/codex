use std::net::SocketAddr;

use pretty_assertions::assert_eq;

use super::ListenTransport;
use super::parse_listen_url;

#[test]
fn parse_listen_url_accepts_stdio_transports() {
    assert_eq!(
        parse_listen_url("stdio").expect("stdio listen URL should parse"),
        ListenTransport::Stdio
    );
    assert_eq!(
        parse_listen_url("stdio://").expect("stdio URL should parse"),
        ListenTransport::Stdio
    );
}

#[test]
fn parse_listen_url_accepts_websocket_addresses() {
    assert_eq!(
        parse_listen_url("ws://127.0.0.1:0").expect("websocket listen URL should parse"),
        ListenTransport::WebSocket(
            "127.0.0.1:0"
                .parse::<SocketAddr>()
                .expect("valid socket address")
        )
    );
    assert_eq!(
        parse_listen_url("ws://[::1]:9000").expect("IPv6 websocket listen URL should parse"),
        ListenTransport::WebSocket(
            "[::1]:9000"
                .parse::<SocketAddr>()
                .expect("valid IPv6 socket address")
        )
    );
}

#[test]
fn parse_listen_url_rejects_non_loopback_websocket_addresses() {
    for listen_url in [
        "ws://0.0.0.0:9000",
        "ws://192.0.2.1:9000",
        "ws://[::]:9000",
        "ws://[2001:db8::1]:9000",
    ] {
        let error = parse_listen_url(listen_url)
            .expect_err("code-mode host websocket listener must remain local-only");
        assert!(
            error.to_string().contains("loopback IP addresses"),
            "unexpected error for {listen_url}: {error:#}"
        );
    }
}

#[test]
fn parse_listen_url_rejects_invalid_transports() {
    let invalid_address = parse_listen_url("ws://localhost:9000")
        .expect_err("websocket listener requires an IP address");
    assert!(
        invalid_address
            .to_string()
            .contains("expected `ws://IP:PORT`")
    );

    let unsupported =
        parse_listen_url("http://127.0.0.1:9000").expect_err("HTTP is not a listen transport");
    assert!(unsupported.to_string().contains("unsupported --listen URL"));
}
