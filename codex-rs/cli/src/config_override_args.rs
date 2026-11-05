//! Preserve config overrides across command boundaries without changing typed child arguments.

use crate::MultitoolCli;
use clap::CommandFactory;
use clap::FromArgMatches;
use codex_utils_cli::collect_config_overrides;
use std::ffi::OsString;
use std::sync::OnceLock;

pub(super) fn try_parse_from(
    args: impl IntoIterator<Item = impl Into<OsString>>,
) -> Result<MultitoolCli, clap::Error> {
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let mut command = MultitoolCli::command().version(cli_display_version());
    let mut matches = command.try_get_matches_from_mut(&args)?;
    crate::queue_cmd::validate_options(&command, &matches)?;
    let mut cli = MultitoolCli::from_arg_matches_mut(&mut matches)
        .map_err(|error| error.format(&mut command))?;
    if cli.subcommand.is_none() || cli.config_overrides.raw_overrides.is_empty() {
        return Ok(cli);
    }

    // Keep child copies unchanged: dispatch merges apply them after feature overrides.
    cli.config_overrides = collect_config_overrides(MultitoolCli::command(), args)?;
    Ok(cli)
}

fn cli_display_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION
        .get_or_init(|| cli_display_version_from(&codex_build_info::BuildInfo::get()))
        .as_str()
}

fn cli_display_version_from(build_info: &codex_build_info::BuildInfo) -> String {
    if build_info.is_source_build() {
        env!("CARGO_PKG_VERSION").to_string()
    } else {
        build_info.version().to_string()
    }
}

#[cfg(test)]
#[path = "config_override_args_tests.rs"]
mod tests;

#[cfg(test)]
mod version_tests {
    use super::cli_display_version_from;
    use codex_build_info::BuildInfo;

    #[test]
    fn source_cli_version_uses_workspace_version() {
        assert_eq!(
            cli_display_version_from(&BuildInfo::from_version("0.0.0")),
            env!("CARGO_PKG_VERSION")
        );
    }

    #[test]
    fn packaged_cli_version_uses_full_package_manifest_version() {
        let version = "0.164.0-dev.distro.1+build.1";
        assert_eq!(
            cli_display_version_from(&BuildInfo::from_version(version)),
            version
        );
    }
}
