/// The current Codex CLI version as embedded at compile time.
///
/// Keep this upstream-compatible package version for protocol negotiation,
/// update checks, and provider-facing client metadata.
pub const CODEX_CLI_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Human-readable version shown by an installed package.
///
/// Installed Sedna packages carry their progressive build identity in the
/// package manifest. Source builds retain the upstream Cargo version for
/// stable development UI output.
pub fn display_version() -> &'static str {
    use std::sync::OnceLock;

    static DISPLAY_VERSION: OnceLock<String> = OnceLock::new();
    DISPLAY_VERSION
        .get_or_init(|| {
            let build_info = codex_build_info::BuildInfo::get();
            if build_info.is_source_build() {
                CODEX_CLI_VERSION.to_string()
            } else {
                build_info.version().to_string()
            }
        })
        .as_str()
}
