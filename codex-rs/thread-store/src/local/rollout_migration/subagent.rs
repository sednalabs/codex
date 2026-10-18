//! Selects a smaller resume context for legacy subagent rollouts.
//!
//! Legacy subagents copied the parent's full rollout into every child rollout. Migrating those
//! files verbatim can preserve gigabytes of duplicated history even though a resumed subagent only
//! needs its latest bounded model context.
//!
//! This module reverse-scans the legacy rollout for the newest compaction with replacement history
//! and a window number. Malformed records are skipped. If no such compaction exists, the caller
//! falls back to full tolerant replay.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::File;
use std::path::PathBuf;

use codex_protocol::AgentPath;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_rollout::ModelContextScan;
use codex_rollout::ModelContextScanProgress;
use codex_rollout::ResponseItemEnvelope;
use codex_rollout::ReverseJsonlScanner;
use codex_rollout::RolloutItem;
use codex_rollout::ScanOutcome;
use serde_json::Value;

use super::line_parser;
use super::migration_error;
use crate::ThreadStoreResult;

pub(super) type AdmissionIdempotencyScope = (ThreadId, ThreadId, ThreadId, String);

pub(super) fn admission_idempotency_scope(
    root_thread_id: &ThreadId,
    sender_thread_id: &ThreadId,
    recipient_thread_id: &ThreadId,
    idempotency_key: &str,
) -> AdmissionIdempotencyScope {
    (
        *root_thread_id,
        *sender_thread_id,
        *recipient_thread_id,
        idempotency_key.to_string(),
    )
}

/// Returns the suffix starting at the newest usable compaction, if one exists.
pub(super) async fn select_bounded_context(
    rollout_path: PathBuf,
) -> ThreadStoreResult<Option<Vec<RolloutItem>>> {
    tokio::task::spawn_blocking(move || {
        let file = File::open(rollout_path).map_err(migration_error)?;
        let mut scanner = ReverseJsonlScanner::new(file)
            .map_err(migration_error)?
            .with_max_record_bytes(super::MAX_ROLLOUT_LINE_BYTES);
        let mut scan = Some(ModelContextScan::default());
        let mut bounded_context = None;
        let mut admissions_by_id = HashMap::new();
        let mut admission_ids_newest_first = Vec::new();
        let mut admission_idempotency_scopes = HashSet::new();
        let mut represented_response_ids = HashMap::new();
        let mut represented_update_ids = HashSet::new();

        while let Some(outcome) = scanner.scan_next::<Value>().map_err(migration_error)? {
            let value = match outcome {
                ScanOutcome::Parsed(value) => value,
                ScanOutcome::Rejected(_) => continue,
            };
            let Ok(Some(line)) = line_parser::parse_legacy_rollout_value(value) else {
                continue;
            };
            collect_response_ids(
                &line.item,
                &mut represented_response_ids,
                &mut represented_update_ids,
            )?;
            if let RolloutItem::InterAgentCommunicationMetadata {
                trigger_turn,
                admission: Some(admission),
            } = &line.item
            {
                if *trigger_turn {
                    return Err(migration_error(
                        "explicit inter-agent admission cannot use legacy trigger-turn behavior",
                    ));
                }
                validate_admission_fields(&line.item)?;
                if !admission_idempotency_scopes.insert(admission_idempotency_scope(
                    &admission.root_thread_id,
                    &admission.sender_thread_id,
                    &admission.recipient_thread_id,
                    &admission.idempotency_key,
                )) {
                    return Err(migration_error(
                        "legacy rollout contains duplicate explicit inter-agent idempotency keys in scope",
                    ));
                }
                if admissions_by_id
                    .insert(admission.update_id.clone(), line.item.clone())
                    .is_some()
                {
                    return Err(migration_error(
                        "legacy rollout contains duplicate explicit inter-agent admission IDs",
                    ));
                }
                admission_ids_newest_first.push(admission.update_id.clone());
            }

            let bounded_context_complete = scan.as_mut().is_some_and(|context_scan| {
                context_scan.push(line.item.clone()) == ModelContextScanProgress::Complete
            });
            if bounded_context_complete {
                let mut items = scan
                    .take()
                    .ok_or_else(|| migration_error("bounded context scan is not active"))?
                    .finish();
                items.retain(|item| !matches!(item, RolloutItem::SessionMeta(_)));
                bounded_context = Some(items);
            }
        }

        let Some(mut items) = bounded_context else {
            return Ok(None);
        };

        if represented_update_ids
            .iter()
            .any(|update_id| !admissions_by_id.contains_key(update_id))
        {
            return Err(migration_error(
                "legacy rollout contains an inter-agent representation without its admission",
            ));
        }

        for item in &mut items {
            fold_representation_id(item, &represented_response_ids)?;
        }

        let retained_admission_ids = items
            .iter()
            .filter_map(|item| match item {
                RolloutItem::InterAgentCommunicationMetadata {
                    admission: Some(admission),
                    ..
                } => Some(admission.update_id.clone()),
                _ => None,
            })
            .collect::<HashSet<_>>();
        let newest_compaction_index = items
            .iter()
            .rposition(|item| matches!(item, RolloutItem::Compacted(_)))
            .ok_or_else(|| {
                migration_error("bounded subagent context is missing its retained compaction")
            })?;

        // Keep only the omitted pre-compaction admissions as a source-ordered prefix after the
        // selected compaction. Admissions already in the bounded suffix stay at their original
        // positions so legacy metadata/response adjacency and ordering are unchanged.
        let mut omitted_admissions = Vec::new();
        for update_id in admission_ids_newest_first.into_iter().rev() {
            if retained_admission_ids.contains(&update_id) {
                continue;
            }
            let mut item = admissions_by_id
                .remove(&update_id)
                .ok_or_else(|| migration_error("indexed admission has no rollout record"))?;
            fold_representation_id(&mut item, &represented_response_ids)?;
            omitted_admissions.push(item);
        }
        let insertion_index = newest_compaction_index + 1;
        items.splice(insertion_index..insertion_index, omitted_admissions);
        Ok(Some(items))
    })
    .await
    .map_err(migration_error)?
}

fn validate_admission_fields(item: &RolloutItem) -> ThreadStoreResult<()> {
    let RolloutItem::InterAgentCommunicationMetadata {
        admission: Some(admission),
        ..
    } = item
    else {
        return Ok(());
    };
    if admission.idempotency_key.trim().is_empty()
        || admission.original_communication.id.as_ref() != Some(&admission.update_id)
        || admission.original_communication.author != admission.sender_path
        || admission.original_communication.recipient != admission.recipient_path
        || admission.root_thread_id != admission.recipient_thread_id
        || !admission.recipient_path.is_root()
        || admission.original_communication.trigger_turn
        || !admission.original_communication.other_recipients.is_empty()
    {
        return Err(migration_error(
            "explicit inter-agent admission has inconsistent identity or scope",
        ));
    }
    Ok(())
}

fn collect_response_ids(
    item: &RolloutItem,
    response_ids: &mut HashMap<ResponseItemId, ResponseItemId>,
    update_ids: &mut HashSet<ResponseItemId>,
) -> ThreadStoreResult<()> {
    match item {
        RolloutItem::ResponseItem(response) => {
            collect_response_id(response, response_ids, update_ids)?;
        }
        RolloutItem::Compacted(compacted) => {
            for response in compacted.replacement_history.iter().flatten() {
                collect_response_id(response, response_ids, update_ids)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn collect_response_id(
    response: &ResponseItemEnvelope,
    response_ids: &mut HashMap<ResponseItemId, ResponseItemId>,
    update_ids: &mut HashSet<ResponseItemId>,
) -> ThreadStoreResult<()> {
    let Some(update_id) = response
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.inter_agent_update_id.as_ref())
    else {
        return Ok(());
    };
    if !matches!(&response.item, ResponseItem::AgentMessage { .. }) {
        return Err(migration_error(
            "explicit inter-agent update identity is attached to a non-agent response",
        ));
    }
    let response_id = response.item.id().cloned().ok_or_else(|| {
        migration_error("explicit inter-agent representation is missing its response ID")
    })?;
    if let Some(existing) = response_ids.get(update_id)
        && existing != &response_id
    {
        return Err(migration_error(
            "explicit inter-agent update has conflicting response IDs",
        ));
    }
    response_ids.insert(update_id.clone(), response_id);
    update_ids.insert(update_id.clone());
    Ok(())
}

fn fold_representation_id(
    item: &mut RolloutItem,
    response_ids: &HashMap<ResponseItemId, ResponseItemId>,
) -> ThreadStoreResult<()> {
    validate_admission_fields(item)?;
    let RolloutItem::InterAgentCommunicationMetadata {
        trigger_turn,
        admission: Some(admission),
    } = item
    else {
        return Ok(());
    };
    if *trigger_turn {
        return Err(migration_error(
            "explicit inter-agent admission cannot use legacy trigger-turn behavior",
        ));
    }
    if let Some(response_id) = response_ids.get(&admission.update_id) {
        if admission
            .represented_response_id
            .as_ref()
            .is_some_and(|recorded| recorded != response_id)
        {
            return Err(migration_error(
                "explicit inter-agent receipt conflicts with its response identity",
            ));
        }
        admission.represented_response_id = Some(response_id.clone());
    }
    Ok(())
}
