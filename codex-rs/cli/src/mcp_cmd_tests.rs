//! Verifies MCP client-secret argument requirements and redacted CLI diagnostics.

use clap::Parser;
use pretty_assertions::assert_eq;

use super::McpCli;
use super::McpSubcommand;

#[test]
fn oauth_client_secret_is_redacted_in_parsed_command_debug() {
    let cli = McpCli::try_parse_from([
        "mcp",
        "add",
        "private",
        "--url",
        "https://example.com/mcp",
        "--oauth-client-id",
        "registered-client",
        "--oauth-client-secret",
        "cli-secret-marker",
    ])
    .expect("parse confidential client arguments");
    let debug = format!("{cli:?}");
    assert!(!debug.contains("cli-secret-marker"));
    assert!(debug.contains("oauth_client_secret: Some(<redacted>)"));
    let McpSubcommand::Add(add) = cli.subcommand else {
        panic!("expected MCP add");
    };
    let http = add.transport_args.streamable_http.expect("HTTP arguments");
    assert_eq!(
        http.oauth_client_secret
            .as_ref()
            .map(|secret| secret.as_str()),
        Some("cli-secret-marker")
    );
}

#[test]
fn oauth_client_secret_requires_url_and_client_id_without_disclosure() {
    for args in [
        vec!["--url", "https://example.com/mcp"],
        vec!["--oauth-client-id", "registered-client"],
    ] {
        let error = McpCli::try_parse_from(
            [
                "mcp",
                "add",
                "private",
                "--oauth-client-secret",
                "cli-secret-marker",
            ]
            .into_iter()
            .chain(args),
        )
        .expect_err("client secret requires both HTTP URL and client ID");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
        assert!(!error.to_string().contains("cli-secret-marker"));
    }
}

#[test]
fn mcp_device_auth_flag_parses_and_conflicts_with_no_browser() {
    let help = McpCli::try_parse_from(["mcp", "login", "--help"])
        .expect_err("login help exits without a server name");
    assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
    assert!(help.to_string().contains("--device-auth"));

    let cli = McpCli::try_parse_from(["mcp", "login", "synthetic", "--device-auth"])
        .expect("device authorization flag parses");
    let McpSubcommand::Login(args) = cli.subcommand else {
        panic!("expected MCP login");
    };
    assert!(args.device_auth);
    assert!(!args.no_browser);

    let error =
        McpCli::try_parse_from(["mcp", "login", "synthetic", "--device-auth", "--no-browser"])
            .expect_err("device auth and paste-callback modes are mutually exclusive");
    assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
}
