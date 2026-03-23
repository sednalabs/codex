/// The current Codex CLI version as embedded at compile time.
pub const CODEX_CLI_VERSION: &str = match option_env!("CODEX_RELEASE_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

/// The GitHub repository used for release/update checks.
pub const CODEX_RELEASE_REPOSITORY: &str = match option_env!("CODEX_RELEASE_REPOSITORY") {
    Some(repository) => repository,
    None => "SednaLabs/codex",
};

/// The tag prefix used to derive a version string from release tags.
pub const CODEX_RELEASE_TAG_PREFIX: &str = match option_env!("CODEX_RELEASE_TAG_PREFIX") {
    Some(prefix) => prefix,
    None => "v",
};

/// The npm package used for self-update guidance when the binary is npm-managed.
pub const CODEX_UPDATE_NPM_PACKAGE: &str = match option_env!("CODEX_UPDATE_NPM_PACKAGE") {
    Some(package) => package,
    None => "@openai/codex",
};

/// The brew cask used for self-update guidance when the binary is brew-managed.
pub const CODEX_UPDATE_BREW_CASK: &str = match option_env!("CODEX_UPDATE_BREW_CASK") {
    Some(cask) => cask,
    None => "codex",
};

pub fn installation_options_url() -> String {
    format!("https://github.com/{CODEX_RELEASE_REPOSITORY}")
}

pub fn latest_release_api_url() -> String {
    format!("https://api.github.com/repos/{CODEX_RELEASE_REPOSITORY}/releases/latest")
}

pub fn latest_release_notes_url() -> String {
    format!("https://github.com/{CODEX_RELEASE_REPOSITORY}/releases/latest")
}
