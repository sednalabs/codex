use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    emit_git_rerun_hints();

    let package_version =
        std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    let release_version = env_or_default("CODEX_RELEASE_VERSION", &package_version);

    let short_sha = git_stdout(["rev-parse", "--short=8", "HEAD"]);
    let full_sha = git_stdout(["rev-parse", "HEAD"]);
    let is_dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .is_some_and(|output| output.status.success() && !output.stdout.is_empty());

    let git_sha = short_sha
        .clone()
        .map(|hash| {
            if is_dirty {
                format!("{hash}-dirty")
            } else {
                hash
            }
        })
        .unwrap_or_else(|| "unknown".to_string());

    let git_describe = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
            } else {
                None
            }
        })
        .filter(|describe| !describe.is_empty())
        .unwrap_or_else(|| git_sha.clone());

    let upstream_track = env_or_empty("CODEX_UPSTREAM_TRACK");
    let upstream_base_commit = env_or_empty("CODEX_UPSTREAM_BASE_COMMIT");
    let upstream_base_tag = env_or_empty("CODEX_UPSTREAM_BASE_TAG");
    let downstream_commit = env_or_default(
        "CODEX_DOWNSTREAM_COMMIT",
        short_sha.as_deref().unwrap_or("unknown"),
    );

    let build_provenance = if !upstream_base_commit.is_empty() && !downstream_commit.is_empty() {
        format!("up:{upstream_base_commit} down:{downstream_commit}")
    } else if git_sha != "unknown" {
        format!("git:{git_sha}")
    } else {
        String::new()
    };
    let version_display = if build_provenance.is_empty() {
        release_version.clone()
    } else {
        format!("{release_version} ({build_provenance})")
    };

    println!("cargo:rustc-env=CODEX_RELEASE_VERSION_EFFECTIVE={release_version}");
    println!("cargo:rustc-env=CODEX_VERSION_DISPLAY={version_display}");
    println!("cargo:rustc-env=CODEX_BUILD_PROVENANCE={build_provenance}");
    println!("cargo:rustc-env=CODEX_GIT_SHA={git_sha}");
    println!("cargo:rustc-env=CODEX_GIT_DESCRIBE={git_describe}");
    println!("cargo:rustc-env=CODEX_UPSTREAM_TRACK={upstream_track}");
    println!("cargo:rustc-env=CODEX_UPSTREAM_BASE_COMMIT={upstream_base_commit}");
    println!("cargo:rustc-env=CODEX_UPSTREAM_BASE_TAG={upstream_base_tag}");
    println!("cargo:rustc-env=CODEX_DOWNSTREAM_COMMIT={downstream_commit}");
    println!(
        "cargo:rustc-env=CODEX_DOWNSTREAM_COMMIT_FULL={}",
        full_sha.unwrap_or_default()
    );
}

fn env_or_empty(key: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_default()
}

fn env_or_default(key: &str, default: &str) -> String {
    let value = env_or_empty(key);
    if value.is_empty() {
        default.to_string()
    } else {
        value
    }
}

fn emit_git_rerun_hints() {
    println!("cargo:rerun-if-env-changed=CODEX_RELEASE_VERSION");
    println!("cargo:rerun-if-env-changed=CODEX_UPSTREAM_TRACK");
    println!("cargo:rerun-if-env-changed=CODEX_UPSTREAM_BASE_COMMIT");
    println!("cargo:rerun-if-env-changed=CODEX_UPSTREAM_BASE_TAG");
    println!("cargo:rerun-if-env-changed=CODEX_DOWNSTREAM_COMMIT");
    println!("cargo:rerun-if-env-changed=GIT_DIR");
    println!("cargo:rerun-if-env-changed=GIT_WORK_TREE");

    // Worktree state spans both the per-worktree gitdir and the shared common gitdir.
    let mut rerun_paths = BTreeSet::new();
    for git_path in ["HEAD", "index", "packed-refs", "refs/tags"] {
        if let Some(path) = resolve_git_path(git_path) {
            rerun_paths.insert(path);
        }
    }

    if let Some(head_ref) = git_stdout(["symbolic-ref", "-q", "HEAD"])
        && let Some(path) = resolve_git_path(&head_ref)
    {
        rerun_paths.insert(path);
    }

    for path in rerun_paths {
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

fn resolve_git_path(git_path: &str) -> Option<PathBuf> {
    git_stdout([
        "rev-parse",
        "--path-format=absolute",
        "--git-path",
        git_path,
    ])
    .map(PathBuf::from)
}

fn git_stdout<const N: usize>(args: [&str; N]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
            } else {
                None
            }
        })
        .filter(|stdout| !stdout.is_empty())
}
