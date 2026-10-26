//! Preserve config overrides across command boundaries without changing typed child arguments.

use crate::MultitoolCli;
use clap::Command;
use clap::CommandFactory;
use clap::FromArgMatches;
use codex_utils_cli::CliConfigOverrides;
use std::ffi::OsString;
use std::sync::OnceLock;

pub(super) fn try_parse_from(
    args: impl IntoIterator<Item = impl Into<OsString>>,
) -> Result<MultitoolCli, clap::Error> {
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let matches = MultitoolCli::command()
        .version(cli_display_version())
        .try_get_matches_from(&args)?;
    let mut cli = MultitoolCli::from_arg_matches(&matches)?;
    if cli.subcommand.is_none() || cli.config_overrides.raw_overrides.is_empty() {
        return Ok(cli);
    }

    // Clap propagates the deepest global argument's values instead of appending
    // values from each scope. Reparse with local config arguments to recover them.
    let matches = localize_config_arg(MultitoolCli::command()).try_get_matches_from(args)?;

    // Keep child copies unchanged: existing dispatch merges apply them after
    // generated feature overrides, and clearing them would change precedence.
    cli.config_overrides.raw_overrides = std::iter::successors(Some(&matches), |scope| {
        scope.subcommand().map(|(_, child)| child)
    })
    .filter_map(|scope| scope.get_many::<String>("raw_overrides"))
    .flatten()
    .cloned()
    .collect();
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

fn localize_config_arg(mut command: Command) -> Command {
    if !command
        .get_arguments()
        .any(|arg| arg.get_id() == "raw_overrides")
    {
        command = command.args(CliConfigOverrides::command().get_arguments().cloned());
    }
    command
        .mut_arg("raw_overrides", |arg| arg.global(false))
        .mut_subcommands(localize_config_arg)
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
