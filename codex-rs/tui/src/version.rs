/// The current Codex CLI version as embedded at compile time.
///
/// Keep this upstream-compatible value for provider/client metadata and update
/// checks. Human-facing package headers should use [`display_version`].
pub const CODEX_CLI_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The version shown in a human-facing TUI session header.
///
/// Branch packages may keep Cargo's upstream-compatible `0.0.0` package
/// version while carrying their progressive identity in `codex-package.json`.
/// Prefer that manifest version when it is not the source-build placeholder;
/// source builds and ordinary upstream releases retain their existing display.
pub fn display_version() -> &'static str {
    use std::sync::OnceLock;

    static DISPLAY_VERSION: OnceLock<String> = OnceLock::new();
    DISPLAY_VERSION
        .get_or_init(|| {
            let packaged_version = codex_build_info::BuildInfo::get().version().to_string();
            format_display_version(CODEX_CLI_VERSION, &packaged_version)
        })
        .as_str()
}

fn format_display_version(cargo_version: &str, packaged_version: &str) -> String {
    if packaged_version != "0.0.0" {
        packaged_version.to_string()
    } else {
        cargo_version.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::format_display_version;

    #[test]
    fn package_manifest_version_is_used_for_progressive_display() {
        assert_eq!(
            format_display_version("0.0.0", "0.0.0-sedna.0-ci.798+g2ac6f684"),
            "0.0.0-sedna.0-ci.798+g2ac6f684"
        );
    }

    #[test]
    fn unchanged_release_version_keeps_upstream_display() {
        assert_eq!(format_display_version("0.153.0", "0.153.0"), "0.153.0");
    }

    #[test]
    fn missing_package_version_keeps_cargo_fallback() {
        assert_eq!(format_display_version("0.153.0", "0.0.0"), "0.153.0");
    }
}
