/// The current Codex release version used for semver comparisons and persistence.
pub const CODEX_CLI_VERSION: &str = codex_utils_version::RELEASE_VERSION;

/// The human-readable version label shown in user-facing surfaces.
pub const CODEX_DISPLAY_VERSION: &str = codex_utils_version::DISPLAY_VERSION;

/// The GitHub repository used for release/update checks.
pub const CODEX_RELEASE_REPOSITORY: &str = match option_env!("CODEX_RELEASE_REPOSITORY") {
    Some(repository) => repository,
    None => "sednalabs/codex",
};

/// The tag prefix used to derive a version string from release tags.
#[cfg_attr(debug_assertions, allow(dead_code))]
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

#[cfg_attr(debug_assertions, allow(dead_code))]
pub fn latest_release_api_url() -> String {
    format!("https://api.github.com/repos/{CODEX_RELEASE_REPOSITORY}/releases/latest")
}

#[cfg_attr(debug_assertions, allow(dead_code))]
pub fn latest_release_notes_url() -> String {
    format!("https://github.com/{CODEX_RELEASE_REPOSITORY}/releases/latest")
}
