use std::collections::HashMap;
use std::collections::VecDeque;

use codex_exec_server_protocol::EXEC_CLOSED_METHOD;
use codex_exec_server_protocol::EXEC_METHOD;
use codex_exec_server_protocol::JSONRPCMessage;
use codex_exec_server_protocol::RequestId;
use codex_protocol::protocol::W3cTraceContext;

const MAX_TRACE_CONTEXT_ENTRIES: usize = 256;
const MAX_REQUEST_ID_BYTES: usize = 256;
const MAX_PROCESS_ID_BYTES: usize = 256;
const MAX_TRACEPARENT_BYTES: usize = 128;
const MAX_TRACESTATE_BYTES: usize = 512;
const MAX_TRACE_CONTEXT_BYTES_PER_MAP: usize = 128 * 1024;

/// Correlates return traffic with the trace carried by its originating request.
///
/// Responses do not repeat the request's W3C carrier, and process notifications
/// are correlated by `processId`. State is scoped to one authenticated virtual
/// stream so telemetry does not change the JSON-RPC wire protocol.
#[derive(Default)]
pub(super) struct NoiseTraceContext {
    requests: HashMap<RequestId, TrackedRequest>,
    request_order: VecDeque<RequestId>,
    request_bytes: usize,
    processes: HashMap<String, TrackedProcess>,
    process_order: VecDeque<String>,
    process_bytes: usize,
}

struct TrackedRequest {
    trace: W3cTraceContext,
    /// Only an exact `process/start` request may establish process correlation.
    /// The entry remains provisional until its response succeeds.
    process_id: Option<String>,
}

struct TrackedProcess {
    trace: W3cTraceContext,
    /// A process-start request which may still establish this mapping. `None`
    /// means the mapping has been promoted by a successful response.
    pending_request: Option<RequestId>,
}

impl NoiseTraceContext {
    pub(super) fn observe_request(&mut self, message: &JSONRPCMessage) {
        let JSONRPCMessage::Request(request) = message else {
            return;
        };
        let Some(trace) = request.trace.as_ref() else {
            return;
        };
        let Some(trace_bytes) = trace_context_bytes(trace) else {
            return;
        };
        let process_id = (request.method == EXEC_METHOD)
            .then(|| message_process_id(message))
            .flatten()
            .filter(|process_id| process_id.len() <= MAX_PROCESS_ID_BYTES)
            .map(str::to_string);
        let request_id_bytes = request_id_bytes(&request.id);
        if let Some(request_id_bytes) = request_id_bytes {
            let request_entry_bytes =
                request_id_bytes + trace_bytes + process_id.as_ref().map_or(0, String::len);
            insert_bounded(
                &mut self.requests,
                &mut self.request_order,
                &mut self.request_bytes,
                request.id.clone(),
                TrackedRequest {
                    trace: trace.clone(),
                    process_id: process_id.clone(),
                },
                request_entry_bytes,
            );
        }
        if let Some(process_id) = process_id
            && request_id_bytes.is_some()
        {
            if !self.processes.contains_key(&process_id) {
                let process_entry_bytes =
                    process_id.len() + trace_bytes + request_id_bytes.unwrap_or_default();
                insert_bounded(
                    &mut self.processes,
                    &mut self.process_order,
                    &mut self.process_bytes,
                    process_id,
                    TrackedProcess {
                        trace: trace.clone(),
                        pending_request: Some(request.id.clone()),
                    },
                    process_entry_bytes,
                );
            }
        }
    }

    pub(super) fn return_trace(&mut self, message: &JSONRPCMessage) -> Option<W3cTraceContext> {
        match message {
            JSONRPCMessage::Response(response) => {
                let tracked = remove_tracked(
                    &mut self.requests,
                    &mut self.request_order,
                    &mut self.request_bytes,
                    &response.id,
                )?;
                self.complete_process_start(&response.id, tracked.process_id, true, &tracked.trace);
                Some(tracked.trace)
            }
            JSONRPCMessage::Error(error) => {
                let tracked = remove_tracked(
                    &mut self.requests,
                    &mut self.request_order,
                    &mut self.request_bytes,
                    &error.id,
                )?;
                self.complete_process_start(&error.id, tracked.process_id, false, &tracked.trace);
                Some(tracked.trace)
            }
            JSONRPCMessage::Notification(notification) => {
                let process_id = message_process_id(message)?;
                if process_id.len() > MAX_PROCESS_ID_BYTES {
                    return None;
                }
                let process_id = process_id.to_string();
                let trace = self
                    .processes
                    .get(&process_id)
                    .map(|process| process.trace.clone());
                if notification.method == EXEC_CLOSED_METHOD {
                    remove_tracked(
                        &mut self.processes,
                        &mut self.process_order,
                        &mut self.process_bytes,
                        &process_id,
                    );
                }
                trace
            }
            JSONRPCMessage::Request(request) => request.trace.clone(),
        }
    }

    fn complete_process_start(
        &mut self,
        request_id: &RequestId,
        process_id: Option<String>,
        success: bool,
        trace: &W3cTraceContext,
    ) {
        let Some(process_id) = process_id else {
            return;
        };
        let Some(process) = self.processes.get(&process_id) else {
            if success {
                self.insert_established_process(process_id, trace.clone());
            }
            return;
        };
        if success {
            // A duplicate successful start must never replace an established
            // mapping, regardless of response order.
            if let Some(process) = self.processes.get_mut(&process_id) {
                process.pending_request = None;
            }
            self.process_bytes = retained_payload_bytes(&self.processes);
            return;
        }
        if process.pending_request.as_ref() != Some(request_id) {
            return;
        }
        let next = self
            .request_order
            .iter()
            .filter_map(|candidate_id| {
                let candidate = self.requests.get(candidate_id)?;
                (candidate.process_id.as_deref() == Some(process_id.as_str()))
                    .then_some((candidate_id.clone(), candidate.trace.clone()))
            })
            .next();
        if let Some((next_request_id, next_trace)) = next {
            if let Some(process) = self.processes.get_mut(&process_id) {
                process.trace = next_trace;
                process.pending_request = Some(next_request_id);
            }
            self.process_bytes = retained_payload_bytes(&self.processes);
        } else {
            self.processes.remove(&process_id);
            self.process_bytes = retained_payload_bytes(&self.processes);
            self.process_order
                .retain(|candidate| candidate != &process_id);
        }
    }

    fn insert_established_process(&mut self, process_id: String, trace: W3cTraceContext) {
        let Some(trace_bytes) = trace_context_bytes(&trace) else {
            return;
        };
        insert_bounded(
            &mut self.processes,
            &mut self.process_order,
            &mut self.process_bytes,
            process_id.clone(),
            TrackedProcess {
                trace,
                pending_request: None,
            },
            process_id.len() + trace_bytes,
        );
    }

    #[cfg(test)]
    pub(super) fn state_sizes(&self) -> (usize, usize) {
        (self.requests.len(), self.processes.len())
    }

    #[cfg(test)]
    pub(super) fn retained_bytes(&self) -> usize {
        self.request_bytes + self.process_bytes
    }
}

fn insert_bounded<K, V>(
    values: &mut HashMap<K, V>,
    order: &mut VecDeque<K>,
    retained_bytes: &mut usize,
    key: K,
    value: V,
    entry_bytes: usize,
) where
    K: Clone + Eq + std::hash::Hash + RetainedKeyBytes,
    V: RetainedValueBytes,
{
    if entry_bytes > MAX_TRACE_CONTEXT_BYTES_PER_MAP {
        return;
    }
    if values.remove(&key).is_some() {
        order.retain(|candidate| candidate != &key);
        *retained_bytes = retained_payload_bytes(values);
    }
    while values.len() >= MAX_TRACE_CONTEXT_ENTRIES
        || retained_bytes.saturating_add(entry_bytes) > MAX_TRACE_CONTEXT_BYTES_PER_MAP
    {
        let Some(oldest) = order.pop_front() else {
            break;
        };
        values.remove(&oldest);
        *retained_bytes = retained_payload_bytes(values);
    }
    values.insert(key.clone(), value);
    order.push_back(key);
    *retained_bytes += entry_bytes;
}

fn remove_tracked<K, V>(
    values: &mut HashMap<K, V>,
    order: &mut VecDeque<K>,
    retained_bytes: &mut usize,
    key: &K,
) -> Option<V>
where
    K: Eq + std::hash::Hash + RetainedKeyBytes,
    V: RetainedValueBytes,
{
    let removed = values.remove(key);
    if removed.is_some() {
        order.retain(|candidate| candidate != key);
        *retained_bytes = retained_payload_bytes(values);
    }
    removed
}

fn retained_payload_bytes<K, V>(values: &HashMap<K, V>) -> usize
where
    K: RetainedKeyBytes,
    V: RetainedValueBytes,
{
    values
        .iter()
        .map(|(key, value)| key.retained_bytes() + value.retained_bytes())
        .sum()
}

trait RetainedValueBytes {
    fn retained_bytes(&self) -> usize;
}

impl RetainedValueBytes for TrackedRequest {
    fn retained_bytes(&self) -> usize {
        trace_context_bytes(&self.trace).unwrap_or(0)
            + self.process_id.as_ref().map_or(0, String::len)
    }
}

impl RetainedValueBytes for TrackedProcess {
    fn retained_bytes(&self) -> usize {
        trace_context_bytes(&self.trace).unwrap_or(0)
            + self
                .pending_request
                .as_ref()
                .map_or(0, RetainedKeyBytes::retained_bytes)
    }
}

trait RetainedKeyBytes {
    fn retained_bytes(&self) -> usize;
}

impl RetainedKeyBytes for RequestId {
    fn retained_bytes(&self) -> usize {
        match self {
            RequestId::String(value) => value.len(),
            RequestId::Integer(_) => std::mem::size_of::<i64>(),
        }
    }
}

impl RetainedKeyBytes for String {
    fn retained_bytes(&self) -> usize {
        self.len()
    }
}

fn request_id_bytes(request_id: &RequestId) -> Option<usize> {
    let bytes = request_id.retained_bytes();
    (bytes <= MAX_REQUEST_ID_BYTES).then_some(bytes)
}

fn trace_context_bytes(trace: &W3cTraceContext) -> Option<usize> {
    let traceparent = trace.traceparent.as_deref().unwrap_or_default();
    let tracestate = trace.tracestate.as_deref().unwrap_or_default();
    if traceparent.len() > MAX_TRACEPARENT_BYTES || tracestate.len() > MAX_TRACESTATE_BYTES {
        return None;
    }
    Some(traceparent.len() + tracestate.len())
}

fn message_process_id(message: &JSONRPCMessage) -> Option<&str> {
    let params = match message {
        JSONRPCMessage::Request(request) => request.params.as_ref(),
        JSONRPCMessage::Notification(notification) => notification.params.as_ref(),
        JSONRPCMessage::Response(_) | JSONRPCMessage::Error(_) => None,
    }?;
    params.get("processId")?.as_str()
}

#[cfg(test)]
#[path = "trace_context_tests.rs"]
mod tests;
