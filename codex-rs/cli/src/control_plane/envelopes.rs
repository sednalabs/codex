//! Typed allowlist for the finite existing rollout call/output envelope.

use chrono::{DateTime, Utc};
use serde::de::IgnoredAny;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

const MAX_RECORD_BYTES: usize = 65_536;
const MAX_EVENTS: usize = 2_048;
const MAX_ID_BYTES: usize = 256;
const MAX_STATUS_ACTORS: usize = 64;
impl ExternalStatusTag {
    pub fn as_static_tag(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Other => "other",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum EnvelopeKind {
    Wait,
    StatusQuery,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WaitRequest {
    pub targets: Vec<String>,
    pub target_set_complete: bool,
    pub return_when: HostReturnWhen,
    pub timeout_ms: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HostReturnWhen { Any, All, TargetTerminal, Unknown }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExternalStatusTag {
    Running,
    Completed,
    Other,
}
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct StatusActor {
    pub agent_id: String,
    pub canonical_path: String,
    pub configured_model: Option<String>,
    pub configured_effort: Option<String>,
    pub status: ExternalStatusTag,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StatusRequest {
    pub path_prefix: Option<String>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StatusResult {
    pub actors: Vec<StatusActor>,
    pub complete: bool,
    pub actor_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EnvelopeObservation {
    pub kind: EnvelopeKind,
    pub call_id: String,
    pub started_at: DateTime<Utc>,
    pub returned_at: DateTime<Utc>,
    pub wait_request: Option<WaitRequest>,
    pub timed_out: Option<bool>,
    pub host_timeout_reason: Option<bool>,
    pub host_target_terminal: Option<bool>,
    pub host_target_status_wake: Option<bool>,
    pub queued_update_count: Option<u64>,
    pub target_status_observed: bool,
    pub status_request: Option<StatusRequest>,
    pub status_result: Option<StatusResult>,
}

#[derive(Default)]
pub(crate) struct EnvelopeCoverage {
    pub malformed_records: usize,
    pub oversized_records: usize,
    pub unmatched_outputs: usize,
    pub duplicate_records: usize,
    pub conflicting_pairs: usize,
    pub unsupported_records: usize,
    pub incomplete_status_results: usize,
}

#[derive(Clone, Debug, Deserialize)]
struct RolloutRecord {
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(rename = "type")]
    record_type: String,
    #[serde(default)]
    payload: Option<RolloutPayload>,
}

#[derive(Clone, Debug, Deserialize)]
struct RolloutPayload {
    #[serde(rename = "type")]
    payload_type: String,
    #[serde(default)]
    name: Option<String>,
    call_id: String,
    #[serde(default)]
    arguments: Option<String>,
    #[serde(default)]
    output: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArguments {
    #[serde(default)]
    targets: Option<Vec<String>>,
    #[serde(default)]
    return_when: Option<String>,
    #[serde(default)]
    timeout_ms: Option<i64>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentListArguments {
    #[serde(default)]
    path_prefix: Option<String>,
}

#[derive(Deserialize)]
struct RawStatusActor {
    agent_id: String,
    canonical_path: String,
    #[serde(default)]
    configured_model: Option<String>,
    #[serde(default)]
    configured_reasoning_effort: Option<String>,
    agent_status: RawAgentStatus,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawAgentStatus {
    Label(String),
    Completed { completed: IgnoredAny },
}

#[derive(Deserialize)]
struct RawOutput {
    #[serde(default)]
    timed_out: Option<bool>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    wake_cause: Option<String>,
    #[serde(default)]
    queued_update_count: Option<u64>,
    #[serde(default)]
    target_status: Option<IgnoredAny>,
    #[serde(default)]
    agents: Option<Vec<RawStatusActor>>,
}

#[derive(Clone, Debug)]
enum Call {
    Wait { at: DateTime<Utc>, request: WaitRequest },
    StatusQuery { at: DateTime<Utc>, request: StatusRequest },
}

type CallKey = (String, String);

#[derive(Clone, Copy)]
enum HostReason {
    TargetTerminal,
    Timeout,
    Other,
    Missing,
}

#[derive(Clone, Copy)]
enum HostWakeCause {
    TargetStatus,
    Other,
    Missing,
}

struct WaitReturn {
    timed_out: Option<bool>,
    reason: HostReason,
    wake_cause: HostWakeCause,
    queued_update_count: Option<u64>,
    target_status_observed: bool,
}

struct ParsedOutput {
    wait: Option<WaitReturn>,
    status: Option<StatusResult>,
}

pub(crate) fn parse(
    bytes: &[u8],
    source_namespace: &str,
) -> (Vec<EnvelopeObservation>, EnvelopeCoverage) {
    let mut coverage = EnvelopeCoverage::default();
    let mut calls = BTreeMap::<CallKey, Call>::new();
    let mut outputs = BTreeMap::<CallKey, (DateTime<Utc>, ParsedOutput)>::new();
    let mut ambiguous = BTreeSet::<CallKey>::new();
    let mut relevant_records = 0usize;
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        if line.len() > MAX_RECORD_BYTES {
            coverage.oversized_records += 1;
            continue;
        }
        let Ok(row) = serde_json::from_slice::<RolloutRecord>(line) else {
            coverage.malformed_records += 1;
            continue;
        };
        if row.record_type != "response_item" {
            continue;
        }
        let (Some(timestamp), Some(payload)) = (row.timestamp, row.payload) else {
            coverage.malformed_records += 1;
            continue;
        };
        let Some(at) = DateTime::parse_from_rfc3339(&timestamp)
            .ok()
            .map(|time| time.with_timezone(&Utc))
        else {
            coverage.malformed_records += 1;
            continue;
        };
        if payload.call_id.is_empty()
            || payload.call_id.len() > MAX_ID_BYTES
            || payload.call_id.chars().any(char::is_control)
        {
            coverage.malformed_records += 1;
            continue;
        }
        let key = (source_namespace.to_owned(), payload.call_id.clone());
        match payload.payload_type.as_str() {
            "function_call" => {
                let request = match (payload.name.as_deref(), payload.arguments) {
                    (Some("wait_agent"), Some(args)) => {
                        let Ok(args) = serde_json::from_str::<WaitArguments>(&args) else {
                            coverage.malformed_records += 1;
                            continue;
                        };
                        if !valid_wait(&args) {
                            coverage.malformed_records += 1;
                            continue;
                        }
                        let target_set_complete = args.targets.is_some();
                        Call::Wait {
                            at,
                            request: WaitRequest {
                                targets: args.targets.unwrap_or_default(),
                                target_set_complete,
                                return_when: match args.return_when.as_deref() {
                                    Some("any") => HostReturnWhen::Any,
                                    Some("all") => HostReturnWhen::All,
                                    Some("target_terminal") => HostReturnWhen::TargetTerminal,
                                    Some(_) | None => HostReturnWhen::Unknown,
                                },
                                timeout_ms: args.timeout_ms,
                            },
                        }
                    }
                    (Some("list_agents"), Some(args)) => {
                        let Ok(args) = serde_json::from_str::<AgentListArguments>(&args) else {
                            coverage.malformed_records += 1;
                            continue;
                        };
                        if !valid_optional_id(args.path_prefix.as_deref()) {
                            coverage.malformed_records += 1;
                            continue;
                        }
                        Call::StatusQuery {
                            at,
                            request: StatusRequest { path_prefix: args.path_prefix },
                        }
                    }
                    _ => continue,
                };
                if calls.insert(key.clone(), request).is_some() {
                    coverage.duplicate_records += 1;
                    ambiguous.insert(key);
                }
                relevant_records += 1;
            }
            "function_call_output" => {
                let Some(output) = payload.output else {
                    coverage.malformed_records += 1;
                    continue;
                };
                let Some(parsed_output) = parse_output(output, &mut coverage) else {
                    continue;
                };
                if outputs.insert(key.clone(), (at, parsed_output)).is_some() {
                    coverage.duplicate_records += 1;
                    ambiguous.insert(key);
                }
                relevant_records += 1;
            }
            _ => {}
        }
        if relevant_records > MAX_EVENTS * 2 {
            coverage.unsupported_records += 1;
            break;
        }
    }

    let mut observations = Vec::new();
    for (key, (returned_at, output)) in outputs {
        if observations.len() >= MAX_EVENTS {
            coverage.unsupported_records += 1;
            break;
        }
        if ambiguous.contains(&key) {
            coverage.conflicting_pairs += 1;
            continue;
        }
        let Some(call) = calls.remove(&key) else {
            coverage.unmatched_outputs += 1;
            continue;
        };
        let observation = match call {
            Call::Wait { at, request } => {
                let Some(output) = output.wait else {
                    coverage.unmatched_outputs += 1;
                    continue;
                };
                wait_observation(key.1, at, returned_at, request, output)
            }
            Call::StatusQuery { at, request } => {
                let Some(result) = output.status else {
                    coverage.unmatched_outputs += 1;
                    continue;
                };
                EnvelopeObservation {
                    kind: EnvelopeKind::StatusQuery,
                    call_id: key.1,
                    started_at: at,
                    returned_at,
                    wait_request: None,
                    timed_out: None,
                    host_timeout_reason: None,
                    host_target_terminal: None,
                    host_target_status_wake: None,
                    queued_update_count: None,
                    target_status_observed: false,
                    status_request: Some(request),
                    status_result: Some(result),
                }
            }
        };
        observations.push(observation);
    }
    (observations, coverage)
}

fn parse_output(output: String, coverage: &mut EnvelopeCoverage) -> Option<ParsedOutput> {
    let Ok(raw) = serde_json::from_str::<RawOutput>(&output) else {
        coverage.malformed_records += 1;
        return None;
    };
    if raw.reason.as_ref().is_some_and(|value| value.len() > 128)
        || raw.wake_cause.as_ref().is_some_and(|value| value.len() > 128)
    {
        coverage.malformed_records += 1;
        return None;
    }
    let wait = recognized_wait_output(&raw).then(|| WaitReturn {
        timed_out: raw.timed_out,
        reason: host_reason(raw.reason.as_deref()),
        wake_cause: host_wake_cause(raw.wake_cause.as_deref()),
        queued_update_count: raw.queued_update_count,
        target_status_observed: raw.target_status.is_some(),
    });
    let status = raw.agents.map(|agents| {
        let actor_count = agents.len();
        let complete = actor_count <= MAX_STATUS_ACTORS
            && agents.iter().all(valid_status_actor);
        if !complete {
            coverage.incomplete_status_results += 1;
        }
        let mut actors = if complete {
            agents.into_iter().map(project_actor).collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        actors.sort();
        StatusResult { actors, complete, actor_count }
    });
    (wait.is_some() || status.is_some()).then_some(ParsedOutput { wait, status })
}

fn recognized_wait_output(output: &RawOutput) -> bool {
    output.timed_out.is_some()
        || output.reason.is_some()
        || output.wake_cause.is_some()
        || output.queued_update_count.is_some()
        || output.target_status.is_some()
}

fn host_reason(value: Option<&str>) -> HostReason {
    match value {
        Some("target_terminal") => HostReason::TargetTerminal,
        Some("timeout") => HostReason::Timeout,
        Some(_) => HostReason::Other,
        None => HostReason::Missing,
    }
}

fn host_wake_cause(value: Option<&str>) -> HostWakeCause {
    match value {
        Some("target_status") => HostWakeCause::TargetStatus,
        Some(_) => HostWakeCause::Other,
        None => HostWakeCause::Missing,
    }
}

fn valid_status_actor(actor: &RawStatusActor) -> bool {
    valid_id(&actor.agent_id)
        && valid_id(&actor.canonical_path)
        && valid_optional_id(actor.configured_model.as_deref())
        && valid_optional_id(actor.configured_reasoning_effort.as_deref())
        && match &actor.agent_status {
            RawAgentStatus::Label(label) => label.len() <= 128,
            RawAgentStatus::Completed { completed: _ } => true,
        }
}

fn wait_observation(
    call_id: String,
    started_at: DateTime<Utc>,
    returned_at: DateTime<Utc>,
    request: WaitRequest,
    output: WaitReturn,
) -> EnvelopeObservation {
    EnvelopeObservation {
        kind: EnvelopeKind::Wait,
        call_id,
        started_at,
        returned_at,
        wait_request: Some(request),
        timed_out: output.timed_out,
        host_timeout_reason: match output.reason {
            HostReason::Timeout => Some(true),
            HostReason::TargetTerminal | HostReason::Other => Some(false),
            HostReason::Missing => None,
        },
        host_target_terminal: match output.reason {
            HostReason::TargetTerminal => Some(true),
            HostReason::Timeout | HostReason::Other => Some(false),
            HostReason::Missing => None,
        },
        host_target_status_wake: match output.wake_cause {
            HostWakeCause::TargetStatus => Some(true),
            HostWakeCause::Other => Some(false),
            HostWakeCause::Missing => None,
        },
        queued_update_count: output.queued_update_count,
        target_status_observed: output.target_status_observed,
        status_request: None,
        status_result: None,
    }
}

fn project_actor(actor: RawStatusActor) -> StatusActor {
    StatusActor {
        agent_id: actor.agent_id,
        canonical_path: actor.canonical_path,
        configured_model: actor.configured_model,
        configured_effort: actor.configured_reasoning_effort,
        status: match actor.agent_status {
            RawAgentStatus::Label(label) if label == "running" => ExternalStatusTag::Running,
            RawAgentStatus::Label(_) => ExternalStatusTag::Other,
            RawAgentStatus::Completed { completed: _ } => ExternalStatusTag::Completed,
        },
    }
}

fn valid_wait(request: &WaitArguments) -> bool {
        request.targets.as_ref().is_none_or(|targets| {
            targets.len() <= MAX_STATUS_ACTORS && targets.iter().all(|value| valid_id(value))
        })
        && request.timeout_ms.is_none_or(|timeout| timeout >= 0)
        && request.return_when.as_ref().is_none_or(|value| {
            value.len() <= MAX_ID_BYTES && !value.chars().any(char::is_control)
        })
}

fn valid_optional_id(value: Option<&str>) -> bool {
    value.is_none_or(valid_id)
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_ID_BYTES && !value.chars().any(char::is_control)
}
