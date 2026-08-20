use crate::version::CODEX_RELEASE_TAG_PREFIX;
use semver::Version;

pub(crate) fn is_newer(latest: &str, current: &str) -> Option<bool> {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => Some(l > c),
        _ => None,
    }
}

/// Cache values are untrusted across releases. A cached value may be compared
/// only after it passes the Sedna release grammar used for live release tags.
pub(crate) fn is_actionable_sedna_update(latest: &str, current: &str) -> bool {
    is_sedna_release_version(latest)
        && is_sedna_release_version(current)
        && is_newer(latest, current).unwrap_or(false)
}

pub(crate) fn extract_version_from_latest_tag(latest_tag_name: &str) -> anyhow::Result<String> {
    let version = latest_tag_name
        .strip_prefix(CODEX_RELEASE_TAG_PREFIX)
        .filter(|version| is_sedna_release_version(version))
        .ok_or_else(|| anyhow::anyhow!("Failed to parse latest tag name '{latest_tag_name}'"))?;
    Ok(version.to_owned())
}

/// Matches the release-tag grammar used by `resolve_sedna_release_version.py`:
/// `<track>-sedna.<ordinal>[+upstream.<distance>]`.
///
/// Keep this structural validation separate from [`Version::parse`]: the release
/// resolver is the authority for which tags belong to this channel, while semver
/// comparison may intentionally fail closed for values it cannot compare.
pub(crate) fn is_sedna_release_version(version: &str) -> bool {
    let (version, metadata) = match version.split_once('+') {
        Some((version, metadata)) => (version, Some(metadata)),
        None => (version, None),
    };
    if metadata.is_some_and(|metadata| {
        metadata.strip_prefix("upstream.").is_none_or(|distance| {
            distance.is_empty() || !distance.bytes().all(|b| b.is_ascii_digit())
        })
    }) {
        return false;
    }

    let Some((track, ordinal)) = version.rsplit_once("-sedna.") else {
        return false;
    };
    !ordinal.is_empty() && ordinal.bytes().all(|b| b.is_ascii_digit()) && is_sedna_track(track)
}

fn is_sedna_track(track: &str) -> bool {
    let (core, prerelease) = match track.split_once('-') {
        Some((core, prerelease)) => (core, Some(prerelease)),
        None => (track, None),
    };
    let mut core_parts = core.split('.');
    let valid_core = (0..3).all(|_| {
        core_parts
            .next()
            .is_some_and(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
    }) && core_parts.next().is_none();
    valid_core
        && prerelease.is_none_or(|prerelease| {
            !prerelease.is_empty()
                && prerelease.split('.').all(|identifier| {
                    !identifier.is_empty() && identifier.bytes().all(|b| b.is_ascii_alphanumeric())
                })
        })
}

pub(crate) fn is_source_build_version(version: &str) -> bool {
    parse_version(version) == Some(Version::new(0, 0, 0))
}

fn parse_version(v: &str) -> Option<Version> {
    Version::parse(v.trim()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn extracts_version_from_latest_tag() {
        assert!(extract_version_from_latest_tag("rust-v1.5.0").is_err());
        assert_eq!(
            extract_version_from_latest_tag("v1.5.0-sedna.1").expect("failed to parse version"),
            "1.5.0-sedna.1"
        );
        assert_eq!(
            extract_version_from_latest_tag("v0.119.0-alpha.4-sedna.2+upstream.17")
                .expect("failed to parse version"),
            "0.119.0-alpha.4-sedna.2+upstream.17"
        );
    }

    #[test]
    fn latest_tag_without_version_prefix_is_invalid() {
        assert!(extract_version_from_latest_tag("release-1.5.0").is_err());
    }

    #[test]
    fn only_sedna_release_tags_are_admitted() {
        for tag in [
            "rust-v1.5.0",
            "v1.5.0",
            "v1.5.0-sedna",
            "v1.5.0-sedna.x",
            "v1.5.0-sedna.1+build.2",
            "v1.5.0-sedna.1+upstream.x",
            "v1.5.0-rc-1-sedna.1",
            "v1.5.0-sedna.1+upstream.2+extra",
        ] {
            assert!(
                extract_version_from_latest_tag(tag).is_err(),
                "accepted {tag}"
            );
        }
    }

    #[test]
    fn cached_updates_must_be_sedna_releases_before_comparison() {
        assert!(is_actionable_sedna_update("1.5.0-sedna.2", "1.5.0-sedna.1"));
        for cached_version in ["1.5.1", "1.5.1+upstream.4", "1.5.1-sedna.x"] {
            assert!(
                !is_actionable_sedna_update(cached_version, "1.5.0-sedna.1"),
                "accepted cached version {cached_version}"
            );
        }
    }

    #[test]
    fn prerelease_version_is_not_considered_newer() {
        assert_eq!(is_newer("0.11.0-beta.1", "0.11.0"), Some(false));
        assert_eq!(is_newer("1.0.0-rc.1", "1.0.0"), Some(false));
    }

    #[test]
    fn fork_release_suffixes_compare_correctly() {
        assert_eq!(is_newer("0.117.0-sedna.2", "0.117.0-sedna.1"), Some(true));
        assert_eq!(is_newer("0.117.0-sedna.1", "0.117.0+abcdef12"), Some(false));
        assert_eq!(
            is_newer("0.119.0-sedna.2", "0.119.0-alpha.2-sedna.1"),
            Some(true)
        );
    }

    #[test]
    fn plain_semver_comparisons_work() {
        assert_eq!(is_newer("0.11.1", "0.11.0"), Some(true));
        assert_eq!(is_newer("0.11.0", "0.11.1"), Some(false));
        assert_eq!(is_newer("1.0.0", "0.9.9"), Some(true));
        assert_eq!(is_newer("0.9.9", "1.0.0"), Some(false));
    }

    #[test]
    fn source_build_version_is_not_checked() {
        assert!(is_source_build_version("0.0.0"));
        assert!(!is_source_build_version("0.1.0"));
    }

    #[test]
    fn whitespace_is_ignored() {
        assert_eq!(
            parse_version(" 1.2.3 \n"),
            Some(Version::parse("1.2.3").expect("valid semver"))
        );
        assert_eq!(is_newer(" 1.2.3 ", "1.2.2"), Some(true));
    }
}
