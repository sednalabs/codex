/// The current Codex CLI version as embedded at compile time.
pub const CODEX_CLI_VERSION: &str = match option_env!("CODEX_RELEASE_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

/// The GitHub repository used for release/update checks.
pub const CODEX_RELEASE_REPOSITORY: &str = match option_env!("CODEX_RELEASE_REPOSITORY") {
    Some(repository) => repository,
    None => "openai/codex",
};

/// The tag prefix used to derive a version string from release tags.
pub const CODEX_RELEASE_TAG_PREFIX: &str = match option_env!("CODEX_RELEASE_TAG_PREFIX") {
    Some(prefix) => prefix,
    None => "rust-v",
};
