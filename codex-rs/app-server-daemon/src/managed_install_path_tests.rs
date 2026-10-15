use pretty_assertions::assert_eq;

#[test]
fn discovers_package_and_legacy_installs() {
    let home = tempfile::TempDir::new().expect("home");
    let current = home.path().join("packages/standalone/current");
    let legacy = current.join(super::managed_codex_file_name());
    assert_eq!(
        super::managed_codex_bin(home.path()),
        home.path()
            .join("packages/app-server-daemon/current/bin")
            .join(super::managed_codex_file_name())
    );
    std::fs::create_dir_all(&current).expect("current directory");
    std::fs::write(&legacy, b"legacy").expect("legacy executable");
    let state = home.path().join("app-server-daemon");
    std::fs::create_dir(&state).unwrap();
    for name in ["settings.json", "daemon.lock", "app-server.pid.lock"] {
        std::fs::write(state.join(name), b"").unwrap();
    }
    // A CLI install and a previous stop/status operation do not establish ownership.
    assert_eq!(
        super::package_root(home.path()),
        home.path().join("packages/app-server-daemon")
    );
    std::fs::write(state.join("app-server.stderr.log"), b"").unwrap();
    assert_eq!(super::managed_codex_bin(home.path()), legacy);
    let packaged = current.join("bin").join(super::managed_codex_file_name());
    std::fs::create_dir(current.join("bin")).expect("bin directory");
    std::fs::write(&packaged, b"packaged").expect("packaged executable");
    assert_eq!(super::managed_codex_bin(home.path()), packaged);

    std::fs::remove_dir_all(home.path().join("packages/standalone")).unwrap();
    assert_eq!(
        super::package_root(home.path()),
        home.path().join("packages/standalone")
    );
    std::fs::remove_file(state.join("app-server.stderr.log")).unwrap();
    std::fs::write(state.join("app-server.pid"), b"running daemon").unwrap();
    assert_eq!(
        super::package_root(home.path()),
        home.path().join("packages/standalone")
    );

    std::fs::write(state.join("daemon.pid"), b"running dedicated daemon").unwrap();
    assert_eq!(
        super::package_root(home.path()),
        home.path().join("packages/app-server-daemon")
    );
    std::fs::remove_file(state.join("daemon.pid")).unwrap();
    std::fs::write(state.join("daemon.stderr.log"), b"").unwrap();
    assert_eq!(
        super::package_root(home.path()),
        home.path().join("packages/app-server-daemon")
    );

    #[cfg(unix)]
    {
        let dedicated = home.path().join("packages/app-server-daemon");
        std::fs::write(&dedicated, b"not a directory").expect("unreadable selection");
        assert_eq!(super::package_root(home.path()), dedicated);
    }
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[test]
fn updater_requires_a_verified_release_on_the_selected_channel() {
    let home = tempfile::TempDir::new().expect("home");
    let state = home.path().join("app-server-daemon");
    std::fs::create_dir(&state).unwrap();
    std::fs::write(state.join("app-server.stderr.log"), b"").unwrap();
    let standalone = home.path().join("packages/standalone");
    let current = standalone.join("current");
    let target = format!("{}-unknown-linux-musl", std::env::consts::ARCH);
    let native_version = env!("CARGO_PKG_VERSION");
    let package_version = "0.146.0";
    let release = standalone
        .join("releases")
        .join(format!("{package_version}-{target}"));
    let managed = release.join("bin/codex");
    std::fs::create_dir_all(managed.parent().expect("bin parent")).expect("release");
    std::fs::write(&managed, b"stable").expect("managed bin");
    write_sedna_fixture(&release, package_version, native_version, &target, "stable");
    std::os::unix::fs::symlink(&release, &current).expect("current release");
    assert!(!super::is_sedna_update_eligible(home.path(), &managed));
    let marker = standalone.join("auto-update-version");
    let release_name = release.file_name().expect("release name");
    std::fs::write(&marker, release_name.as_encoded_bytes()).expect("latest selection");
    assert!(super::is_sedna_update_eligible(home.path(), &managed));
    std::fs::write(&marker, b"0.145.0-x86_64-unknown-linux-musl").expect("stale selection");
    assert!(!super::is_sedna_update_eligible(home.path(), &managed));
    std::fs::remove_file(&marker).expect("pinned selection");
    assert!(!super::is_sedna_update_eligible(home.path(), &managed));

    let mut alpha_semver = semver::Version::parse(native_version).unwrap();
    alpha_semver.pre = semver::Prerelease::new("sedna-test.1").unwrap();
    let alpha_version = alpha_semver.to_string();
    let alpha = standalone
        .join("releases")
        .join(format!("{alpha_version}-{target}"));
    let alpha_managed = alpha.join("bin/codex");
    std::fs::create_dir_all(alpha_managed.parent().expect("alpha bin parent"))
        .expect("alpha release");
    std::fs::write(&alpha_managed, b"alpha").expect("alpha bin");
    write_sedna_fixture(
        &alpha,
        &alpha_version,
        native_version,
        &target,
        "prerelease",
    );
    std::fs::remove_file(&current).expect("remove current");
    std::os::unix::fs::symlink(&alpha, &current).expect("current alpha");
    std::fs::write(&marker, alpha.file_name().unwrap().as_encoded_bytes()).unwrap();
    assert!(super::is_sedna_update_eligible(home.path(), &alpha_managed));

    let local = standalone.join("local-main");
    let local_managed = local.join("bin/codex");
    std::fs::create_dir_all(local_managed.parent().expect("local bin parent"))
        .expect("local build");
    std::fs::write(&local_managed, b"local").expect("local bin");
    std::fs::remove_file(&current).expect("remove current");
    std::os::unix::fs::symlink(local, &current).expect("current local build");
    assert!(!super::is_sedna_update_eligible(
        home.path(),
        &local_managed
    ));
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn write_sedna_fixture(
    release: &std::path::Path,
    package_version: &str,
    native_version: &str,
    target: &str,
    channel: &str,
) {
    std::fs::write(
        release.join("codex-package.json"),
        format!(
            r#"{{
  "layoutVersion": 1,
  "version": "{package_version}",
  "nativeVersion": "{native_version}",
  "target": "{target}",
  "variant": "codex",
  "entrypoint": "bin/codex"
}}"#
        ),
    )
    .unwrap();
    std::fs::write(
        release.join("sedna-release.json"),
        format!(
            r#"{{
  "schemaVersion": 1,
  "selectionChannel": "{channel}",
  "release": {{
    "schemaVersion": 1,
    "repository": "sednalabs/codex",
    "releaseTag": "rust-v{package_version}",
    "packageVersion": "{package_version}",
    "nativeVersion": "{native_version}",
    "channel": "{channel}",
    "target": "{target}",
    "sourceCommit": "1111111111111111111111111111111111111111",
    "sourceTree": "2222222222222222222222222222222222222222",
    "workflowHostCommit": "3333333333333333333333333333333333333333",
    "workflowHostTree": "4444444444444444444444444444444444444444",
    "workflowRunId": "123",
    "archive": {{
      "name": "codex-package-{target}.tar.gz",
      "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    }}
  }}
}}"#
        ),
    )
    .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn older_managed_binary_does_not_claim_updater_support() {
    let temp = tempfile::TempDir::new().expect("home");
    let binary = temp.path().join("codex");
    codex_utils_cargo_bin::write_executable(&binary, "#!/bin/sh\nexit 2\n").expect("older binary");
    assert!(!super::supports_daemon_update_loop(&binary).await);
    codex_utils_cargo_bin::write_executable(&binary, "#!/bin/sh\nexit 0\n").expect("newer binary");
    assert!(super::supports_daemon_update_loop(&binary).await);
}
