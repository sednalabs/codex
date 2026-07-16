use super::*;
use crate::ClaSource;
use crate::CurSource;
use crate::migration_source::ExternalAgentSource;
use pretty_assertions::assert_eq;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use tempfile::TempDir;

fn service_for_cursor_paths(
    external_agent_home: PathBuf,
    codex_home: PathBuf,
) -> ExternalAgentConfigService {
    let mut service = service_for_paths(external_agent_home, codex_home);
    service.source = ExternalAgentSource::Cur;
    service
}

enum CursorRepoSourceFixture {
    File(&'static str),
    Directory,
}

fn cursor_repo_source_cases() -> Vec<(
    PathBuf,
    ExternalAgentConfigMigrationItemType,
    CursorRepoSourceFixture,
)> {
    vec![
        (
            PathBuf::from(CurSource::CONFIG_DIR).join(CurSource::PROJECT_CONFIG_FILE),
            ExternalAgentConfigMigrationItemType::Config,
            CursorRepoSourceFixture::File(r#"{"env":{"FOO":"bar"}}"#),
        ),
        (
            PathBuf::from(CurSource::CONFIG_DIR).join(CurSource::SANDBOX_CONFIG_FILE),
            ExternalAgentConfigMigrationItemType::Config,
            CursorRepoSourceFixture::File(r#"{"type":"read_only"}"#),
        ),
        (
            PathBuf::from(CurSource::CONFIG_DIR).join(CurSource::MCP_CONFIG_FILE),
            ExternalAgentConfigMigrationItemType::McpServerConfig,
            CursorRepoSourceFixture::File(
                r#"{"mcpServers":{"outside":{"command":"outside"}}}"#,
            ),
        ),
        (
            PathBuf::from(CurSource::CONFIG_DIR).join(CurSource::HOOKS_CONFIG_FILE),
            ExternalAgentConfigMigrationItemType::Hooks,
            CursorRepoSourceFixture::File(
                r#"{"hooks":{"stop":[{"command":"echo outside"}]}}"#,
            ),
        ),
        (
            PathBuf::from(CurSource::CONFIG_DIR).join(CurSource::HOOKS_DIR),
            ExternalAgentConfigMigrationItemType::Hooks,
            CursorRepoSourceFixture::Directory,
        ),
        (
            PathBuf::from(CurSource::LEGACY_RULES_FILE),
            ExternalAgentConfigMigrationItemType::AgentsMd,
            CursorRepoSourceFixture::File("outside instructions"),
        ),
    ]
}

fn assert_single_symlink_import_error(
    outcome: &ExternalAgentConfigImportOutcome,
    item_type: ExternalAgentConfigMigrationItemType,
) {
    assert_eq!(outcome.item_results.len(), 1);
    let result = &outcome.item_results[0];
    assert_eq!(result.item_type, item_type);
    assert_eq!(result.success_count, 0);
    assert_eq!(result.error_count, 1);
    assert_eq!(result.raw_errors.len(), 1);
    assert!(result.raw_errors[0].message.contains("symlink"));
}

fn assert_is_symlink(path: &Path) {
    assert!(
        fs::symlink_metadata(path)
            .expect("read symlink metadata")
            .file_type()
            .is_symlink()
    );
}

#[tokio::test]
async fn detect_repo_canonicalizes_symlinked_nested_cwd_before_containment_checks() {
    let root = TempDir::new().expect("create tempdir");
    let repo_root = root.path().join("repo");
    let nested = repo_root.join("nested");
    let linked_cwd = root.path().join("linked-cwd");
    let external_settings = root.path().join("external-settings.json");
    fs::create_dir_all(repo_root.join(".git")).expect("create git dir");
    fs::create_dir_all(repo_root.join(ClaSource::CONFIG_DIR))
        .expect("create source config dir");
    fs::create_dir_all(&nested).expect("create nested dir");
    fs::write(&external_settings, r#"{"sandbox":{"enabled":true}}"#)
        .expect("write external settings");
    let local_settings = repo_root
        .join(ClaSource::CONFIG_DIR)
        .join(ClaSource::LOCAL_SETTINGS_FILE);
    std::os::unix::fs::symlink(&external_settings, &local_settings)
        .expect("create settings symlink");
    std::os::unix::fs::symlink(&nested, &linked_cwd).expect("create cwd symlink");

    let error = service_for_paths(
        root.path().join(ClaSource::CONFIG_DIR),
        root.path().join(".codex"),
    )
    .detect(ExternalAgentConfigDetectOptions {
        include_home: false,
        include_memory: false,
        cwds: Some(vec![linked_cwd]),
    })
    .await
    .expect_err("reject repository symlink after canonical cwd selection");

    assert!(error.to_string().contains("symlink"));
    assert_is_symlink(&local_settings);
}

#[tokio::test]
async fn detect_repo_rejects_symlinked_local_settings() {
    let root = TempDir::new().expect("create tempdir");
    let repo_root = root.path().join("repo");
    let external_settings = root.path().join("external-settings.json");
    fs::create_dir_all(repo_root.join(".git")).expect("create git dir");
    fs::create_dir_all(repo_root.join(ClaSource::CONFIG_DIR))
        .expect("create source config dir");
    fs::write(&external_settings, r#"{"sandbox":{"enabled":true}}"#)
        .expect("write external settings");
    let local_settings = repo_root
        .join(ClaSource::CONFIG_DIR)
        .join(ClaSource::LOCAL_SETTINGS_FILE);
    std::os::unix::fs::symlink(&external_settings, &local_settings)
        .expect("create settings symlink");

    let error = service_for_paths(
        root.path().join(ClaSource::CONFIG_DIR),
        root.path().join(".codex"),
    )
    .detect(ExternalAgentConfigDetectOptions {
        include_home: false,
        include_memory: false,
        cwds: Some(vec![repo_root]),
    })
    .await
    .expect_err("reject symlinked local settings");

    assert!(error.to_string().contains("symlink"));
    assert_is_symlink(&local_settings);
}

#[tokio::test]
async fn detect_repo_rejects_symlinked_mcp_source() {
    let root = TempDir::new().expect("create tempdir");
    let repo_root = root.path().join("repo");
    let external_mcp = root.path().join("external-mcp.json");
    fs::create_dir_all(repo_root.join(".git")).expect("create git dir");
    fs::write(
        &external_mcp,
        r#"{"mcpServers":{"outside":{"command":"outside"}}}"#,
    )
    .expect("write external MCP config");
    let mcp_source = repo_root.join(ClaSource::MCP_CONFIG_FILE);
    std::os::unix::fs::symlink(&external_mcp, &mcp_source).expect("create MCP symlink");

    let error = service_for_paths(
        root.path().join(ClaSource::CONFIG_DIR),
        root.path().join(".codex"),
    )
    .detect(ExternalAgentConfigDetectOptions {
        include_home: false,
        include_memory: false,
        cwds: Some(vec![repo_root]),
    })
    .await
    .expect_err("reject symlinked MCP source");

    assert!(error.to_string().contains("symlink"));
    assert_is_symlink(&mcp_source);
}

#[tokio::test]
async fn detect_cursor_repo_rejects_symlinked_source_files() {
    for (relative_source, _, source_fixture) in cursor_repo_source_cases() {
        let root = TempDir::new().expect("create tempdir");
        let repo_root = root.path().join("repo");
        let external_source = root.path().join("external-source");
        let source = repo_root.join(relative_source);
        fs::create_dir_all(repo_root.join(".git")).expect("create git dir");
        fs::create_dir_all(source.parent().expect("source parent"))
            .expect("create source parent");
        match source_fixture {
            CursorRepoSourceFixture::File(contents) => {
                fs::write(&external_source, contents).expect("write external source");
            }
            CursorRepoSourceFixture::Directory => {
                fs::create_dir_all(&external_source).expect("create external source directory");
            }
        }
        std::os::unix::fs::symlink(&external_source, &source).expect("create source symlink");

        let error = service_for_cursor_paths(
            root.path().join(CurSource::CONFIG_DIR),
            root.path().join(".codex"),
        )
        .detect(ExternalAgentConfigDetectOptions {
            include_home: false,
            include_memory: false,
            cwds: Some(vec![repo_root]),
        })
        .await
        .expect_err("reject symlinked Cursor repository source");

        assert!(error.to_string().contains("symlink"));
        assert_is_symlink(&source);
    }
}

#[tokio::test]
async fn import_repo_config_rejects_symlinked_local_settings() {
    let root = TempDir::new().expect("create tempdir");
    let repo_root = root.path().join("repo");
    let external_settings = root.path().join("external-settings.json");
    fs::create_dir_all(repo_root.join(".git")).expect("create git dir");
    fs::create_dir_all(repo_root.join(ClaSource::CONFIG_DIR))
        .expect("create source config dir");
    fs::write(&external_settings, r#"{"sandbox":{"enabled":true}}"#)
        .expect("write external settings");
    let local_settings = repo_root
        .join(ClaSource::CONFIG_DIR)
        .join(ClaSource::LOCAL_SETTINGS_FILE);
    std::os::unix::fs::symlink(&external_settings, &local_settings)
        .expect("create settings symlink");

    let outcome = service_for_paths(
        root.path().join(ClaSource::CONFIG_DIR),
        root.path().join(".codex"),
    )
    .import(vec![ExternalAgentConfigMigrationItem {
        item_type: ExternalAgentConfigMigrationItemType::Config,
        description: String::new(),
        cwd: Some(repo_root.clone()),
        details: None,
    }])
    .await;

    assert_single_symlink_import_error(&outcome, ExternalAgentConfigMigrationItemType::Config);
    assert_is_symlink(&local_settings);
    assert!(!repo_root.join(".codex").exists());
}

#[tokio::test]
async fn import_repo_mcp_rejects_symlinked_source_files() {
    for source_name in [
        ClaSource::MCP_CONFIG_FILE,
        ClaSource::PROJECT_CONFIG_FILE,
    ] {
        let root = TempDir::new().expect("create tempdir");
        let repo_root = root.path().join("repo");
        let external_mcp = root.path().join("external-mcp.json");
        fs::create_dir_all(repo_root.join(".git")).expect("create git dir");
        fs::write(
            &external_mcp,
            r#"{"mcpServers":{"outside":{"command":"outside"}}}"#,
        )
        .expect("write external MCP config");
        let mcp_source = repo_root.join(source_name);
        std::os::unix::fs::symlink(&external_mcp, &mcp_source).expect("create MCP symlink");

        let outcome = service_for_paths(
            root.path().join(ClaSource::CONFIG_DIR),
            root.path().join(".codex"),
        )
        .import(vec![ExternalAgentConfigMigrationItem {
            item_type: ExternalAgentConfigMigrationItemType::McpServerConfig,
            description: String::new(),
            cwd: Some(repo_root.clone()),
            details: None,
        }])
        .await;

        assert_single_symlink_import_error(
            &outcome,
            ExternalAgentConfigMigrationItemType::McpServerConfig,
        );
        assert_is_symlink(&mcp_source);
        assert!(!repo_root.join(".codex").exists());
    }
}

#[tokio::test]
async fn import_cursor_repo_rejects_symlinked_source_files() {
    for (relative_source, item_type, source_fixture) in cursor_repo_source_cases() {
        let root = TempDir::new().expect("create tempdir");
        let repo_root = root.path().join("repo");
        let external_source = root.path().join("external-source");
        let source = repo_root.join(relative_source);
        fs::create_dir_all(repo_root.join(".git")).expect("create git dir");
        fs::create_dir_all(source.parent().expect("source parent"))
            .expect("create source parent");
        match source_fixture {
            CursorRepoSourceFixture::File(contents) => {
                fs::write(&external_source, contents).expect("write external source");
            }
            CursorRepoSourceFixture::Directory => {
                fs::create_dir_all(&external_source).expect("create external source directory");
            }
        }
        std::os::unix::fs::symlink(&external_source, &source).expect("create source symlink");

        let outcome = service_for_cursor_paths(
            root.path().join(CurSource::CONFIG_DIR),
            root.path().join(".codex"),
        )
        .import(vec![ExternalAgentConfigMigrationItem {
            item_type,
            description: String::new(),
            cwd: Some(repo_root.clone()),
            details: None,
        }])
        .await;

        assert_single_symlink_import_error(&outcome, item_type);
        assert_is_symlink(&source);
        assert!(!repo_root.join(".codex").exists());
    }
}

#[tokio::test]
async fn import_repo_hooks_rejects_symlinked_local_settings() {
    let root = TempDir::new().expect("create tempdir");
    let repo_root = root.path().join("repo");
    let external_settings = root.path().join("external-settings.json");
    fs::create_dir_all(repo_root.join(".git")).expect("create git dir");
    fs::create_dir_all(repo_root.join(ClaSource::CONFIG_DIR))
        .expect("create source config dir");
    fs::write(
        &external_settings,
        r#"{"hooks":{"Stop":[{"hooks":[{"command":"echo outside"}]}]}}"#,
    )
    .expect("write external settings");
    let local_settings = repo_root
        .join(ClaSource::CONFIG_DIR)
        .join(ClaSource::LOCAL_SETTINGS_FILE);
    std::os::unix::fs::symlink(&external_settings, &local_settings)
        .expect("create settings symlink");

    let outcome = service_for_paths(
        root.path().join(ClaSource::CONFIG_DIR),
        root.path().join(".codex"),
    )
    .import(vec![ExternalAgentConfigMigrationItem {
        item_type: ExternalAgentConfigMigrationItemType::Hooks,
        description: String::new(),
        cwd: Some(repo_root.clone()),
        details: None,
    }])
    .await;

    assert_single_symlink_import_error(&outcome, ExternalAgentConfigMigrationItemType::Hooks);
    assert_is_symlink(&local_settings);
    assert!(!repo_root.join(".codex").exists());
}

#[tokio::test]
async fn import_repo_hooks_rejects_symlinked_script_root() {
    let root = TempDir::new().expect("create tempdir");
    let repo_root = root.path().join("repo");
    let external_hooks = root.path().join("external-hooks");
    fs::create_dir_all(repo_root.join(".git")).expect("create git dir");
    fs::create_dir_all(repo_root.join(ClaSource::CONFIG_DIR))
        .expect("create source config dir");
    fs::create_dir_all(&external_hooks).expect("create external hooks");
    fs::write(external_hooks.join("outside.py"), "print('outside')")
        .expect("write external hook");
    fs::write(
        repo_root
            .join(ClaSource::CONFIG_DIR)
            .join(ClaSource::SETTINGS_FILE),
        r#"{"hooks":{"Stop":[{"hooks":[{"command":"python .claude/hooks/outside.py"}]}]}}"#,
    )
    .expect("write source hooks");
    let hooks_source = repo_root
        .join(ClaSource::CONFIG_DIR)
        .join(ClaSource::HOOKS_DIR);
    std::os::unix::fs::symlink(&external_hooks, &hooks_source)
        .expect("create hooks symlink");

    let outcome = service_for_paths(
        root.path().join(ClaSource::CONFIG_DIR),
        root.path().join(".codex"),
    )
    .import(vec![ExternalAgentConfigMigrationItem {
        item_type: ExternalAgentConfigMigrationItemType::Hooks,
        description: String::new(),
        cwd: Some(repo_root.clone()),
        details: None,
    }])
    .await;

    assert_single_symlink_import_error(&outcome, ExternalAgentConfigMigrationItemType::Hooks);
    assert_is_symlink(&hooks_source);
    assert!(!repo_root.join(".codex").exists());
}

#[tokio::test]
async fn import_repo_skills_rejects_symlinked_source_root() {
    let root = TempDir::new().expect("create tempdir");
    let repo_root = root.path().join("repo");
    let external_source = root.path().join("external-source");
    fs::create_dir_all(repo_root.join(".git")).expect("create git dir");
    fs::create_dir_all(repo_root.join(ClaSource::CONFIG_DIR))
        .expect("create source config dir");
    fs::create_dir_all(external_source.join("outside-skill")).expect("create external skill");
    fs::write(
        external_source.join("outside-skill").join("SKILL.md"),
        "external skill",
    )
    .expect("write external skill");
    std::os::unix::fs::symlink(
        &external_source,
        repo_root.join(ClaSource::CONFIG_DIR).join("skills"),
    )
    .expect("create source symlink");

    let outcome = service_for_paths(
        root.path().join(ClaSource::CONFIG_DIR),
        root.path().join(".codex"),
    )
    .import(vec![ExternalAgentConfigMigrationItem {
        item_type: ExternalAgentConfigMigrationItemType::Skills,
        description: String::new(),
        cwd: Some(repo_root.clone()),
        details: None,
    }])
    .await;

    assert_single_symlink_import_error(&outcome, ExternalAgentConfigMigrationItemType::Skills);
    assert_is_symlink(&repo_root.join(ClaSource::CONFIG_DIR).join("skills"));
    assert!(!repo_root.join(".agents").exists());
}

#[tokio::test]
async fn import_repo_skills_rejects_symlinked_destination_root() {
    let root = TempDir::new().expect("create tempdir");
    let repo_root = root.path().join("repo");
    let external_target = root.path().join("external-target");
    fs::create_dir_all(repo_root.join(".git")).expect("create git dir");
    fs::create_dir_all(
        repo_root
            .join(ClaSource::CONFIG_DIR)
            .join("skills")
            .join("repo-skill"),
    )
    .expect("create source skill");
    fs::write(
        repo_root
            .join(ClaSource::CONFIG_DIR)
            .join("skills")
            .join("repo-skill")
            .join("SKILL.md"),
        "repository skill",
    )
    .expect("write source skill");
    fs::create_dir_all(&external_target).expect("create external target");
    std::os::unix::fs::symlink(&external_target, repo_root.join(".agents"))
        .expect("create destination symlink");

    let outcome = service_for_paths(
        root.path().join(ClaSource::CONFIG_DIR),
        root.path().join(".codex"),
    )
    .import(vec![ExternalAgentConfigMigrationItem {
        item_type: ExternalAgentConfigMigrationItemType::Skills,
        description: String::new(),
        cwd: Some(repo_root.clone()),
        details: None,
    }])
    .await;

    assert_single_symlink_import_error(&outcome, ExternalAgentConfigMigrationItemType::Skills);
    assert_is_symlink(&repo_root.join(".agents"));
    assert_eq!(
        fs::read_dir(external_target).expect("read target").count(),
        0
    );
}

#[tokio::test]
async fn import_repo_hooks_rejects_symlinked_target_file() {
    let root = TempDir::new().expect("create tempdir");
    let repo_root = root.path().join("repo");
    let linked_target = root.path().join("linked-hooks.json");
    fs::create_dir_all(repo_root.join(".git")).expect("create git dir");
    fs::create_dir_all(repo_root.join(ClaSource::CONFIG_DIR))
        .expect("create source config dir");
    fs::create_dir_all(repo_root.join(".codex")).expect("create target config dir");
    fs::write(
        repo_root
            .join(ClaSource::CONFIG_DIR)
            .join(ClaSource::SETTINGS_FILE),
        r#"{"hooks":{"Stop":[{"hooks":[{"command":"echo done"}]}]}}"#,
    )
    .expect("write source hooks");
    fs::write(&linked_target, "").expect("write linked target");
    std::os::unix::fs::symlink(&linked_target, repo_root.join(".codex").join("hooks.json"))
        .expect("create target symlink");

    let outcome = service_for_paths(
        root.path().join(ClaSource::CONFIG_DIR),
        root.path().join(".codex"),
    )
    .import(vec![ExternalAgentConfigMigrationItem {
        item_type: ExternalAgentConfigMigrationItemType::Hooks,
        description: String::new(),
        cwd: Some(repo_root.clone()),
        details: None,
    }])
    .await;

    assert_single_symlink_import_error(&outcome, ExternalAgentConfigMigrationItemType::Hooks);
    assert_eq!(
        fs::read_to_string(linked_target).expect("read linked target"),
        ""
    );
    assert_is_symlink(&repo_root.join(".codex").join("hooks.json"));
}

#[tokio::test]
async fn import_repo_hooks_rejects_symlinked_target_parent() {
    let root = TempDir::new().expect("create tempdir");
    let repo_root = root.path().join("repo");
    let external_target = root.path().join("external-target");
    fs::create_dir_all(repo_root.join(".git")).expect("create git dir");
    fs::create_dir_all(repo_root.join(ClaSource::CONFIG_DIR))
        .expect("create source config dir");
    fs::create_dir_all(&external_target).expect("create external target");
    fs::write(
        repo_root
            .join(ClaSource::CONFIG_DIR)
            .join(ClaSource::SETTINGS_FILE),
        r#"{"hooks":{"Stop":[{"hooks":[{"command":"echo done"}]}]}}"#,
    )
    .expect("write source hooks");
    std::os::unix::fs::symlink(&external_target, repo_root.join(".codex"))
        .expect("create target parent symlink");

    let outcome = service_for_paths(
        root.path().join(ClaSource::CONFIG_DIR),
        root.path().join(".codex-home"),
    )
    .import(vec![ExternalAgentConfigMigrationItem {
        item_type: ExternalAgentConfigMigrationItemType::Hooks,
        description: String::new(),
        cwd: Some(repo_root.clone()),
        details: None,
    }])
    .await;

    assert_single_symlink_import_error(&outcome, ExternalAgentConfigMigrationItemType::Hooks);
    assert_is_symlink(&repo_root.join(".codex"));
    assert_eq!(
        fs::read_dir(external_target).expect("read target").count(),
        0
    );
}
