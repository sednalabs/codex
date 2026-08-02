//! Compatibility normalization for spawn-request provenance.

use codex_protocol::openai_models::ReasoningEffort;

use crate::protocol::v2::CollabAgentTool;
use crate::protocol::v2::CollabAgentToolCallStatus;
use crate::protocol::v2::ThreadItem;

/// Normalizes the V1 omitted-override sentinel before it reaches v2 request provenance.
///
/// Legacy rollout records encoded an absent model/effort override as an empty model paired with
/// the default `Medium` effort. The pair is meaningful only on legacy spawn-start boundaries;
/// callers must not use this to normalize a confirmed terminal effective identity.
pub(crate) fn normalize_legacy_spawn_requested_identity(
    model: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
) -> (Option<String>, Option<ReasoningEffort>) {
    if model.as_deref() == Some("") && reasoning_effort == Some(ReasoningEffort::Medium) {
        (None, None)
    } else {
        (model, reasoning_effort)
    }
}

/// Lifts the required legacy spawn-begin fields into optional v2 provenance.
///
/// The original event contract represented an omitted override as an empty
/// model and default effort. A required legacy field cannot separately encode
/// an explicit default effort, so retain that historical sentinel at this
/// boundary instead of inventing an override in the richer v2 representation.
pub(crate) fn normalize_required_legacy_spawn_requested_identity(
    model: String,
    reasoning_effort: ReasoningEffort,
) -> (Option<String>, Option<ReasoningEffort>) {
    (
        (!model.is_empty()).then_some(model),
        (reasoning_effort != ReasoningEffort::default()).then_some(reasoning_effort),
    )
}

/// Moves request provenance out of pre-v2 canonical spawn-start fields.
///
/// Old `ItemStarted` snapshots stored the requested model/effort in `model` and
/// `reasoning_effort`; a terminal item later inherits those values through
/// `merge_spawn_request_provenance`. Translate that shape before the merge so the V1 omitted
/// override sentinel cannot be promoted back into request provenance on completion.
pub(crate) fn normalize_legacy_canonical_spawn_start_provenance(item: &mut ThreadItem) {
    let ThreadItem::CollabAgentToolCall {
        tool,
        status,
        model,
        reasoning_effort,
        requested_model,
        requested_reasoning_effort,
        ..
    } = item
    else {
        return;
    };
    if tool != &CollabAgentTool::SpawnAgent
        || status != &CollabAgentToolCallStatus::InProgress
        || requested_model.is_some()
        || requested_reasoning_effort.is_some()
    {
        return;
    }

    let (normalized_model, normalized_reasoning_effort) =
        normalize_legacy_spawn_requested_identity(model.take(), reasoning_effort.take());
    *requested_model = normalized_model;
    *requested_reasoning_effort = normalized_reasoning_effort;
}

/// Normalizes the V1 omitted-override sentinel from a terminal spawn that never created a
/// receiver. Such a terminal has no confirmed effective child identity, so the legacy pair must
/// not be presented as one. A terminal with a receiver retains its effective values unchanged.
pub(crate) fn normalize_legacy_failed_spawn_effective_identity(
    has_receiver: bool,
    model: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
) -> (Option<String>, Option<ReasoningEffort>) {
    if has_receiver {
        (model, reasoning_effort)
    } else {
        normalize_legacy_spawn_requested_identity(model, reasoning_effort)
    }
}

/// Applies the failed/no-receiver terminal normalization to canonical persisted snapshots.
pub(crate) fn normalize_legacy_failed_canonical_spawn_terminal_effective_identity(
    item: &mut ThreadItem,
) {
    let ThreadItem::CollabAgentToolCall {
        tool,
        status,
        receiver_thread_ids,
        model,
        reasoning_effort,
        ..
    } = item
    else {
        return;
    };
    if tool != &CollabAgentTool::SpawnAgent || status != &CollabAgentToolCallStatus::Failed {
        return;
    }

    let (normalized_model, normalized_reasoning_effort) =
        normalize_legacy_failed_spawn_effective_identity(
            !receiver_thread_ids.is_empty(),
            model.take(),
            reasoning_effort.take(),
        );
    *model = normalized_model;
    *reasoning_effort = normalized_reasoning_effort;
}
