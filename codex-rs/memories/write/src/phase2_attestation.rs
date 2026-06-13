use crate::artifacts::EXTENSIONS_SUBDIR;
use crate::artifacts::RAW_MEMORIES_FILENAME;
use crate::artifacts::ROLLOUT_SUMMARIES_SUBDIR;
use crate::stage_two;
use crate::workspace_diff;
use anyhow::Context;
use codex_core::config::Config;
use codex_protocol::user_input::UserInput;
use codex_state::Phase2AttestedBaseline;
use codex_state::Stage1Output;
use codex_state::StateRuntime;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::ffi::OsStr;
use std::path::Path;

const SCHEMA_VERSION: i64 = 1;
const MEMORY_INDEX_FILENAME: &str = "MEMORY.md";
const MEMORY_SUMMARY_FILENAME: &str = "memory_summary.md";

#[derive(Debug, Clone)]
pub(super) struct Phase2AttestationContext {
    pub memory_root_key: String,
    pub selection_sha256: String,
    pub prepared_inputs_sha256: String,
    pub consolidator_sha256: String,
    pub selected_count: i64,
}

pub(super) async fn capture_prepared_context(
    root: &Path,
    base_config: &Config,
    agent_config: &Config,
    prompt: &[UserInput],
    selected_outputs: &[Stage1Output],
) -> anyhow::Result<Phase2AttestationContext> {
    let memory_root_key = memory_root_key(root).await?;
    let selection_sha256 = hash_json(&selection_manifest(selected_outputs)?)?;
    let prepared_inputs_sha256 = hash_prepared_inputs_tree(root).await?;
    let consolidator_sha256 =
        hash_json(&consolidator_manifest(base_config, agent_config, prompt)?)?;

    Ok(Phase2AttestationContext {
        memory_root_key,
        selection_sha256,
        prepared_inputs_sha256,
        consolidator_sha256,
        selected_count: selected_outputs.len() as i64,
    })
}

pub(super) async fn validate_completed_run(
    root: &Path,
    context: &Phase2AttestationContext,
) -> anyhow::Result<String> {
    let observed_root_key = memory_root_key(root).await?;
    anyhow::ensure!(
        observed_root_key == context.memory_root_key,
        "memory root changed during phase-2 consolidation"
    );
    validate_required_outputs(root).await?;
    current_output_tree_sha256(root).await
}

pub(super) async fn current_output_tree_sha256(root: &Path) -> anyhow::Result<String> {
    hash_tree(root, TreeHashMode::Output).await
}

pub(super) async fn matching_attested_baseline_exists(
    db: &StateRuntime,
    memory_root_key: &str,
    output_tree_sha256: &str,
) -> anyhow::Result<bool> {
    Ok(db
        .get_phase2_attested_baseline(memory_root_key, output_tree_sha256)
        .await?
        .is_some())
}

pub(super) async fn record_completed_baseline(
    db: &StateRuntime,
    context: &Phase2AttestationContext,
    output_tree_sha256: String,
    completion_watermark: i64,
) -> anyhow::Result<()> {
    let attested_at = chrono::Utc::now().timestamp();
    db.record_phase2_attested_baseline(&Phase2AttestedBaseline {
        memory_root_key: context.memory_root_key.clone(),
        output_tree_sha256,
        schema_version: SCHEMA_VERSION,
        selection_sha256: context.selection_sha256.clone(),
        prepared_inputs_sha256: context.prepared_inputs_sha256.clone(),
        consolidator_sha256: context.consolidator_sha256.clone(),
        completion_watermark,
        selected_count: context.selected_count,
        attested_at,
    })
    .await
}

pub(super) async fn memory_root_key(root: &Path) -> anyhow::Result<String> {
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let canonical = std::fs::canonicalize(&root)
            .with_context(|| format!("canonicalize memory root {}", root.display()))?;
        Ok::<_, anyhow::Error>(canonical.to_string_lossy().into_owned())
    })
    .await?
}

async fn validate_required_outputs(root: &Path) -> anyhow::Result<()> {
    let index_path = root.join(MEMORY_INDEX_FILENAME);
    let summary_path = root.join(MEMORY_SUMMARY_FILENAME);

    let index = tokio::fs::read_to_string(&index_path)
        .await
        .with_context(|| format!("read required memory index {}", index_path.display()))?;
    anyhow::ensure!(
        !index.trim().is_empty(),
        "{} must not be empty after phase-2 consolidation",
        MEMORY_INDEX_FILENAME
    );

    let summary = tokio::fs::read_to_string(&summary_path)
        .await
        .with_context(|| format!("read required memory summary {}", summary_path.display()))?;
    anyhow::ensure!(
        !summary.trim().is_empty(),
        "{} must not be empty after phase-2 consolidation",
        MEMORY_SUMMARY_FILENAME
    );
    anyhow::ensure!(
        summary.lines().next() == Some("v1"),
        "{} must start with schema line `v1`",
        MEMORY_SUMMARY_FILENAME
    );

    Ok(())
}

async fn hash_prepared_inputs_tree(root: &Path) -> anyhow::Result<String> {
    hash_tree(root, TreeHashMode::PreparedInputs).await
}

#[cfg(test)]
pub(super) async fn prepared_inputs_tree_sha256_for_tests(root: &Path) -> anyhow::Result<String> {
    hash_prepared_inputs_tree(root).await
}

async fn hash_tree(root: &Path, mode: TreeHashMode) -> anyhow::Result<String> {
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || hash_tree_blocking(&root, mode)).await?
}

fn hash_tree_blocking(root: &Path, mode: TreeHashMode) -> anyhow::Result<String> {
    let mut entries = Vec::new();
    collect_tree_entries(root, root, mode, &mut entries)?;
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    hash_json(&entries)
}

fn collect_tree_entries(
    root: &Path,
    dir: &Path,
    mode: TreeHashMode,
    entries: &mut Vec<TreeEntryHash>,
) -> anyhow::Result<()> {
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("read directory {}", dir.display()))?
    {
        let entry = entry.with_context(|| format!("read entry in {}", dir.display()))?;
        let path = entry.path();
        let relative = path.strip_prefix(root).with_context(|| {
            format!(
                "compute relative path for {} under {}",
                path.display(),
                root.display()
            )
        })?;

        if should_skip_entry(relative, mode) {
            continue;
        }

        let file_type = entry
            .file_type()
            .with_context(|| format!("read file type for {}", path.display()))?;
        if file_type.is_symlink() {
            anyhow::bail!(
                "refusing to attest symlink in memory workspace: {}",
                relative_path_for_hash(relative)
            );
        }
        if file_type.is_dir() {
            collect_tree_entries(root, &path, mode, entries)?;
            continue;
        }
        if !file_type.is_file() {
            anyhow::bail!(
                "refusing to attest non-regular file in memory workspace: {}",
                relative_path_for_hash(relative)
            );
        }

        let bytes =
            std::fs::read(&path).with_context(|| format!("read file {}", path.display()))?;
        entries.push(TreeEntryHash {
            path: relative_path_for_hash(relative),
            sha256: sha256_hex(&bytes),
        });
    }

    Ok(())
}

fn should_skip_entry(relative: &Path, mode: TreeHashMode) -> bool {
    if relative
        .components()
        .next()
        .and_then(|component| component.as_os_str().to_str())
        == Some(".git")
    {
        return true;
    }

    match mode {
        TreeHashMode::Output => relative == Path::new(workspace_diff::FILENAME),
        TreeHashMode::PreparedInputs => !is_prepared_input_path(relative),
    }
}

fn is_prepared_input_path(relative: &Path) -> bool {
    if relative == Path::new(RAW_MEMORIES_FILENAME) {
        return true;
    }
    if relative == Path::new(workspace_diff::FILENAME) {
        return true;
    }

    let mut components = relative.components();
    let Some(first) = components.next() else {
        return false;
    };
    first.as_os_str() == OsStr::new(ROLLOUT_SUMMARIES_SUBDIR)
        || first.as_os_str() == OsStr::new(EXTENSIONS_SUBDIR)
}

fn relative_path_for_hash(relative: &Path) -> String {
    relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[derive(Clone, Copy)]
enum TreeHashMode {
    Output,
    PreparedInputs,
}

#[derive(Serialize)]
struct TreeEntryHash {
    path: String,
    sha256: String,
}

#[derive(Serialize)]
struct SelectionEntry {
    thread_id: String,
    rollout_path: String,
    source_updated_at: i64,
    raw_memory: String,
    rollout_summary: String,
    rollout_slug: Option<String>,
    cwd: String,
    git_branch: Option<String>,
    generated_at: i64,
}

fn selection_manifest(selected_outputs: &[Stage1Output]) -> anyhow::Result<Vec<SelectionEntry>> {
    let mut entries = selected_outputs
        .iter()
        .map(|output| SelectionEntry {
            thread_id: output.thread_id.to_string(),
            rollout_path: output.rollout_path.to_string_lossy().into_owned(),
            source_updated_at: output.source_updated_at.timestamp(),
            raw_memory: output.raw_memory.clone(),
            rollout_summary: output.rollout_summary.clone(),
            rollout_slug: output.rollout_slug.clone(),
            cwd: output.cwd.to_string_lossy().into_owned(),
            git_branch: output.git_branch.clone(),
            generated_at: output.generated_at.timestamp(),
        })
        .collect::<Vec<_>>();
    entries.sort_by(|a, b| {
        a.thread_id
            .cmp(&b.thread_id)
            .then_with(|| a.rollout_path.cmp(&b.rollout_path))
            .then_with(|| a.generated_at.cmp(&b.generated_at))
    });
    Ok(entries)
}

#[derive(Serialize)]
struct ConsolidatorManifest<'a> {
    model_provider_id: &'a str,
    model: &'a str,
    reasoning_effort: String,
    approval_policy: String,
    sandbox_policy: codex_protocol::protocol::SandboxPolicy,
    disabled_features: &'a [&'a str],
    prompt: &'a [UserInput],
}

fn consolidator_manifest<'a>(
    base_config: &'a Config,
    agent_config: &'a Config,
    prompt: &'a [UserInput],
) -> anyhow::Result<ConsolidatorManifest<'a>> {
    let model = agent_config
        .model
        .as_deref()
        .or(base_config.model.as_deref())
        .unwrap_or("unknown");
    let reasoning_effort = agent_config
        .model_reasoning_effort
        .as_ref()
        .unwrap_or(&stage_two::REASONING_EFFORT)
        .to_string();
    let sandbox_policy = agent_config.legacy_sandbox_policy();
    anyhow::ensure!(
        !matches!(
            sandbox_policy,
            codex_protocol::protocol::SandboxPolicy::DangerFullAccess
        ),
        "phase-2 consolidator must not run with danger-full-access"
    );

    Ok(ConsolidatorManifest {
        model_provider_id: base_config.model_provider_id.as_str(),
        model,
        reasoning_effort,
        approval_policy: agent_config.permissions.approval_policy.value().to_string(),
        sandbox_policy,
        disabled_features: &[
            "SpawnCsv",
            "Collab",
            "MemoryTool",
            "Apps",
            "Plugins",
            "SkillMcpDependencyInstall",
        ],
        prompt,
    })
}

fn hash_json(value: &impl Serialize) -> anyhow::Result<String> {
    let bytes = serde_json::to_vec(value)?;
    Ok(sha256_hex(&bytes))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    hex
}
