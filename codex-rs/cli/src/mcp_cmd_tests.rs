//! Verifies MCP client-secret argument requirements and redacted CLI diagnostics.

use clap::Parser;
use pretty_assertions::assert_eq;

use super::McpCli;
use super::McpOAuthClientRegistration;
use super::McpSubcommand;
use super::escape_terminal_text;
use super::validate_device_auth_options;

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

#[test]
fn mcp_device_auth_rejects_unsupported_client_credentials_and_registration() {
    for registration in [
        McpOAuthClientRegistration::Auto,
        McpOAuthClientRegistration::Dcr,
        McpOAuthClientRegistration::Cimd,
    ] {
        let error = validate_device_auth_options(
            registration,
            /*has_registered_client_id*/ true,
            /*has_client_secret*/ true,
        )
        .expect_err("device flow must not silently discard a configured client secret");
        assert!(
            error
                .to_string()
                .contains("does not support a configured client secret")
        );
    }

    let error = validate_device_auth_options(
        McpOAuthClientRegistration::Cimd,
        /*has_registered_client_id*/ false,
        /*has_client_secret*/ false,
    )
    .expect_err("device flow must not silently turn CIMD into DCR");
    assert!(
        error
            .to_string()
            .contains("does not support CIMD registration")
    );

    validate_device_auth_options(
        McpOAuthClientRegistration::Cimd,
        /*has_registered_client_id*/ true,
        /*has_client_secret*/ false,
    )
    .expect("an existing client ID makes registration strategy inapplicable");
    validate_device_auth_options(
        McpOAuthClientRegistration::Auto,
        /*has_registered_client_id*/ false,
        /*has_client_secret*/ false,
    )
    .expect("auto may use advertised DCR for a device grant");
    validate_device_auth_options(
        McpOAuthClientRegistration::Dcr,
        /*has_registered_client_id*/ false,
        /*has_client_secret*/ false,
    )
    .expect("explicit DCR is supported");
}

#[test]
fn mcp_device_prompt_escapes_terminal_controls() {
    assert_eq!(escape_terminal_text("ABCD\n\u{1b}[2J"), "ABCD\\n\\u{1b}[2J");
}
