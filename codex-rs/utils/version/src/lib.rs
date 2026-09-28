/// The canonical Sedna release version used for updater comparisons, persistence, and telemetry.
pub const RELEASE_VERSION: &str = env!("CODEX_RELEASE_VERSION_EFFECTIVE");

/// A compact human-readable version label that can include build provenance.
pub const DISPLAY_VERSION: &str = env!("CODEX_VERSION_DISPLAY");

/// The current build provenance label, if one was embedded at build time.
pub fn build_provenance() -> Option<&'static str> {
    match env!("CODEX_BUILD_PROVENANCE") {
        "" => None,
        value => Some(value),
    }
}

/// The current upstream track, if one was embedded at build time.
pub fn upstream_track() -> Option<&'static str> {
    match env!("CODEX_UPSTREAM_TRACK") {
        "" => None,
        value => Some(value),
    }
}

/// The merge-base commit against the mirrored upstream branch, if embedded.
pub fn upstream_base_commit() -> Option<&'static str> {
    match env!("CODEX_UPSTREAM_BASE_COMMIT") {
        "" => None,
        value => Some(value),
    }
}

/// The exact upstream tag at the merge-base commit, if there is one.
pub fn upstream_base_tag() -> Option<&'static str> {
    match env!("CODEX_UPSTREAM_BASE_TAG") {
        "" => None,
        value => Some(value),
    }
}

/// The current downstream commit identifier for this build.
pub const DOWNSTREAM_COMMIT: &str = env!("CODEX_DOWNSTREAM_COMMIT");

/// The full downstream commit SHA for this build when it is available.
pub fn downstream_commit_full() -> Option<&'static str> {
    match env!("CODEX_DOWNSTREAM_COMMIT_FULL") {
        "" => None,
        value => Some(value),
    }
}

/// The short git SHA used by local build displays.
pub const GIT_SHA: &str = env!("CODEX_GIT_SHA");

/// The human-readable git describe string for this build.
pub const GIT_DESCRIBE: &str = env!("CODEX_GIT_DESCRIBE");
