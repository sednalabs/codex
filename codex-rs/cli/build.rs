use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    emit_git_rerun_hints();

    let short_sha = Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
            } else {
                None
            }
        })
        .filter(|hash| !hash.is_empty());

    let is_dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .is_some_and(|output| output.status.success() && !output.stdout.is_empty());

    let git_info = short_sha.map_or_else(
        || "unknown".to_string(),
        |hash| {
            if is_dirty {
                format!("{hash}-dirty")
            } else {
                hash
            }
        },
    );

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
        .unwrap_or_else(|| git_info.clone());

    println!("cargo:rustc-env=CODEX_GIT_SHA={git_info}");
    println!("cargo:rustc-env=CODEX_GIT_DESCRIBE={git_describe}");
}

fn emit_git_rerun_hints() {
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
