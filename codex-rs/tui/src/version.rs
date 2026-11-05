use std::sync::OnceLock;

/// The current Codex CLI version as embedded at compile time.
pub const CODEX_CLI_VERSION: &str = env!("CARGO_PKG_VERSION");
static CODEX_CLI_DISPLAY_VERSION: OnceLock<String> = OnceLock::new();

/// The version shown in local UI surfaces, including the installed package identity.
pub(crate) fn display_version() -> &'static str {
    CODEX_CLI_DISPLAY_VERSION
        .get_or_init(|| display_version_from(&codex_build_info::BuildInfo::get()))
        .as_str()
}

fn display_version_from(build_info: &codex_build_info::BuildInfo) -> String {
    if build_info.is_source_build() {
        CODEX_CLI_VERSION.to_string()
    } else {
        build_info.version().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::CODEX_CLI_VERSION;
    use super::display_version_from;
    use codex_build_info::BuildInfo;

    #[test]
    fn source_build_display_uses_workspace_version() {
        assert_eq!(
            display_version_from(&BuildInfo::from_version("0.0.0")),
            CODEX_CLI_VERSION
        );
    }

    #[test]
    fn installed_display_uses_full_package_manifest_version() {
        let version = "0.164.0-dev.distro.1+build.1";
        assert_eq!(
            display_version_from(&BuildInfo::from_version(version)),
            version
        );
    }
}
