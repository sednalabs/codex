use pretty_assertions::assert_eq;

use super::standalone_installer_env;
use super::standalone_installer_url;
use super::update_modes_for_identities;
use crate::RestartMode;
use crate::UpdaterRefreshMode;
use crate::managed_install::executable_identity_from_bytes;

#[test]
fn unchanged_updater_uses_version_based_restart() {
    assert_eq!(
        update_modes_for_identities(
            &executable_identity_from_bytes(b"same"),
            &executable_identity_from_bytes(b"same"),
        ),
        (RestartMode::IfVersionChanged, UpdaterRefreshMode::None)
    );
}

#[test]
fn changed_updater_forces_refresh_even_when_version_may_match() {
    assert_eq!(
        update_modes_for_identities(
            &executable_identity_from_bytes(b"old"),
            &executable_identity_from_bytes(b"new"),
        ),
        (
            RestartMode::Always,
            UpdaterRefreshMode::ReexecIfManagedBinaryChanged,
        )
    );
}

#[test]
fn standalone_updater_uses_configured_release_channel() {
    assert_eq!(
        standalone_installer_url(),
        "https://github.com/sednalabs/codex/releases/latest/download/install.sh"
    );
    assert_eq!(
        standalone_installer_env(),
        [
            ("CODEX_RELEASE_REPOSITORY", "sednalabs/codex"),
            ("CODEX_RELEASE_TAG_PREFIX", "v"),
            ("CODEX_NON_INTERACTIVE", "1"),
        ]
    );
}
