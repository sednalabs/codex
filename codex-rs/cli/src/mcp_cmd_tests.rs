use clap::CommandFactory;
use clap::Parser;

use super::McpCli;
use super::McpSubcommand;

#[test]
fn mcp_login_device_auth_flag_is_available() {
    let cli = McpCli::try_parse_from(["mcp", "login", "ops", "--device-auth"])
        .expect("parse device login flag");
    let McpSubcommand::Login(args) = cli.subcommand else {
        panic!("expected login subcommand");
    };
    assert!(args.device_auth);

    let help = McpCli::command()
        .find_subcommand_mut("login")
        .expect("login subcommand")
        .render_long_help()
        .to_string();
    assert!(help.contains("--device-auth"));
}

#[test]
fn mcp_login_device_auth_conflicts_with_manual_callback_mode() {
    assert!(
        McpCli::try_parse_from(["mcp", "login", "ops", "--device-auth", "--no-browser"]).is_err()
    );
}
