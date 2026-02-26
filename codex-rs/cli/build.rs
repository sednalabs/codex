use std::process::Command;

fn main() {
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
