use super::*;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

fn write_project_session(project_root: &Path, project_cwd: &Path) {
    fs::create_dir_all(project_cwd).expect("create project cwd");
    fs::write(
        project_root.join("session.jsonl"),
        serde_json::json!({
            "type": "user",
            "cwd": project_cwd,
            "timestamp": "2026-07-13T00:00:00Z",
            "message": { "content": "remember this" },
        })
        .to_string(),
    )
    .expect("write project session");
}

#[test]
fn copies_only_selected_projects_and_recopies_changed_content() {
    let root = TempDir::new().expect("create tempdir");
    let codex_home = root.path().join(".codex");
    let source_home = root.path().join(".external-agent");
    let project_a_memory = source_home.join("projects/project-a/memory");
    let project_b_memory = source_home.join("projects/project-b/memory");
    fs::create_dir_all(&project_a_memory).expect("create project A memory");
    fs::create_dir_all(&project_b_memory).expect("create project B memory");
    write_project_session(
        project_a_memory.parent().expect("project A root"),
        &root.path().join("project-a-cwd"),
    );
    write_project_session(
        project_b_memory.parent().expect("project B root"),
        &root.path().join("project-b-cwd"),
    );
    let project_a_source = project_a_memory.join("MEMORY.md");
    fs::write(&project_a_source, b"project A memory").expect("write project A memory");
    let project_a_topic = project_a_memory.join("release-process.md");
    fs::write(&project_a_topic, b"project A release process").expect("write project A topic");
    fs::write(project_b_memory.join("MEMORY.md"), b"project B memory")
        .expect("write project B memory");

    let all_files = discover_external_memory_files(&source_home).expect("discover memories");
    assert_eq!(
        projects_needing_import(&codex_home, &all_files).expect("detect new memories"),
        BTreeSet::from(["project-a".to_string(), "project-b".to_string()])
    );
    let selected_memory = BTreeSet::from(["project-a"]);

    let outcome =
        copy_resources(&codex_home, &all_files, &selected_memory).expect("copy project A");
    assert_eq!(outcome.synchronized_projects, vec!["project-a"]);
    assert_eq!(outcome.failures, Vec::new());
    assert_eq!(
        fs::read(resources_root(&codex_home).join("project-a/MEMORY.md"))
            .expect("read project A memory"),
        b"project A memory".to_vec()
    );
    assert!(!resources_root(&codex_home).join("project-b").exists());
    assert_eq!(
        projects_needing_import(&codex_home, &all_files).expect("detect exact imported content"),
        BTreeSet::from(["project-b".to_string()])
    );

    fs::remove_file(&project_a_topic).expect("remove project A topic");
    let updated_files = discover_external_memory_files(&source_home).expect("rediscover memories");
    assert_eq!(
        projects_needing_import(&codex_home, &updated_files).expect("detect project file changes"),
        BTreeSet::from(["project-a".to_string(), "project-b".to_string()])
    );
    fs::write(project_a_memory.join("updated.md"), b"updated memory")
        .expect("write updated project A topic");
    let updated_files = discover_external_memory_files(&source_home).expect("rediscover memories");
    copy_resources(&codex_home, &updated_files, &selected_memory)
        .expect("replace project A resources");
    assert!(
        !resources_root(&codex_home)
            .join("project-a/release-process.md")
            .exists()
    );
    assert_eq!(
        fs::read(resources_root(&codex_home).join("project-a/updated.md"))
            .expect("read updated project A topic"),
        b"updated memory".to_vec()
    );

    fs::write(&project_a_source, b"project A changed").expect("change project A memory");
    let changed_files = discover_external_memory_files(&source_home).expect("rediscover memories");
    assert_eq!(
        projects_needing_import(&codex_home, &changed_files).expect("detect changed memory"),
        BTreeSet::from(["project-a".to_string(), "project-b".to_string()])
    );
    let outcome =
        copy_resources(&codex_home, &changed_files, &selected_memory).expect("recopy project A");
    assert_eq!(outcome.synchronized_projects, vec!["project-a"]);
    assert_eq!(outcome.failures, Vec::new());
    assert_eq!(
        fs::read(resources_root(&codex_home).join("project-a/MEMORY.md"))
            .expect("read changed project A memory"),
        b"project A changed".to_vec()
    );
}

#[test]
fn preserves_project_successes_and_reports_each_failed_selection() {
    let root = TempDir::new().expect("create tempdir");
    let codex_home = root.path().join(".codex");
    let source_home = root.path().join(".external-agent");
    let project_a_memory = source_home.join("projects/project-a/memory");
    let project_b_memory = source_home.join("projects/project-b/memory");
    fs::create_dir_all(&project_a_memory).expect("create project A memory");
    fs::create_dir_all(&project_b_memory).expect("create project B memory");
    write_project_session(
        project_a_memory.parent().expect("project A root"),
        &root.path().join("project-a-cwd"),
    );
    write_project_session(
        project_b_memory.parent().expect("project B root"),
        &root.path().join("project-b-cwd"),
    );
    fs::write(project_a_memory.join("MEMORY.md"), b"project A memory")
        .expect("write project A memory");
    let project_b_source = project_b_memory.join("MEMORY.md");
    fs::write(&project_b_source, b"project B memory").expect("write project B memory");

    let memory_files = discover_external_memory_files(&source_home).expect("discover memories");
    fs::remove_file(project_b_source).expect("remove project B source after discovery");
    let selected_memory = BTreeSet::from(["missing-project", "project-a", "project-b"]);
    let outcome = copy_resources(&codex_home, &memory_files, &selected_memory)
        .expect("copy selected memories");

    assert_eq!(outcome.synchronized_projects, vec!["project-a"]);
    assert_eq!(
        outcome
            .failures
            .iter()
            .map(|failure| failure.project_key.as_str())
            .collect::<Vec<_>>(),
        vec!["missing-project", "project-b"]
    );
    assert_eq!(
        outcome.failures[0].message,
        "selected memory was not found: missing-project"
    );
    assert!(
        outcome.failures[1]
            .message
            .starts_with("failed to synchronize selected memory project-b:")
    );
    assert!(
        resources_root(&codex_home)
            .join("project-a/MEMORY.md")
            .exists()
    );
    assert!(!resources_root(&codex_home).join("project-b").exists());
}

#[test]
fn rejects_memory_project_keys_that_are_not_single_components() {
    let root = TempDir::new().expect("create tempdir");
    let codex_home = root.path().join(".codex");
    let outside = root.path().join("outside");
    fs::create_dir_all(&outside).expect("create outside directory");
    fs::write(outside.join("sentinel"), b"outside").expect("write outside sentinel");
    let absolute_key = outside.to_string_lossy().into_owned();

    for project_key in [absolute_key.as_str(), "../outside", "nested/project"] {
        let selected_memory = BTreeSet::from([project_key]);
        let error = copy_resources(&codex_home, &[], &selected_memory)
            .expect_err("reject unsafe memory project key");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains(project_key));
    }

    assert_eq!(
        fs::read(outside.join("sentinel")).expect("read outside sentinel"),
        b"outside".to_vec()
    );
}

#[cfg(unix)]
#[test]
fn rejects_symlinked_memory_destination_ancestors() {
    let root = TempDir::new().expect("create tempdir");

    for (case_name, relative_target) in [
        ("memory-root", Path::new("memories")),
        (
            "resources-root",
            Path::new("memories/extensions/external_agent_import/resources"),
        ),
    ] {
        let case_root = root.path().join(case_name);
        let codex_home = case_root.join(".codex");
        let target = codex_home.join(relative_target);
        let outside = case_root.join("outside");
        fs::create_dir_all(target.parent().expect("target parent")).expect("create target parent");
        fs::create_dir_all(&outside).expect("create outside directory");
        fs::write(outside.join("sentinel"), b"outside").expect("write outside sentinel");
        std::os::unix::fs::symlink(&outside, &target).expect("create destination symlink");

        let error = ensure_memory_destination_paths(&codex_home)
            .expect_err("reject symlinked memory destination ancestor");

        assert!(error.to_string().contains("symlink"));
        assert!(
            fs::symlink_metadata(&target)
                .expect("read destination symlink")
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read(outside.join("sentinel")).expect("read outside sentinel"),
            b"outside".to_vec()
        );
    }
}

#[cfg(unix)]
#[test]
fn projects_needing_import_rejects_symlinked_stale_memory_project() {
    let root = TempDir::new().expect("create tempdir");
    let codex_home = root.path().join(".codex");
    let source_home = root.path().join(".external-agent");
    let stale_project = resources_root(&codex_home).join("stale-project");
    let outside = root.path().join("outside");
    fs::create_dir_all(stale_project.parent().expect("stale project parent"))
        .expect("create resources root");
    fs::create_dir_all(&source_home).expect("create source home");
    fs::create_dir_all(&outside).expect("create outside directory");
    fs::write(outside.join("sentinel"), b"outside").expect("write outside sentinel");
    std::os::unix::fs::symlink(&outside, &stale_project).expect("create stale project symlink");

    let memory_files = discover_external_memory_files(&source_home).expect("discover memory files");
    let error = projects_needing_import(&codex_home, &memory_files)
        .expect_err("reject symlinked stale project during detection");

    assert!(error.to_string().contains("symlink"));
    assert_eq!(
        fs::read(outside.join("sentinel")).expect("read outside sentinel"),
        b"outside".to_vec()
    );
    assert!(
        fs::symlink_metadata(&stale_project)
            .expect("read stale project symlink")
            .file_type()
            .is_symlink()
    );
}

#[cfg(unix)]
#[test]
fn rejects_symlinked_memory_instructions_before_project_mutation() {
    let root = TempDir::new().expect("create tempdir");
    let codex_home = root.path().join(".codex");
    let instructions_path = extension_root(&codex_home).join("instructions.md");
    let outside = root.path().join("outside-instructions.md");
    fs::create_dir_all(instructions_path.parent().expect("instructions parent"))
        .expect("create instructions parent");
    fs::write(&outside, b"outside").expect("write outside instructions");
    std::os::unix::fs::symlink(&outside, &instructions_path).expect("create instructions symlink");
    let selected_memory = BTreeSet::from(["missing-project"]);

    let error = copy_resources(&codex_home, &[], &selected_memory)
        .expect_err("reject symlinked instructions");

    assert!(error.to_string().contains("symlink"));
    assert_eq!(
        fs::read(&outside).expect("read outside instructions"),
        b"outside".to_vec()
    );
    assert!(
        fs::symlink_metadata(&instructions_path)
            .expect("read instructions symlink")
            .file_type()
            .is_symlink()
    );
    assert!(!resources_root(&codex_home).join("missing-project").exists());
}

#[cfg(unix)]
#[test]
fn rejects_symlinked_memory_resource_and_scope_leaves() {
    let root = TempDir::new().expect("create tempdir");

    for relative_target in ["MEMORY.md", PROJECT_SCOPE_FILE] {
        let case_root = root.path().join(relative_target.replace('.', "-"));
        let codex_home = case_root.join(".codex");
        let source_home = case_root.join(".external-agent");
        let project_root = source_home.join("projects/project-a");
        let project_memory = project_root.join("memory");
        let target_root = resources_root(&codex_home).join("project-a");
        let target = target_root.join(relative_target);
        let outside = case_root.join("outside");
        fs::create_dir_all(&project_memory).expect("create project memory");
        write_project_session(&project_root, &case_root.join("project-cwd"));
        fs::write(project_memory.join("MEMORY.md"), b"project memory")
            .expect("write project memory");
        fs::create_dir_all(&target_root).expect("create target root");
        fs::write(&outside, b"outside").expect("write outside target");
        std::os::unix::fs::symlink(&outside, &target).expect("create target symlink");
        let memory_files =
            discover_external_memory_files(&source_home).expect("discover project memory files");
        let selected_memory = BTreeSet::from(["project-a"]);

        let error = copy_resources(&codex_home, &memory_files, &selected_memory)
            .expect_err("reject symlinked target leaf");

        assert!(error.to_string().contains("symlink"));
        assert_eq!(
            fs::read(&outside).expect("read outside target"),
            b"outside".to_vec()
        );
        assert!(
            fs::symlink_metadata(&target)
                .expect("read target symlink")
                .file_type()
                .is_symlink()
        );
    }
}

#[test]
fn removes_project_resources_when_the_source_project_disappears() {
    let root = TempDir::new().expect("create tempdir");
    let codex_home = root.path().join(".codex");
    let source_home = root.path().join(".external-agent");
    let project_root = source_home.join("projects/project-a");
    let project_memory = project_root.join("memory");
    fs::create_dir_all(&project_memory).expect("create project memory");
    write_project_session(&project_root, &root.path().join("project-a-cwd"));
    fs::write(project_memory.join("MEMORY.md"), b"project A memory").expect("write project memory");
    let selected_memory = BTreeSet::from(["project-a"]);
    let memory_files = discover_external_memory_files(&source_home).expect("discover memories");
    copy_resources(&codex_home, &memory_files, &selected_memory).expect("copy project");

    fs::remove_dir_all(project_root).expect("remove source project");
    let memory_files = discover_external_memory_files(&source_home).expect("rediscover memories");

    assert_eq!(
        projects_needing_import(&codex_home, &memory_files).expect("detect removed project"),
        BTreeSet::from(["project-a".to_string()])
    );
    assert_eq!(
        copy_resources(&codex_home, &memory_files, &selected_memory)
            .expect("remove imported project"),
        MemoryImportOutcome {
            synchronized_projects: vec!["project-a".to_string()],
            failures: Vec::new(),
            workspace_changed: true,
        }
    );
    assert!(!resources_root(&codex_home).join("project-a").exists());
}

#[test]
fn uses_scope_file_to_identify_owned_projects() {
    let root = TempDir::new().expect("create tempdir");
    let codex_home = root.path().join(".codex");
    let resources_root = resources_root(&codex_home);
    fs::create_dir_all(resources_root.join("project-a")).expect("create project resources");
    fs::write(resources_root.join("project-a/scope.json"), b"{}").expect("write project scope");
    fs::create_dir(resources_root.join(".project")).expect("create hidden project resources");
    fs::write(resources_root.join(".project/scope.json"), b"{}")
        .expect("write hidden project scope");
    fs::create_dir(resources_root.join("metadata")).expect("create metadata directory");
    fs::create_dir(resources_root.join(".metadata")).expect("create hidden metadata directory");
    fs::write(resources_root.join(".DS_Store"), b"metadata").expect("write metadata file");

    assert_eq!(
        projects_needing_import(&codex_home, &[]).expect("detect removed projects"),
        BTreeSet::from([".project".to_string(), "project-a".to_string()])
    );
}

#[cfg(unix)]
#[test]
fn projects_needing_import_rejects_symlinked_stale_project_scope() {
    let root = TempDir::new().expect("create tempdir");
    let codex_home = root.path().join(".codex");
    let stale_project = resources_root(&codex_home).join("stale-project");
    let scope_path = stale_project.join(PROJECT_SCOPE_FILE);
    let outside_scope = root.path().join("outside-scope.json");
    fs::create_dir_all(&stale_project).expect("create stale project directory");
    fs::write(&outside_scope, b"{}\n").expect("write outside scope");
    std::os::unix::fs::symlink(&outside_scope, &scope_path).expect("create stale scope symlink");

    let error = projects_needing_import(&codex_home, &[])
        .expect_err("reject symlinked stale project scope during detection");

    assert!(error.to_string().contains("symlink"));
    assert_eq!(
        fs::read(&outside_scope).expect("read outside scope"),
        b"{}\n".to_vec()
    );
    assert!(
        fs::symlink_metadata(&scope_path)
            .expect("read stale scope symlink")
            .file_type()
            .is_symlink()
    );
}

#[test]
fn project_rename_removes_the_old_target_and_imports_the_new_target() {
    let root = TempDir::new().expect("create tempdir");
    let codex_home = root.path().join(".codex");
    let source_home = root.path().join(".external-agent");
    let project_a_root = source_home.join("projects/project-a");
    let project_a_memory = project_a_root.join("memory");
    fs::create_dir_all(&project_a_memory).expect("create project memory");
    write_project_session(&project_a_root, &root.path().join("project-cwd"));
    fs::write(project_a_memory.join("MEMORY.md"), b"project memory").expect("write project memory");
    let project_a_selection = BTreeSet::from(["project-a"]);
    let memory_files = discover_external_memory_files(&source_home).expect("discover memories");
    copy_resources(&codex_home, &memory_files, &project_a_selection).expect("copy project");

    fs::rename(&project_a_root, source_home.join("projects/project-b"))
        .expect("rename source project");
    let memory_files = discover_external_memory_files(&source_home).expect("rediscover memories");
    let selected_memory = BTreeSet::from(["project-a", "project-b"]);

    assert_eq!(
        projects_needing_import(&codex_home, &memory_files).expect("detect renamed project"),
        BTreeSet::from(["project-a".to_string(), "project-b".to_string()])
    );
    assert_eq!(
        copy_resources(&codex_home, &memory_files, &selected_memory)
            .expect("synchronize renamed project"),
        MemoryImportOutcome {
            synchronized_projects: vec!["project-a".to_string(), "project-b".to_string()],
            failures: Vec::new(),
            workspace_changed: true,
        }
    );
    assert!(!resources_root(&codex_home).join("project-a").exists());
    assert!(
        resources_root(&codex_home)
            .join("project-b/scope.json")
            .is_file()
    );
}

#[test]
fn does_not_import_a_new_project_without_a_reliable_cwd() {
    let root = TempDir::new().expect("create tempdir");
    let codex_home = root.path().join(".codex");
    let source_home = root.path().join(".external-agent");
    let project_memory = source_home.join("projects/project-a/memory");
    fs::create_dir_all(&project_memory).expect("create project memory");
    fs::write(project_memory.join("MEMORY.md"), b"project memory").expect("write project memory");
    let memory_files = discover_external_memory_files(&source_home).expect("discover memories");
    let selected_memory = BTreeSet::from(["project-a"]);

    assert_eq!(
        projects_needing_import(&codex_home, &memory_files).expect("detect memories"),
        BTreeSet::new()
    );
    let outcome =
        copy_resources(&codex_home, &memory_files, &selected_memory).expect("attempt copy");
    assert_eq!(
        outcome,
        MemoryImportOutcome {
            synchronized_projects: Vec::new(),
            failures: vec![MemoryImportFailure {
                project_key: "project-a".to_string(),
                message: "failed to synchronize selected memory project-a: selected memory project has no reliable cwd: project-a".to_string(),
            }],
            workspace_changed: false,
        }
    );
    assert!(!resources_root(&codex_home).join("project-a").exists());
}

#[test]
fn missing_cwd_does_not_make_an_existing_scoped_project_look_deleted() {
    let root = TempDir::new().expect("create tempdir");
    let codex_home = root.path().join(".codex");
    let source_home = root.path().join(".external-agent");
    let project_root = source_home.join("projects/project-a");
    let project_memory = project_root.join("memory");
    fs::create_dir_all(&project_memory).expect("create project memory");
    write_project_session(&project_root, &root.path().join("project-a-cwd"));
    fs::write(project_memory.join("MEMORY.md"), b"project memory").expect("write project memory");
    let selected_memory = BTreeSet::from(["project-a"]);
    let memory_files = discover_external_memory_files(&source_home).expect("discover memories");
    copy_resources(&codex_home, &memory_files, &selected_memory).expect("copy project");

    fs::remove_file(project_root.join("session.jsonl")).expect("remove project session");
    let memory_files = discover_external_memory_files(&source_home).expect("rediscover memories");

    assert_eq!(
        projects_needing_import(&codex_home, &memory_files).expect("detect memories"),
        BTreeSet::new()
    );
    assert!(resources_root(&codex_home).join("project-a").is_dir());
}

#[test]
fn removes_an_existing_unscoped_target_when_cwd_is_unavailable() {
    let root = TempDir::new().expect("create tempdir");
    let codex_home = root.path().join(".codex");
    let source_home = root.path().join(".external-agent");
    let project_memory = source_home.join("projects/project-a/memory");
    fs::create_dir_all(&project_memory).expect("create project memory");
    fs::write(project_memory.join("MEMORY.md"), b"source memory").expect("write source memory");
    let target_root = resources_root(&codex_home).join("project-a");
    fs::create_dir_all(&target_root).expect("create unscoped target");
    fs::write(target_root.join("MEMORY.md"), b"imported memory").expect("write unscoped target");
    let memory_files = discover_external_memory_files(&source_home).expect("discover memories");
    let selected_memory = BTreeSet::from(["project-a"]);

    assert_eq!(
        projects_needing_import(&codex_home, &memory_files).expect("detect unscoped target"),
        BTreeSet::from(["project-a".to_string()])
    );
    assert_eq!(
        copy_resources(&codex_home, &memory_files, &selected_memory)
            .expect("remove unscoped target"),
        MemoryImportOutcome {
            synchronized_projects: vec!["project-a".to_string()],
            failures: Vec::new(),
            workspace_changed: true,
        }
    );
    assert!(!target_root.exists());
}
