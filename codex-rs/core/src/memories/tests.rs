use super::storage::rebuild_raw_memories_file_from_memories;
use super::storage::sync_rollout_summaries_from_memories;
use crate::config::types::DEFAULT_MEMORIES_MAX_RAW_MEMORIES_FOR_CONSOLIDATION;
use crate::memories::clear_memory_root_contents;
use crate::memories::ensure_layout;
use crate::memories::memory_root;
use crate::memories::raw_memories_file;
use crate::memories::rollout_summaries_dir;
use chrono::TimeZone;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_state::Stage1Output;
use pretty_assertions::assert_eq;
use serde_json::Value;
use std::path::PathBuf;
use tempfile::tempdir;

#[test]
fn memory_root_uses_shared_global_path() {
    let dir = tempdir().expect("tempdir");
    let codex_home = dir.path().join("codex");
    assert_eq!(memory_root(&codex_home), codex_home.join("memories"));
}

#[test]
fn stage_one_output_schema_requires_rollout_slug_and_keeps_it_nullable() {
    let schema = crate::memories::phase1::output_schema();
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .expect("properties object");
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .expect("required array");

    let mut required_keys = required
        .iter()
        .map(|key| key.as_str().expect("required key string"))
        .collect::<Vec<_>>();
    required_keys.sort_unstable();

    assert!(
        properties.contains_key("rollout_slug"),
        "schema should declare rollout_slug"
    );

    let rollout_slug_type = properties
        .get("rollout_slug")
        .and_then(Value::as_object)
        .and_then(|schema| schema.get("type"))
        .and_then(Value::as_array)
        .expect("rollout_slug type array");
    let mut rollout_slug_types = rollout_slug_type
        .iter()
        .map(|entry| entry.as_str().expect("type entry string"))
        .collect::<Vec<_>>();
    rollout_slug_types.sort_unstable();

    assert_eq!(
        required_keys,
        vec!["raw_memory", "rollout_slug", "rollout_summary"]
    );
    assert_eq!(rollout_slug_types, vec!["null", "string"]);
}

#[tokio::test]
async fn clear_memory_root_contents_preserves_root_directory() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path().join("memory");
    let nested_dir = root.join("rollout_summaries");
    tokio::fs::create_dir_all(&nested_dir)
        .await
        .expect("create rollout summaries dir");
    tokio::fs::write(root.join("MEMORY.md"), "stale memory index\n")
        .await
        .expect("write memory index");
    tokio::fs::write(nested_dir.join("rollout.md"), "stale rollout\n")
        .await
        .expect("write rollout summary");

    clear_memory_root_contents(&root)
        .await
        .expect("clear memory root contents");

    assert!(
        tokio::fs::try_exists(&root)
            .await
            .expect("check memory root existence"),
        "memory root should still exist after clearing contents"
    );
    let mut entries = tokio::fs::read_dir(&root)
        .await
        .expect("read memory root after clear");
    assert!(
        entries
            .next_entry()
            .await
            .expect("read next entry")
            .is_none(),
        "memory root should be empty after clearing contents"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn clear_memory_root_contents_rejects_symlinked_root() {
    let dir = tempdir().expect("tempdir");
    let target = dir.path().join("outside");
    tokio::fs::create_dir_all(&target)
        .await
        .expect("create symlink target dir");
    let target_file = target.join("keep.txt");
    tokio::fs::write(&target_file, "keep\n")
        .await
        .expect("write target file");

    let root = dir.path().join("memory");
    std::os::unix::fs::symlink(&target, &root).expect("create memory root symlink");

    let err = clear_memory_root_contents(&root)
        .await
        .expect_err("symlinked memory root should be rejected");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        tokio::fs::try_exists(&target_file)
            .await
            .expect("check target file existence"),
        "rejecting a symlinked memory root should not delete the symlink target"
    );
}

#[tokio::test]
async fn sync_rollout_summaries_and_raw_memories_file_keeps_latest_memories_only() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path().join("memory");
    ensure_layout(&root).await.expect("ensure layout");

    let keep_id = ThreadId::default().to_string();
    let drop_id = ThreadId::default().to_string();
    let keep_path = rollout_summaries_dir(&root).join(format!("{keep_id}.md"));
    let drop_path = rollout_summaries_dir(&root).join(format!("{drop_id}.md"));
    tokio::fs::write(&keep_path, "keep")
        .await
        .expect("write keep");
    tokio::fs::write(&drop_path, "drop")
        .await
        .expect("write drop");

    let memories = vec![Stage1Output {
        thread_id: ThreadId::try_from(keep_id.clone()).expect("thread id"),
        source_updated_at: Utc.timestamp_opt(100, 0).single().expect("timestamp"),
        raw_memory: "raw memory".to_string(),
        rollout_summary: "short summary".to_string(),
        rollout_slug: None,
        rollout_path: PathBuf::from("/tmp/rollout-100.jsonl"),
        cwd: PathBuf::from("/tmp/workspace"),
        git_branch: None,
        generated_at: Utc.timestamp_opt(101, 0).single().expect("timestamp"),
    }];

    sync_rollout_summaries_from_memories(
        &root,
        &memories,
        DEFAULT_MEMORIES_MAX_RAW_MEMORIES_FOR_CONSOLIDATION,
    )
    .await
    .expect("sync rollout summaries");
    rebuild_raw_memories_file_from_memories(
        &root,
        &memories,
        DEFAULT_MEMORIES_MAX_RAW_MEMORIES_FOR_CONSOLIDATION,
    )
    .await
    .expect("rebuild raw memories");

    assert!(
        !tokio::fs::try_exists(&keep_path)
            .await
            .expect("check stale keep path"),
        "sync should prune stale filename that used thread id only"
    );
    assert!(
        !tokio::fs::try_exists(&drop_path)
            .await
            .expect("check stale drop path"),
        "sync should prune stale filename for dropped thread"
    );

    let mut dir = tokio::fs::read_dir(rollout_summaries_dir(&root))
        .await
        .expect("open rollout summaries dir");
    let mut files = Vec::new();
    while let Some(entry) = dir.next_entry().await.expect("read dir entry") {
        files.push(entry.file_name().to_string_lossy().to_string());
    }
    files.sort_unstable();
    assert_eq!(files.len(), 1);
    let canonical_rollout_summary_file = &files[0];

    let raw_memories = tokio::fs::read_to_string(raw_memories_file(&root))
        .await
        .expect("read raw memories");
    assert!(raw_memories.contains("raw memory"));
    assert!(raw_memories.contains(&keep_id));
    assert!(raw_memories.contains("cwd: /tmp/workspace"));
    assert!(raw_memories.contains("rollout_path: /tmp/rollout-100.jsonl"));
    assert!(raw_memories.contains(&format!(
        "rollout_summary_file: {canonical_rollout_summary_file}"
    )));
    let thread_header = format!("## Thread `{keep_id}`");
    let thread_pos = raw_memories
        .find(&thread_header)
        .expect("thread header should exist");
    let updated_pos = raw_memories[thread_pos..]
        .find("updated_at: ")
        .map(|offset| thread_pos + offset)
        .expect("updated_at should exist after thread header");
    let cwd_pos = raw_memories[thread_pos..]
        .find("cwd: /tmp/workspace")
        .map(|offset| thread_pos + offset)
        .expect("cwd should exist after thread header");
    let rollout_path_pos = raw_memories[thread_pos..]
        .find("rollout_path: /tmp/rollout-100.jsonl")
        .map(|offset| thread_pos + offset)
        .expect("rollout_path should exist after thread header");
    let file_pos = raw_memories[thread_pos..]
        .find(&format!(
            "rollout_summary_file: {canonical_rollout_summary_file}"
        ))
        .map(|offset| thread_pos + offset)
        .expect("rollout_summary_file should exist after thread header");
    assert!(thread_pos < updated_pos);
    assert!(updated_pos < cwd_pos);
    assert!(cwd_pos < rollout_path_pos);
    assert!(rollout_path_pos < file_pos);
}

#[tokio::test]
async fn sync_rollout_summaries_uses_timestamp_hash_and_sanitized_slug_filename() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path().join("memory");
    ensure_layout(&root).await.expect("ensure layout");

    let thread_id = ThreadId::new();
    let stale_unslugged_path = rollout_summaries_dir(&root).join(format!("{thread_id}.md"));
    let stale_old_slug_path =
        rollout_summaries_dir(&root).join(format!("{thread_id}--old-slug.md"));
    tokio::fs::write(&stale_unslugged_path, "stale")
        .await
        .expect("write stale unslugged file");
    tokio::fs::write(&stale_old_slug_path, "stale")
        .await
        .expect("write stale old-slug file");

    let memories = vec![Stage1Output {
        thread_id,
        source_updated_at: Utc.timestamp_opt(200, 0).single().expect("timestamp"),
        raw_memory: "raw memory".to_string(),
        rollout_summary: "short summary".to_string(),
        rollout_slug: Some("Unsafe Slug/With Spaces & Symbols + EXTRA_LONG_12345".to_string()),
        rollout_path: PathBuf::from("/tmp/rollout-200.jsonl"),
        cwd: PathBuf::from("/tmp/workspace"),
        git_branch: Some("feature/memory-branch".to_string()),
        generated_at: Utc.timestamp_opt(201, 0).single().expect("timestamp"),
    }];

    sync_rollout_summaries_from_memories(
        &root,
        &memories,
        DEFAULT_MEMORIES_MAX_RAW_MEMORIES_FOR_CONSOLIDATION,
    )
    .await
    .expect("sync rollout summaries");

    let mut dir = tokio::fs::read_dir(rollout_summaries_dir(&root))
        .await
        .expect("open rollout summaries dir");
    let mut files = Vec::new();
    while let Some(entry) = dir.next_entry().await.expect("read dir entry") {
        files.push(entry.file_name().to_string_lossy().to_string());
    }
    files.sort_unstable();

    assert_eq!(files.len(), 1);
    let file_name = &files[0];
    let stem = file_name
        .strip_suffix(".md")
        .expect("rollout summary file should end with .md");
    let (prefix, slug) = stem
        .rsplit_once('-')
        .expect("rollout summary filename should include slug");
    let (timestamp, short_hash) = prefix
        .rsplit_once('-')
        .expect("rollout summary filename should include short hash");

    assert_eq!(timestamp.len(), 19, "timestamp should be second precision");
    let parsed_timestamp = chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%dT%H-%M-%S");
    assert!(
        parsed_timestamp.is_ok(),
        "timestamp should use YYYY-MM-DDThh-mm-ss"
    );
    assert_eq!(short_hash.len(), 4, "short hash should be exactly 4 chars");
    assert!(
        short_hash.chars().all(|ch| ch.is_ascii_alphanumeric()),
        "short hash should use only alphanumeric chars"
    );
    assert!(slug.len() <= 60, "slug should be capped at 60 chars");
    assert!(
        slug.chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_'),
        "slug should be file-safe lowercase ascii with underscores"
    );

    let summary = tokio::fs::read_to_string(rollout_summaries_dir(&root).join(file_name))
        .await
        .expect("read rollout summary");
    assert!(summary.contains(&format!("thread_id: {thread_id}")));
    assert!(summary.contains("rollout_path: /tmp/rollout-200.jsonl"));
    assert!(summary.contains("git_branch: feature/memory-branch"));
    assert!(
        !tokio::fs::try_exists(&stale_unslugged_path)
            .await
            .expect("check stale unslugged path"),
        "slugged sync should prune stale unslugged filename for same thread"
    );
    assert!(
        !tokio::fs::try_exists(&stale_old_slug_path)
            .await
            .expect("check stale old slug path"),
        "slugged sync should prune stale slugged filename for same thread"
    );
}

#[tokio::test]
async fn rebuild_raw_memories_file_adds_canonical_rollout_summary_file_header() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path().join("memory");
    ensure_layout(&root).await.expect("ensure layout");

    let thread_id =
        ThreadId::try_from("0194f5a6-89ab-7cde-8123-456789abcdef").expect("valid thread id");
    let memories = vec![Stage1Output {
        thread_id,
        source_updated_at: Utc.timestamp_opt(200, 0).single().expect("timestamp"),
        raw_memory: "\
---
description: Added a migration test
keywords: codex-state, migrations
---
### Task 1: migration-test
task: add-migration-test
task_group: codex-state
task_outcome: success
- Added regression coverage for migration uniqueness.

### Task 2: validate-migration
task: validate-migration-ordering
task_group: codex-state
task_outcome: success
- Confirmed no ordering regressions."
            .to_string(),
        rollout_summary: "short summary".to_string(),
        rollout_slug: Some("Unsafe Slug/With Spaces & Symbols + EXTRA_LONG_12345".to_string()),
        rollout_path: PathBuf::from("/tmp/rollout-200.jsonl"),
        cwd: PathBuf::from("/tmp/workspace"),
        git_branch: None,
        generated_at: Utc.timestamp_opt(201, 0).single().expect("timestamp"),
    }];

    sync_rollout_summaries_from_memories(
        &root,
        &memories,
        DEFAULT_MEMORIES_MAX_RAW_MEMORIES_FOR_CONSOLIDATION,
    )
    .await
    .expect("sync rollout summaries");
    rebuild_raw_memories_file_from_memories(
        &root,
        &memories,
        DEFAULT_MEMORIES_MAX_RAW_MEMORIES_FOR_CONSOLIDATION,
    )
    .await
    .expect("rebuild raw memories");

    let mut dir = tokio::fs::read_dir(rollout_summaries_dir(&root))
        .await
        .expect("open rollout summaries dir");
    let mut files = Vec::new();
    while let Some(entry) = dir.next_entry().await.expect("read dir entry") {
        files.push(entry.file_name().to_string_lossy().to_string());
    }
    files.sort_unstable();
    assert_eq!(files.len(), 1);
    let canonical_rollout_summary_file = &files[0];

    let raw_memories = tokio::fs::read_to_string(raw_memories_file(&root))
        .await
        .expect("read raw memories");
    let summary = tokio::fs::read_to_string(
        rollout_summaries_dir(&root).join(canonical_rollout_summary_file),
    )
    .await
    .expect("read rollout summary");
    assert!(summary.contains("rollout_path: /tmp/rollout-200.jsonl"));
    assert!(raw_memories.contains(&format!(
        "rollout_summary_file: {canonical_rollout_summary_file}"
    )));
    assert!(raw_memories.contains("description: Added a migration test"));
    assert!(raw_memories.contains("### Task 1: migration-test"));
    assert!(raw_memories.contains("task: add-migration-test"));
    assert!(raw_memories.contains("task_group: codex-state"));
    assert!(raw_memories.contains("task_outcome: success"));
}

mod phase2 {
    use crate::CodexAuth;
    use crate::ThreadManager;
    use crate::agent::AgentControl;
    use crate::codex::Session;
    use crate::codex::make_session_and_context;
    use crate::config::Config;
    use crate::config::test_config;
    use crate::memories::memory_root;
    use crate::memories::phase2;
    use crate::memories::prompts::build_consolidation_prompt;
    use crate::memories::raw_memories_file;
    use crate::memories::rollout_summaries_dir;
    use chrono::Utc;
    use codex_config::Constrained;
    use codex_protocol::ThreadId;
    use codex_protocol::protocol::AskForApproval;
    use codex_protocol::protocol::FileSystemSandboxPolicy;
    use codex_protocol::protocol::NetworkSandboxPolicy;
    use codex_protocol::protocol::Op;
    use codex_protocol::protocol::SandboxPolicy;
    use codex_protocol::protocol::SessionSource;
    use codex_state::Phase2InputSelection;
    use codex_state::Phase2JobClaimOutcome;
    use codex_state::Stage1Output;
    use codex_state::Stage1OutputRef;
    use codex_state::ThreadMetadataBuilder;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use core_test_support::PathBufExt;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::TempDir;

    fn stage1_output_with_source_updated_at(source_updated_at: i64) -> Stage1Output {
        Stage1Output {
            thread_id: ThreadId::new(),
            source_updated_at: chrono::DateTime::<Utc>::from_timestamp(source_updated_at, 0)
                .expect("valid source_updated_at timestamp"),
            raw_memory: "raw memory".to_string(),
            rollout_summary: "rollout summary".to_string(),
            rollout_slug: None,
            rollout_path: PathBuf::from("/tmp/rollout-summary.jsonl"),
            cwd: PathBuf::from("/tmp/workspace"),
            git_branch: None,
            generated_at: chrono::DateTime::<Utc>::from_timestamp(source_updated_at + 1, 0)
                .expect("valid generated_at timestamp"),
        }
    }

    fn selection_for_attested_outputs(selected: Vec<Stage1Output>) -> Phase2InputSelection {
        Phase2InputSelection {
            previous_selected: selected.clone(),
            retained_thread_ids: selected.iter().map(|output| output.thread_id).collect(),
            selected,
            removed: Vec::new(),
        }
    }

    fn config_for_memory_root(root: &std::path::Path) -> Arc<Config> {
        let mut config = test_config();
        config.codex_home = root
            .parent()
            .expect("memory root should have a codex home parent")
            .to_path_buf();
        Arc::new(config)
    }

    struct DispatchHarness {
        _codex_home: TempDir,
        config: Arc<Config>,
        session: Arc<Session>,
        manager: ThreadManager,
        state_db: Arc<codex_state::StateRuntime>,
    }

    impl DispatchHarness {
        async fn new() -> Self {
            let codex_home = tempfile::tempdir().expect("create temp codex home");
            let mut config = test_config();
            config.codex_home = codex_home.path().to_path_buf();
            config.cwd = config.codex_home.abs();
            let config = Arc::new(config);

            let state_db = codex_state::StateRuntime::init(
                config.codex_home.clone(),
                config.model_provider_id.clone(),
            )
            .await
            .expect("initialize state db");

            let manager = ThreadManager::with_models_provider_and_home_for_tests(
                CodexAuth::from_api_key("dummy"),
                config.model_provider.clone(),
                config.codex_home.clone(),
                Arc::new(codex_exec_server::EnvironmentManager::new(
                    /*exec_server_url*/ None,
                )),
            );
            let (mut session, _turn_context) = make_session_and_context().await;
            session.services.state_db = Some(Arc::clone(&state_db));
            session.services.agent_control = manager.agent_control();

            Self {
                _codex_home: codex_home,
                config,
                session: Arc::new(session),
                manager,
                state_db,
            }
        }

        async fn seed_stage1_output(&self, source_updated_at: i64) {
            let thread_id = ThreadId::new();
            let mut metadata_builder = ThreadMetadataBuilder::new(
                thread_id,
                self.config
                    .codex_home
                    .join(format!("rollout-{thread_id}.jsonl")),
                Utc::now(),
                SessionSource::Cli,
            );
            metadata_builder.cwd = self.config.cwd.to_path_buf();
            metadata_builder.model_provider = Some(self.config.model_provider_id.clone());
            let metadata = metadata_builder.build(&self.config.model_provider_id);

            self.state_db
                .upsert_thread(&metadata)
                .await
                .expect("upsert thread metadata");

            let claim = self
                .state_db
                .try_claim_stage1_job(
                    thread_id,
                    self.session.conversation_id,
                    source_updated_at,
                    /*lease_seconds*/ 3_600,
                    /*max_running_jobs*/ 64,
                )
                .await
                .expect("claim stage-1 job");
            let ownership_token = match claim {
                codex_state::Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
                other => panic!("unexpected stage-1 claim outcome: {other:?}"),
            };
            assert!(
                self.state_db
                    .mark_stage1_job_succeeded(
                        thread_id,
                        &ownership_token,
                        source_updated_at,
                        "raw memory",
                        "rollout summary",
                        /*rollout_slug*/ None,
                    )
                    .await
                    .expect("mark stage-1 success"),
                "stage-1 success should enqueue global consolidation"
            );
        }

        async fn shutdown_threads(&self) {
            let report = self
                .manager
                .shutdown_all_threads_bounded(std::time::Duration::from_secs(10))
                .await;
            assert!(report.submit_failed.is_empty());
            assert!(report.timed_out.is_empty());
        }

        fn user_input_ops_count(&self) -> usize {
            self.manager
                .captured_ops()
                .into_iter()
                .filter(|(_, op)| matches!(op, Op::UserInput { .. }))
                .count()
        }
    }

    #[test]
    fn completion_watermark_never_regresses_below_claimed_input_watermark() {
        let stage1_output = stage1_output_with_source_updated_at(/*source_updated_at*/ 123);

        let completion = phase2::get_watermark(/*claimed_watermark*/ 1_000, &[stage1_output]);
        pretty_assertions::assert_eq!(completion, 1_000);
    }

    #[test]
    fn completion_watermark_uses_claimed_watermark_when_there_are_no_memories() {
        let completion = phase2::get_watermark(/*claimed_watermark*/ 777, &[]);
        pretty_assertions::assert_eq!(completion, 777);
    }

    #[test]
    fn completion_watermark_uses_latest_memory_timestamp_when_it_is_newer() {
        let older = stage1_output_with_source_updated_at(/*source_updated_at*/ 123);
        let newer = stage1_output_with_source_updated_at(/*source_updated_at*/ 456);

        let completion = phase2::get_watermark(/*claimed_watermark*/ 200, &[older, newer]);
        pretty_assertions::assert_eq!(completion, 456);
    }

    #[tokio::test]
    async fn consolidation_artifacts_ready_requires_recent_non_empty_outputs_when_selection_changed()
     {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let root = temp_dir.path();
        let config = config_for_memory_root(root);
        let selection = selection_for_attested_outputs(Vec::new());
        let memory_index_path = root.join("MEMORY.md");
        let memory_summary_path = root.join("memory_summary.md");

        tokio::fs::write(&memory_index_path, "memory index\n")
            .await
            .expect("write memory index");
        tokio::fs::write(&memory_summary_path, "memory summary\n")
            .await
            .expect("write memory summary");

        assert!(
            !phase2::agent::consolidation_artifacts_ready(
                root,
                &config,
                std::time::SystemTime::now() + Duration::from_secs(60),
                /*allow_existing_artifacts_without_rewrite*/ false,
                &selection,
            )
            .await,
            "artifacts should be rejected when they are older than the current consolidation run"
        );

        assert!(
            phase2::agent::consolidation_artifacts_ready(
                root,
                &config,
                std::time::SystemTime::UNIX_EPOCH,
                /*allow_existing_artifacts_without_rewrite*/ false,
                &selection,
            )
            .await,
            "artifacts should be accepted when both files are fresh enough and non-empty"
        );

        tokio::fs::write(&memory_index_path, "")
            .await
            .expect("clear memory index");
        assert!(
            !phase2::agent::consolidation_artifacts_ready(
                root,
                &config,
                std::time::SystemTime::UNIX_EPOCH,
                /*allow_existing_artifacts_without_rewrite*/ false,
                &selection,
            )
            .await,
            "artifacts should be rejected when MEMORY.md is empty"
        );

        tokio::fs::write(&memory_index_path, "memory index\n")
            .await
            .expect("rewrite memory index");
        tokio::fs::write(&memory_summary_path, "")
            .await
            .expect("clear memory summary");
        assert!(
            !phase2::agent::consolidation_artifacts_ready(
                root,
                &config,
                std::time::SystemTime::UNIX_EPOCH,
                /*allow_existing_artifacts_without_rewrite*/ false,
                &selection,
            )
            .await,
            "artifacts should be rejected when memory_summary.md is empty"
        );
    }

    #[tokio::test]
    async fn consolidation_artifacts_ready_allows_existing_outputs_when_selection_is_unchanged() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let codex_home = temp_dir.path().join("codex-home");
        let root = memory_root(&codex_home);
        let config = config_for_memory_root(&root);
        let memory_index_path = root.join("MEMORY.md");
        let memory_summary_path = root.join("memory_summary.md");
        tokio::fs::create_dir_all(&root)
            .await
            .expect("create memory root");

        tokio::fs::write(&memory_index_path, "memory index\n")
            .await
            .expect("write memory index");
        tokio::fs::write(&memory_summary_path, "memory summary\n")
            .await
            .expect("write memory summary");

        let selected_outputs = vec![stage1_output_with_source_updated_at(
            /*source_updated_at*/ 200,
        )];
        let selection = selection_for_attested_outputs(selected_outputs.clone());
        phase2::test_write_consolidation_artifact_attestation(
            Arc::clone(&config),
            &root,
            &selection,
        )
        .await
        .expect("write attestation");

        assert!(
            phase2::agent::consolidation_artifacts_ready(
                &root,
                &config,
                std::time::SystemTime::now() + Duration::from_secs(60),
                /*allow_existing_artifacts_without_rewrite*/ true,
                &selection,
            )
            .await,
            "unchanged selections should accept existing non-empty artifacts even if mtimes do not advance"
        );
    }

    #[tokio::test]
    async fn consolidation_artifacts_ready_still_requires_non_empty_outputs_when_reuse_is_allowed()
    {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let codex_home = temp_dir.path().join("codex-home");
        let root = memory_root(&codex_home);
        let config = config_for_memory_root(&root);
        let selection = selection_for_attested_outputs(Vec::new());
        let memory_index_path = root.join("MEMORY.md");
        let memory_summary_path = root.join("memory_summary.md");
        tokio::fs::create_dir_all(&root)
            .await
            .expect("create memory root");

        tokio::fs::write(&memory_index_path, "")
            .await
            .expect("write empty memory index");
        tokio::fs::write(&memory_summary_path, "memory summary\n")
            .await
            .expect("write memory summary");

        assert!(
            !phase2::agent::consolidation_artifacts_ready(
                &root,
                &config,
                std::time::SystemTime::now() + Duration::from_secs(60),
                /*allow_existing_artifacts_without_rewrite*/ true,
                &selection,
            )
            .await,
            "reuse should still fail closed when MEMORY.md is empty"
        );
    }

    #[tokio::test]
    async fn consolidation_artifacts_ready_bootstraps_matching_existing_artifacts_without_attestation()
     {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let codex_home = temp_dir.path().join("codex-home");
        let root = memory_root(&codex_home);
        let config = config_for_memory_root(&root);
        let memory_index_path = root.join("MEMORY.md");
        let memory_summary_path = root.join("memory_summary.md");
        tokio::fs::create_dir_all(&root)
            .await
            .expect("create memory root");

        tokio::fs::write(&memory_index_path, "memory index\n")
            .await
            .expect("write memory index");
        tokio::fs::write(&memory_summary_path, "memory summary\n")
            .await
            .expect("write memory summary");

        let selected_outputs = vec![stage1_output_with_source_updated_at(
            /*source_updated_at*/ 200,
        )];
        let selection = selection_for_attested_outputs(selected_outputs);
        let expected_supporting_tree = phase2::test_prepared_input_artifact_tree_sha256(&root)
            .expect("prepared input tree hash");

        assert!(
            phase2::agent::consolidation_artifacts_ready_with_expected_supporting_tree(
                &root,
                &config,
                std::time::SystemTime::now() + Duration::from_secs(60),
                Some(expected_supporting_tree.as_str()),
                /*allow_existing_artifacts_without_rewrite*/ true,
                &selection,
            )
            .await,
            "first-rollout unchanged selections should bootstrap from matching existing artifacts even before an attestation exists"
        );
    }

    #[tokio::test]
    async fn consolidation_artifacts_ready_rejects_malformed_attestation_when_reuse_is_allowed() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let codex_home = temp_dir.path().join("codex-home");
        let root = memory_root(&codex_home);
        let config = config_for_memory_root(&root);
        let memory_index_path = root.join("MEMORY.md");
        let memory_summary_path = root.join("memory_summary.md");
        tokio::fs::create_dir_all(&root)
            .await
            .expect("create memory root");

        tokio::fs::write(&memory_index_path, "memory index\n")
            .await
            .expect("write memory index");
        tokio::fs::write(&memory_summary_path, "memory summary\n")
            .await
            .expect("write memory summary");

        let selected_outputs = vec![stage1_output_with_source_updated_at(
            /*source_updated_at*/ 200,
        )];
        let selection = selection_for_attested_outputs(selected_outputs);
        let expected_supporting_tree = phase2::test_prepared_input_artifact_tree_sha256(&root)
            .expect("prepared input tree hash");
        let attestation_path =
            phase2::test_consolidation_artifact_attestation_path(&root).expect("attestation path");
        tokio::fs::write(attestation_path, b"{ not valid json")
            .await
            .expect("write malformed attestation");

        assert!(
            !phase2::agent::consolidation_artifacts_ready_with_expected_supporting_tree(
                &root,
                &config,
                std::time::SystemTime::now() + Duration::from_secs(60),
                Some(expected_supporting_tree.as_str()),
                /*allow_existing_artifacts_without_rewrite*/ true,
                &selection,
            )
            .await,
            "malformed attestations should remain fail-closed"
        );
    }

    #[tokio::test]
    async fn consolidation_artifacts_ready_rejects_missing_attestation_after_support_initialized() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let codex_home = temp_dir.path().join("codex-home");
        let root = memory_root(&codex_home);
        let config = config_for_memory_root(&root);
        let memory_index_path = root.join("MEMORY.md");
        let memory_summary_path = root.join("memory_summary.md");
        tokio::fs::create_dir_all(&root)
            .await
            .expect("create memory root");

        tokio::fs::write(&memory_index_path, "memory index\n")
            .await
            .expect("write memory index");
        tokio::fs::write(&memory_summary_path, "memory summary\n")
            .await
            .expect("write memory summary");

        let selected_outputs = vec![stage1_output_with_source_updated_at(
            /*source_updated_at*/ 200,
        )];
        let selection = selection_for_attested_outputs(selected_outputs);
        let expected_supporting_tree = phase2::test_prepared_input_artifact_tree_sha256(&root)
            .expect("prepared input tree hash");

        phase2::test_write_consolidation_artifact_attestation(
            Arc::clone(&config),
            &root,
            &selection,
        )
        .await
        .expect("write attestation");

        let attestation_path =
            phase2::test_consolidation_artifact_attestation_path(&root).expect("attestation path");
        tokio::fs::remove_file(attestation_path)
            .await
            .expect("remove attestation");

        assert!(
            !phase2::agent::consolidation_artifacts_ready_with_expected_supporting_tree(
                &root,
                &config,
                std::time::SystemTime::now() + Duration::from_secs(60),
                Some(expected_supporting_tree.as_str()),
                /*allow_existing_artifacts_without_rewrite*/ true,
                &selection,
            )
            .await,
            "once attestation support has been initialized, missing attestations should remain fail-closed"
        );
    }

    #[tokio::test]
    async fn consolidation_artifacts_ready_rejects_tampered_outputs_when_reuse_is_allowed() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let codex_home = temp_dir.path().join("codex-home");
        let root = memory_root(&codex_home);
        let memory_index_path = root.join("MEMORY.md");
        let memory_summary_path = root.join("memory_summary.md");
        let selected_outputs = vec![stage1_output_with_source_updated_at(
            /*source_updated_at*/ 200,
        )];
        let selection = selection_for_attested_outputs(selected_outputs.clone());
        let config = config_for_memory_root(&root);
        tokio::fs::create_dir_all(&root)
            .await
            .expect("create memory root");

        tokio::fs::write(&memory_index_path, "memory index\n")
            .await
            .expect("write memory index");
        tokio::fs::write(&memory_summary_path, "memory summary\n")
            .await
            .expect("write memory summary");
        phase2::test_write_consolidation_artifact_attestation(
            Arc::clone(&config),
            &root,
            &selection,
        )
        .await
        .expect("write attestation");

        tokio::fs::write(&memory_index_path, "tampered memory index\n")
            .await
            .expect("tamper memory index");

        assert!(
            !phase2::agent::consolidation_artifacts_ready(
                &root,
                &config,
                std::time::SystemTime::now() + Duration::from_secs(60),
                /*allow_existing_artifacts_without_rewrite*/ true,
                &selection,
            )
            .await,
            "reuse should fail closed when non-empty artifacts no longer match the last attested successful state"
        );
    }

    #[tokio::test]
    async fn consolidation_artifacts_ready_rejects_stale_skill_artifacts_when_reuse_is_allowed() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let codex_home = temp_dir.path().join("codex-home");
        let root = memory_root(&codex_home);
        let config = config_for_memory_root(&root);
        let selection = selection_for_attested_outputs(vec![stage1_output_with_source_updated_at(
            /*source_updated_at*/ 200,
        )]);

        tokio::fs::create_dir_all(root.join("skills/demo"))
            .await
            .expect("create skills dir");
        tokio::fs::write(root.join("MEMORY.md"), "memory index\n")
            .await
            .expect("write memory index");
        tokio::fs::write(root.join("memory_summary.md"), "memory summary\n")
            .await
            .expect("write memory summary");
        tokio::fs::write(root.join("skills/demo/SKILL.md"), "trusted skill\n")
            .await
            .expect("write skill");

        phase2::test_write_consolidation_artifact_attestation(
            Arc::clone(&config),
            &root,
            &selection,
        )
        .await
        .expect("write attestation");

        tokio::fs::write(root.join("skills/demo/SKILL.md"), "tampered skill\n")
            .await
            .expect("tamper skill");

        assert!(
            !phase2::agent::consolidation_artifacts_ready(
                &root,
                &config,
                std::time::SystemTime::now() + Duration::from_secs(60),
                /*allow_existing_artifacts_without_rewrite*/ true,
                &selection,
            )
            .await,
            "reuse should fail closed when managed skill artifacts drift from the attested tree state"
        );
    }

    #[tokio::test]
    async fn consolidation_artifacts_ready_rejects_stale_prepared_inputs_even_when_outputs_are_fresh()
     {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let root = temp_dir.path();
        let config = config_for_memory_root(root);
        let selection = selection_for_attested_outputs(Vec::new());
        let memory_index_path = root.join("MEMORY.md");
        let memory_summary_path = root.join("memory_summary.md");
        let raw_memories_path = root.join("raw_memories.md");

        tokio::fs::write(&memory_index_path, "memory index\n")
            .await
            .expect("write memory index");
        tokio::fs::write(&memory_summary_path, "memory summary\n")
            .await
            .expect("write memory summary");
        tokio::fs::write(
            &raw_memories_path,
            "# Raw Memories\n\ntrusted raw memories\n",
        )
        .await
        .expect("write raw memories");

        let expected_supporting_tree = phase2::test_prepared_input_artifact_tree_sha256(root)
            .expect("fingerprint prepared immutable inputs");

        tokio::fs::write(
            &raw_memories_path,
            "# Raw Memories\n\ntampered raw memories\n",
        )
        .await
        .expect("tamper raw memories");
        tokio::fs::write(&memory_index_path, "fresh memory index\n")
            .await
            .expect("refresh memory index");
        tokio::fs::write(&memory_summary_path, "fresh memory summary\n")
            .await
            .expect("refresh memory summary");

        assert!(
            !phase2::agent::consolidation_artifacts_ready_with_expected_supporting_tree(
                root,
                &config,
                std::time::SystemTime::UNIX_EPOCH,
                Some(expected_supporting_tree.as_str()),
                /*allow_existing_artifacts_without_rewrite*/ false,
                &selection,
            )
            .await,
            "fresh outputs should still fail closed when prepared immutable inputs drift before validation"
        );
    }

    #[tokio::test]
    async fn consolidation_artifacts_ready_accepts_fresh_skill_updates_when_prepared_inputs_match()
    {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let root = temp_dir.path();
        let config = config_for_memory_root(root);
        let selection = selection_for_attested_outputs(Vec::new());
        let memory_index_path = root.join("MEMORY.md");
        let memory_summary_path = root.join("memory_summary.md");
        let raw_memories_path = root.join("raw_memories.md");
        let skill_path = root.join("skills/demo/SKILL.md");

        tokio::fs::create_dir_all(
            skill_path
                .parent()
                .expect("skills subdirectory parent should exist"),
        )
        .await
        .expect("create skills dir");
        tokio::fs::write(&memory_index_path, "memory index\n")
            .await
            .expect("write memory index");
        tokio::fs::write(&memory_summary_path, "memory summary\n")
            .await
            .expect("write memory summary");
        tokio::fs::write(
            &raw_memories_path,
            "# Raw Memories\n\ntrusted raw memories\n",
        )
        .await
        .expect("write raw memories");
        tokio::fs::write(&skill_path, "old skill\n")
            .await
            .expect("write original skill");

        let expected_supporting_tree = phase2::test_prepared_input_artifact_tree_sha256(root)
            .expect("fingerprint prepared immutable inputs");

        tokio::fs::write(&skill_path, "updated skill\n")
            .await
            .expect("update skill");
        tokio::fs::write(&memory_index_path, "fresh memory index\n")
            .await
            .expect("refresh memory index");
        tokio::fs::write(&memory_summary_path, "fresh memory summary\n")
            .await
            .expect("refresh memory summary");

        assert!(
            phase2::agent::consolidation_artifacts_ready_with_expected_supporting_tree(
                root,
                &config,
                std::time::SystemTime::UNIX_EPOCH,
                Some(expected_supporting_tree.as_str()),
                /*allow_existing_artifacts_without_rewrite*/ false,
                &selection,
            )
            .await,
            "fresh outputs should still succeed when the agent updates skills but prepared immutable inputs remain unchanged"
        );
    }

    #[tokio::test]
    async fn consolidation_attestation_is_isolated_per_memory_root() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let codex_home_a = temp_dir.path().join("codex-home-a");
        let codex_home_b = temp_dir.path().join("codex-home-b");
        let root_a = memory_root(&codex_home_a);
        let root_b = memory_root(&codex_home_b);
        let config_a = config_for_memory_root(&root_a);
        let config_b = config_for_memory_root(&root_b);
        let selected_outputs = vec![stage1_output_with_source_updated_at(
            /*source_updated_at*/ 200,
        )];
        let selection = selection_for_attested_outputs(selected_outputs);

        for root in [&root_a, &root_b] {
            tokio::fs::create_dir_all(root)
                .await
                .expect("create memory root");
            tokio::fs::write(root.join("MEMORY.md"), "memory index\n")
                .await
                .expect("write memory index");
            tokio::fs::write(root.join("memory_summary.md"), "memory summary\n")
                .await
                .expect("write memory summary");
        }

        phase2::test_write_consolidation_artifact_attestation(
            Arc::clone(&config_a),
            &root_a,
            &selection,
        )
        .await
        .expect("write attestation for root A");

        assert!(
            phase2::agent::consolidation_artifacts_ready(
                &root_a,
                &config_a,
                std::time::SystemTime::now() + Duration::from_secs(60),
                /*allow_existing_artifacts_without_rewrite*/ true,
                &selection,
            )
            .await,
            "root A should accept its own attestation"
        );
        assert!(
            !phase2::agent::consolidation_artifacts_ready(
                &root_b,
                &config_b,
                std::time::SystemTime::now() + Duration::from_secs(60),
                /*allow_existing_artifacts_without_rewrite*/ true,
                &selection,
            )
            .await,
            "root B should not reuse a sibling root's attestation"
        );
    }

    #[tokio::test]
    async fn consolidation_attestation_rejects_provider_drift() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let codex_home = temp_dir.path().join("codex-home");
        let root = memory_root(&codex_home);
        let config = config_for_memory_root(&root);
        let mut drifted_config = (*config).clone();
        drifted_config.model_provider_id = "different-provider".to_string();
        let drifted_config = Arc::new(drifted_config);
        let selected_outputs = vec![stage1_output_with_source_updated_at(
            /*source_updated_at*/ 200,
        )];
        let selection = selection_for_attested_outputs(selected_outputs);

        tokio::fs::create_dir_all(&root)
            .await
            .expect("create memory root");
        tokio::fs::write(root.join("MEMORY.md"), "memory index\n")
            .await
            .expect("write memory index");
        tokio::fs::write(root.join("memory_summary.md"), "memory summary\n")
            .await
            .expect("write memory summary");

        phase2::test_write_consolidation_artifact_attestation(
            Arc::clone(&config),
            &root,
            &selection,
        )
        .await
        .expect("write attestation");

        assert!(
            !phase2::agent::consolidation_artifacts_ready(
                &root,
                &drifted_config,
                std::time::SystemTime::now() + Duration::from_secs(60),
                /*allow_existing_artifacts_without_rewrite*/ true,
                &selection,
            )
            .await,
            "reuse should fail closed when the consolidator provider contract changes"
        );
    }

    #[tokio::test]
    async fn consolidation_attestation_rejects_model_drift() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let codex_home = temp_dir.path().join("codex-home");
        let root = memory_root(&codex_home);
        let config = config_for_memory_root(&root);
        let mut drifted_config = (*config).clone();
        drifted_config.memories.consolidation_model = Some("other-model".to_string());
        let drifted_config = Arc::new(drifted_config);
        let selection = selection_for_attested_outputs(vec![stage1_output_with_source_updated_at(
            /*source_updated_at*/ 200,
        )]);

        tokio::fs::create_dir_all(&root)
            .await
            .expect("create memory root");
        tokio::fs::write(root.join("MEMORY.md"), "memory index\n")
            .await
            .expect("write memory index");
        tokio::fs::write(root.join("memory_summary.md"), "memory summary\n")
            .await
            .expect("write memory summary");

        phase2::test_write_consolidation_artifact_attestation(
            Arc::clone(&config),
            &root,
            &selection,
        )
        .await
        .expect("write attestation");

        assert!(
            !phase2::agent::consolidation_artifacts_ready(
                &root,
                &drifted_config,
                std::time::SystemTime::now() + Duration::from_secs(60),
                /*allow_existing_artifacts_without_rewrite*/ true,
                &selection,
            )
            .await,
            "reuse should fail closed when the consolidator model changes"
        );
    }

    #[tokio::test]
    async fn consolidation_attestation_rejects_prompt_contract_drift() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let codex_home = temp_dir.path().join("codex-home");
        let root = memory_root(&codex_home);
        let config = config_for_memory_root(&root);
        let selected_output = stage1_output_with_source_updated_at(/*source_updated_at*/ 200);
        let selection = selection_for_attested_outputs(vec![selected_output.clone()]);
        let prompt_drift_selection = Phase2InputSelection {
            previous_selected: Vec::new(),
            retained_thread_ids: Vec::new(),
            selected: vec![selected_output.clone()],
            removed: vec![Stage1OutputRef {
                thread_id: selected_output.thread_id,
                source_updated_at: selected_output.source_updated_at,
                rollout_slug: selected_output.rollout_slug.clone(),
            }],
        };

        tokio::fs::create_dir_all(&root)
            .await
            .expect("create memory root");
        tokio::fs::write(root.join("MEMORY.md"), "memory index\n")
            .await
            .expect("write memory index");
        tokio::fs::write(root.join("memory_summary.md"), "memory summary\n")
            .await
            .expect("write memory summary");

        phase2::test_write_consolidation_artifact_attestation(
            Arc::clone(&config),
            &root,
            &selection,
        )
        .await
        .expect("write attestation");

        assert!(
            !phase2::agent::consolidation_artifacts_ready(
                &root,
                &config,
                std::time::SystemTime::now() + Duration::from_secs(60),
                /*allow_existing_artifacts_without_rewrite*/ true,
                &prompt_drift_selection,
            )
            .await,
            "reuse should fail closed when the consolidation prompt contract changes"
        );
    }

    #[tokio::test]
    async fn consolidation_attestation_rejects_reasoning_effort_drift() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let codex_home = temp_dir.path().join("codex-home");
        let root = memory_root(&codex_home);
        let config = config_for_memory_root(&root);
        let selection = selection_for_attested_outputs(vec![stage1_output_with_source_updated_at(
            /*source_updated_at*/ 200,
        )]);
        let model = config
            .memories
            .consolidation_model
            .as_deref()
            .unwrap_or("gpt-5.3-codex");
        let prompt = build_consolidation_prompt(&root, &selection);
        let drifted_fingerprint = phase2::test_consolidator_contract_fingerprint(
            &config.model_provider_id,
            model,
            "High",
            &prompt,
            &root,
        );

        tokio::fs::create_dir_all(&root)
            .await
            .expect("create memory root");
        tokio::fs::write(root.join("MEMORY.md"), "memory index\n")
            .await
            .expect("write memory index");
        tokio::fs::write(root.join("memory_summary.md"), "memory summary\n")
            .await
            .expect("write memory summary");

        phase2::test_write_consolidation_artifact_attestation_with_fingerprint(
            &root,
            &selection,
            drifted_fingerprint,
        )
        .await
        .expect("write attestation");

        assert!(
            !phase2::agent::consolidation_artifacts_ready(
                &root,
                &config,
                std::time::SystemTime::now() + Duration::from_secs(60),
                /*allow_existing_artifacts_without_rewrite*/ true,
                &selection,
            )
            .await,
            "reuse should fail closed when the reasoning-effort contract changes"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn consolidation_artifacts_ready_rejects_symlinked_artifacts() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let codex_home = temp_dir.path().join("codex-home");
        let root = memory_root(&codex_home);
        let config = config_for_memory_root(&root);
        let selection = selection_for_attested_outputs(vec![stage1_output_with_source_updated_at(
            /*source_updated_at*/ 200,
        )]);
        let external_dir = temp_dir.path().join("external");
        let external_memory = external_dir.join("MEMORY.md");
        let external_summary = external_dir.join("memory_summary.md");

        tokio::fs::create_dir_all(&root)
            .await
            .expect("create memory root");
        tokio::fs::create_dir_all(&external_dir)
            .await
            .expect("create external dir");
        tokio::fs::write(&external_memory, "external memory index\n")
            .await
            .expect("write external memory index");
        tokio::fs::write(&external_summary, "external memory summary\n")
            .await
            .expect("write external memory summary");

        std::os::unix::fs::symlink(&external_memory, root.join("MEMORY.md"))
            .expect("symlink memory index");
        std::os::unix::fs::symlink(&external_summary, root.join("memory_summary.md"))
            .expect("symlink memory summary");

        assert!(
            !phase2::agent::consolidation_artifacts_ready(
                &root,
                &config,
                std::time::SystemTime::UNIX_EPOCH,
                /*allow_existing_artifacts_without_rewrite*/ false,
                &selection,
            )
            .await,
            "symlinked artifacts should be rejected even when they point to non-empty files"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn writing_attestation_rejects_symlinked_attestation_path() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let codex_home = temp_dir.path().join("codex-home");
        let root = memory_root(&codex_home);
        let config = config_for_memory_root(&root);
        let selection = selection_for_attested_outputs(vec![stage1_output_with_source_updated_at(
            /*source_updated_at*/ 200,
        )]);
        let external_dir = temp_dir.path().join("external");
        let external_attestation = external_dir.join("attestation.json");

        tokio::fs::create_dir_all(&root)
            .await
            .expect("create memory root");
        tokio::fs::create_dir_all(&external_dir)
            .await
            .expect("create external dir");
        tokio::fs::write(root.join("MEMORY.md"), "memory index\n")
            .await
            .expect("write memory index");
        tokio::fs::write(root.join("memory_summary.md"), "memory summary\n")
            .await
            .expect("write memory summary");
        tokio::fs::write(&external_attestation, "placeholder\n")
            .await
            .expect("write external attestation placeholder");

        let attestation_path =
            phase2::test_consolidation_artifact_attestation_path(&root).expect("attestation path");
        std::os::unix::fs::symlink(&external_attestation, &attestation_path)
            .expect("symlink attestation");

        let err = phase2::test_write_consolidation_artifact_attestation(
            Arc::clone(&config),
            &root,
            &selection,
        )
        .await
        .expect_err("symlinked attestation path should be rejected");

        let err_text = err.to_string().to_lowercase();
        assert!(
            err_text.contains("symlink") || err_text.contains("symbolic link"),
            "expected a symlink safety error, got: {err}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn writing_attestation_does_not_mark_requirement_when_file_write_fails() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let codex_home = temp_dir.path().join("codex-home");
        let root = memory_root(&codex_home);
        let config = config_for_memory_root(&root);
        let selection = selection_for_attested_outputs(vec![stage1_output_with_source_updated_at(
            /*source_updated_at*/ 200,
        )]);
        let state_db =
            codex_state::StateRuntime::init(codex_home.clone(), config.model_provider_id.clone())
                .await
                .expect("initialize state db");
        let external_dir = temp_dir.path().join("external");
        let external_attestation = external_dir.join("attestation.json");

        tokio::fs::create_dir_all(&root)
            .await
            .expect("create memory root");
        tokio::fs::create_dir_all(&external_dir)
            .await
            .expect("create external dir");
        tokio::fs::write(root.join("MEMORY.md"), "memory index\n")
            .await
            .expect("write memory index");
        tokio::fs::write(root.join("memory_summary.md"), "memory summary\n")
            .await
            .expect("write memory summary");
        tokio::fs::write(&external_attestation, "placeholder\n")
            .await
            .expect("write external attestation placeholder");

        let attestation_path =
            phase2::test_consolidation_artifact_attestation_path(&root).expect("attestation path");
        std::os::unix::fs::symlink(&external_attestation, &attestation_path)
            .expect("symlink attestation");

        let err = phase2::test_write_consolidation_artifact_attestation_with_state_db(
            Arc::clone(&config),
            &root,
            &selection,
            &state_db,
        )
        .await
        .expect_err("symlinked attestation path should be rejected");

        let err_text = err.to_string().to_lowercase();
        assert!(
            err_text.contains("symlink") || err_text.contains("symbolic link"),
            "expected a symlink safety error, got: {err}"
        );
        let memory_root_key = phase2::test_memory_root_attestation_key(&root);
        assert!(
            !state_db
                .global_phase2_attestation_required_for_root(memory_root_key.as_str())
                .await
                .expect("load attestation requirement after write failure"),
            "failed attestation writes must not mark the root as attestation-required"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn writing_attestation_rejects_hard_linked_attestation_path_without_truncating_target() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let codex_home = temp_dir.path().join("codex-home");
        let root = memory_root(&codex_home);
        let config = config_for_memory_root(&root);
        let selection = selection_for_attested_outputs(vec![stage1_output_with_source_updated_at(
            /*source_updated_at*/ 200,
        )]);
        let external_dir = temp_dir.path().join("external");
        let protected_target = external_dir.join("protected.json");
        let original_contents = "{\n  \"protected\": true\n}\n";

        tokio::fs::create_dir_all(&root)
            .await
            .expect("create memory root");
        tokio::fs::create_dir_all(&external_dir)
            .await
            .expect("create external dir");
        tokio::fs::write(root.join("MEMORY.md"), "memory index\n")
            .await
            .expect("write memory index");
        tokio::fs::write(root.join("memory_summary.md"), "memory summary\n")
            .await
            .expect("write memory summary");
        tokio::fs::write(&protected_target, original_contents)
            .await
            .expect("write protected target");

        let attestation_path =
            phase2::test_consolidation_artifact_attestation_path(&root).expect("attestation path");
        std::fs::hard_link(&protected_target, &attestation_path)
            .expect("create hard-linked attestation path");

        let err = phase2::test_write_consolidation_artifact_attestation(
            Arc::clone(&config),
            &root,
            &selection,
        )
        .await
        .expect_err("hard-linked attestation path should be rejected");

        assert!(
            err.to_string().contains("multiple hard links"),
            "expected a hard-link safety error, got: {err}"
        );
        let preserved_contents = tokio::fs::read_to_string(&protected_target)
            .await
            .expect("read protected target after rejection");
        assert_eq!(
            preserved_contents, original_contents,
            "rejecting a hard-linked attestation path should not truncate the linked target"
        );
    }

    #[test]
    fn unchanged_selection_reuse_only_applies_to_exact_previous_snapshot() {
        let thread_id = ThreadId::new();
        let unchanged = Phase2InputSelection {
            selected: vec![Stage1Output {
                thread_id,
                ..stage1_output_with_source_updated_at(/*source_updated_at*/ 200)
            }],
            previous_selected: vec![Stage1Output {
                thread_id,
                ..stage1_output_with_source_updated_at(/*source_updated_at*/ 200)
            }],
            retained_thread_ids: vec![thread_id],
            removed: Vec::new(),
        };
        assert!(
            phase2::test_can_reuse_existing_consolidation_artifacts(&unchanged),
            "exact retained snapshots should allow existing artifacts"
        );

        let changed_timestamp = Phase2InputSelection {
            selected: vec![Stage1Output {
                thread_id,
                ..stage1_output_with_source_updated_at(/*source_updated_at*/ 201)
            }],
            previous_selected: vec![Stage1Output {
                thread_id,
                ..stage1_output_with_source_updated_at(/*source_updated_at*/ 200)
            }],
            retained_thread_ids: Vec::new(),
            removed: Vec::new(),
        };
        assert!(
            !phase2::test_can_reuse_existing_consolidation_artifacts(&changed_timestamp),
            "changed snapshots must require a rewrite even when the same thread id remains selected"
        );

        let removed = Phase2InputSelection {
            selected: vec![Stage1Output {
                thread_id,
                ..stage1_output_with_source_updated_at(/*source_updated_at*/ 200)
            }],
            previous_selected: vec![Stage1Output {
                thread_id,
                ..stage1_output_with_source_updated_at(/*source_updated_at*/ 200)
            }],
            retained_thread_ids: vec![thread_id],
            removed: vec![Stage1OutputRef {
                thread_id: ThreadId::new(),
                source_updated_at: chrono::DateTime::<Utc>::from_timestamp(100, 0)
                    .expect("valid removed timestamp"),
                rollout_slug: None,
            }],
        };
        assert!(
            !phase2::test_can_reuse_existing_consolidation_artifacts(&removed),
            "removed rows must force a rewrite"
        );
    }

    #[tokio::test]
    async fn dispatch_skips_when_global_job_is_not_dirty() {
        let harness = DispatchHarness::new().await;

        phase2::run(&harness.session, Arc::clone(&harness.config)).await;

        pretty_assertions::assert_eq!(harness.user_input_ops_count(), 0);
        let thread_ids = harness.manager.list_thread_ids().await;
        pretty_assertions::assert_eq!(thread_ids.len(), 0);
    }

    #[tokio::test]
    async fn dispatch_skips_when_global_job_is_already_running() {
        let harness = DispatchHarness::new().await;
        harness
            .state_db
            .enqueue_global_consolidation(/*input_watermark*/ 123)
            .await
            .expect("enqueue global consolidation");
        let claimed = harness
            .state_db
            .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 3_600)
            .await
            .expect("claim running global lock");
        assert!(
            matches!(claimed, Phase2JobClaimOutcome::Claimed { .. }),
            "precondition should claim the running lock"
        );

        phase2::run(&harness.session, Arc::clone(&harness.config)).await;

        let running_claim = harness
            .state_db
            .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 3_600)
            .await
            .expect("claim while lock is still running");
        pretty_assertions::assert_eq!(running_claim, Phase2JobClaimOutcome::SkippedRunning);
        pretty_assertions::assert_eq!(harness.user_input_ops_count(), 0);
        let thread_ids = harness.manager.list_thread_ids().await;
        pretty_assertions::assert_eq!(thread_ids.len(), 0);
    }

    #[test]
    fn consolidation_agent_config_keeps_split_sandbox_policies_in_sync() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let codex_home = temp_dir.path().join("codex-home");
        std::fs::create_dir_all(&codex_home).expect("create codex home");
        let mut config = test_config();
        config.codex_home = codex_home;
        config.cwd = AbsolutePathBuf::from_absolute_path(PathBuf::from("/tmp/workspace"))
            .expect("workspace path");
        let config = Arc::new(config);

        let agent_config =
            phase2::test_consolidation_agent_config(config).expect("consolidation config");
        let expected_memory_root = memory_root(&agent_config.codex_home);
        let expected_memory_root_abs = AbsolutePathBuf::from_absolute_path(&expected_memory_root)
            .expect("absolute expected memory root");

        pretty_assertions::assert_eq!(agent_config.cwd.as_path(), expected_memory_root.as_path());
        pretty_assertions::assert_eq!(
            agent_config.permissions.file_system_sandbox_policy,
            FileSystemSandboxPolicy::from_legacy_sandbox_policy(
                agent_config.permissions.sandbox_policy.get(),
                &agent_config.cwd,
            )
        );
        pretty_assertions::assert_eq!(
            agent_config.permissions.network_sandbox_policy,
            NetworkSandboxPolicy::from(agent_config.permissions.sandbox_policy.get())
        );
        match agent_config.permissions.sandbox_policy.get() {
            SandboxPolicy::WorkspaceWrite {
                writable_roots,
                network_access,
                exclude_tmpdir_env_var,
                exclude_slash_tmp,
                ..
            } => {
                pretty_assertions::assert_eq!(
                    writable_roots.as_slice(),
                    &[expected_memory_root_abs],
                    "consolidation subagent should use only the memory root as a writable root"
                );
                assert!(
                    !network_access,
                    "consolidation subagent should keep network disabled"
                );
                assert!(
                    *exclude_tmpdir_env_var,
                    "consolidation subagent should not inherit writable TMPDIR access"
                );
                assert!(
                    *exclude_slash_tmp,
                    "consolidation subagent should not inherit writable /tmp access"
                );
            }
            other => panic!("unexpected sandbox policy: {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn consolidation_agent_config_rejects_symlinked_codex_home() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let real_codex_home = temp_dir.path().join("real-codex-home");
        std::fs::create_dir_all(&real_codex_home).expect("create real codex home");
        let linked_codex_home = temp_dir.path().join("linked-codex-home");
        std::os::unix::fs::symlink(&real_codex_home, &linked_codex_home)
            .expect("symlink codex home");

        let mut config = test_config();
        config.codex_home = linked_codex_home;
        config.cwd = AbsolutePathBuf::from_absolute_path(PathBuf::from("/tmp/workspace"))
            .expect("workspace path");

        assert!(
            phase2::test_consolidation_agent_config(Arc::new(config)).is_none(),
            "symlinked codex_home should be rejected before building consolidation agent config"
        );
    }

    #[tokio::test]
    async fn dispatch_reclaims_stale_global_lock_and_starts_consolidation() {
        let harness = DispatchHarness::new().await;
        harness
            .seed_stage1_output(/*source_updated_at*/ Utc::now().timestamp())
            .await;

        let stale_claim = harness
            .state_db
            .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 0)
            .await
            .expect("claim stale global lock");
        assert!(
            matches!(stale_claim, Phase2JobClaimOutcome::Claimed { .. }),
            "stale lock precondition should be claimed"
        );

        phase2::run(&harness.session, Arc::clone(&harness.config)).await;

        let post_dispatch_claim = harness
            .state_db
            .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 3_600)
            .await
            .expect("claim after stale lock dispatch");
        assert!(
            matches!(
                post_dispatch_claim,
                Phase2JobClaimOutcome::SkippedRunning | Phase2JobClaimOutcome::SkippedNotDirty
            ),
            "stale-lock dispatch should either keep the reclaimed job running or finish it before re-claim"
        );

        let user_input_ops = harness.user_input_ops_count();
        pretty_assertions::assert_eq!(user_input_ops, 1);
        let thread_ids = harness.manager.list_thread_ids().await;
        pretty_assertions::assert_eq!(thread_ids.len(), 1);
        let thread_id = thread_ids[0];
        let subagent = harness
            .manager
            .get_thread(thread_id)
            .await
            .expect("get consolidation thread");
        let config_snapshot = subagent.config_snapshot().await;
        pretty_assertions::assert_eq!(config_snapshot.approval_policy, AskForApproval::Never);
        pretty_assertions::assert_eq!(config_snapshot.cwd, memory_root(&harness.config.codex_home));
        match config_snapshot.sandbox_policy {
            SandboxPolicy::WorkspaceWrite { writable_roots, .. } => {
                let expected_root =
                    AbsolutePathBuf::from_absolute_path(memory_root(&harness.config.codex_home))
                        .expect("absolute expected memory root");
                pretty_assertions::assert_eq!(
                    writable_roots,
                    vec![expected_root],
                    "consolidation subagent should only need the memory root as a writable root"
                );
            }
            other => panic!("unexpected sandbox policy: {other:?}"),
        }
        subagent.codex.session.ensure_rollout_materialized().await;
        subagent.codex.session.flush_rollout().await;
        let rollout_path = subagent
            .rollout_path()
            .expect("consolidation thread should have a rollout path");
        crate::state_db::read_repair_rollout_path(
            Some(harness.state_db.as_ref()),
            Some(thread_id),
            Some(/*archived_only*/ false),
            rollout_path.as_path(),
        )
        .await;
        let memory_mode = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let memory_mode = harness
                    .state_db
                    .get_thread_memory_mode(thread_id)
                    .await
                    .expect("read consolidation thread memory mode");
                if memory_mode.is_some() {
                    break memory_mode;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed out waiting for consolidation thread memory mode to persist");
        pretty_assertions::assert_eq!(memory_mode.as_deref(), Some("disabled"));

        harness.shutdown_threads().await;
    }

    #[tokio::test]
    async fn dispatch_with_empty_stage1_outputs_rebuilds_local_artifacts() {
        let harness = DispatchHarness::new().await;
        let root = memory_root(&harness.config.codex_home);
        let summaries_dir = rollout_summaries_dir(&root);
        tokio::fs::create_dir_all(&summaries_dir)
            .await
            .expect("create rollout summaries dir");

        let stale_summary_path = summaries_dir.join(format!("{}.md", ThreadId::new()));
        tokio::fs::write(&stale_summary_path, "stale summary\n")
            .await
            .expect("write stale rollout summary");
        let raw_memories_path = raw_memories_file(&root);
        tokio::fs::write(&raw_memories_path, "stale raw memories\n")
            .await
            .expect("write stale raw memories");
        let memory_index_path = root.join("MEMORY.md");
        tokio::fs::write(&memory_index_path, "stale memory index\n")
            .await
            .expect("write stale memory index");
        let memory_summary_path = root.join("memory_summary.md");
        tokio::fs::write(&memory_summary_path, "stale memory summary\n")
            .await
            .expect("write stale memory summary");
        let stale_skill_file = root.join("skills/demo/SKILL.md");
        tokio::fs::create_dir_all(
            stale_skill_file
                .parent()
                .expect("skills subdirectory parent should exist"),
        )
        .await
        .expect("create stale skills dir");
        tokio::fs::write(&stale_skill_file, "stale skill\n")
            .await
            .expect("write stale skill");

        harness
            .state_db
            .enqueue_global_consolidation(/*input_watermark*/ 999)
            .await
            .expect("enqueue global consolidation");

        phase2::run(&harness.session, Arc::clone(&harness.config)).await;

        assert!(
            !tokio::fs::try_exists(&stale_summary_path)
                .await
                .expect("check stale summary existence"),
            "empty consolidation should prune stale rollout summary files"
        );
        let raw_memories = tokio::fs::read_to_string(&raw_memories_path)
            .await
            .expect("read rebuilt raw memories");
        pretty_assertions::assert_eq!(raw_memories, "# Raw Memories\n\nNo raw memories yet.\n");
        assert!(
            !tokio::fs::try_exists(&memory_index_path)
                .await
                .expect("check memory index existence"),
            "empty consolidation should remove stale MEMORY.md"
        );
        assert!(
            !tokio::fs::try_exists(&memory_summary_path)
                .await
                .expect("check memory summary existence"),
            "empty consolidation should remove stale memory_summary.md"
        );
        assert!(
            !tokio::fs::try_exists(&stale_skill_file)
                .await
                .expect("check stale skill existence"),
            "empty consolidation should remove stale skills artifacts"
        );
        assert!(
            !tokio::fs::try_exists(root.join("skills"))
                .await
                .expect("check skills dir existence"),
            "empty consolidation should remove stale skills directory"
        );
        let next_claim = harness
            .state_db
            .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 3_600)
            .await
            .expect("claim global job after empty consolidation success");
        pretty_assertions::assert_eq!(next_claim, Phase2JobClaimOutcome::SkippedNotDirty);
        pretty_assertions::assert_eq!(harness.user_input_ops_count(), 0);
        let thread_ids = harness.manager.list_thread_ids().await;
        pretty_assertions::assert_eq!(thread_ids.len(), 0);

        harness.shutdown_threads().await;
    }

    #[tokio::test]
    async fn dispatch_marks_job_for_retry_when_sandbox_policy_cannot_be_overridden() {
        let harness = DispatchHarness::new().await;
        harness
            .state_db
            .enqueue_global_consolidation(/*input_watermark*/ 99)
            .await
            .expect("enqueue global consolidation");
        let mut constrained_config = harness.config.as_ref().clone();
        constrained_config.permissions.sandbox_policy =
            Constrained::allow_only(SandboxPolicy::DangerFullAccess);

        phase2::run(&harness.session, Arc::new(constrained_config)).await;

        let retry_claim = harness
            .state_db
            .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 3_600)
            .await
            .expect("claim global job after sandbox policy failure");
        pretty_assertions::assert_eq!(retry_claim, Phase2JobClaimOutcome::SkippedNotDirty);
        pretty_assertions::assert_eq!(harness.user_input_ops_count(), 0);
        let thread_ids = harness.manager.list_thread_ids().await;
        pretty_assertions::assert_eq!(thread_ids.len(), 0);
    }

    #[tokio::test]
    async fn dispatch_marks_job_for_retry_when_syncing_artifacts_fails() {
        let harness = DispatchHarness::new().await;
        harness.seed_stage1_output(/*source_updated_at*/ 100).await;
        let root = memory_root(&harness.config.codex_home);
        tokio::fs::write(&root, "not a directory")
            .await
            .expect("create file at memory root");

        phase2::run(&harness.session, Arc::clone(&harness.config)).await;

        let retry_claim = harness
            .state_db
            .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 3_600)
            .await
            .expect("claim global job after sync failure");
        pretty_assertions::assert_eq!(retry_claim, Phase2JobClaimOutcome::SkippedNotDirty);
        pretty_assertions::assert_eq!(harness.user_input_ops_count(), 0);
        let thread_ids = harness.manager.list_thread_ids().await;
        pretty_assertions::assert_eq!(thread_ids.len(), 0);
    }

    #[tokio::test]
    async fn dispatch_marks_job_for_retry_when_rebuilding_raw_memories_fails() {
        let harness = DispatchHarness::new().await;
        harness.seed_stage1_output(/*source_updated_at*/ 100).await;
        let root = memory_root(&harness.config.codex_home);
        tokio::fs::create_dir_all(raw_memories_file(&root))
            .await
            .expect("create raw_memories.md as a directory");

        phase2::run(&harness.session, Arc::clone(&harness.config)).await;

        let retry_claim = harness
            .state_db
            .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 3_600)
            .await
            .expect("claim global job after rebuild failure");
        pretty_assertions::assert_eq!(retry_claim, Phase2JobClaimOutcome::SkippedNotDirty);
        pretty_assertions::assert_eq!(harness.user_input_ops_count(), 0);
        let thread_ids = harness.manager.list_thread_ids().await;
        pretty_assertions::assert_eq!(thread_ids.len(), 0);
    }

    #[tokio::test]
    async fn dispatch_marks_job_for_retry_when_spawn_agent_fails() {
        let codex_home = tempfile::tempdir().expect("create temp codex home");
        let mut config = test_config();
        config.codex_home = codex_home.path().to_path_buf();
        config.cwd = config.codex_home.abs();
        let config = Arc::new(config);

        let state_db = codex_state::StateRuntime::init(
            config.codex_home.clone(),
            config.model_provider_id.clone(),
        )
        .await
        .expect("initialize state db");

        let (mut session, _turn_context) = make_session_and_context().await;
        session.services.state_db = Some(Arc::clone(&state_db));
        session.services.agent_control = AgentControl::default();
        let session = Arc::new(session);

        let thread_id = ThreadId::new();
        let mut metadata_builder = ThreadMetadataBuilder::new(
            thread_id,
            config.codex_home.join(format!("rollout-{thread_id}.jsonl")),
            Utc::now(),
            SessionSource::Cli,
        );
        metadata_builder.cwd = config.cwd.to_path_buf();
        metadata_builder.model_provider = Some(config.model_provider_id.clone());
        let metadata = metadata_builder.build(&config.model_provider_id);
        state_db
            .upsert_thread(&metadata)
            .await
            .expect("upsert thread metadata");

        let claim = state_db
            .try_claim_stage1_job(
                thread_id,
                session.conversation_id,
                /*source_updated_at*/ 100,
                /*lease_seconds*/ 3_600,
                /*max_running_jobs*/ 64,
            )
            .await
            .expect("claim stage-1 job");
        let ownership_token = match claim {
            codex_state::Stage1JobClaimOutcome::Claimed { ownership_token } => ownership_token,
            other => panic!("unexpected stage-1 claim outcome: {other:?}"),
        };
        assert!(
            state_db
                .mark_stage1_job_succeeded(
                    thread_id,
                    &ownership_token,
                    /*source_updated_at*/ 100,
                    "raw memory",
                    "rollout summary",
                    /*rollout_slug*/ None,
                )
                .await
                .expect("mark stage-1 success"),
            "stage-1 success should enqueue global consolidation"
        );

        phase2::run(&session, Arc::clone(&config)).await;

        let retry_claim = state_db
            .try_claim_global_phase2_job(ThreadId::new(), /*lease_seconds*/ 3_600)
            .await
            .expect("claim global job after spawn failure");
        pretty_assertions::assert_eq!(
            retry_claim,
            Phase2JobClaimOutcome::SkippedNotDirty,
            "spawn failures should leave the job in retry backoff instead of running"
        );
    }
}
