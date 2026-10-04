//! Finite, explicit reader for an already-existing rollout byte range.
//!
//! The envelope adapter retains only typed allowlisted metadata. The shared
//! diagnostics reducer owns all event grouping and conservation logic.

mod envelopes;
mod summary;
mod usage;

use clap::Parser;
use clap::ValueEnum;
use codex_diagnostics::control_plane::CaptureMode;
use codex_diagnostics::control_plane::CoverageField;
use codex_diagnostics::control_plane::CoverageMark;
use codex_diagnostics::control_plane::ControlPlaneRecorder;
use codex_diagnostics::control_plane::EventInput;
use codex_diagnostics::control_plane::EventKind;
use codex_diagnostics::control_plane::ExternalOperationObservation;
use codex_diagnostics::control_plane::ObservationQuality;
use codex_diagnostics::control_plane::QueueObservation;
use codex_diagnostics::control_plane::QueueScope;
use codex_diagnostics::control_plane::ReturnWhen;
use codex_diagnostics::control_plane::SelectedOutcome;
use codex_diagnostics::control_plane::SourcePlane;
use codex_diagnostics::control_plane::StatusActorProjection;
use codex_diagnostics::control_plane::StatusQueryObservation;
use codex_diagnostics::control_plane::StatusRequestProjection;
use codex_diagnostics::control_plane::StatusResultProjection;
use codex_diagnostics::control_plane::Summary;
use codex_diagnostics::control_plane::TargetMode;
use codex_diagnostics::control_plane::TargetReferenceKind;
use codex_diagnostics::control_plane::UnknownReason;
use codex_diagnostics::control_plane::WaitObservation;
use codex_diagnostics::control_plane::WaitPhase;
use codex_diagnostics::control_plane::WaitPrimitive;
use envelopes::HostReturnWhen;
use serde_json::json;
use std::fs::File;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::PathBuf;
use std::time::SystemTime;

const MAX_RANGE_BYTES: u64 = 1_048_576;
const MAX_IDENTIFIER_BYTES: usize = 256;
const MAX_EVENTS: usize = 2_048;
const MAX_OUTPUT_BYTES: usize = 1_048_576;
const MAX_EVENT_PROJECTION_BYTES: usize = 262_144;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum SourcePlaneSelector {
    ExternalHostEnvelope,
}

#[derive(Debug, Parser)]
pub struct ControlPlaneCommand {
    /// Explicit rollout file selected by the operator; no discovery occurs.
    #[arg(long, value_name = "FILE")]
    input: PathBuf,
    /// Inclusive start byte of the selected input range.
    #[arg(long)]
    start_byte: u64,
    /// Exclusive end byte of the selected input range.
    #[arg(long)]
    end_byte: u64,
    /// Caller-supplied namespace used with actual call IDs for pairing.
    #[arg(long)]
    source_namespace: String,
    /// Currently supported source plane.
    #[arg(long, value_enum)]
    source_plane: SourcePlaneSelector,
    /// Optional authoritative root UUID selector. External paths never resolve it.
    #[arg(long)]
    root_thread_id: Option<String>,
    /// Explicit bounded request for an existing custodian-attested usage snapshot.
    #[arg(long, value_name = "REQUEST.json")]
    usage_request: Option<PathBuf>,
}

#[derive(Default)]
struct Coverage {
    root_filtered: usize,
    unknown_root: usize,
    rejected_events: usize,
    snapshot_unavailable: bool,
    projection_omitted: usize,
}

pub async fn run(command: ControlPlaneCommand) -> anyhow::Result<()> {
    let output = execute(command).await?;
    let encoded = serde_json::to_vec_pretty(&output)?;
    println!("{}", String::from_utf8_lossy(&encoded));
    Ok(())
}

/// The command's real read/parse/capture/reduce/render seam, shared with controls.
async fn execute(command: ControlPlaneCommand) -> anyhow::Result<serde_json::Value> {
    validate(&command)?;
    let mut raw = read_range(&command.input, command.start_byte, command.end_byte)?;
    let (observations, parser_coverage) =
        envelopes::parse(&raw, &command.source_namespace);
    raw.fill(0);

    let recorder = ControlPlaneRecorder::new(CaptureMode::Session, "external-host-envelope")
        .map_err(|_| anyhow::anyhow!("invalid recorder identity"))?;
    let mut coverage = Coverage::default();
    let mut event_projections = Vec::new();
    let mut projected_bytes = 0usize;
    for observation in observations.into_iter().take(MAX_EVENTS) {
        if command.root_thread_id.is_some() {
            coverage.root_filtered += 1;
            coverage.unknown_root += 1;
            continue;
        }
        let projection = project_envelope(&observation);
        let projection_size = serde_json::to_vec(&projection)?.len();
        if projected_bytes.saturating_add(projection_size) <= MAX_EVENT_PROJECTION_BYTES {
            projected_bytes += projection_size;
            event_projections.push(projection);
        } else {
            coverage.projection_omitted += 1;
        }
        let Some(event) = to_event(observation) else {
            coverage.rejected_events += 1;
            continue;
        };
        if recorder.record(event).is_none() {
            coverage.rejected_events += 1;
        }
    }
    let snapshot = recorder.snapshot();
    coverage.snapshot_unavailable = snapshot.is_none();
    let summary = snapshot.as_ref().map(Summary::reduce_snapshot);
    let usage_view = match command.usage_request.as_ref() {
        Some(path) => usage::run(path, &command.source_namespace, snapshot.as_ref()).await,
        None => json!({ "requested": false, "status": "notRequested", "join": null }),
    };
    let output = render(
        summary.as_ref(),
        &parser_coverage,
        &coverage,
        snapshot.as_ref(),
        &command,
        event_projections,
        usage_view,
    );
    let encoded = serde_json::to_vec_pretty(&output)?;
    if encoded.len() > MAX_OUTPUT_BYTES {
        return Ok(json!({"schemaVersion": 1, "complete": false, "errorCode": "OutputBoundExceeded"}));
    }
    Ok(output)
}

fn validate(command: &ControlPlaneCommand) -> anyhow::Result<()> {
    anyhow::ensure!(command.end_byte > command.start_byte, "byte range must be non-empty");
    anyhow::ensure!(
        command.end_byte - command.start_byte <= MAX_RANGE_BYTES,
        "byte range exceeds the 1 MiB limit"
    );
    match command.source_plane {
        SourcePlaneSelector::ExternalHostEnvelope => {}
    }
    anyhow::ensure!(
        !command.source_namespace.is_empty()
            && command.source_namespace.len() <= MAX_IDENTIFIER_BYTES
            && !command.source_namespace.chars().any(char::is_control),
        "source namespace is empty or exceeds the 256-byte limit"
    );
    anyhow::ensure!(
        command.root_thread_id.as_ref().is_none_or(|root| {
            !root.is_empty() && root.len() <= MAX_IDENTIFIER_BYTES && !root.chars().any(char::is_control)
        }),
        "root thread identifier is empty or exceeds the 256-byte limit"
    );
    Ok(())
}

fn read_range(path: &PathBuf, start: u64, end: u64) -> anyhow::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity((end - start) as usize);
    file.take(end - start).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() as u64 == end - start, "selected byte range is incomplete");
    Ok(bytes)
}

fn to_event(item: envelopes::EnvelopeObservation) -> Option<EventInput> {
    let duration_ns = item
        .returned_at
        .signed_duration_since(item.started_at)
        .num_nanoseconds()
        .and_then(|value| u64::try_from(value).ok());
    let event_kind;
    let mut wait = None;
    let mut status_query = None;
    let mut coverage = unknown_fields();
    let operation_id = match item.kind {
        envelopes::EnvelopeKind::StatusQuery => Some(item.call_id.clone()),
        envelopes::EnvelopeKind::Wait => item.wait_request.as_ref().and_then(|request| {
            matches!(request.return_when, HostReturnWhen::Any | HostReturnWhen::All)
                .then(|| item.call_id.clone())
        }),
    };
    match item.kind {
        envelopes::EnvelopeKind::Wait => {
            event_kind = EventKind::WaitCompleted;
            let request = item.wait_request?;
            let targets_present = !request.targets.is_empty();
            let mut observed_host_return = item.observed_host_return;
            if let Some(result) = &mut observed_host_return {
                let returned = result.target_statuses.iter().map(|row| row.target_reference.id.as_str())
                    .collect::<std::collections::BTreeSet<_>>();
                result.complete &= request.targets.iter().all(|target| returned.contains(target.as_str()))
                    && returned.len() == request.targets.len();
            }
            if !request.target_set_complete {
                coverage.push(CoverageMark {
                    field: CoverageField::TargetSet,
                    unknown: Some(UnknownReason::NotExposed),
                });
            }
            if request.timeout_ms.is_none() {
                coverage.push(CoverageMark {
                    field: CoverageField::RequestedTimeout,
                    unknown: Some(UnknownReason::NotExposed),
                });
            }
            wait = Some(WaitObservation {
                wait_id: item.call_id.clone(),
                phase: WaitPhase::Completed,
                primitive: WaitPrimitive::Unknown,
                return_when: match request.return_when {
                    HostReturnWhen::Any => ReturnWhen::Any,
                    HostReturnWhen::All => ReturnWhen::All,
                    HostReturnWhen::TargetTerminal | HostReturnWhen::Unknown => ReturnWhen::Unknown,
                },
                helper_id: Some("wait_agent".to_owned()),
                helper_version: None,
                requested_timeout_ms: request.timeout_ms,
                effective_timeout_ms: None,
                target_mode: if targets_present { TargetMode::Targeted } else { TargetMode::Untargeted },
                any_targets: None,
                target_ids: Vec::new(),
                target_set_complete: false,
                requested_target_ids: request.targets,
                requested_target_kind: TargetReferenceKind::ExposedAgentPath,
                requested_target_set_complete: request.target_set_complete,
                resolved_target_kind: TargetReferenceKind::Unknown,
                resolved_target_set_complete: Some(false),
                subscribed_readiness: Vec::new(),
                subscribed_readiness_complete: false,
                selected_readiness: Vec::new(),
                selected_readiness_complete: false,
                blocked_start_offset_ns: None,
                blocked_end_offset_ns: None,
                operation_duration_ns: duration_ns,
                blocked_duration_ns: None,
                // Host return metadata is retained below; it is not a native
                // selected branch, producer receipt or logical readiness proof.
                selected_outcome: SelectedOutcome::Unknown,
                selected_producer: None,
                selected_target_id: None,
                selected_target_turn_id: None,
                continuation_of_wait_id: None,
                request_fingerprint: None,
                result_fingerprint: None,
                observed_host_return,
            });
        }
        envelopes::EnvelopeKind::StatusQuery => {
            event_kind = EventKind::StatusQueryObserved;
            let request = item.status_request?;
            let result = item.status_result?;
            let actor_rows = result
                .actors
                .into_iter()
                .map(|actor| StatusActorProjection {
                    agent_id: actor.agent_id,
                    canonical_path: actor.canonical_path,
                    configured_model: actor.configured_model,
                    configured_reasoning_effort: actor.configured_effort,
                    raw_status_tag: Some(actor.status.as_static_tag().to_owned()),
                })
                .collect();
            status_query = Some(StatusQueryObservation {
                request_fingerprint: None,
                result_fingerprint: None,
                readiness: None,
                request_projection: Some(StatusRequestProjection {
                    path_prefix: request.path_prefix,
                    requested_agent_ids: Vec::new(),
                    complete: true,
                }),
                result_projection: Some(StatusResultProjection {
                    observed_actor_count: u64::try_from(result.actor_count).ok(),
                    actors: actor_rows,
                    complete: result.complete,
                }),
            });
            if !result.complete {
                coverage.push(CoverageMark {
                    field: CoverageField::Other,
                    unknown: Some(UnknownReason::Truncated),
                });
            }
        }
    }
    Some(EventInput {
        source_plane: SourcePlane::ExternalHostEnvelope,
        producer_boundary: "externalHostEnvelope".to_owned(),
        producer_version: None,
        kind: event_kind,
        quality: ObservationQuality::ObservedOnly,
        wall_correlation: SystemTime::from(item.returned_at),
        monotonic_offset_ns: None,
        operation_id,
        thread_id: None,
        turn_id: None,
        root_thread_id: None,
        parent_thread_id: None,
        fork_parent_thread_id: None,
        window_id: None,
        window_number: None,
        previous_window_id: None,
        wait,
        sleep: None,
        readiness: None,
        status_query,
        queue: Some(QueueObservation {
            scope: QueueScope::Unknown,
            transition_sequence: None,
            queue_before: None,
            queue_after: None,
        }),
        field_coverage: coverage,
        message: None,
        scheduler: None,
        provider_call: None,
        external_operation: Some(ExternalOperationObservation {
            call_id: item.call_id,
            returned_duration_ns: duration_ns,
        }),
    })
}

fn unknown_fields() -> Vec<CoverageMark> {
    [
        CoverageField::ThreadId,
        CoverageField::RootThreadId,
        CoverageField::TurnId,
        CoverageField::EffectiveTimeout,
        CoverageField::SelectedProducer,
        CoverageField::QueueState,
        CoverageField::SemanticAcknowledgement,
        CoverageField::ProviderObservedIdentity,
    ]
    .into_iter()
    .map(|field| CoverageMark {
        field,
        unknown: Some(if field == CoverageField::SelectedProducer {
            UnknownReason::UnsupportedProducer
        } else {
            UnknownReason::NotExposed
        }),
    })
    .collect()
}

fn project_envelope(item: &envelopes::EnvelopeObservation) -> serde_json::Value {
    let wait = item.wait_request.as_ref().map(|request| {
        let return_when = match request.return_when {
            HostReturnWhen::Any => "any",
            HostReturnWhen::All => "all",
            HostReturnWhen::TargetTerminal => "target_terminal",
            HostReturnWhen::Unknown => "unknown",
        };
        json!({
            "targetMode": if request.targets.is_empty() { "untargeted" } else { "targeted" },
            "requestedTargetReferences": &request.targets,
            "requestedTargetKind": "exposedAgentPath",
            "requestedTargetSetComplete": request.target_set_complete,
            "requestedTimeoutMs": request.timeout_ms,
            "returnWhen": return_when,
            "resolvedTargetIds": [],
            "resolvedTargetSet": "unknown"
        })
    });
    json!({
        "operation": match item.kind {
            envelopes::EnvelopeKind::Wait => "wait_agent",
            envelopes::EnvelopeKind::StatusQuery => "list_agents",
        },
        "callId": &item.call_id,
        "requestObservedAt": item.started_at.to_rfc3339(),
        "returnObservedAt": item.returned_at.to_rfc3339(),
        "observedRequestReturnDurationNs": item.returned_at
            .signed_duration_since(item.started_at)
            .num_nanoseconds()
            .and_then(|value| u64::try_from(value).ok()),
        "wait": wait,
        "hostReturn": {
            "timedOut": item.timed_out,
            "timeoutReasonObserved": item.host_timeout_reason,
            "targetTerminalReasonObserved": item.host_target_terminal,
            "targetStatusWakeCauseObserved": item.host_target_status_wake,
            "queuedUpdateCount": item.queued_update_count,
            "targetStatusPresent": item.target_status_observed,
            "winner": "unknown",
            "blockedDuration": "unknown",
            "enqueuePendingAcknowledgement": "unknown"
        },
        "statusQuery": item.status_result.as_ref().map(|result| json!({
            "pathPrefix": item.status_request.as_ref().and_then(|request| request.path_prefix.as_deref()),
            "observedActorCount": result.actor_count,
            "complete": result.complete,
            "actors": result.actors.iter().map(|actor| json!({
                "actorId": &actor.agent_id,
                "canonicalPath": &actor.canonical_path,
                "configuredModel": &actor.configured_model,
                "configuredReasoningEffort": &actor.configured_effort,
                "statusTag": actor.status.as_static_tag()
            })).collect::<Vec<_>>(),
            "readiness": "unknown",
            "resolvedThreadOrIncarnation": "unknown"
        }))
    })
}

fn render(
    summary: Option<&Summary>,
    parser: &envelopes::EnvelopeCoverage,
    coverage: &Coverage,
    snapshot: Option<&codex_diagnostics::control_plane::RecorderSnapshot>,
    command: &ControlPlaneCommand,
    event_projections: Vec<serde_json::Value>,
    usage_view: serde_json::Value,
) -> serde_json::Value {
    let event_count = summary.map_or(0, |value| value.event_count);
    json!({
        "schemaVersion": 1,
        "sourcePlane": "externalHostEnvelope",
        "selectedRange": {"startByte": command.start_byte, "endByteExclusive": command.end_byte},
        "recorder": snapshot.map(|snapshot| json!({
            "captureInstanceId": &snapshot.capture_instance_id,
            "highWater": snapshot.high_water,
            "losses": {
                "contention": snapshot.losses.contention,
                "capacity": snapshot.losses.capacity,
                "disabled": snapshot.losses.disabled,
                "invalidIdentity": snapshot.losses.invalid_identity
            }
        })),
        "data": summary.map(summary::project),
        "coverage": {
            "malformedRecords": parser.malformed_records,
            "oversizedRecords": parser.oversized_records,
            "unmatchedOutputs": parser.unmatched_outputs,
            "unmatchedCalls": parser.unmatched_calls,
            "duplicateRecords": parser.duplicate_records,
            "conflictingPairs": parser.conflicting_pairs,
            "unsupportedRecords": parser.unsupported_records + coverage.rejected_events,
            "incompleteStatusResults": parser.incomplete_status_results,
            "projectionOmittedByBound": coverage.projection_omitted,
            "rootFilterExcluded": coverage.root_filtered,
            "unknownRoot": coverage.unknown_root,
            "parsedEvents": event_count,
            "snapshotUnavailable": coverage.snapshot_unavailable,
            "externalWinner": "notExposed",
            "enqueuePendingAckChain": "notExposed",
            "agentIncarnation": "notExposed",
            "providerEffectiveIdentity": "unverified",
            "usageCoverage": if usage_view["requested"] == false { "notRequested" } else { "requested" }
        },
        "events": event_projections,
        "usage": usage_view
    })
}

#[cfg(test)]
#[path = "control_plane_tests.rs"]
mod tests;
