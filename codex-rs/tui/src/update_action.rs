#[cfg(any(not(debug_assertions), test))]
use codex_install_context::InstallContext;
#[cfg(any(not(debug_assertions), test))]
use codex_install_context::InstallMethod;
#[cfg(any(not(debug_assertions), test))]
use codex_install_context::StandalonePlatform;

use crate::version::CODEX_RELEASE_REPOSITORY;
use crate::version::CODEX_RELEASE_TAG_PREFIX;
use crate::version::CODEX_UPDATE_BREW_CASK;
use crate::version::CODEX_UPDATE_NPM_PACKAGE;

/// Update action the CLI should perform after the TUI exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    /// Update via `npm install -g <configured npm package>@latest`.
    NpmGlobalLatest,
    /// Update via `bun install -g <configured npm package>@latest`.
    BunGlobalLatest,
    /// Update via `brew upgrade --cask <configured cask>`.
    BrewUpgrade,
    /// Update via the configured GitHub release's `install.sh` asset.
    StandaloneUnix,
    /// Update via the configured GitHub release's `install.ps1` asset.
    StandaloneWindows,
}

impl UpdateAction {
    #[cfg(any(not(debug_assertions), test))]
    pub(crate) fn from_install_context(context: &InstallContext) -> Option<Self> {
        match &context.method {
            InstallMethod::Npm => Some(UpdateAction::NpmGlobalLatest),
            InstallMethod::Bun => Some(UpdateAction::BunGlobalLatest),
            InstallMethod::Brew => Some(UpdateAction::BrewUpgrade),
            InstallMethod::Standalone { platform, .. } => Some(match platform {
                StandalonePlatform::Unix => UpdateAction::StandaloneUnix,
                StandalonePlatform::Windows => UpdateAction::StandaloneWindows,
            }),
            InstallMethod::Other => None,
        }
    }

    /// Returns the list of command-line arguments for invoking the update.
    pub fn command_args(self) -> (String, Vec<String>) {
        match self {
            UpdateAction::NpmGlobalLatest => (
                "npm".to_string(),
                vec![
                    "install".to_string(),
                    "-g".to_string(),
                    CODEX_UPDATE_NPM_PACKAGE.to_string(),
                ],
            ),
            UpdateAction::BunGlobalLatest => (
                "bun".to_string(),
                vec![
                    "install".to_string(),
                    "-g".to_string(),
                    CODEX_UPDATE_NPM_PACKAGE.to_string(),
                ],
            ),
            UpdateAction::BrewUpgrade => (
                "brew".to_string(),
                vec![
                    "upgrade".to_string(),
                    "--cask".to_string(),
                    CODEX_UPDATE_BREW_CASK.to_string(),
                ],
            ),
            UpdateAction::StandaloneUnix => (
                "sh".to_string(),
                vec![
                    "-c".to_string(),
                    format!(
                        "curl -fsSL https://github.com/{CODEX_RELEASE_REPOSITORY}/releases/latest/download/install.sh | CODEX_RELEASE_REPOSITORY={CODEX_RELEASE_REPOSITORY} CODEX_RELEASE_TAG_PREFIX={CODEX_RELEASE_TAG_PREFIX} CODEX_NON_INTERACTIVE=1 sh"
                    ),
                ],
            ),
            UpdateAction::StandaloneWindows => (
                "powershell".to_string(),
                vec![
                    "-ExecutionPolicy".to_string(),
                    "Bypass".to_string(),
                    "-c".to_string(),
                    format!(
                        "$env:CODEX_RELEASE_REPOSITORY='{CODEX_RELEASE_REPOSITORY}'; $env:CODEX_RELEASE_TAG_PREFIX='{CODEX_RELEASE_TAG_PREFIX}'; $env:CODEX_NON_INTERACTIVE=1; irm https://github.com/{CODEX_RELEASE_REPOSITORY}/releases/latest/download/install.ps1 | iex"
                    ),
                ],
            ),
        }
    }

    /// Returns string representation of the command-line arguments for invoking the update.
    pub fn command_str(self) -> String {
        let (command, args) = self.command_args();
        shlex::try_join(std::iter::once(command.as_str()).chain(args.iter().map(String::as_str)))
            .unwrap_or_else(|_| format!("{command} {}", args.join(" ")))
    }

    #[cfg_attr(debug_assertions, allow(dead_code))]
    pub(crate) fn cache_key(self) -> &'static str {
        match self {
            UpdateAction::NpmGlobalLatest => "npm-global-latest",
            UpdateAction::BunGlobalLatest => "bun-global-latest",
            UpdateAction::BrewUpgrade => "brew-upgrade",
            UpdateAction::StandaloneUnix => "standalone-unix",
            UpdateAction::StandaloneWindows => "standalone-windows",
        }
    }
}

#[cfg(not(debug_assertions))]
pub fn get_update_action() -> Option<UpdateAction> {
    UpdateAction::from_install_context(InstallContext::current())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;

    #[test]
    fn maps_install_context_to_update_action() {
        let native_release_dir =
            AbsolutePathBuf::from_absolute_path(std::env::temp_dir().join("native-release"))
                .expect("temp dir path should be absolute");

        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method: InstallMethod::Other,
                package_layout: None,
            }),
            None
        );
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method: InstallMethod::Npm,
                package_layout: None,
            }),
            Some(UpdateAction::NpmGlobalLatest)
        );
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method: InstallMethod::Bun,
                package_layout: None,
            }),
            Some(UpdateAction::BunGlobalLatest)
        );
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method: InstallMethod::Brew,
                package_layout: None,
            }),
            Some(UpdateAction::BrewUpgrade)
        );
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method: InstallMethod::Standalone {
                    platform: StandalonePlatform::Unix,
                    release_dir: native_release_dir.clone(),
                    resources_dir: Some(native_release_dir.join("codex-resources")),
                },
                package_layout: None,
            }),
            Some(UpdateAction::StandaloneUnix)
        );
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method: InstallMethod::Standalone {
                    platform: StandalonePlatform::Windows,
                    release_dir: native_release_dir.clone(),
                    resources_dir: Some(native_release_dir.join("codex-resources")),
                },
                package_layout: None,
            }),
            Some(UpdateAction::StandaloneWindows)
        );
    }

    #[test]
    fn standalone_update_commands_rerun_latest_installer() {
        assert_eq!(
            UpdateAction::StandaloneUnix.command_args(),
            (
                "sh".to_string(),
                vec![
                    "-c".to_string(),
                    "curl -fsSL https://github.com/sednalabs/codex/releases/latest/download/install.sh | CODEX_RELEASE_REPOSITORY=sednalabs/codex CODEX_RELEASE_TAG_PREFIX=v CODEX_NON_INTERACTIVE=1 sh".to_string()
                ],
            )
        );
        assert_eq!(
            UpdateAction::StandaloneWindows.command_args(),
            (
                "powershell".to_string(),
                vec![
                    "-ExecutionPolicy".to_string(),
                    "Bypass".to_string(),
                    "-c".to_string(),
                    "$env:CODEX_RELEASE_REPOSITORY='sednalabs/codex'; $env:CODEX_RELEASE_TAG_PREFIX='v'; $env:CODEX_NON_INTERACTIVE=1; irm https://github.com/sednalabs/codex/releases/latest/download/install.ps1 | iex".to_string(),
                ],
            )
        );
    }

    #[test]
    fn update_commands_use_configured_channels() {
        assert_eq!(
            UpdateAction::NpmGlobalLatest.command_args(),
            (
                "npm".to_string(),
                vec![
                    "install".to_string(),
                    "-g".to_string(),
                    "@openai/codex".to_string()
                ],
            )
        );
        assert_eq!(
            UpdateAction::BrewUpgrade.command_args(),
            (
                "brew".to_string(),
                vec![
                    "upgrade".to_string(),
                    "--cask".to_string(),
                    "codex".to_string()
                ],
            )
        );
    }
}
