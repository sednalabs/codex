use codex_install_context::InstallContext;
#[cfg(any(not(debug_assertions), test))]
use codex_install_context::InstallMethod;

/// Update action the CLI should perform after the TUI exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    /// Replace the local daemon after restoring the terminal.
    Daemon(DaemonUpdateSource),
    /// Update a verified Sedna standalone Linux package.
    SednaStandalone,
}

impl UpdateAction {
    #[cfg(any(not(debug_assertions), test))]
    pub(crate) fn from_install_context(context: &InstallContext) -> Option<Self> {
        match &context.method {
            InstallMethod::Npm
            | InstallMethod::Bun
            | InstallMethod::VitePlus
            | InstallMethod::Pnpm
            | InstallMethod::Brew
            | InstallMethod::Other => None,
            InstallMethod::Standalone { .. } => {
                if !cfg!(all(
                    target_os = "linux",
                    any(target_arch = "x86_64", target_arch = "aarch64")
                )) {
                    return None;
                }
                let expected_target = format!("{}-unknown-linux-musl", std::env::consts::ARCH);
                context
                    .sedna_release_receipt()
                    .filter(|receipt| {
                        receipt.release.target == expected_target
                            && receipt.release.native_version.to_string()
                                == crate::version::CODEX_CLI_VERSION
                    })
                    .map(|_| UpdateAction::SednaStandalone)
            }
        }
    }

    /// Returns the list of command-line arguments for invoking the update.
    pub fn command_args(self) -> (String, Vec<String>) {
        match self {
            UpdateAction::Daemon(source) => ("codex".into(), strings(source.command_args())),
            UpdateAction::SednaStandalone => {
                let receipt = InstallContext::current().sedna_release_receipt();
                let Some(receipt) = receipt else {
                    return (
                        "sh".into(),
                        strings(&[
                            "-c",
                            "echo 'Manual reinstall from https://github.com/sednalabs/codex/releases'; exit 1",
                        ]),
                    );
                };
                let command = format!(
                    "curl -fsSL https://raw.githubusercontent.com/sednalabs/codex/{}/scripts/install/install.sh | CODEX_SEDNA_UPDATE=1 CODEX_SEDNA_RELEASE_CHANNEL={} CODEX_SEDNA_PREVIOUS_VERSION={} CODEX_RELEASE=latest CODEX_NON_INTERACTIVE=1 sh",
                    receipt.release.source_commit,
                    receipt.selection_channel,
                    receipt.release.package_version
                );
                ("sh".into(), vec!["-c".into(), command])
            }
        }
    }

    /// Returns string representation of the command-line arguments for invoking the update.
    pub fn command_str(self) -> String {
        let (command, args) = self.command_args();
        shlex::try_join(std::iter::once(command.as_str()).chain(args.iter().map(String::as_str)))
            .unwrap_or_else(|_| format!("{command} {}", args.join(" ")))
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
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
        for method in [
            InstallMethod::Npm,
            InstallMethod::Bun,
            InstallMethod::Pnpm,
            InstallMethod::VitePlus,
            InstallMethod::Brew,
        ] {
            assert_eq!(
                UpdateAction::from_install_context(&InstallContext {
                    method,
                    package_layout: None,
                }),
                None
            );
        }
        assert_eq!(
            UpdateAction::from_install_context(&InstallContext {
                method: InstallMethod::Standalone {
                    platform: codex_install_context::StandalonePlatform::Unix,
                    release_dir: native_release_dir.clone(),
                    resources_dir: Some(native_release_dir.join("codex-resources")),
                },
                package_layout: None,
            }),
            None
        );
    }
}

/// Package source explicitly selected by the user in the daemon menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonUpdateSource {
    PublicStable,
    ThisCli,
}

impl DaemonUpdateSource {
    pub fn command_args(self) -> &'static [&'static str] {
        match self {
            Self::PublicStable => &["app-server", "daemon", "update"],
            Self::ThisCli => &["app-server", "daemon", "update", "--from-cli", "--yes"],
        }
    }
}
