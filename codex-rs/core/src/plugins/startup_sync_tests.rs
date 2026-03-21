use super::*;
use crate::auth::AuthCredentialsStoreMode;
use crate::auth::AuthDotJson;
use crate::auth::CodexAuth;
use crate::config::CONFIG_TOML_FILE;
use crate::plugins::PluginId;
use crate::plugins::manager::install_startup_remote_plugin_sync_test_pause;
use crate::plugins::test_support::TEST_CURATED_PLUGIN_SHA;
use crate::plugins::test_support::write_curated_plugin_sha;
use crate::plugins::test_support::write_file;
use crate::plugins::test_support::write_openai_curated_marketplace;
use crate::token_data::IdTokenInfo;
use crate::token_data::TokenData;
use chrono::Utc;
use codex_app_server_protocol::AuthMode;
use pretty_assertions::assert_eq;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;
use tokio::sync::Notify;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

#[test]
fn curated_plugins_repo_path_uses_codex_home_tmp_dir() {
    let tmp = tempdir().expect("tempdir");
    assert_eq!(
        curated_plugins_repo_path(tmp.path()),
        tmp.path().join(".tmp/plugins")
    );
}

#[test]
fn read_curated_plugins_sha_reads_trimmed_sha_file() {
    let tmp = tempdir().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join(".tmp")).expect("create tmp");
    std::fs::write(tmp.path().join(".tmp/plugins.sha"), "abc123\n").expect("write sha");

    assert_eq!(
        read_curated_plugins_sha(tmp.path()).as_deref(),
        Some("abc123")
    );
}

#[cfg(unix)]
#[test]
fn sync_openai_plugins_repo_prefers_git_when_available() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempdir().expect("tempdir");
    let bin_dir = tempfile::Builder::new()
        .prefix("fake-git-")
        .tempdir()
        .expect("tempdir");
    let git_path = bin_dir.path().join("git");
    let sha = "0123456789abcdef0123456789abcdef01234567";

    std::fs::write(
        &git_path,
        format!(
            r#"#!/bin/sh
if [ "$1" = "ls-remote" ]; then
  printf '%s\tHEAD\n' "{sha}"
  exit 0
fi
if [ "$1" = "clone" ]; then
  dest="$5"
  mkdir -p "$dest/.git" "$dest/.agents/plugins" "$dest/plugins/gmail/.codex-plugin"
  cat > "$dest/.agents/plugins/marketplace.json" <<'EOF'
{{"name":"openai-curated","plugins":[{{"name":"gmail","source":{{"source":"local","path":"./plugins/gmail"}}}}]}}
EOF
  printf '%s\n' '{{"name":"gmail"}}' > "$dest/plugins/gmail/.codex-plugin/plugin.json"
  exit 0
fi
if [ "$1" = "-C" ] && [ "$3" = "rev-parse" ] && [ "$4" = "HEAD" ]; then
  printf '%s\n' "{sha}"
  exit 0
fi
echo "unexpected git invocation: $@" >&2
exit 1
"#
        ),
    )
    .expect("write fake git");
    let mut permissions = std::fs::metadata(&git_path)
        .expect("metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&git_path, permissions).expect("chmod");

    let synced_sha = sync_openai_plugins_repo_with_transport_overrides(
        tmp.path(),
        git_path.to_str().expect("utf8 path"),
        "http://127.0.0.1:9",
    )
    .expect("git sync should succeed");

    assert_eq!(synced_sha, sha);
    assert!(curated_plugins_repo_path(tmp.path()).join(".git").is_dir());
    assert!(
        curated_plugins_repo_path(tmp.path())
            .join(".agents/plugins/marketplace.json")
            .is_file()
    );
    assert_eq!(read_curated_plugins_sha(tmp.path()).as_deref(), Some(sha));
}

#[tokio::test]
async fn sync_openai_plugins_repo_falls_back_to_http_when_git_is_unavailable() {
    let tmp = tempdir().expect("tempdir");
    let server = MockServer::start().await;
    let sha = "0123456789abcdef0123456789abcdef01234567";

    Mock::given(method("GET"))
        .and(path("/repos/openai/plugins"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"default_branch":"main"}"#))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/openai/plugins/git/ref/heads/main"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(format!(r#"{{"object":{{"sha":"{sha}"}}}}"#)),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/openai/plugins/zipball/{sha}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/zip")
                .set_body_bytes(curated_repo_zipball_bytes(sha)),
        )
        .mount(&server)
        .await;

    let server_uri = server.uri();
    let tmp_path = tmp.path().to_path_buf();
    let synced_sha = tokio::task::spawn_blocking(move || {
        sync_openai_plugins_repo_with_transport_overrides(
            tmp_path.as_path(),
            "missing-git-for-test",
            &server_uri,
        )
    })
    .await
    .expect("sync task should join")
    .expect("fallback sync should succeed");

    let repo_path = curated_plugins_repo_path(tmp.path());
    assert_eq!(synced_sha, sha);
    assert!(repo_path.join(".agents/plugins/marketplace.json").is_file());
    assert!(
        repo_path
            .join("plugins/gmail/.codex-plugin/plugin.json")
            .is_file()
    );
    assert_eq!(read_curated_plugins_sha(tmp.path()).as_deref(), Some(sha));
}

#[cfg(unix)]
#[tokio::test]
async fn sync_openai_plugins_repo_falls_back_to_http_when_git_sync_fails() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempdir().expect("tempdir");
    let bin_dir = tempfile::Builder::new()
        .prefix("fake-git-fail-")
        .tempdir()
        .expect("tempdir");
    let git_path = bin_dir.path().join("git");
    let sha = "0123456789abcdef0123456789abcdef01234567";

    std::fs::write(
        &git_path,
        r#"#!/bin/sh
echo "simulated git failure" >&2
exit 1
"#,
    )
    .expect("write fake git");
    let mut permissions = std::fs::metadata(&git_path)
        .expect("metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&git_path, permissions).expect("chmod");

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/openai/plugins"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"default_branch":"main"}"#))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/openai/plugins/git/ref/heads/main"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(format!(r#"{{"object":{{"sha":"{sha}"}}}}"#)),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/openai/plugins/zipball/{sha}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/zip")
                .set_body_bytes(curated_repo_zipball_bytes(sha)),
        )
        .mount(&server)
        .await;

    let server_uri = server.uri();
    let tmp_path = tmp.path().to_path_buf();
    let synced_sha = tokio::task::spawn_blocking(move || {
        sync_openai_plugins_repo_with_transport_overrides(
            tmp_path.as_path(),
            git_path.to_str().expect("utf8 path"),
            &server_uri,
        )
    })
    .await
    .expect("sync task should join")
    .expect("fallback sync should succeed");

    let repo_path = curated_plugins_repo_path(tmp.path());
    assert_eq!(synced_sha, sha);
    assert!(repo_path.join(".agents/plugins/marketplace.json").is_file());
    assert!(
        repo_path
            .join("plugins/gmail/.codex-plugin/plugin.json")
            .is_file()
    );
    assert_eq!(read_curated_plugins_sha(tmp.path()).as_deref(), Some(sha));
}

#[tokio::test]
async fn sync_openai_plugins_repo_skips_archive_download_when_sha_matches() {
    let tmp = tempdir().expect("tempdir");
    let repo_path = curated_plugins_repo_path(tmp.path());
    std::fs::create_dir_all(repo_path.join(".agents/plugins")).expect("create repo");
    std::fs::write(
        repo_path.join(".agents/plugins/marketplace.json"),
        r#"{"name":"openai-curated","plugins":[]}"#,
    )
    .expect("write marketplace");
    std::fs::create_dir_all(tmp.path().join(".tmp")).expect("create tmp");
    let sha = "fedcba9876543210fedcba9876543210fedcba98";
    std::fs::write(tmp.path().join(".tmp/plugins.sha"), format!("{sha}\n")).expect("write sha");

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/openai/plugins"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"default_branch":"main"}"#))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/openai/plugins/git/ref/heads/main"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(format!(r#"{{"object":{{"sha":"{sha}"}}}}"#)),
        )
        .mount(&server)
        .await;

    let server_uri = server.uri();
    let tmp_path = tmp.path().to_path_buf();
    tokio::task::spawn_blocking(move || {
        sync_openai_plugins_repo_with_transport_overrides(
            tmp_path.as_path(),
            "missing-git-for-test",
            &server_uri,
        )
    })
    .await
    .expect("sync task should join")
    .expect("sync should succeed");

    assert_eq!(read_curated_plugins_sha(tmp.path()).as_deref(), Some(sha));
    assert!(repo_path.join(".agents/plugins/marketplace.json").is_file());
}

#[tokio::test]
async fn startup_remote_plugin_sync_writes_marker_and_reconciles_state() {
    let tmp = tempdir().expect("tempdir");
    let curated_root = curated_plugins_repo_path(tmp.path());
    write_openai_curated_marketplace(&curated_root, &["linear"]);
    write_curated_plugin_sha(tmp.path());
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true

[plugins."linear@openai-curated"]
enabled = false
"#,
    );

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/plugins/list"))
        .and(header("authorization", "Bearer Access Token"))
        .and(header("chatgpt-account-id", "account_id"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"[
  {"id":"1","name":"linear","marketplace_name":"openai-curated","version":"1.0.0","enabled":true}
]"#,
        ))
        .mount(&server)
        .await;

    let mut config = crate::plugins::test_support::load_plugins_config(tmp.path()).await;
    config.chatgpt_base_url = format!("{}/backend-api/", server.uri());
    let manager = Arc::new(PluginsManager::new(tmp.path().to_path_buf()));
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());

    start_startup_remote_plugin_sync_once(
        Arc::clone(&manager),
        tmp.path().to_path_buf(),
        config,
        auth_manager,
    );

    let marker_path = tmp.path().join(STARTUP_REMOTE_PLUGIN_SYNC_MARKER_FILE);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if marker_path.is_file() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("marker should be written");

    assert!(
        tmp.path()
            .join(format!(
                "plugins/cache/openai-curated/linear/{TEST_CURATED_PLUGIN_SHA}"
            ))
            .is_dir()
    );
    let config =
        std::fs::read_to_string(tmp.path().join(CONFIG_TOML_FILE)).expect("config should exist");
    assert!(config.contains(r#"[plugins."linear@openai-curated"]"#));
    assert!(config.contains("enabled = true"));

    let marker_contents = std::fs::read_to_string(marker_path).expect("marker should be readable");
    assert_eq!(marker_contents, "ok\n");
}

#[tokio::test]
async fn startup_remote_plugin_sync_waits_for_late_prerequisites() {
    let tmp = tempdir().expect("tempdir");
    let curated_root = curated_plugins_repo_path(tmp.path());
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true

[plugins."linear@openai-curated"]
enabled = false
"#,
    );

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/plugins/list"))
        .and(header("authorization", "Bearer Access Token"))
        .and(header("chatgpt-account-id", "account_id"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"[
  {"id":"1","name":"linear","marketplace_name":"openai-curated","version":"1.0.0","enabled":true}
]"#,
        ))
        .mount(&server)
        .await;

    let mut config = crate::plugins::test_support::load_plugins_config(tmp.path()).await;
    config.chatgpt_base_url = format!("{}/backend-api/", server.uri());
    let manager = Arc::new(PluginsManager::new(tmp.path().to_path_buf()));
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());

    start_startup_remote_plugin_sync_once(
        Arc::clone(&manager),
        tmp.path().to_path_buf(),
        config,
        auth_manager,
    );

    tokio::time::sleep(Duration::from_millis(150)).await;
    write_openai_curated_marketplace(&curated_root, &["linear"]);
    write_curated_plugin_sha(tmp.path());

    let marker_path = tmp.path().join(STARTUP_REMOTE_PLUGIN_SYNC_MARKER_FILE);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if marker_path.is_file() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("marker should be written after late prerequisites become available");

    assert!(
        tmp.path()
            .join(format!(
                "plugins/cache/openai-curated/linear/{TEST_CURATED_PLUGIN_SHA}"
            ))
            .is_dir()
    );
    let config =
        std::fs::read_to_string(tmp.path().join(CONFIG_TOML_FILE)).expect("config should exist");
    assert!(config.contains(r#"[plugins."linear@openai-curated"]"#));
    assert!(config.contains("enabled = true"));

    let marker_contents = std::fs::read_to_string(marker_path).expect("marker should be readable");
    assert_eq!(marker_contents, "ok\n");
}

#[tokio::test]
async fn startup_remote_plugin_sync_observes_completion_signal_during_bounded_wait() {
    let tmp = tempdir().expect("tempdir");
    let completion_signal = Arc::new(Notify::new());
    let signal = Arc::clone(&completion_signal);

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        signal.notify_one();
    });

    let wait_result = tokio::time::timeout(
        Duration::from_secs(1),
        wait_for_startup_remote_plugin_sync_prerequisites(tmp.path(), &completion_signal),
    )
    .await
    .expect("wait should observe the completion signal");

    assert!(matches!(
        wait_result,
        StartupRemotePluginSyncWaitResult::Signaled
    ));
}

#[tokio::test]
async fn startup_remote_plugin_sync_keeps_aborted_state_on_late_completion_signal() {
    let tmp = tempdir().expect("tempdir");
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true
"#,
    );

    let config = crate::plugins::test_support::load_plugins_config(tmp.path()).await;
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());

    startup_remote_plugin_sync_install_state_for_test(
        tmp.path(),
        7,
        StartupRemotePluginSyncPhase::Aborted,
        config,
        Arc::clone(&auth_manager),
    );

    signal_startup_remote_plugin_sync_completion(tmp.path());

    let snapshot = startup_remote_plugin_sync_state_snapshot_for_test(tmp.path())
        .expect("state should stay present");
    assert_eq!(snapshot, (7, StartupRemotePluginSyncPhase::Aborted));
}

#[tokio::test]
async fn startup_remote_plugin_sync_restarts_immediately_from_aborted_state() {
    let tmp = tempdir().expect("tempdir");
    let curated_root = curated_plugins_repo_path(tmp.path());
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true

[plugins."linear@openai-curated"]
enabled = false
"#,
    );

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/plugins/list"))
        .and(header("authorization", "Bearer Access Token"))
        .and(header("chatgpt-account-id", "account_id"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"[
  {"id":"1","name":"linear","marketplace_name":"openai-curated","version":"1.0.0","enabled":true}
]"#,
        ))
        .mount(&server)
        .await;

    let manager = Arc::new(PluginsManager::new(tmp.path().to_path_buf()));
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    let mut config = crate::plugins::test_support::load_plugins_config(tmp.path()).await;
    config.chatgpt_base_url = format!("{}/backend-api/", server.uri());

    startup_remote_plugin_sync_install_state_for_test(
        tmp.path(),
        7,
        StartupRemotePluginSyncPhase::Aborted,
        config.clone(),
        Arc::clone(&auth_manager),
    );

    start_startup_remote_plugin_sync_once(
        Arc::clone(&manager),
        tmp.path().to_path_buf(),
        config,
        auth_manager,
    );

    let snapshot = startup_remote_plugin_sync_state_snapshot_for_test(tmp.path())
        .expect("state should be replaced");
    assert_eq!(snapshot.0, 8);
    assert_eq!(
        snapshot.1,
        StartupRemotePluginSyncPhase::WaitingForPrerequisites
    );

    write_openai_curated_marketplace(&curated_root, &["linear"]);
    write_curated_plugin_sha(tmp.path());

    let marker_path = tmp.path().join(STARTUP_REMOTE_PLUGIN_SYNC_MARKER_FILE);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if marker_path.is_file() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("restarted startup sync should complete");

    tokio::time::sleep(Duration::from_millis(250)).await;
    let requests = server.received_requests().await.unwrap_or_default();
    assert_eq!(
        requests.len(),
        1,
        "expected the restarted worker to reach the server exactly once"
    );
}

#[tokio::test]
async fn startup_remote_plugin_sync_signals_after_failed_curated_postprocessing() {
    let tmp = tempdir().expect("tempdir");
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true
"#,
    );

    let curated_root = curated_plugins_repo_path(tmp.path());
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/plugins/list"))
        .and(header("authorization", "Bearer Access Token"))
        .and(header("chatgpt-account-id", "account_id"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"[
  {"id":"1","name":"linear","marketplace_name":"openai-curated","version":"1.0.0","enabled":true}
]"#,
        ))
        .mount(&server)
        .await;

    let manager = Arc::new(PluginsManager::new(tmp.path().to_path_buf()));
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    let mut config = crate::plugins::test_support::load_plugins_config(tmp.path()).await;
    config.chatgpt_base_url = format!("{}/backend-api/", server.uri());

    start_startup_remote_plugin_sync_once(
        Arc::clone(&manager),
        tmp.path().to_path_buf(),
        config,
        auth_manager,
    );

    tokio::time::sleep(
        STARTUP_REMOTE_PLUGIN_SYNC_PREREQUISITE_TIMEOUT + Duration::from_millis(250),
    )
    .await;

    let plugin_id = PluginId::new(
        "linear".to_string(),
        crate::plugins::OPENAI_CURATED_MARKETPLACE_NAME.to_string(),
    )
    .expect("plugin id should parse");
    let result = PluginsManager::complete_curated_repo_sync_postprocessing(
        manager.as_ref(),
        tmp.path(),
        TEST_CURATED_PLUGIN_SHA,
        &[plugin_id],
    );

    assert!(
        result.is_err(),
        "expected curated postprocessing to fail without a marketplace file"
    );

    write_openai_curated_marketplace(&curated_root, &["linear"]);
    write_curated_plugin_sha(tmp.path());

    let marker_path = tmp.path().join(STARTUP_REMOTE_PLUGIN_SYNC_MARKER_FILE);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if marker_path.is_file() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("parked startup worker should still be woken on failure");

    let requests = server.received_requests().await.unwrap_or_default();
    assert_eq!(
        requests.len(),
        1,
        "expected the resumed sync to reach the server"
    );
}

#[tokio::test]
async fn startup_remote_plugin_sync_aborts_in_flight_before_stamping_marker() {
    let tmp = tempdir().expect("tempdir");
    let curated_root = curated_plugins_repo_path(tmp.path());
    let entered = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true

[plugins."linear@openai-curated"]
enabled = false
"#,
    );
    install_startup_remote_plugin_sync_test_pause(
        tmp.path(),
        Arc::clone(&entered),
        Arc::clone(&resume),
    );

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/plugins/list"))
        .and(header("authorization", "Bearer Access Token"))
        .and(header("chatgpt-account-id", "account_id"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"[
  {"id":"1","name":"linear","marketplace_name":"openai-curated","version":"1.0.0","enabled":true}
]"#,
        ))
        .mount(&server)
        .await;

    let manager = Arc::new(PluginsManager::new(tmp.path().to_path_buf()));
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    let mut config = crate::plugins::test_support::load_plugins_config(tmp.path()).await;
    config.chatgpt_base_url = format!("{}/backend-api/", server.uri());

    write_openai_curated_marketplace(&curated_root, &["linear"]);
    write_curated_plugin_sha(tmp.path());

    start_startup_remote_plugin_sync_once(
        Arc::clone(&manager),
        tmp.path().to_path_buf(),
        config.clone(),
        Arc::clone(&auth_manager),
    );

    tokio::time::timeout(Duration::from_secs(5), async {
        entered.notified().await;
    })
    .await
    .expect("startup sync should reach the post-fetch pause before aborting");

    abort_startup_remote_plugin_sync(tmp.path());

    let marker_path = tmp.path().join(STARTUP_REMOTE_PLUGIN_SYNC_MARKER_FILE);
    let cache_path = tmp.path().join(format!(
        "plugins/cache/openai-curated/linear/{TEST_CURATED_PLUGIN_SHA}"
    ));
    let config_path = tmp.path().join(CONFIG_TOML_FILE);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !marker_path.is_file(),
        "stale worker should not stamp the marker after aborting in-flight sync"
    );
    assert!(
        !cache_path.is_dir(),
        "stale worker should not install the plugin after aborting in-flight sync"
    );
    let config_contents = std::fs::read_to_string(&config_path).expect("config should exist");
    assert!(config_contents.contains("enabled = false"));
    assert!(
        !config_contents.contains("enabled = true"),
        "stale worker should not flip config to enabled after aborting in-flight sync"
    );

    let requests = server.received_requests().await.unwrap_or_default();
    assert_eq!(
        requests.len(),
        1,
        "expected only the aborted launch to have reached the server before the relaunch starts"
    );

    resume.notify_one();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !marker_path.is_file(),
        "stale worker should not stamp the marker after aborting in-flight sync"
    );
    assert!(
        !cache_path.is_dir(),
        "stale worker should not install the plugin after aborting in-flight sync"
    );
    let config_contents = std::fs::read_to_string(&config_path).expect("config should exist");
    assert!(config_contents.contains("enabled = false"));
    assert!(
        !config_contents.contains("enabled = true"),
        "stale worker should not flip config to enabled after aborting in-flight sync"
    );

    start_startup_remote_plugin_sync_once(
        Arc::clone(&manager),
        tmp.path().to_path_buf(),
        config.clone(),
        auth_manager,
    );

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if marker_path.is_file() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("restarted startup sync should complete after abort");

    tokio::time::sleep(Duration::from_millis(250)).await;
    let requests = server.received_requests().await.unwrap_or_default();
    assert_eq!(
        requests.len(),
        2,
        "expected the aborted launch and relaunched worker to reach the server"
    );
    assert!(
        cache_path.is_dir(),
        "restarted startup sync should install the plugin"
    );
    let config = std::fs::read_to_string(&config_path).expect("config should exist");
    assert!(config.contains(r#"[plugins."linear@openai-curated"]"#));
    assert!(config.contains("enabled = true"));
}

#[tokio::test]
async fn startup_remote_plugin_sync_relaunches_immediately_after_abort_even_if_late_completion_signal_arrives()
 {
    let tmp = tempdir().expect("tempdir");
    let curated_root = curated_plugins_repo_path(tmp.path());
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true

[plugins."linear@openai-curated"]
enabled = false
"#,
    );

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/plugins/list"))
        .and(header("authorization", "Bearer Access Token"))
        .and(header("chatgpt-account-id", "account_id"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"[
  {"id":"1","name":"linear","marketplace_name":"openai-curated","version":"1.0.0","enabled":true}
]"#,
        ))
        .mount(&server)
        .await;

    let manager = Arc::new(PluginsManager::new(tmp.path().to_path_buf()));
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    let mut config = crate::plugins::test_support::load_plugins_config(tmp.path()).await;
    config.chatgpt_base_url = format!("{}/backend-api/", server.uri());

    start_startup_remote_plugin_sync_once(
        Arc::clone(&manager),
        tmp.path().to_path_buf(),
        config.clone(),
        Arc::clone(&auth_manager),
    );

    tokio::time::sleep(
        STARTUP_REMOTE_PLUGIN_SYNC_PREREQUISITE_TIMEOUT + Duration::from_millis(250),
    )
    .await;

    abort_startup_remote_plugin_sync(tmp.path());
    signal_startup_remote_plugin_sync_completion(tmp.path());

    let marker_path = tmp.path().join(STARTUP_REMOTE_PLUGIN_SYNC_MARKER_FILE);
    assert!(!marker_path.is_file());

    start_startup_remote_plugin_sync_once(
        Arc::clone(&manager),
        tmp.path().to_path_buf(),
        config,
        auth_manager,
    );

    write_openai_curated_marketplace(&curated_root, &["linear"]);
    write_curated_plugin_sha(tmp.path());

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if marker_path.is_file() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fresh launch should succeed after abort cleanup");

    tokio::time::sleep(Duration::from_millis(250)).await;
    let requests = server.received_requests().await.unwrap_or_default();
    assert_eq!(
        requests.len(),
        1,
        "expected only the relaunched worker to reach the server"
    );
}

#[tokio::test]
async fn startup_remote_plugin_sync_is_single_flight_before_prerequisites_exist() {
    let tmp = tempdir().expect("tempdir");
    let curated_root = curated_plugins_repo_path(tmp.path());
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true

[plugins."linear@openai-curated"]
enabled = false
"#,
    );

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/plugins/list"))
        .and(header("authorization", "Bearer Access Token"))
        .and(header("chatgpt-account-id", "account_id"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(200))
                .set_body_string(
                    r#"[
  {"id":"1","name":"linear","marketplace_name":"openai-curated","version":"1.0.0","enabled":true}
]"#,
                ),
        )
        .expect(1)
        .mount(&server)
        .await;

    let mut config = crate::plugins::test_support::load_plugins_config(tmp.path()).await;
    config.chatgpt_base_url = format!("{}/backend-api/", server.uri());
    let manager = Arc::new(PluginsManager::new(tmp.path().to_path_buf()));
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());

    start_startup_remote_plugin_sync_once(
        Arc::clone(&manager),
        tmp.path().to_path_buf(),
        config.clone(),
        Arc::clone(&auth_manager),
    );
    start_startup_remote_plugin_sync_once(
        Arc::clone(&manager),
        tmp.path().to_path_buf(),
        config,
        auth_manager,
    );

    tokio::time::sleep(Duration::from_millis(150)).await;
    write_openai_curated_marketplace(&curated_root, &["linear"]);
    write_curated_plugin_sha(tmp.path());

    let marker_path = tmp.path().join(STARTUP_REMOTE_PLUGIN_SYNC_MARKER_FILE);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if marker_path.is_file() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("marker should be written after late prerequisites become available");

    tokio::time::sleep(Duration::from_millis(250)).await;
    let requests = server.received_requests().await.unwrap_or_default();
    assert_eq!(requests.len(), 1, "expected a single remote sync request");
}

#[tokio::test]
async fn startup_remote_plugin_sync_uses_latest_config_and_auth_snapshot() {
    let tmp = tempdir().expect("tempdir");
    let curated_root = curated_plugins_repo_path(tmp.path());
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true

[plugins."linear@openai-curated"]
enabled = false
"#,
    );

    let make_auth_manager = |codex_home: &Path, access_token: &str, account_id: &str| {
        let auth = AuthDotJson {
            auth_mode: Some(AuthMode::ChatgptAuthTokens),
            openai_api_key: None,
            tokens: Some(TokenData {
                id_token: IdTokenInfo {
                    raw_jwt: "e30.e30.e30".to_string(),
                    ..Default::default()
                },
                access_token: access_token.to_string(),
                refresh_token: "refresh-token".to_string(),
                account_id: Some(account_id.to_string()),
            }),
            last_refresh: Some(Utc::now()),
        };
        crate::auth::save_auth(codex_home, &auth, AuthCredentialsStoreMode::File)
            .expect("save auth");
        AuthManager::shared(
            codex_home.to_path_buf(),
            /*enable_codex_api_key_env*/ false,
            AuthCredentialsStoreMode::File,
        )
    };

    let auth_home_one = tempdir().expect("auth tempdir one");
    let auth_home_two = tempdir().expect("auth tempdir two");
    let auth_manager_one =
        make_auth_manager(auth_home_one.path(), "Access Token One", "account-one");
    let auth_manager_two =
        make_auth_manager(auth_home_two.path(), "Access Token Two", "account-two");

    let server_one = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/plugins/list"))
        .and(header("authorization", "Bearer Access Token One"))
        .and(header("chatgpt-account-id", "account-one"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"[
  {"id":"1","name":"linear","marketplace_name":"openai-curated","version":"1.0.0","enabled":true}
]"#,
        ))
        .mount(&server_one)
        .await;

    let server_two = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/plugins/list"))
        .and(header("authorization", "Bearer Access Token Two"))
        .and(header("chatgpt-account-id", "account-two"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"[
  {"id":"1","name":"linear","marketplace_name":"openai-curated","version":"1.0.0","enabled":true}
]"#,
        ))
        .mount(&server_two)
        .await;

    let mut config_one = crate::plugins::test_support::load_plugins_config(tmp.path()).await;
    config_one.chatgpt_base_url = format!("{}/backend-api/", server_one.uri());
    let mut config_two = crate::plugins::test_support::load_plugins_config(tmp.path()).await;
    config_two.chatgpt_base_url = format!("{}/backend-api/", server_two.uri());
    let manager = Arc::new(PluginsManager::new(tmp.path().to_path_buf()));

    start_startup_remote_plugin_sync_once(
        Arc::clone(&manager),
        tmp.path().to_path_buf(),
        config_one,
        auth_manager_one,
    );
    start_startup_remote_plugin_sync_once(
        Arc::clone(&manager),
        tmp.path().to_path_buf(),
        config_two,
        auth_manager_two,
    );

    tokio::time::sleep(Duration::from_millis(150)).await;
    write_openai_curated_marketplace(&curated_root, &["linear"]);
    write_curated_plugin_sha(tmp.path());

    let marker_path = tmp.path().join(STARTUP_REMOTE_PLUGIN_SYNC_MARKER_FILE);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if marker_path.is_file() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("marker should be written after late prerequisites become available");

    tokio::time::sleep(Duration::from_millis(250)).await;
    let requests_one = server_one.received_requests().await.unwrap_or_default();
    let requests_two = server_two.received_requests().await.unwrap_or_default();
    assert_eq!(
        requests_one.len(),
        0,
        "expected the delayed sync to skip the stale config/auth snapshot"
    );
    assert_eq!(
        requests_two.len(),
        1,
        "expected the delayed sync to use the latest config/auth snapshot"
    );
}

#[tokio::test]
async fn startup_remote_plugin_sync_rearms_after_curated_repo_completion_signal_uses_latest_config_and_auth_snapshot()
 {
    let tmp = tempdir().expect("tempdir");
    let curated_root = curated_plugins_repo_path(tmp.path());
    write_file(
        &tmp.path().join(CONFIG_TOML_FILE),
        r#"[features]
plugins = true

[plugins."linear@openai-curated"]
enabled = false
"#,
    );

    let make_auth_manager = |codex_home: &Path, access_token: &str, account_id: &str| {
        let auth = AuthDotJson {
            auth_mode: Some(AuthMode::ChatgptAuthTokens),
            openai_api_key: None,
            tokens: Some(TokenData {
                id_token: IdTokenInfo {
                    raw_jwt: "e30.e30.e30".to_string(),
                    ..Default::default()
                },
                access_token: access_token.to_string(),
                refresh_token: "refresh-token".to_string(),
                account_id: Some(account_id.to_string()),
            }),
            last_refresh: Some(Utc::now()),
        };
        crate::auth::save_auth(codex_home, &auth, AuthCredentialsStoreMode::File)
            .expect("save auth");
        AuthManager::shared(
            codex_home.to_path_buf(),
            /*enable_codex_api_key_env*/ false,
            AuthCredentialsStoreMode::File,
        )
    };

    let auth_home_one = tempdir().expect("auth tempdir one");
    let auth_home_two = tempdir().expect("auth tempdir two");
    let auth_manager_one =
        make_auth_manager(auth_home_one.path(), "Access Token One", "account-one");
    let auth_manager_two =
        make_auth_manager(auth_home_two.path(), "Access Token Two", "account-two");

    let server_one = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/plugins/list"))
        .and(header("authorization", "Bearer Access Token One"))
        .and(header("chatgpt-account-id", "account-one"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"[
  {"id":"1","name":"linear","marketplace_name":"openai-curated","version":"1.0.0","enabled":true}
]"#,
        ))
        .mount(&server_one)
        .await;

    let server_two = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/plugins/list"))
        .and(header("authorization", "Bearer Access Token Two"))
        .and(header("chatgpt-account-id", "account-two"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"[
  {"id":"1","name":"linear","marketplace_name":"openai-curated","version":"1.0.0","enabled":true}
]"#,
        ))
        .mount(&server_two)
        .await;

    let mut config_one = crate::plugins::test_support::load_plugins_config(tmp.path()).await;
    config_one.chatgpt_base_url = format!("{}/backend-api/", server_one.uri());
    let mut config_two = crate::plugins::test_support::load_plugins_config(tmp.path()).await;
    config_two.chatgpt_base_url = format!("{}/backend-api/", server_two.uri());
    let manager = Arc::new(PluginsManager::new(tmp.path().to_path_buf()));

    start_startup_remote_plugin_sync_once(
        Arc::clone(&manager),
        tmp.path().to_path_buf(),
        config_one,
        auth_manager_one,
    );

    tokio::time::sleep(
        STARTUP_REMOTE_PLUGIN_SYNC_PREREQUISITE_TIMEOUT + Duration::from_millis(250),
    )
    .await;

    let marker_path = tmp.path().join(STARTUP_REMOTE_PLUGIN_SYNC_MARKER_FILE);
    assert!(!marker_path.is_file());
    assert!(
        server_one
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
    );

    start_startup_remote_plugin_sync_once(
        Arc::clone(&manager),
        tmp.path().to_path_buf(),
        config_two,
        auth_manager_two,
    );

    tokio::time::sleep(Duration::from_millis(100)).await;
    write_openai_curated_marketplace(&curated_root, &["linear"]);
    write_curated_plugin_sha(tmp.path());

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!marker_path.is_file());
    assert!(
        server_one
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
    );
    assert!(
        server_two
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
    );

    let plugin_id = PluginId::new(
        "linear".to_string(),
        crate::plugins::OPENAI_CURATED_MARKETPLACE_NAME.to_string(),
    )
    .expect("plugin id should parse");
    PluginsManager::complete_curated_repo_sync_postprocessing(
        manager.as_ref(),
        tmp.path(),
        TEST_CURATED_PLUGIN_SHA,
        &[plugin_id],
    )
    .expect("curated repo postprocessing should succeed");

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if marker_path.is_file() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("marker should be written after the later retry sees prerequisites");

    assert!(
        tmp.path()
            .join(format!(
                "plugins/cache/openai-curated/linear/{TEST_CURATED_PLUGIN_SHA}"
            ))
            .is_dir()
    );
    let config =
        std::fs::read_to_string(tmp.path().join(CONFIG_TOML_FILE)).expect("config should exist");
    assert!(config.contains(r#"[plugins."linear@openai-curated"]"#));
    assert!(config.contains("enabled = true"));

    let requests_one = server_one.received_requests().await.unwrap_or_default();
    let requests_two = server_two.received_requests().await.unwrap_or_default();
    assert_eq!(
        requests_one.len(),
        0,
        "expected the timed-out first attempt to stay inactive"
    );
    assert_eq!(
        requests_two.len(),
        1,
        "expected the later retry to use the refreshed snapshot"
    );
}

fn curated_repo_zipball_bytes(sha: &str) -> Vec<u8> {
    let cursor = std::io::Cursor::new(Vec::new());
    let mut writer = ZipWriter::new(cursor);
    let options = SimpleFileOptions::default();
    let root = format!("openai-plugins-{sha}");
    writer
        .start_file(format!("{root}/.agents/plugins/marketplace.json"), options)
        .expect("start marketplace entry");
    writer
        .write_all(
            br#"{
  "name": "openai-curated",
  "plugins": [
    {
      "name": "gmail",
      "source": {
        "source": "local",
        "path": "./plugins/gmail"
      }
    }
  ]
}"#,
        )
        .expect("write marketplace");
    writer
        .start_file(
            format!("{root}/plugins/gmail/.codex-plugin/plugin.json"),
            options,
        )
        .expect("start plugin manifest entry");
    writer
        .write_all(br#"{"name":"gmail"}"#)
        .expect("write plugin manifest");

    writer.finish().expect("finish zip writer").into_inner()
}
