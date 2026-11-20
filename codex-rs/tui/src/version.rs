/// The current Codex CLI version as embedded at compile time.
pub const CODEX_CLI_VERSION: &str = match option_env!("CODEX_RELEASE_VERSION") {
    Some(version) if !version.is_empty() => version,
    _ => env!("CARGO_PKG_VERSION"),
};
