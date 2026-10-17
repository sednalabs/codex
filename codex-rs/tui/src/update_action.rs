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
                            && (receipt.selection_channel == "prerelease"
                                || receipt.release.channel == "stable")
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
                let Some(receipt) = receipt.filter(|receipt| {
                    receipt.selection_channel == "prerelease" || receipt.release.channel == "stable"
                }) else {
                    return (
                        "sh".into(),
                        strings(&[
                            "-c",
                            "echo 'Manual reinstall from https://github.com/sednalabs/codex/releases'; exit 1",
                        ]),
                    );
                };
                sedna_standalone_command_args(
                    &receipt.release.source_commit,
                    &receipt.selection_channel,
                    &receipt.release.package_version.to_string(),
                    &receipt.release.native_version.to_string(),
                )
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

fn sedna_standalone_command_args(
    source_commit: &str,
    selection_channel: &str,
    previous_package_version: &str,
    previous_native_version: &str,
) -> (String, Vec<String>) {
    let url = format!(
        "https://raw.githubusercontent.com/sednalabs/codex/{source_commit}/scripts/install/install.sh"
    );
    let command = format!(
        "installer=$(curl -fsSL {url}) && [ -n \"$installer\" ] && printf '%s\\n' \"$installer\" | env CODEX_SEDNA_UPDATE=1 CODEX_SEDNA_RELEASE_CHANNEL={selection_channel} CODEX_SEDNA_PREVIOUS_VERSION={previous_package_version} CODEX_SEDNA_PREVIOUS_NATIVE_VERSION={previous_native_version} CODEX_RELEASE=latest CODEX_NON_INTERACTIVE=1 sh"
    );
    ("sh".into(), vec!["-c".into(), command])
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

    #[cfg(unix)]
    #[test]
    fn sedna_update_rejects_failed_or_empty_script_fetches() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temporary directory");
        let curl = temp.path().join("curl");
        std::fs::write(
            &curl,
            "#!/bin/sh\nif [ \"$CODEX_TEST_CURL_EMPTY\" = 1 ]; then exit 0; fi\nexit 22\n",
        )
        .expect("mock curl");
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755))
            .expect("make mock curl executable");
        let marker = temp.path().join("selected-version");
        std::fs::write(&marker, "current").expect("selection marker");
        let (shell, args) = sedna_standalone_command_args(
            "1111111111111111111111111111111111111111",
            "stable",
            "0.146.0",
            "0.162.0-sedna.dev.be2e129",
        );

        assert_eq!(shell, "sh");
        for empty_response in [false, true] {
            let status = std::process::Command::new("/bin/sh")
                .args(&args)
                .env("PATH", temp.path())
                .env(
                    "CODEX_TEST_CURL_EMPTY",
                    if empty_response { "1" } else { "0" },
                )
                .status()
                .expect("run mocked update shell");

            assert!(!status.success(), "empty_response={empty_response}");
            assert_eq!(std::fs::read_to_string(&marker).unwrap(), "current");
        }
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
