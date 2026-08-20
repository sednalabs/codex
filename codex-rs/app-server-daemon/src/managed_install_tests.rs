use pretty_assertions::assert_eq;

use super::ManagedReleaseMetadata;
use super::executable_identity_from_bytes;
use super::managed_sedna_automatic_update_release_from_metadata;
use super::parse_codex_version;

#[test]
fn parses_codex_cli_version_output() {
    assert_eq!(
        parse_codex_version("codex 1.2.3\n").expect("version"),
        "1.2.3"
    );
}

#[test]
fn rejects_malformed_codex_cli_version_output() {
    assert!(parse_codex_version("codex\n").is_err());
}

#[test]
fn executable_identity_uses_binary_contents() {
    let old = executable_identity_from_bytes(b"old");
    let same = executable_identity_from_bytes(b"old");
    let new = executable_identity_from_bytes(b"new");

    assert_eq!(old, same);
    assert_ne!(old, new);
}

#[test]
fn managed_release_metadata_controls_sedna_automatic_update_authority() {
    let managed_sedna = ManagedReleaseMetadata {
        release_tag: "v1.2.3-sedna.1".to_string(),
        release_version: "1.2.3-sedna.1".to_string(),
        repository: "sednalabs/codex".to_string(),
        target: "x86_64-unknown-linux-gnu".to_string(),
    };
    for invoking_binary in ["Sedna", "upstream"] {
        assert_eq!(
            managed_sedna_automatic_update_release_from_metadata(
                &managed_sedna,
                "linux",
                "x86_64",
            )
            .map(|release| release.version),
            Some("1.2.3-sedna.1".to_string()),
            "{invoking_binary} caller must not override eligible managed Sedna metadata"
        );
    }

    let managed_upstream = ManagedReleaseMetadata {
        repository: "openai/codex".to_string(),
        ..managed_sedna
    };
    assert_eq!(
        managed_sedna_automatic_update_release_from_metadata(&managed_upstream, "linux", "x86_64",),
        None,
        "Sedna caller must fail closed for managed upstream metadata"
    );
}

#[test]
fn managed_release_metadata_rejects_prerelease_and_wrong_target() {
    for metadata in [
        ManagedReleaseMetadata {
            release_tag: "v1.2.3-alpha.1-sedna.1".to_string(),
            release_version: "1.2.3-alpha.1-sedna.1".to_string(),
            repository: "sednalabs/codex".to_string(),
            target: "x86_64-unknown-linux-gnu".to_string(),
        },
        ManagedReleaseMetadata {
            release_tag: "v1.2.3-sedna.1".to_string(),
            release_version: "1.2.3-sedna.1".to_string(),
            repository: "sednalabs/codex".to_string(),
            target: "x86_64-apple-darwin".to_string(),
        },
    ] {
        assert_eq!(
            managed_sedna_automatic_update_release_from_metadata(&metadata, "linux", "x86_64"),
            None
        );
    }
}
