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

const SEDNA_RELEASE_REPOSITORY: &str = "sednalabs/codex";
const SEDNA_RELEASE_TAG_PREFIX: &str = "v";

/// The npm package used for self-update guidance when the binary is npm-managed.
#[cfg_attr(debug_assertions, allow(dead_code))]
pub const CODEX_UPDATE_NPM_PACKAGE: &str = match option_env!("CODEX_UPDATE_NPM_PACKAGE") {
    Some(package) => package,
    None => "@openai/codex",
};

/// The brew cask used for self-update guidance when the binary is brew-managed.
#[cfg_attr(debug_assertions, allow(dead_code))]
pub const CODEX_UPDATE_BREW_CASK: &str = match option_env!("CODEX_UPDATE_BREW_CASK") {
    Some(cask) => cask,
    None => "codex",
};

/// Whether this binary was compiled for the Sedna release/update channel.
pub const fn is_sedna_release_channel() -> bool {
    is_sedna_release_identity(
        option_env!("CODEX_RELEASE_REPOSITORY"),
        option_env!("CODEX_RELEASE_TAG_PREFIX"),
    )
}

/// Requires an explicit build-time release identity. Display fallbacks must not
/// turn an unconfigured binary into a release update channel.
pub const fn is_sedna_release_identity(repository: Option<&str>, tag_prefix: Option<&str>) -> bool {
    matches!(repository, Some(SEDNA_RELEASE_REPOSITORY))
        && matches!(tag_prefix, Some(SEDNA_RELEASE_TAG_PREFIX))
}

#[cfg_attr(debug_assertions, allow(dead_code))]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sedna_update_identity_requires_both_explicit_build_values() {
        assert!(is_sedna_release_identity(
            Some("sednalabs/codex"),
            Some("v")
        ));
        for identity in [
            (None, None),
            (Some("sednalabs/codex"), None),
            (None, Some("v")),
            (Some("openai/codex"), Some("v")),
            (Some("sednalabs/codex"), Some("rust-v")),
        ] {
            assert!(!is_sedna_release_identity(identity.0, identity.1));
        }
    }
}
