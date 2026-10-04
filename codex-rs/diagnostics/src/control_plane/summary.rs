use std::collections::{BTreeMap, BTreeSet};

use super::lifecycle_timelines::{
    BoundaryTimeline, MessageTimeline, ProviderCallTimeline, WaitTimeline,
    project as project_lifecycles,
};
use super::types::*;

pub const MAX_REDUCER_EVENTS: usize = 2048;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Summary {
    pub event_count: usize,
    pub duplicate_count: usize,
    /// Number of conflicting alternate payloads, counted once per identity.
    pub conflicting_identity_count: usize,
    /// Number of rows whose event identity is empty, overbound, or sequence zero.
    pub invalid_event_identity_count: usize,
    /// Exact recorder loss counters when this reduction came from a frozen capture.
    pub recorder_losses: Option<LossCounts>,
    pub by_source: BTreeMap<SourcePlane, usize>,
    pub by_kind: BTreeMap<EventKind, usize>,
    pub waits_by_outcome: BTreeMap<SelectedOutcome, usize>,
    pub wait_timelines: Vec<WaitTimeline>,
    pub sleep_timelines: Vec<SleepTimeline>,
    pub scheduler_timelines: Vec<SchedulerTimeline>,
    pub external_durations: Vec<ExternalDuration>,
    pub queue_timelines: Vec<QueueTimeline>,
    pub message_timelines: Vec<MessageTimeline>,
    pub boundary_timelines: Vec<BoundaryTimeline>,
    pub provider_call_timelines: Vec<ProviderCallTimeline>,
    pub conflicting_wait_count: usize,
    pub repeated_wait_groups: usize,
    pub repeated_status_query_groups: usize,
    pub unknown_root_events: usize,
    pub truncated_events: usize,
    pub omitted_input_events: usize,
    pub capture_loss_observed: bool,
}
impl Summary {
    pub fn reduce(events: &[RecordedEvent]) -> Self { Self::reduce_inner(events, 0, false, None) }
    pub fn reduce_snapshot(snapshot: &RecorderSnapshot) -> Self {
        Self::reduce_inner(&snapshot.events, snapshot.high_water, snapshot.losses.contention > 0
            || snapshot.losses.capacity > 0 || snapshot.losses.disabled > 0
            || snapshot.losses.invalid_identity > 0, Some(snapshot.losses))
    }
    fn reduce_inner(events: &[RecordedEvent], high_water: u64, loss: bool, recorder_losses: Option<LossCounts>) -> Self {
        let mut out = Self { capture_loss_observed: loss, recorder_losses, ..Self::default() };
        out.omitted_input_events = events.len().saturating_sub(MAX_REDUCER_EVENTS);
        let events = &events[..events.len().min(MAX_REDUCER_EVENTS)];
        let mut identities = BTreeMap::<EventIdentity, Vec<&RecordedEvent>>::new();
        for event in events {
            if event.identity.capture_instance_id.is_empty()
                || event.identity.capture_instance_id.len() > MAX_IDENTIFIER_BYTES
                || event.identity.capture_instance_id.chars().any(char::is_control)
                || event.identity.sequence == 0 {
                out.invalid_event_identity_count += 1;
                out.capture_loss_observed = true;
                continue;
            }
            let variants = identities.entry(event.identity.clone()).or_default();
            if variants.contains(&event) { out.duplicate_count += 1; }
            else {
                variants.push(event);
            }
        }
        let mut clean = Vec::new();
        for variants in identities.values() {
            if variants.len() == 1 {
                clean.push(variants[0]);
            } else {
                out.conflicting_identity_count += 1;
                out.capture_loss_observed = true;
            }
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
        let lifecycle = project_lifecycles(&clean);
        out.wait_timelines = lifecycle.wait_timelines;
        out.message_timelines = lifecycle.message_timelines;
        out.boundary_timelines = lifecycle.boundary_timelines;
        out.provider_call_timelines = lifecycle.provider_call_timelines;
        out.waits_by_outcome = lifecycle.waits_by_outcome;
        out.conflicting_wait_count = lifecycle.conflicting_wait_count;
        Self::sleeps(&mut out, &clean);
        Self::schedulers(&mut out, &clean);
        Self::external(&mut out, &clean);
        Self::queues(&mut out, &clean);
        Self::repetitions(&mut out, &clean);
        if out.capture_loss_observed || out.omitted_input_events > 0 {
            for row in &mut out.wait_timelines { row.complete = false; }
            for row in &mut out.sleep_timelines { row.complete = false; }
            for row in &mut out.scheduler_timelines { row.complete = false; row.incomplete = true; }
            for row in &mut out.message_timelines { row.incomplete = true; }
            for row in &mut out.boundary_timelines { row.incomplete = true; }
            for row in &mut out.provider_call_timelines { row.complete = false; }
        }
        out
    }
    fn sleeps(out: &mut Self, events: &[&RecordedEvent]) {
        for event in events {
            if let Some(sleep) = &event.input.sleep {
                let timing_consistent = match (sleep.operation_duration_ns, sleep.blocked_duration_ns) {
                    (Some(operation), Some(blocked)) => operation >= blocked,
                    _ => false,
                };
                let selection_consistent = match sleep.selection {
                    SleepSelection::AlreadyPending => sleep.blocked_duration_ns == Some(0)
                        && !matches!(sleep.pending_activity, SleepPendingActivity::None | SleepPendingActivity::Unknown),
                    SleepSelection::ActivityChanged | SleepSelection::TimeProviderCompleted
                    | SleepSelection::TimeProviderFailed => sleep.pending_activity != SleepPendingActivity::Unknown,
                    SleepSelection::AbandonedUnknown | SleepSelection::Unknown => false,
                };
                out.sleep_timelines.push(SleepTimeline {
                    identity: event.identity.clone(),
                    capture_instance_id: event.identity.capture_instance_id.clone(),
                    source_plane: event.input.source_plane, thread_id: event.input.thread_id.clone(),
                    operation_id: event.input.operation_id.clone(), pending_activity: sleep.pending_activity,
                    selection: sleep.selection, requested_duration_ns: sleep.requested_duration_ns,
                    operation_duration_ns: sleep.operation_duration_ns, blocked_duration_ns: sleep.blocked_duration_ns,
                    complete: timing_consistent && selection_consistent && !event.truncated,
                });
            }
        }
    }
    fn schedulers(out: &mut Self, events: &[&RecordedEvent]) {
        type SchedulerKey = (String, SourcePlane, String, Option<String>, String);
        type SchedulerPrefix = (String, SourcePlane, String, String);
        let mut groups = BTreeMap::<SchedulerKey, Vec<(&SchedulerObservation, bool, EventIdentity)>>::new();
        let mut unjoined = Vec::new();
        for event in events {
            if let Some(s) = &event.input.scheduler {
                if let (Some(thread), Some(correlation)) = (&event.input.thread_id, &s.correlation_id) {
                    groups.entry((event.identity.capture_instance_id.clone(), event.input.source_plane,
                        thread.clone(), s.scheduled_turn_id.clone(), correlation.clone()))
                        .or_default().push((s, event.truncated, event.identity.clone()));
                } else {
                    unjoined.push((event, s));
                }
            }
        }
        let mut turns_by_prefix = BTreeMap::<SchedulerPrefix, BTreeSet<String>>::new();
        for (capture, source, thread, turn, correlation) in groups.keys() {
            if let Some(turn) = turn {
                turns_by_prefix.entry((capture.clone(), *source, thread.clone(), correlation.clone()))
                    .or_default().insert(turn.clone());
            }
        }
        let pre_turn_keys = groups.keys().filter(|(_, _, _, turn, _)| turn.is_none()).cloned().collect::<Vec<_>>();
        for (capture, source, thread, _, correlation) in pre_turn_keys {
            let prefix = (capture.clone(), source, thread.clone(), correlation.clone());
            if let Some(turns) = turns_by_prefix.get(&prefix).filter(|turns| turns.len() == 1) {
                let turn = turns.iter().next().cloned().expect("one scheduled turn");
                if let Some(rows) = groups.remove(&(capture.clone(), source, thread.clone(), None, correlation.clone())) {
                    groups.entry((capture, source, thread, Some(turn), correlation)).or_default().extend(rows);
                }
            }
        }
        let ambiguous_prefixes = turns_by_prefix.into_iter()
            .filter_map(|(prefix, turns)| (turns.len() > 1).then_some(prefix))
            .collect::<BTreeSet<_>>();
        for ((capture, source, thread, turn, correlation), rows) in groups {
            let mut eligible = None; let mut basis = EligibilityBasis::Unknown;
            let mut pending_mail = None; let mut trigger_mail = None; let mut durable_sleep = None;
            let mut reservation = None; let mut reservation_matches = None; let mut lost = None;
            let mut registered = None; let mut published = None;
            let mut cohort = Vec::new(); let mut cohort_complete = false; let mut cohort_trigger_mail = None;
            let mut cohort_seen = false; let mut outcome = SchedulerOutcome::Unknown;
            let mut stage_rows = Vec::<&SchedulerObservation>::new();
            let mut conflicting = ambiguous_prefixes.contains(&(capture.clone(), source, thread.clone(), correlation.clone()));
            let mut incomplete = false;
            let mut source_events = rows.iter().map(|(_, _, identity)| identity.clone()).collect::<Vec<_>>();
            source_events.sort_by_key(|identity| identity.sequence);
            for (row, truncated, _) in rows {
                incomplete |= truncated;
                if let Some(previous) = stage_rows.iter().find(|previous| previous.phase == row.phase) {
                    conflicting |= *previous != row;
                } else {
                    stage_rows.push(row);
                }
                match row.phase {
                    SchedulerPhase::Eligibility => {
                        if !matches!(row.eligibility_basis, EligibilityBasis::Unknown)
                            && matches!(row.outcome, SchedulerOutcome::NotEligibleObserved | SchedulerOutcome::ActiveTurnPresentObserved) {
                            conflicting = true;
                        }
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
                        if row.reservation_still_matches == Some(false) {
                            lost = Some(true);
                            outcome = SchedulerOutcome::ReservationLostObserved;
                        }
                    }
                    SchedulerPhase::ReservationLost => {
                        lost = Some(true);
                        outcome = SchedulerOutcome::ReservationLostObserved;
                    }
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
            if lost == Some(true) && (registered.is_some() || published.is_some()) {
                conflicting = true;
            }
            let complete = eligible == Some(true) && reservation == Some(true) && cohort_seen
                && reservation_matches == Some(true) && registered.is_some() && published.is_some()
                && !lost.unwrap_or(false) && cohort_complete
                && !conflicting && !incomplete;
            out.scheduler_timelines.push(SchedulerTimeline {
                source_events,
                capture_instance_id: capture, source_plane: source, thread_id: Some(thread), turn_id: turn, correlation_id: Some(correlation),
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
                drained_cohort_complete: cohort_complete && !conflicting && !incomplete, complete, conflicting, incomplete,
                drained_cohort_contains_trigger_turn_mail: if conflicting { None } else { cohort_trigger_mail },
                outcome: if conflicting { SchedulerOutcome::Unknown } else { outcome },
            });
        }
        for (event, row) in unjoined {
            out.scheduler_timelines.push(SchedulerTimeline {
                source_events: vec![event.identity.clone()],
                capture_instance_id: event.identity.capture_instance_id.clone(),
                source_plane: event.input.source_plane,
                thread_id: event.input.thread_id.clone(), turn_id: row.scheduled_turn_id.clone(),
                correlation_id: row.correlation_id.clone(), eligible: None, eligibility_basis: EligibilityBasis::Unknown,
                pending_mail_observed: None, trigger_turn_mail_observed: None, durable_sleep_observed: None,
                reservation_accepted: None, reservation_lost: None, task_registered: None,
                reservation_still_matches: None,
                turn_start_published: None, drained_cohort_ids: Vec::new(),
                drained_cohort_complete: false, drained_cohort_contains_trigger_turn_mail: None,
                outcome: SchedulerOutcome::Unknown, complete: false, conflicting: false, incomplete: event.truncated,
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
                    identity: event.identity.clone(),
                    capture_instance_id: event.identity.capture_instance_id.clone(),
                    source_plane: event.input.source_plane, thread_id: event.input.thread_id.clone(),
                    turn_id: event.input.turn_id.clone(), observation: observation.clone(),
                });
            }
        }
    }
    fn repetitions(out: &mut Self, events: &[&RecordedEvent]) {
        type WaitRequest = (WaitPrimitive, ReturnWhen, TargetMode, Option<bool>, Option<i64>, Option<u64>,
            TargetReferenceKind, Vec<String>, bool, TargetReferenceKind, Vec<String>, Option<bool>,
            Option<String>, Option<String>);
        type WaitResult = (SelectedOutcome, Vec<ReadinessObservation>, Option<String>, Option<String>,
            Vec<ReadinessObservation>, Vec<String>, TargetReferenceKind, Option<bool>, Option<ObservedHostWaitReturn>);
        let mut waits = BTreeMap::<(String, SourcePlane, Option<String>, WaitRequest, WaitResult), BTreeSet<String>>::new();
        let mut queries = BTreeMap::<(String, SourcePlane, Option<String>, StatusRequestProjection, StatusResultProjection), BTreeSet<String>>::new();
        for event in events {
            let Some(operation) = event.input.operation_id.as_ref() else { continue };
            if let Some(wait) = &event.input.wait {
                let result_complete = wait.observed_host_return.as_ref().is_some_and(|result| result.complete)
                    || wait.observed_host_return.is_none() && wait.selected_outcome != SelectedOutcome::Unknown
                        && wait.selected_readiness_complete;
                if !event.truncated && wait.phase == WaitPhase::Completed
                    && wait.return_when != ReturnWhen::Unknown
                    && wait.requested_target_set_complete && result_complete {
                    let request = (wait.primitive, wait.return_when, wait.target_mode, wait.any_targets,
                        wait.requested_timeout_ms, wait.effective_timeout_ms, wait.requested_target_kind,
                        wait.requested_target_ids.clone(), wait.requested_target_set_complete,
                        wait.resolved_target_kind, wait.target_ids.clone(), wait.resolved_target_set_complete,
                        wait.helper_id.clone(), wait.helper_version.clone());
                    let result = (wait.selected_outcome, wait.selected_readiness.clone(),
                        wait.selected_target_id.clone(), wait.selected_target_turn_id.clone(),
                        wait.subscribed_readiness.clone(), wait.target_ids.clone(),
                        wait.resolved_target_kind, wait.resolved_target_set_complete,
                        wait.observed_host_return.clone());
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
