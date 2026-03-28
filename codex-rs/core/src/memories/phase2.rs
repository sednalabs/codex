use crate::agent::AgentStatus;
use crate::agent::status::is_final as is_final_agent_status;
use crate::codex::Session;
use crate::config::Config;
use crate::memories::memory_root;
use crate::memories::metrics;
use crate::memories::phase_two;
use crate::memories::prompts::build_consolidation_prompt;
use crate::memories::storage::rebuild_raw_memories_file_from_memories;
use crate::memories::storage::rollout_summary_file_stem;
use crate::memories::storage::sync_rollout_summaries_from_memories;
use codex_config::Constrained;
use codex_features::Feature;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::FileSystemSandboxPolicy;
use codex_protocol::protocol::NetworkSandboxPolicy;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::user_input::UserInput;
use codex_state::Stage1Output;
use codex_state::StateRuntime;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest as _;
use sha2::Sha256;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use tokio::sync::watch;
use tracing::warn;
use walkdir::WalkDir;

#[derive(Debug, Clone, Default)]
struct Claim {
    token: String,
    watermark: i64,
}

#[derive(Debug, Clone, Default)]
struct Counters {
    input: i64,
}

const CONSOLIDATION_ARTIFACT_ATTESTATION_FILE_PREFIX: &str = ".phase2-artifact-attestation";
const CONSOLIDATION_ARTIFACT_ATTESTATION_SUPPORT_FILE_PREFIX: &str =
    ".phase2-artifact-attestation-support";
const CONSOLIDATION_ARTIFACT_ATTESTATION_SCHEMA_VERSION: u32 = 4;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ConsolidationArtifactAttestation {
    schema_version: u32,
    artifacts_freshly_rewritten: bool,
    selection_fingerprint: String,
    consolidator_fingerprint: String,
    memory_sha256: String,
    memory_summary_sha256: String,
    artifact_tree_sha256: String,
}

/// Runs memory phase 2 (aka consolidation) in strict order. The method represents the linear
/// flow of the consolidation phase.
pub(super) async fn run(session: &Arc<Session>, config: Arc<Config>) {
    let phase_two_e2e_timer = session
        .services
        .session_telemetry
        .start_timer(metrics::MEMORY_PHASE_TWO_E2E_MS, &[])
        .ok();

    let Some(db) = session.services.state_db.as_deref() else {
        // This should not happen.
        return;
    };
    let root = memory_root(&config.codex_home);
    let max_raw_memories = config.memories.max_raw_memories_for_consolidation;
    let max_unused_days = config.memories.max_unused_days;

    // 1. Claim the job.
    let claim = match job::claim(session, db).await {
        Ok(claim) => claim,
        Err(e) => {
            session.services.session_telemetry.counter(
                metrics::MEMORY_PHASE_TWO_JOBS,
                /*inc*/ 1,
                &[("status", e)],
            );
            return;
        }
    };

    // 2. Get the config for the agent
    let Some(agent_config) = agent::get_config(config.clone()) else {
        // If we can't get the config, we can't consolidate.
        tracing::error!("failed to get agent config");
        job::failed(session, db, &claim, "failed_sandbox_policy").await;
        return;
    };

    // 3. Query the memories
    let selection = match db
        .get_phase2_input_selection(max_raw_memories, max_unused_days)
        .await
    {
        Ok(selection) => selection,
        Err(err) => {
            tracing::error!("failed to list stage1 outputs from global: {}", err);
            job::failed(session, db, &claim, "failed_load_stage1_outputs").await;
            return;
        }
    };
    let raw_memories = selection.selected.to_vec();
    let artifact_memories = artifact_memories_for_phase2(&selection);
    let new_watermark = get_watermark(claim.watermark, &raw_memories);

    // 4. Update the file system by syncing the raw memories with the one extracted from DB at
    //    step 3
    // [`rollout_summaries/`]
    if let Err(err) =
        sync_rollout_summaries_from_memories(&root, &artifact_memories, artifact_memories.len())
            .await
    {
        tracing::error!("failed syncing local memory artifacts for global consolidation: {err}");
        job::failed(session, db, &claim, "failed_sync_artifacts").await;
        return;
    }
    // [`raw_memories.md`]
    if let Err(err) =
        rebuild_raw_memories_file_from_memories(&root, &artifact_memories, artifact_memories.len())
            .await
    {
        tracing::error!("failed syncing local memory artifacts for global consolidation: {err}");
        job::failed(session, db, &claim, "failed_rebuild_raw_memories").await;
        return;
    }
    let Some(prepared_input_artifact_tree_sha256) =
        agent::prepared_input_artifact_tree_sha256(&root)
    else {
        tracing::error!("failed to fingerprint prepared immutable inputs for global consolidation");
        job::failed(session, db, &claim, "failed_prepare_artifacts").await;
        return;
    };
    if raw_memories.is_empty() {
        // We check only after sync of the file system.
        job::succeed(
            session,
            db,
            &claim,
            new_watermark,
            &[],
            "succeeded_no_input",
        )
        .await;
        return;
    }

    // 5. Spawn the agent
    let prompt = agent::get_prompt(&config, &selection);
    let source = SessionSource::SubAgent(SubAgentSource::MemoryConsolidation);
    let artifacts_not_before = SystemTime::now();
    let allow_existing_artifacts_without_rewrite =
        agent::can_reuse_existing_consolidation_artifacts(&selection);
    let thread_id = match session
        .services
        .agent_control
        .spawn_agent(agent_config, prompt, Some(source))
        .await
    {
        Ok(thread_id) => thread_id,
        Err(err) => {
            tracing::error!("failed to spawn global memory consolidation agent: {err}");
            job::failed(session, db, &claim, "failed_spawn_agent").await;
            return;
        }
    };

    // 6. Spawn the agent handler.
    agent::handle(
        session,
        claim,
        config,
        selection,
        new_watermark,
        raw_memories.clone(),
        thread_id,
        root,
        artifacts_not_before,
        allow_existing_artifacts_without_rewrite,
        prepared_input_artifact_tree_sha256,
        phase_two_e2e_timer,
    );

    // 7. Metrics and logs.
    let counters = Counters {
        input: raw_memories.len() as i64,
    };
    emit_metrics(session, counters);
}

fn artifact_memories_for_phase2(
    selection: &codex_state::Phase2InputSelection,
) -> Vec<Stage1Output> {
    let mut seen = HashSet::new();
    let mut memories = selection.selected.clone();
    for memory in &selection.selected {
        seen.insert(rollout_summary_file_stem(memory));
    }
    for memory in &selection.previous_selected {
        if seen.insert(rollout_summary_file_stem(memory)) {
            memories.push(memory.clone());
        }
    }
    memories
}

mod job {
    use super::*;

    pub(super) async fn claim(
        session: &Arc<Session>,
        db: &StateRuntime,
    ) -> Result<Claim, &'static str> {
        let session_telemetry = &session.services.session_telemetry;
        let claim = db
            .try_claim_global_phase2_job(session.conversation_id, phase_two::JOB_LEASE_SECONDS)
            .await
            .map_err(|e| {
                tracing::error!("failed to claim job: {}", e);
                "failed_claim"
            })?;
        let (token, watermark) = match claim {
            codex_state::Phase2JobClaimOutcome::Claimed {
                ownership_token,
                input_watermark,
            } => {
                session_telemetry.counter(
                    metrics::MEMORY_PHASE_TWO_JOBS,
                    /*inc*/ 1,
                    &[("status", "claimed")],
                );
                (ownership_token, input_watermark)
            }
            codex_state::Phase2JobClaimOutcome::SkippedNotDirty => return Err("skipped_not_dirty"),
            codex_state::Phase2JobClaimOutcome::SkippedRunning => return Err("skipped_running"),
        };

        Ok(Claim { token, watermark })
    }

    pub(super) async fn failed(
        session: &Arc<Session>,
        db: &StateRuntime,
        claim: &Claim,
        reason: &'static str,
    ) {
        session.services.session_telemetry.counter(
            metrics::MEMORY_PHASE_TWO_JOBS,
            /*inc*/ 1,
            &[("status", reason)],
        );
        if matches!(
            db.mark_global_phase2_job_failed(
                &claim.token,
                reason,
                phase_two::JOB_RETRY_DELAY_SECONDS,
            )
            .await,
            Ok(false)
        ) {
            let _ = db
                .mark_global_phase2_job_failed_if_unowned(
                    &claim.token,
                    reason,
                    phase_two::JOB_RETRY_DELAY_SECONDS,
                )
                .await;
        }
    }

    pub(super) async fn succeed(
        session: &Arc<Session>,
        db: &StateRuntime,
        claim: &Claim,
        completion_watermark: i64,
        selected_outputs: &[codex_state::Stage1Output],
        reason: &'static str,
    ) {
        session.services.session_telemetry.counter(
            metrics::MEMORY_PHASE_TWO_JOBS,
            /*inc*/ 1,
            &[("status", reason)],
        );
        let _ = db
            .mark_global_phase2_job_succeeded(&claim.token, completion_watermark, selected_outputs)
            .await;
    }
}

pub(in crate::memories) mod agent {
    use super::*;

    pub(super) fn get_config(config: Arc<Config>) -> Option<Config> {
        let root = memory_root(&config.codex_home);
        if let Err(err) = validate_memory_root_path(&root) {
            warn!(
                "memory phase-2 consolidation refusing untrusted memory root {}: {err}",
                root.display()
            );
            return None;
        }
        let mut agent_config = config.as_ref().clone();

        let absolute_root = match AbsolutePathBuf::from_absolute_path(root.clone()) {
            Ok(root) => root,
            Err(err) => {
                warn!(
                    "memory phase-2 consolidation could not set cwd from memory root {}: {err}",
                    root.display()
                );
                return None;
            }
        };
        agent_config.cwd = absolute_root.into();
        // Consolidation threads must never feed back into phase-1 memory generation.
        agent_config.memories.generate_memories = false;
        // Approval policy
        agent_config.permissions.approval_policy = Constrained::allow_only(AskForApproval::Never);
        // Consolidation runs as an internal sub-agent and must not recursively delegate.
        let _ = agent_config.features.disable(Feature::SpawnCsv);
        let _ = agent_config.features.disable(Feature::Collab);
        let _ = agent_config.features.disable(Feature::MemoryTool);

        // Sandbox policy
        let mut writable_roots = Vec::new();
        match AbsolutePathBuf::from_absolute_path(agent_config.cwd.clone()) {
            Ok(memory_root) => writable_roots.push(memory_root),
            Err(err) => warn!(
                "memory phase-2 consolidation could not add memory root writable root {}: {err}",
                agent_config.cwd.display()
            ),
        }
        // The consolidation agent only needs local memory-root write access and no network.
        let consolidation_sandbox_policy = SandboxPolicy::WorkspaceWrite {
            writable_roots,
            read_only_access: Default::default(),
            network_access: false,
            exclude_tmpdir_env_var: true,
            exclude_slash_tmp: true,
        };
        let file_system_sandbox_policy = FileSystemSandboxPolicy::from_legacy_sandbox_policy(
            &consolidation_sandbox_policy,
            &agent_config.cwd,
        );
        let network_sandbox_policy = NetworkSandboxPolicy::from(&consolidation_sandbox_policy);
        agent_config
            .permissions
            .sandbox_policy
            .set(consolidation_sandbox_policy)
            .ok()?;
        agent_config.permissions.file_system_sandbox_policy = file_system_sandbox_policy;
        agent_config.permissions.network_sandbox_policy = network_sandbox_policy;

        agent_config.model = Some(
            config
                .memories
                .consolidation_model
                .clone()
                .unwrap_or(phase_two::MODEL.to_string()),
        );
        agent_config.model_reasoning_effort = Some(phase_two::REASONING_EFFORT);

        Some(agent_config)
    }

    pub(super) fn get_prompt(
        config: &Config,
        selection: &codex_state::Phase2InputSelection,
    ) -> Vec<UserInput> {
        let root = memory_root(&config.codex_home);
        let prompt = build_consolidation_prompt(&root, selection);
        vec![UserInput::Text {
            text: prompt,
            text_elements: vec![],
        }]
    }

    /// Handle the agent while it is running.
    pub(super) fn handle(
        session: &Arc<Session>,
        claim: Claim,
        config: Arc<Config>,
        selection: codex_state::Phase2InputSelection,
        new_watermark: i64,
        selected_outputs: Vec<codex_state::Stage1Output>,
        thread_id: ThreadId,
        root: PathBuf,
        artifacts_not_before: SystemTime,
        allow_existing_artifacts_without_rewrite: bool,
        prepared_input_artifact_tree_sha256: String,
        phase_two_e2e_timer: Option<codex_otel::Timer>,
    ) {
        let Some(db) = session.services.state_db.clone() else {
            return;
        };
        let session = session.clone();

        tokio::spawn(async move {
            let _phase_two_e2e_timer = phase_two_e2e_timer;
            let agent_control = session.services.agent_control.clone();

            // TODO(jif) we might have a very small race here.
            let rx = match agent_control.subscribe_status(thread_id).await {
                Ok(rx) => rx,
                Err(err) => {
                    tracing::error!("agent_control.subscribe_status failed: {err:?}");
                    job::failed(&session, &db, &claim, "failed_subscribe_status").await;
                    return;
                }
            };

            // Loop the agent until we have the final status.
            let final_status = loop_agent(
                db.clone(),
                claim.token.clone(),
                new_watermark,
                thread_id,
                rx,
            )
            .await;

            if matches!(final_status, AgentStatus::Completed(_)) {
                if let Some(token_usage) = agent_control.get_total_token_usage(thread_id).await {
                    emit_token_usage_metrics(&session, &token_usage);
                }
                if let Some(validated_artifacts) = validated_consolidation_artifact_state(
                    root.as_path(),
                    Some(db.as_ref()),
                    artifacts_not_before,
                    Some(prepared_input_artifact_tree_sha256.as_str()),
                    allow_existing_artifacts_without_rewrite,
                    &config,
                    &selection,
                )
                .await
                {
                    if let Err(err) = write_consolidation_artifact_attestation(
                        root.as_path(),
                        Some(db.as_ref()),
                        &config,
                        &selection,
                        &validated_artifacts,
                    )
                    .await
                    {
                        tracing::warn!(
                            error = %err,
                            "global memory consolidation agent {thread_id} completed but artifact attestation could not be recorded"
                        );
                        job::failed(&session, &db, &claim, "failed_record_artifacts").await;
                    } else {
                        job::succeed(
                            &session,
                            &db,
                            &claim,
                            new_watermark,
                            &selected_outputs,
                            "succeeded",
                        )
                        .await;
                    }
                } else {
                    tracing::warn!(
                        "global memory consolidation agent {thread_id} completed without refreshing non-empty MEMORY.md and memory_summary.md artifacts"
                    );
                    job::failed(&session, &db, &claim, "failed_missing_artifacts").await;
                }
            } else {
                job::failed(&session, &db, &claim, "failed_agent").await;
            }

            // Fire and forget close of the agent.
            if !matches!(final_status, AgentStatus::Shutdown | AgentStatus::NotFound) {
                tokio::spawn(async move {
                    if let Err(err) = agent_control.shutdown_live_agent(thread_id).await {
                        warn!(
                            "failed to auto-close global memory consolidation agent {thread_id}: {err}"
                        );
                    }
                });
            } else {
                tracing::warn!("The agent was already gone");
            }
        });
    }

    #[cfg(test)]
    pub(in crate::memories) async fn consolidation_artifacts_ready(
        root: &Path,
        config: &Config,
        not_before: SystemTime,
        allow_existing_artifacts_without_rewrite: bool,
        selection: &codex_state::Phase2InputSelection,
    ) -> bool {
        let expected_prepared_input_artifact_tree_sha256 =
            (!allow_existing_artifacts_without_rewrite)
                .then(|| prepared_input_artifact_tree_sha256(root))
                .flatten();
        validated_consolidation_artifact_state(
            root,
            None,
            not_before,
            expected_prepared_input_artifact_tree_sha256.as_deref(),
            allow_existing_artifacts_without_rewrite,
            config,
            selection,
        )
        .await
        .is_some()
    }

    #[cfg(test)]
    pub(in crate::memories) async fn consolidation_artifacts_ready_with_expected_supporting_tree(
        root: &Path,
        config: &Config,
        not_before: SystemTime,
        expected_prepared_input_artifact_tree_sha256: Option<&str>,
        allow_existing_artifacts_without_rewrite: bool,
        selection: &codex_state::Phase2InputSelection,
    ) -> bool {
        consolidation_artifacts_ready_with_state_db_and_expected_prepared_input_tree(
            root,
            config,
            None,
            not_before,
            expected_prepared_input_artifact_tree_sha256,
            allow_existing_artifacts_without_rewrite,
            selection,
        )
        .await
    }

    #[cfg(test)]
    pub(in crate::memories) async fn consolidation_artifacts_ready_with_state_db_and_expected_prepared_input_tree(
        root: &Path,
        config: &Config,
        state_db: Option<&StateRuntime>,
        not_before: SystemTime,
        expected_prepared_input_artifact_tree_sha256: Option<&str>,
        allow_existing_artifacts_without_rewrite: bool,
        selection: &codex_state::Phase2InputSelection,
    ) -> bool {
        validated_consolidation_artifact_state(
            root,
            state_db,
            not_before,
            expected_prepared_input_artifact_tree_sha256,
            allow_existing_artifacts_without_rewrite,
            config,
            selection,
        )
        .await
        .is_some()
    }

    pub(super) fn can_reuse_existing_consolidation_artifacts(
        selection: &codex_state::Phase2InputSelection,
    ) -> bool {
        selection.removed.is_empty()
            && selection.selected == selection.previous_selected
            && selection.selected.len() == selection.retained_thread_ids.len()
            && selection
                .selected
                .iter()
                .all(|output| selection.retained_thread_ids.contains(&output.thread_id))
    }

    struct ConsolidationArtifactState {
        memory_modified: SystemTime,
        memory_summary_modified: SystemTime,
        memory_sha256: String,
        memory_summary_sha256: String,
        prepared_input_artifact_tree_sha256: String,
        artifact_tree_sha256: String,
        artifacts_freshly_rewritten: bool,
    }

    async fn validated_consolidation_artifact_state(
        root: &Path,
        state_db: Option<&StateRuntime>,
        not_before: SystemTime,
        expected_prepared_input_artifact_tree_sha256: Option<&str>,
        allow_existing_artifacts_without_rewrite: bool,
        config: &Config,
        selection: &codex_state::Phase2InputSelection,
    ) -> Option<ConsolidationArtifactState> {
        let mut current = current_consolidation_artifact_state(root).await?;
        let prepared_input_tree_matches = expected_prepared_input_artifact_tree_sha256
            .is_some_and(|expected| current.prepared_input_artifact_tree_sha256 == expected);
        if current.memory_modified >= not_before
            && current.memory_summary_modified >= not_before
            && prepared_input_tree_matches
        {
            current.artifacts_freshly_rewritten = true;
            return Some(current);
        }
        if !allow_existing_artifacts_without_rewrite {
            return None;
        }

        let attestation_required = if let Some(state_db) = state_db {
            let memory_root_key = memory_root_attestation_key(root);
            match state_db
                .global_phase2_attestation_required_for_root(memory_root_key.as_str())
                .await
            {
                Ok(required) => required,
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "failed to read global memory consolidation attestation requirement state"
                    );
                    return None;
                }
            }
        } else {
            false
        };

        match read_consolidation_artifact_attestation(root).await {
            Ok(Some(attestation)) => (attestation.schema_version
                == CONSOLIDATION_ARTIFACT_ATTESTATION_SCHEMA_VERSION
                && attestation.selection_fingerprint == selection_fingerprint(&selection.selected)
                && attestation.consolidator_fingerprint
                    == consolidator_fingerprint(config, root, selection)
                && attestation.memory_sha256 == current.memory_sha256
                && attestation.memory_summary_sha256 == current.memory_summary_sha256
                && attestation.artifact_tree_sha256 == current.artifact_tree_sha256)
                .then_some(current),
            Ok(None) => {
                if attestation_required {
                    None
                } else if state_db.is_some() {
                    None
                } else {
                    let root = root.to_path_buf();
                    match tokio::task::spawn_blocking(move || {
                        attestation_support_initialized(&root)
                    })
                    .await
                    {
                        Ok(Ok(false)) => prepared_input_tree_matches.then_some(current),
                        Ok(Ok(true)) => None,
                        Ok(Err(err)) => {
                            tracing::warn!(
                                error = %err,
                                "failed to read global memory consolidation artifact attestation support state"
                            );
                            None
                        }
                        Err(err) => {
                            tracing::warn!(
                                error = %err,
                                "failed to join global memory consolidation attestation support task"
                            );
                            None
                        }
                    }
                }
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "failed to read global memory consolidation artifact attestation"
                );
                None
            }
        }
    }

    async fn current_consolidation_artifact_state(
        root: &Path,
    ) -> Option<ConsolidationArtifactState> {
        let root = root.to_path_buf();
        match tokio::task::spawn_blocking(move || {
            validate_memory_root_path(root.as_path()).ok()?;
            let memory = artifact_file_state(root.as_path(), "MEMORY.md")?;
            let memory_summary = artifact_file_state(root.as_path(), "memory_summary.md")?;
            Some(ConsolidationArtifactState {
                memory_modified: memory.modified,
                memory_summary_modified: memory_summary.modified,
                memory_sha256: memory.sha256,
                memory_summary_sha256: memory_summary.sha256,
                prepared_input_artifact_tree_sha256: prepared_input_artifact_tree_sha256(
                    root.as_path(),
                )?,
                artifact_tree_sha256: artifact_tree_sha256(root.as_path())?,
                artifacts_freshly_rewritten: false,
            })
        })
        .await
        {
            Ok(current) => current,
            Err(err) => {
                warn!(
                    error = %err,
                    "failed to join global memory consolidation artifact state task"
                );
                None
            }
        }
    }

    struct ArtifactFileState {
        modified: SystemTime,
        sha256: String,
    }

    fn artifact_file_state(root: &Path, relative_name: &str) -> Option<ArtifactFileState> {
        let path = root.join(relative_name);
        let mut file = open_read_only_regular_file(&path).ok()?;
        let metadata = file.metadata().ok()?;
        let modified = metadata.modified().ok()?;
        let mut contents = String::new();
        use std::io::Read as _;
        file.read_to_string(&mut contents).ok()?;
        if contents.trim().is_empty() {
            return None;
        }
        Some(ArtifactFileState {
            modified,
            sha256: sha256_hex(contents.as_bytes()),
        })
    }

    async fn write_consolidation_artifact_attestation(
        root: &Path,
        state_db: Option<&StateRuntime>,
        config: &Config,
        selection: &codex_state::Phase2InputSelection,
        artifacts: &ConsolidationArtifactState,
    ) -> std::io::Result<()> {
        let attestation = ConsolidationArtifactAttestation {
            schema_version: CONSOLIDATION_ARTIFACT_ATTESTATION_SCHEMA_VERSION,
            artifacts_freshly_rewritten: artifacts.artifacts_freshly_rewritten,
            selection_fingerprint: selection_fingerprint(&selection.selected),
            consolidator_fingerprint: consolidator_fingerprint(config, root, selection),
            memory_sha256: artifacts.memory_sha256.clone(),
            memory_summary_sha256: artifacts.memory_summary_sha256.clone(),
            artifact_tree_sha256: artifacts.artifact_tree_sha256.clone(),
        };
        let contents = serde_json::to_vec_pretty(&attestation)
            .map_err(|err| std::io::Error::other(format!("serialize attestation: {err}")))?;
        let memory_root_key = memory_root_attestation_key(root);
        let root_buf = root.to_path_buf();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let path =
                consolidation_artifact_attestation_path(root_buf.as_path()).ok_or_else(|| {
                    std::io::Error::other("memory root is missing a codex_home parent")
                })?;
            let mut file = open_write_regular_file_no_follow(&path)?;
            use std::io::Write as _;
            file.write_all(&contents)?;
            file.flush()?;
            write_consolidation_artifact_attestation_support_marker(root_buf.as_path())
        })
        .await
        .map_err(|err| std::io::Error::other(format!("join attestation write task: {err}")))??;

        if let Some(state_db) = state_db {
            state_db
                .mark_global_phase2_attestation_required_for_root(memory_root_key.as_str())
                .await
                .map_err(|err| {
                    std::io::Error::other(format!("persist attestation requirement state: {err}"))
                })?;
        }

        Ok(())
    }

    #[cfg(test)]
    pub(super) async fn write_current_consolidation_artifact_attestation(
        config: &Config,
        root: &Path,
        selection: &codex_state::Phase2InputSelection,
    ) -> std::io::Result<()> {
        write_current_consolidation_artifact_attestation_with_state_db(
            config, root, selection, None,
        )
        .await
    }

    #[cfg(test)]
    pub(super) async fn write_current_consolidation_artifact_attestation_with_state_db(
        config: &Config,
        root: &Path,
        selection: &codex_state::Phase2InputSelection,
        state_db: Option<&StateRuntime>,
    ) -> std::io::Result<()> {
        let artifacts = current_consolidation_artifact_state(root)
            .await
            .ok_or_else(|| std::io::Error::other("missing non-empty consolidation artifacts"))?;
        write_consolidation_artifact_attestation(root, state_db, config, selection, &artifacts)
            .await
    }

    #[cfg(test)]
    pub(super) async fn write_current_consolidation_artifact_attestation_with_fingerprint(
        root: &Path,
        selection: &codex_state::Phase2InputSelection,
        consolidator_fingerprint: String,
    ) -> std::io::Result<()> {
        let artifacts = current_consolidation_artifact_state(root)
            .await
            .ok_or_else(|| std::io::Error::other("missing non-empty consolidation artifacts"))?;
        let attestation = ConsolidationArtifactAttestation {
            schema_version: CONSOLIDATION_ARTIFACT_ATTESTATION_SCHEMA_VERSION,
            artifacts_freshly_rewritten: artifacts.artifacts_freshly_rewritten,
            selection_fingerprint: selection_fingerprint(&selection.selected),
            consolidator_fingerprint,
            memory_sha256: artifacts.memory_sha256,
            memory_summary_sha256: artifacts.memory_summary_sha256,
            artifact_tree_sha256: artifacts.artifact_tree_sha256,
        };
        let contents = serde_json::to_vec_pretty(&attestation)
            .map_err(|err| std::io::Error::other(format!("serialize attestation: {err}")))?;
        let path = consolidation_artifact_attestation_path(root)
            .ok_or_else(|| std::io::Error::other("memory root is missing a codex_home parent"))?;
        tokio::fs::write(path, contents).await
    }

    async fn read_consolidation_artifact_attestation(
        root: &Path,
    ) -> std::io::Result<Option<ConsolidationArtifactAttestation>> {
        let root = root.to_path_buf();
        tokio::task::spawn_blocking(move || -> std::io::Result<Option<_>> {
            let path =
                consolidation_artifact_attestation_path(root.as_path()).ok_or_else(|| {
                    std::io::Error::other("memory root is missing a codex_home parent")
                })?;
            let mut file = match open_read_only_regular_file(&path) {
                Ok(file) => file,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(err) => return Err(err),
            };
            let mut contents = Vec::new();
            use std::io::Read as _;
            file.read_to_end(&mut contents)?;
            let attestation = serde_json::from_slice(&contents).map_err(|err| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("parse attestation: {err}"),
                )
            })?;
            Ok(Some(attestation))
        })
        .await
        .map_err(|err| std::io::Error::other(format!("join attestation read task: {err}")))?
    }

    fn selection_fingerprint(selected_outputs: &[codex_state::Stage1Output]) -> String {
        #[derive(Serialize)]
        struct SelectionRef<'a> {
            thread_id: codex_protocol::ThreadId,
            source_updated_at: chrono::DateTime<chrono::Utc>,
            rollout_slug: Option<&'a str>,
        }

        let refs = selected_outputs
            .iter()
            .map(|output| SelectionRef {
                thread_id: output.thread_id,
                source_updated_at: output.source_updated_at,
                rollout_slug: output.rollout_slug.as_deref(),
            })
            .collect::<Vec<_>>();
        let bytes = serde_json::to_vec(&refs).expect("serialize phase-2 selection refs");
        sha256_hex(&bytes)
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        let digest = Sha256::digest(bytes);
        format!("{digest:x}")
    }

    fn consolidator_fingerprint(
        config: &Config,
        root: &Path,
        selection: &codex_state::Phase2InputSelection,
    ) -> String {
        let model = config
            .memories
            .consolidation_model
            .as_deref()
            .unwrap_or(phase_two::MODEL);
        let prompt = build_consolidation_prompt(root, selection);
        consolidator_contract_fingerprint(
            &config.model_provider_id,
            model,
            &format!("{:?}", phase_two::REASONING_EFFORT),
            &prompt,
            root,
        )
    }

    fn consolidator_contract_fingerprint(
        model_provider_id: &str,
        model: &str,
        reasoning_effort: &str,
        prompt: &str,
        root: &Path,
    ) -> String {
        #[derive(Serialize)]
        struct ConsolidatorFingerprint<'a> {
            attestation_schema_version: u32,
            model_provider_id: &'a str,
            model: &'a str,
            reasoning_effort: &'a str,
            prompt_sha256: String,
            approval_policy: AskForApproval,
            sandbox_policy: SandboxPolicy,
            file_system_sandbox_policy: FileSystemSandboxPolicy,
            network_sandbox_policy: NetworkSandboxPolicy,
        }

        let sandbox_policy = SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![
                AbsolutePathBuf::from_absolute_path(root.to_path_buf())
                    .expect("memory root should be absolute"),
            ],
            read_only_access: Default::default(),
            network_access: false,
            exclude_tmpdir_env_var: true,
            exclude_slash_tmp: true,
        };
        let file_system_sandbox_policy =
            FileSystemSandboxPolicy::from_legacy_sandbox_policy(&sandbox_policy, root);
        let network_sandbox_policy = NetworkSandboxPolicy::from(&sandbox_policy);
        let fingerprint = ConsolidatorFingerprint {
            attestation_schema_version: CONSOLIDATION_ARTIFACT_ATTESTATION_SCHEMA_VERSION,
            model_provider_id,
            model,
            reasoning_effort,
            prompt_sha256: sha256_hex(prompt.as_bytes()),
            approval_policy: AskForApproval::Never,
            sandbox_policy,
            file_system_sandbox_policy,
            network_sandbox_policy,
        };
        let bytes =
            serde_json::to_vec(&fingerprint).expect("serialize phase-2 consolidator fingerprint");
        sha256_hex(&bytes)
    }

    #[cfg(test)]
    pub(super) fn test_consolidator_contract_fingerprint(
        model_provider_id: &str,
        model: &str,
        reasoning_effort: &str,
        prompt: &str,
        root: &Path,
    ) -> String {
        consolidator_contract_fingerprint(model_provider_id, model, reasoning_effort, prompt, root)
    }

    fn memory_root_attestation_key(root: &Path) -> String {
        sha256_hex(&stable_path_identity_bytes(root))
    }

    fn consolidation_artifact_attestation_path(root: &Path) -> Option<PathBuf> {
        let root_hash = memory_root_attestation_key(root);
        Some(root.parent()?.join(format!(
            "{CONSOLIDATION_ARTIFACT_ATTESTATION_FILE_PREFIX}-{root_hash}.json"
        )))
    }

    fn consolidation_artifact_attestation_support_path(root: &Path) -> Option<PathBuf> {
        let root_hash = memory_root_attestation_key(root);
        Some(root.parent()?.join(format!(
            "{CONSOLIDATION_ARTIFACT_ATTESTATION_SUPPORT_FILE_PREFIX}-{root_hash}.json"
        )))
    }

    #[cfg(test)]
    pub(super) fn test_consolidation_artifact_attestation_path(root: &Path) -> Option<PathBuf> {
        consolidation_artifact_attestation_path(root)
    }

    #[cfg(test)]
    pub(super) fn test_consolidation_artifact_attestation_support_path(
        root: &Path,
    ) -> Option<PathBuf> {
        consolidation_artifact_attestation_support_path(root)
    }

    #[cfg(test)]
    pub(super) fn test_memory_root_attestation_key(root: &Path) -> String {
        memory_root_attestation_key(root)
    }

    fn attestation_support_initialized(root: &Path) -> std::io::Result<bool> {
        let path = consolidation_artifact_attestation_support_path(root)
            .ok_or_else(|| std::io::Error::other("memory root is missing a codex_home parent"))?;
        match open_read_only_regular_file(&path) {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }

    fn write_consolidation_artifact_attestation_support_marker(root: &Path) -> std::io::Result<()> {
        let path = consolidation_artifact_attestation_support_path(root)
            .ok_or_else(|| std::io::Error::other("memory root is missing a codex_home parent"))?;
        let mut file = open_write_regular_file_no_follow(&path)?;
        use std::io::Write as _;
        file.write_all(br#"{"attestation_support_initialized":true}"#)?;
        file.flush()
    }

    fn validate_memory_root_path(root: &Path) -> std::io::Result<()> {
        let codex_home = root
            .parent()
            .ok_or_else(|| std::io::Error::other("memory root is missing codex_home parent"))?;
        for ancestor in codex_home.ancestors() {
            validate_non_redirecting_path_component(ancestor)?;
        }
        match std::fs::symlink_metadata(root) {
            Ok(metadata) => validate_non_redirecting_metadata(root, &metadata),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }

    fn validate_non_redirecting_path_component(path: &Path) -> std::io::Result<()> {
        let metadata = std::fs::symlink_metadata(path)?;
        validate_non_redirecting_metadata(path, &metadata)
    }

    fn validate_non_redirecting_metadata(
        path: &Path,
        metadata: &std::fs::Metadata,
    ) -> std::io::Result<()> {
        if metadata.file_type().is_symlink() {
            return Err(std::io::Error::other(format!(
                "path {} is a symlink",
                path.display()
            )));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(std::io::Error::other(format!(
                    "path {} is a reparse point",
                    path.display()
                )));
            }
        }
        Ok(())
    }

    fn artifact_tree_sha256(root: &Path) -> Option<String> {
        artifact_tree_sha256_filtered(root, |_| true)
    }

    pub(super) fn prepared_input_artifact_tree_sha256(root: &Path) -> Option<String> {
        // Prepared immutable phase-2 inputs are the rebuilt raw memories file plus
        // the canonical rollout summaries synced from stage-1 output. Fresh-run
        // validation must cover this exact read-set while excluding mutable
        // outputs the prompt explicitly allows the consolidator to rewrite.
        artifact_tree_sha256_filtered(root, |relative_path| {
            relative_path == Path::new("raw_memories.md")
                || relative_path.starts_with(Path::new("rollout_summaries"))
        })
    }

    fn artifact_tree_sha256_filtered(
        root: &Path,
        include_relative_path: impl Fn(&Path) -> bool,
    ) -> Option<String> {
        #[derive(Serialize)]
        struct ArtifactTreeEntry {
            path_identity: Vec<u8>,
            sha256: String,
        }

        let mut manifest_entries = Vec::new();

        for entry in WalkDir::new(root).follow_links(false) {
            let entry = entry.ok()?;
            if entry.depth() == 0 {
                continue;
            }
            if entry.path_is_symlink() {
                return None;
            }
            if entry.file_type().is_dir() {
                continue;
            }
            if !entry.file_type().is_file() {
                return None;
            }

            let relative_path = entry.path().strip_prefix(root).ok()?;
            if !include_relative_path(relative_path) {
                continue;
            }
            let mut file = open_read_only_regular_file(entry.path()).ok()?;
            let mut contents = Vec::new();
            use std::io::Read as _;
            file.read_to_end(&mut contents).ok()?;
            manifest_entries.push(ArtifactTreeEntry {
                path_identity: stable_path_identity_bytes(relative_path),
                sha256: sha256_hex(&contents),
            });
        }

        manifest_entries.sort_by(|a, b| a.path_identity.cmp(&b.path_identity));
        let manifest = serde_json::to_vec(&manifest_entries).ok()?;
        Some(sha256_hex(&manifest))
    }

    #[cfg(test)]
    pub(super) fn test_prepared_input_artifact_tree_sha256(root: &Path) -> Option<String> {
        prepared_input_artifact_tree_sha256(root)
    }

    fn stable_path_identity_bytes(path: &Path) -> Vec<u8> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;

            path.as_os_str().as_bytes().to_vec()
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;

            path.as_os_str()
                .encode_wide()
                .flat_map(|unit| unit.to_le_bytes())
                .collect()
        }
        #[cfg(all(not(unix), not(windows)))]
        {
            path.to_string_lossy().into_owned().into_bytes()
        }
    }

    fn open_read_only_regular_file(path: &Path) -> std::io::Result<std::fs::File> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            use std::os::unix::fs::OpenOptionsExt;

            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)?;
            let metadata = file.metadata()?;
            if metadata.nlink() > 1 {
                return Err(std::io::Error::other("path has multiple hard links"));
            }
            if !metadata.is_file() {
                return Err(std::io::Error::other("path is not a regular file"));
            }
            Ok(file)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
            use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(path)?;
            let metadata = file.metadata()?;
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(std::io::Error::other("path is a reparse point"));
            }
            if metadata.number_of_links() > 1 {
                return Err(std::io::Error::other("path has multiple hard links"));
            }
            if !metadata.is_file() {
                return Err(std::io::Error::other("path is not a regular file"));
            }
            Ok(file)
        }
        #[cfg(all(not(unix), not(windows)))]
        {
            let _ = path;
            compile_error!(
                "secure file reading is not implemented for this platform; this is required for memory consolidation attestation"
            );
        }
    }

    fn open_write_regular_file_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            use std::os::unix::fs::OpenOptionsExt;

            let file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)?;
            let metadata = file.metadata()?;
            if metadata.nlink() > 1 {
                return Err(std::io::Error::other("path has multiple hard links"));
            }
            if !metadata.is_file() {
                return Err(std::io::Error::other("path is not a regular file"));
            }
            file.set_len(0)?;
            Ok(file)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
            use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

            let file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(path)?;
            let metadata = file.metadata()?;
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(std::io::Error::other("path is a reparse point"));
            }
            if metadata.number_of_links() > 1 {
                return Err(std::io::Error::other("path has multiple hard links"));
            }
            if !metadata.is_file() {
                return Err(std::io::Error::other("path is not a regular file"));
            }
            file.set_len(0)?;
            Ok(file)
        }
        #[cfg(all(not(unix), not(windows)))]
        {
            let _ = path;
            compile_error!(
                "secure file writing is not implemented for this platform; this is required for memory consolidation attestation"
            );
        }
    }

    async fn loop_agent(
        db: Arc<StateRuntime>,
        token: String,
        _new_watermark: i64,
        thread_id: ThreadId,
        mut rx: watch::Receiver<AgentStatus>,
    ) -> AgentStatus {
        let mut heartbeat_interval =
            tokio::time::interval(Duration::from_secs(phase_two::JOB_HEARTBEAT_SECONDS));
        heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            let status = rx.borrow().clone();
            if is_final_agent_status(&status) {
                break status;
            }

            tokio::select! {
                update = rx.changed() => {
                    if update.is_err() {
                        tracing::warn!(
                            "lost status updates for global memory consolidation agent {thread_id}"
                        );
                        break status;
                    }
                }
                _ = heartbeat_interval.tick() => {
                    match db
                        .heartbeat_global_phase2_job(
                            &token,
                            phase_two::JOB_LEASE_SECONDS,
                        )
                        .await
                    {
                        Ok(true) => {}
                        Ok(false) => {
                            break AgentStatus::Errored(
                                "lost global phase-2 ownership during heartbeat".to_string(),
                            );
                        }
                        Err(err) => {
                            break AgentStatus::Errored(format!(
                                "phase-2 heartbeat update failed: {err}"
                            ));
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn test_consolidation_agent_config(config: Arc<Config>) -> Option<Config> {
    agent::get_config(config)
}

#[cfg(test)]
pub(crate) fn test_can_reuse_existing_consolidation_artifacts(
    selection: &codex_state::Phase2InputSelection,
) -> bool {
    agent::can_reuse_existing_consolidation_artifacts(selection)
}

#[cfg(test)]
pub(crate) async fn test_write_consolidation_artifact_attestation(
    config: Arc<Config>,
    root: &Path,
    selection: &codex_state::Phase2InputSelection,
) -> std::io::Result<()> {
    agent::write_current_consolidation_artifact_attestation(&config, root, selection).await
}

#[cfg(test)]
pub(crate) async fn test_write_consolidation_artifact_attestation_with_state_db(
    config: Arc<Config>,
    root: &Path,
    selection: &codex_state::Phase2InputSelection,
    state_db: &StateRuntime,
) -> std::io::Result<()> {
    agent::write_current_consolidation_artifact_attestation_with_state_db(
        &config,
        root,
        selection,
        Some(state_db),
    )
    .await
}

#[cfg(test)]
pub(crate) async fn test_write_consolidation_artifact_attestation_with_fingerprint(
    root: &Path,
    selection: &codex_state::Phase2InputSelection,
    consolidator_fingerprint: String,
) -> std::io::Result<()> {
    agent::write_current_consolidation_artifact_attestation_with_fingerprint(
        root,
        selection,
        consolidator_fingerprint,
    )
    .await
}

#[cfg(test)]
pub(crate) async fn test_consolidation_artifacts_ready_with_state_db_and_expected_prepared_input_tree(
    root: &Path,
    config: &Config,
    state_db: &StateRuntime,
    not_before: SystemTime,
    expected_prepared_input_artifact_tree_sha256: Option<&str>,
    allow_existing_artifacts_without_rewrite: bool,
    selection: &codex_state::Phase2InputSelection,
) -> bool {
    agent::consolidation_artifacts_ready_with_state_db_and_expected_prepared_input_tree(
        root,
        config,
        Some(state_db),
        not_before,
        expected_prepared_input_artifact_tree_sha256,
        allow_existing_artifacts_without_rewrite,
        selection,
    )
    .await
}

#[cfg(test)]
pub(crate) fn test_consolidator_contract_fingerprint(
    model_provider_id: &str,
    model: &str,
    reasoning_effort: &str,
    prompt: &str,
    root: &Path,
) -> String {
    agent::test_consolidator_contract_fingerprint(
        model_provider_id,
        model,
        reasoning_effort,
        prompt,
        root,
    )
}

#[cfg(test)]
pub(crate) fn test_consolidation_artifact_attestation_path(root: &Path) -> Option<PathBuf> {
    agent::test_consolidation_artifact_attestation_path(root)
}

#[cfg(test)]
pub(crate) fn test_consolidation_artifact_attestation_support_path(root: &Path) -> Option<PathBuf> {
    agent::test_consolidation_artifact_attestation_support_path(root)
}

#[cfg(test)]
pub(crate) fn test_memory_root_attestation_key(root: &Path) -> String {
    agent::test_memory_root_attestation_key(root)
}

#[cfg(test)]
pub(crate) fn test_prepared_input_artifact_tree_sha256(root: &Path) -> Option<String> {
    agent::test_prepared_input_artifact_tree_sha256(root)
}

#[cfg(test)]
#[path = "phase2_attestation_tests.rs"]
mod attestation_tests;

pub(super) fn get_watermark(
    claimed_watermark: i64,
    latest_memories: &[codex_state::Stage1Output],
) -> i64 {
    latest_memories
        .iter()
        .map(|memory| memory.source_updated_at.timestamp())
        .max()
        .unwrap_or(claimed_watermark)
        .max(claimed_watermark) // todo double check the claimed here.
}

fn emit_metrics(session: &Arc<Session>, counters: Counters) {
    let otel = session.services.session_telemetry.clone();
    if counters.input > 0 {
        otel.counter(metrics::MEMORY_PHASE_TWO_INPUT, counters.input, &[]);
    }

    otel.counter(
        metrics::MEMORY_PHASE_TWO_JOBS,
        /*inc*/ 1,
        &[("status", "agent_spawned")],
    );
}

fn emit_token_usage_metrics(session: &Arc<Session>, token_usage: &TokenUsage) {
    let otel = session.services.session_telemetry.clone();
    otel.histogram(
        metrics::MEMORY_PHASE_TWO_TOKEN_USAGE,
        token_usage.total_tokens.max(0),
        &[("token_type", "total")],
    );
    otel.histogram(
        metrics::MEMORY_PHASE_TWO_TOKEN_USAGE,
        token_usage.input_tokens.max(0),
        &[("token_type", "input")],
    );
    otel.histogram(
        metrics::MEMORY_PHASE_TWO_TOKEN_USAGE,
        token_usage.cached_input(),
        &[("token_type", "cached_input")],
    );
    otel.histogram(
        metrics::MEMORY_PHASE_TWO_TOKEN_USAGE,
        token_usage.output_tokens.max(0),
        &[("token_type", "output")],
    );
    otel.histogram(
        metrics::MEMORY_PHASE_TWO_TOKEN_USAGE,
        token_usage.reasoning_output_tokens.max(0),
        &[("token_type", "reasoning_output")],
    );
}
