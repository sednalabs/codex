use std::collections::{BTreeMap, BTreeSet};

use super::types::*;

pub const MAX_REDUCER_EVENTS: usize = 2048;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WaitTimeline {
    pub capture_instance_id: String,
    pub source_plane: SourcePlane,
    pub thread_id: Option<String>,
    pub operation_id: Option<String>,
    pub wait_id: String,
    pub primitive: WaitPrimitive,
    pub requested_timeout_ms: Option<i64>,
    pub effective_timeout_ms: Option<u64>,
    pub blocked_start_offset_ns: Option<u64>,
    pub blocked_end_offset_ns: Option<u64>,
    pub operation_duration_ns: Option<u64>,
    pub blocked_duration_ns: Option<u64>,
    pub selected_outcome: SelectedOutcome,
    pub selected_producer: Option<EventIdentity>,
    pub subscribed_readiness: Vec<ReadinessObservation>,
    pub selected_readiness: Vec<ReadinessObservation>,
    pub coverage: Vec<CoverageMark>,
    pub complete: bool,
    pub conflicting: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SleepTimeline {
    pub capture_instance_id: String,
    pub source_plane: SourcePlane,
    pub thread_id: Option<String>,
    pub operation_id: Option<String>,
    pub pending_activity: SleepPendingActivity,
    pub selection: SleepSelection,
    pub requested_duration_ns: Option<u64>,
    pub operation_duration_ns: Option<u64>,
    pub blocked_duration_ns: Option<u64>,
    pub complete: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchedulerTimeline {
    pub capture_instance_id: String,
    pub source_plane: SourcePlane,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub correlation_id: Option<String>,
    pub eligible: Option<bool>,
    pub eligibility_basis: EligibilityBasis,
    pub pending_mail_observed: Option<bool>,
    pub trigger_turn_mail_observed: Option<bool>,
    pub durable_sleep_observed: Option<bool>,
    pub reservation_accepted: Option<bool>,
    pub reservation_still_matches: Option<bool>,
    pub reservation_lost: Option<bool>,
    pub task_registered: Option<bool>,
    pub turn_start_published: Option<bool>,
    pub drained_cohort_ids: Vec<String>,
    pub drained_cohort_complete: bool,
    pub drained_cohort_contains_trigger_turn_mail: Option<bool>,
    pub outcome: SchedulerOutcome,
    pub complete: bool,
    pub conflicting: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalDuration {
    pub identity: EventIdentity,
    pub source_plane: SourcePlane,
    pub thread_id: Option<String>,
    pub operation_id: Option<String>,
    pub call_id: String,
    pub observed_request_return_ns: Option<u64>,
    pub quality: ObservationQuality,
    pub conflicting: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueTimeline {
    pub capture_instance_id: String,
    pub source_plane: SourcePlane,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub observation: QueueObservation,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Summary {
    pub event_count: usize,
    pub duplicate_count: usize,
    /// Number of conflicting alternate payloads, counted once per identity.
    pub conflicting_identity_count: usize,
    pub by_source: BTreeMap<SourcePlane, usize>,
    pub by_kind: BTreeMap<EventKind, usize>,
    pub waits_by_outcome: BTreeMap<SelectedOutcome, usize>,
    pub wait_timelines: Vec<WaitTimeline>,
    pub sleep_timelines: Vec<SleepTimeline>,
    pub scheduler_timelines: Vec<SchedulerTimeline>,
    pub external_durations: Vec<ExternalDuration>,
    pub queue_timelines: Vec<QueueTimeline>,
    pub conflicting_wait_count: usize,
    pub repeated_wait_groups: usize,
    pub repeated_status_query_groups: usize,
    pub unknown_root_events: usize,
    pub truncated_events: usize,
    pub omitted_input_events: usize,
    pub capture_loss_observed: bool,
}
impl Summary {
    pub fn reduce(events: &[RecordedEvent]) -> Self { Self::reduce_inner(events, 0, false) }
    pub fn reduce_snapshot(snapshot: &RecorderSnapshot) -> Self {
        Self::reduce_inner(&snapshot.events, snapshot.high_water, snapshot.losses.contention > 0
            || snapshot.losses.capacity > 0 || snapshot.losses.disabled > 0
            || snapshot.losses.invalid_identity > 0)
    }
    fn reduce_inner(events: &[RecordedEvent], high_water: u64, loss: bool) -> Self {
        let mut out = Self { capture_loss_observed: loss, ..Self::default() };
        out.omitted_input_events = events.len().saturating_sub(MAX_REDUCER_EVENTS);
        let events = &events[..events.len().min(MAX_REDUCER_EVENTS)];
        let mut identities = BTreeMap::<EventIdentity, Vec<&RecordedEvent>>::new();
        for event in events {
            let variants = identities.entry(event.identity.clone()).or_default();
            if variants.contains(&event) { out.duplicate_count += 1; }
            else {
                if !variants.is_empty() { out.conflicting_identity_count += 1; }
                variants.push(event);
            }
        }
        let mut clean = Vec::new();
        for variants in identities.values() {
            if variants.len() == 1 { clean.push(variants[0]); }
        }
        let mut sequences = BTreeMap::<&str, BTreeSet<u64>>::new();
        for event in &clean {
            sequences.entry(&event.identity.capture_instance_id).or_default().insert(event.identity.sequence);
            out.event_count += 1;
            *out.by_source.entry(event.input.source_plane).or_default() += 1;
            *out.by_kind.entry(event.input.kind).or_default() += 1;
            if event.input.root_thread_id.is_none() { out.unknown_root_events += 1; }
            if event.truncated { out.truncated_events += 1; }
        }
        if high_water > 0 {
            if sequences.is_empty() { out.capture_loss_observed = true; }
            for observed in sequences.values() {
                if observed.len() as u64 != high_water
                    || observed.first().copied() != Some(1)
                    || observed.last().copied() != Some(high_water) {
                    out.capture_loss_observed = true;
                }
            }
        } else {
            for observed in sequences.values() {
                let ordered = observed.iter().copied().collect::<Vec<_>>();
                if ordered.first().copied().unwrap_or(1) != 1
                    || ordered.windows(2).any(|pair| pair[1] != pair[0] + 1) {
                    out.capture_loss_observed = true;
                }
            }
        }
        Self::waits(&mut out, &clean);
        Self::sleeps(&mut out, &clean);
        Self::schedulers(&mut out, &clean);
        Self::external(&mut out, &clean);
        Self::queues(&mut out, &clean);
        Self::repetitions(&mut out, &clean);
        if out.capture_loss_observed || out.omitted_input_events > 0 {
            for row in &mut out.wait_timelines { row.complete = false; }
            for row in &mut out.sleep_timelines { row.complete = false; }
            for row in &mut out.scheduler_timelines { row.complete = false; }
        }
        out
    }
    fn waits(out: &mut Self, events: &[&RecordedEvent]) {
        let mut grouped = BTreeMap::<(String, SourcePlane, Option<String>, String, Option<u64>), Vec<&RecordedEvent>>::new();
        for event in events {
            if let Some(wait) = &event.input.wait {
                grouped.entry((event.identity.capture_instance_id.clone(), event.input.source_plane,
                    event.input.thread_id.clone(), wait.wait_id.clone(), event.input.thread_id.is_none().then_some(event.identity.sequence))).or_default().push(event);
            }
        }
        for ((capture, source, thread, wait_id, _), mut rows) in grouped {
            rows.sort_by_key(|event| event.identity.sequence);
            let terminal = rows.iter().filter_map(|event| event.input.wait.as_ref().filter(|wait|
                matches!(wait.phase, WaitPhase::Completed | WaitPhase::Abandoned)).map(|wait| (*event, wait))).collect::<Vec<_>>();
            if terminal.is_empty() { continue; }
            let (event, wait) = terminal[0];
            let mut conflicting = terminal.iter().any(|(other_event, other)| *other != wait
                || other_event.input.operation_id != event.input.operation_id);
            let (mut start, mut end) = (wait.blocked_start_offset_ns, wait.blocked_end_offset_ns);
            for row in &rows {
                if let Some(obs) = &row.input.wait {
                    start = start.or(obs.blocked_start_offset_ns);
                    end = end.or(obs.blocked_end_offset_ns);
                }
            }
            let zero_block = wait.blocked_duration_ns == Some(0);
            let complete = !conflicting && rows.iter().all(|row| !row.truncated)
                && (zero_block || start.is_some() && end.is_some()) && wait.blocked_duration_ns.is_some();
            if conflicting { out.conflicting_wait_count += 1; }
            *out.waits_by_outcome.entry(if conflicting { SelectedOutcome::Unknown } else { wait.selected_outcome }).or_default() += 1;
            out.wait_timelines.push(WaitTimeline {
                capture_instance_id: capture, source_plane: source, thread_id: thread,
                operation_id: event.input.operation_id.clone(), wait_id, primitive: wait.primitive,
                requested_timeout_ms: wait.requested_timeout_ms, effective_timeout_ms: wait.effective_timeout_ms,
                blocked_start_offset_ns: start, blocked_end_offset_ns: end,
                operation_duration_ns: wait.operation_duration_ns, blocked_duration_ns: wait.blocked_duration_ns,
                selected_outcome: if conflicting { SelectedOutcome::Unknown } else { wait.selected_outcome },
                selected_producer: wait.selected_producer.clone(), subscribed_readiness: wait.subscribed_readiness.clone(),
                selected_readiness: wait.selected_readiness.clone(), coverage: event.input.field_coverage.clone(),
                complete, conflicting,
            });
        }
    }
    fn sleeps(out: &mut Self, events: &[&RecordedEvent]) {
        for event in events {
            if let Some(sleep) = &event.input.sleep {
                out.sleep_timelines.push(SleepTimeline {
                    capture_instance_id: event.identity.capture_instance_id.clone(),
                    source_plane: event.input.source_plane, thread_id: event.input.thread_id.clone(),
                    operation_id: event.input.operation_id.clone(), pending_activity: sleep.pending_activity,
                    selection: sleep.selection, requested_duration_ns: sleep.requested_duration_ns,
                    operation_duration_ns: sleep.operation_duration_ns, blocked_duration_ns: sleep.blocked_duration_ns,
                    complete: sleep.operation_duration_ns.is_some() && sleep.blocked_duration_ns.is_some() && !event.truncated,
                });
            }
        }
    }
    fn schedulers(out: &mut Self, events: &[&RecordedEvent]) {
        let mut groups = BTreeMap::<(String, SourcePlane, String, String, String), Vec<&SchedulerObservation>>::new();
        let mut unjoined = Vec::new();
        for event in events {
            if let Some(s) = &event.input.scheduler {
                if let (Some(thread), Some(turn), Some(correlation)) =
                    (&event.input.thread_id, &s.scheduled_turn_id, &s.correlation_id) {
                    groups.entry((event.identity.capture_instance_id.clone(), event.input.source_plane,
                        thread.clone(), turn.clone(), correlation.clone())).or_default().push(s);
                } else {
                    unjoined.push((event, s));
                }
            }
        }
        for ((capture, _source, thread, turn, correlation), rows) in groups {
            let mut eligible = None; let mut basis = EligibilityBasis::Unknown;
            let mut pending_mail = None; let mut trigger_mail = None; let mut durable_sleep = None;
            let mut reservation = None; let mut reservation_matches = None; let mut lost = None;
            let mut registered = None; let mut published = None;
            let mut cohort = Vec::new(); let mut cohort_complete = false; let mut cohort_trigger_mail = None;
            let mut cohort_seen = false; let mut outcome = SchedulerOutcome::Unknown;
            let mut stage_rows = Vec::<&SchedulerObservation>::new();
            let mut conflicting = false;
            for row in rows {
                if let Some(previous) = stage_rows.iter().find(|previous| previous.phase == row.phase) {
                    conflicting |= *previous != row;
                } else {
                    stage_rows.push(row);
                }
                match row.phase {
                    SchedulerPhase::Eligibility => {
                        eligible = match row.eligibility_basis {
                            EligibilityBasis::PendingTriggerTurn | EligibilityBasis::QueueOnlyDurableSleep => Some(true),
                            EligibilityBasis::Unknown => match row.outcome {
                                SchedulerOutcome::NotEligibleObserved | SchedulerOutcome::ActiveTurnPresentObserved => Some(false),
                                _ => None,
                            },
                        };
                        basis = row.eligibility_basis;
                        pending_mail = row.pending_mail_observed;
                        trigger_mail = row.trigger_turn_mail_observed;
                        durable_sleep = row.durable_sleep_observed;
                        outcome = row.outcome;
                    }
                    SchedulerPhase::ReservationAccepted => {
                        reservation = row.idle_reservation_accepted;
                        reservation_matches = row.reservation_still_matches;
                        outcome = row.outcome;
                    }
                    SchedulerPhase::ReservationLost => lost = Some(true),
                    SchedulerPhase::CohortDrained => {
                        cohort = row.drained_message_cohort_ids.clone();
                        cohort_complete = row.drained_cohort_complete;
                        cohort_trigger_mail = row.drained_cohort_contains_trigger_turn_mail;
                        cohort_seen = true;
                        outcome = row.outcome;
                    }
                    SchedulerPhase::TaskRegistered => registered = Some(true),
                    SchedulerPhase::TurnStartProducerPublication => published = Some(true),
                    SchedulerPhase::Unknown => {}
                }
            }
            let complete = eligible == Some(true) && reservation == Some(true) && cohort_seen
                && registered.is_some() && published.is_some() && !lost.unwrap_or(false) && cohort_complete
                && !conflicting;
            out.scheduler_timelines.push(SchedulerTimeline {
                capture_instance_id: capture, source_plane: _source, thread_id: thread, turn_id: turn, correlation_id: correlation,
                eligible: if conflicting { None } else { eligible },
                eligibility_basis: if conflicting { EligibilityBasis::Unknown } else { basis },
                pending_mail_observed: if conflicting { None } else { pending_mail },
                trigger_turn_mail_observed: if conflicting { None } else { trigger_mail },
                durable_sleep_observed: if conflicting { None } else { durable_sleep },
                reservation_accepted: if conflicting { None } else { reservation },
                reservation_still_matches: if conflicting { None } else { reservation_matches },
                reservation_lost: if conflicting { None } else { lost },
                task_registered: if conflicting { None } else { registered },
                turn_start_published: if conflicting { None } else { published },
                drained_cohort_ids: if conflicting { Vec::new() } else { cohort },
                drained_cohort_complete: cohort_complete && !conflicting, complete, conflicting,
                drained_cohort_contains_trigger_turn_mail: if conflicting { None } else { cohort_trigger_mail },
                outcome: if conflicting { SchedulerOutcome::Unknown } else { outcome },
            });
        }
        for (event, row) in unjoined {
            out.scheduler_timelines.push(SchedulerTimeline {
                capture_instance_id: event.identity.capture_instance_id.clone(),
                source_plane: event.input.source_plane,
                thread_id: event.input.thread_id.clone(), turn_id: row.scheduled_turn_id.clone(),
                correlation_id: row.correlation_id.clone(), eligible: None, eligibility_basis: EligibilityBasis::Unknown,
                pending_mail_observed: None, trigger_turn_mail_observed: None, durable_sleep_observed: None,
                reservation_accepted: None, reservation_lost: None, task_registered: None,
                reservation_still_matches: None,
                turn_start_published: None, drained_cohort_ids: Vec::new(),
                drained_cohort_complete: false, drained_cohort_contains_trigger_turn_mail: None,
                outcome: SchedulerOutcome::Unknown, complete: false, conflicting: false,
            });
        }
    }
    fn external(out: &mut Self, events: &[&RecordedEvent]) {
        let mut calls = BTreeMap::<(String, SourcePlane, Option<String>, String), Vec<&RecordedEvent>>::new();
        for event in events {
            if let Some(op) = &event.input.external_operation {
                calls.entry((event.identity.capture_instance_id.clone(), event.input.source_plane,
                    event.input.thread_id.clone(), op.call_id.clone())).or_default().push(event);
            }
        }
        for ((_capture, source, thread, call_id), rows) in calls {
            let first = rows[0];
            let first_op = first.input.external_operation.as_ref().unwrap();
            let conflicting = rows.iter().any(|event| {
                event.input.external_operation.as_ref().map(|op| op.returned_duration_ns) != Some(first_op.returned_duration_ns)
                    || event.input.operation_id != first.input.operation_id
            });
            out.external_durations.push(ExternalDuration {
                identity: first.identity.clone(), source_plane: source, thread_id: thread,
                operation_id: first.input.operation_id.clone(), call_id,
                observed_request_return_ns: if conflicting { None } else { first_op.returned_duration_ns },
                quality: first.input.quality, conflicting,
            });
        }
    }
    fn queues(out: &mut Self, events: &[&RecordedEvent]) {
        for event in events {
            if let Some(observation) = &event.input.queue {
                out.queue_timelines.push(QueueTimeline {
                    capture_instance_id: event.identity.capture_instance_id.clone(),
                    source_plane: event.input.source_plane, thread_id: event.input.thread_id.clone(),
                    turn_id: event.input.turn_id.clone(), observation: observation.clone(),
                });
            }
        }
    }
    fn repetitions(out: &mut Self, events: &[&RecordedEvent]) {
        type WaitRequest = (WaitPrimitive, TargetMode, Option<bool>, Option<i64>, Option<u64>,
            TargetReferenceKind, Vec<String>, bool, bool, Option<String>, Option<String>);
        type WaitResult = (SelectedOutcome, Vec<ReadinessObservation>, Option<String>, Option<String>,
            Vec<ReadinessObservation>, Vec<String>, Option<bool>);
        let mut waits = BTreeMap::<(String, SourcePlane, Option<String>, WaitRequest, WaitResult), BTreeSet<String>>::new();
        let mut queries = BTreeMap::<(String, SourcePlane, Option<String>, StatusRequestProjection, StatusResultProjection), BTreeSet<String>>::new();
        for event in events {
            let Some(operation) = event.input.operation_id.as_ref() else { continue };
            if let Some(wait) = &event.input.wait {
                if !event.truncated {
                    let request = (wait.primitive, wait.target_mode, wait.any_targets,
                        wait.requested_timeout_ms, wait.effective_timeout_ms, wait.requested_target_kind,
                        wait.requested_target_ids.clone(), wait.requested_target_set_complete,
                        wait.target_set_complete, wait.helper_id.clone(), wait.helper_version.clone());
                    let result = (wait.selected_outcome, wait.selected_readiness.clone(),
                        wait.selected_target_id.clone(), wait.selected_target_turn_id.clone(),
                        wait.subscribed_readiness.clone(), wait.target_ids.clone(),
                        wait.resolved_target_set_complete);
                    waits.entry((event.identity.capture_instance_id.clone(), event.input.source_plane,
                        event.input.thread_id.clone(), request, result)).or_default().insert(operation.clone());
                }
            }
            if let Some(query) = &event.input.status_query {
                if let (Some(request), Some(result)) = (&query.request_projection, &query.result_projection) {
                    if request.complete && result.complete && !event.truncated {
                        queries.entry((event.identity.capture_instance_id.clone(), event.input.source_plane,
                            event.input.thread_id.clone(), request.clone(), result.clone())).or_default().insert(operation.clone());
                    }
                }
            }
        }
        out.repeated_wait_groups = waits.values().filter(|ops| ops.len() > 1).count();
        out.repeated_status_query_groups = queries.values().filter(|ops| ops.len() > 1).count();
    }
    pub fn partitions_conserve(&self) -> bool {
        self.by_source.values().sum::<usize>() == self.event_count
            && self.by_kind.values().sum::<usize>() == self.event_count
    }
}
