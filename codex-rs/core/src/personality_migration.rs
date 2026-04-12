use crate::config::ConfigToml;
use crate::config::edit::ConfigEditsBuilder;
use crate::rollout::ARCHIVED_SESSIONS_SUBDIR;
use crate::rollout::SESSIONS_SUBDIR;
use crate::state_db;
use codex_protocol::config_types::Personality;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionSource;
use std::io;
use std::path::Path;
use tokio::fs::OpenOptions;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;

pub const PERSONALITY_MIGRATION_FILENAME: &str = ".personality_migration";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersonalityMigrationStatus {
    SkippedMarker,
    SkippedExplicitPersonality,
    SkippedNoSessions,
    Applied,
}

pub async fn maybe_migrate_personality(
    codex_home: &Path,
    config_toml: &ConfigToml,
) -> io::Result<PersonalityMigrationStatus> {
    let marker_path = codex_home.join(PERSONALITY_MIGRATION_FILENAME);
    if tokio::fs::try_exists(&marker_path).await? {
        return Ok(PersonalityMigrationStatus::SkippedMarker);
    }

    let config_profile = config_toml
        .get_config_profile(/*override_profile*/ None)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    if config_toml.personality.is_some() || config_profile.personality.is_some() {
        create_marker(&marker_path).await?;
        return Ok(PersonalityMigrationStatus::SkippedExplicitPersonality);
    }

    let model_provider_id = config_profile
        .model_provider
        .or_else(|| config_toml.model_provider.clone())
        .unwrap_or_else(|| "openai".to_string());

    if !has_recorded_sessions(codex_home, model_provider_id.as_str()).await? {
        create_marker(&marker_path).await?;
        return Ok(PersonalityMigrationStatus::SkippedNoSessions);
    }

    ConfigEditsBuilder::new(codex_home)
        .set_personality(Some(Personality::Pragmatic))
        .apply()
        .await
        .map_err(|err| {
            io::Error::other(format!("failed to persist personality migration: {err}"))
        })?;

    create_marker(&marker_path).await?;
    Ok(PersonalityMigrationStatus::Applied)
}

async fn has_recorded_sessions(codex_home: &Path, default_provider: &str) -> io::Result<bool> {
    let allowed_sources: &[SessionSource] = &[];

    if let Some(state_db_ctx) = state_db::open_if_present(codex_home, default_provider).await
        && let Some(ids) = state_db::list_thread_ids_db(
            Some(state_db_ctx.as_ref()),
            codex_home,
            /*page_size*/ 1,
            /*cursor*/ None,
            ThreadSortKey::CreatedAt,
            allowed_sources,
            /*model_providers*/ None,
            /*archived_only*/ false,
            "personality_migration",
        )
        .await
        && !ids.is_empty()
    {
        return Ok(true);
    }

    if rollout_tree_has_user_session(&codex_home.join(SESSIONS_SUBDIR)).await? {
        return Ok(true);
    }

    rollout_tree_has_user_session(&codex_home.join(ARCHIVED_SESSIONS_SUBDIR)).await
}

async fn rollout_tree_has_user_session(root: &Path) -> io::Result<bool> {
    let mut to_visit = vec![root.to_path_buf()];

    while let Some(dir) = to_visit.pop() {
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };

        while let Some(entry) = entries.next_entry().await? {
            let file_type = entry.file_type().await?;
            let path = entry.path();

            if file_type.is_dir() {
                to_visit.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }

            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !name.starts_with("rollout-") || !name.ends_with(".jsonl") {
                continue;
            }

            if rollout_file_has_user_session(&path).await? {
                return Ok(true);
            }
        }
    }

    Ok(false)
}

async fn rollout_file_has_user_session(path: &Path) -> io::Result<bool> {
    let file = tokio::fs::File::open(path).await?;
    let reader = tokio::io::BufReader::new(file);
    let mut lines = reader.lines();
    let mut saw_session_meta = false;
    let mut saw_user_event = false;

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Ok(rollout_line) = serde_json::from_str::<RolloutLine>(trimmed) else {
            continue;
        };

        match rollout_line.item {
            RolloutItem::SessionMeta(_) => saw_session_meta = true,
            RolloutItem::EventMsg(EventMsg::UserMessage(_)) => saw_user_event = true,
            _ => {}
        }

        if saw_session_meta && saw_user_event {
            return Ok(true);
        }
    }

    Ok(false)
}

async fn create_marker(marker_path: &Path) -> io::Result<()> {
    match OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(marker_path)
        .await
    {
        Ok(mut file) => file.write_all(b"v1\n").await,
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
#[path = "personality_migration_tests.rs"]
mod tests;
