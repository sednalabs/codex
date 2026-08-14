use std::collections::HashMap;
use std::collections::VecDeque;

use codex_exec_server_protocol::EXEC_CLOSED_METHOD;
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
    requests: HashMap<RequestId, W3cTraceContext>,
    request_order: VecDeque<RequestId>,
    request_bytes: usize,
    processes: HashMap<String, W3cTraceContext>,
    process_order: VecDeque<String>,
    process_bytes: usize,
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
        if let Some(request_id_bytes) = request_id_bytes(&request.id) {
            insert_bounded(
                &mut self.requests,
                &mut self.request_order,
                &mut self.request_bytes,
                request.id.clone(),
                trace.clone(),
                request_id_bytes + trace_bytes,
            );
        }
        if let Some(process_id) = message_process_id(message)
            && process_id.len() <= MAX_PROCESS_ID_BYTES
        {
            let process_id_bytes = process_id.len();
            let process_id = process_id.to_string();
            if !self.processes.contains_key(&process_id) {
                insert_bounded(
                    &mut self.processes,
                    &mut self.process_order,
                    &mut self.process_bytes,
                    process_id,
                    trace.clone(),
                    process_id_bytes + trace_bytes,
                );
            }
        }
    }

    pub(super) fn return_trace(&mut self, message: &JSONRPCMessage) -> Option<W3cTraceContext> {
        match message {
            JSONRPCMessage::Response(response) => remove_tracked(
                &mut self.requests,
                &mut self.request_order,
                &mut self.request_bytes,
                &response.id,
            ),
            JSONRPCMessage::Error(error) => remove_tracked(
                &mut self.requests,
                &mut self.request_order,
                &mut self.request_bytes,
                &error.id,
            ),
            JSONRPCMessage::Notification(notification) => {
                let process_id = message_process_id(message)?;
                if process_id.len() > MAX_PROCESS_ID_BYTES {
                    return None;
                }
                let process_id = process_id.to_string();
                let trace = self.processes.get(&process_id).cloned();
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

    #[cfg(test)]
    pub(super) fn state_sizes(&self) -> (usize, usize) {
        (self.requests.len(), self.processes.len())
    }

    #[cfg(test)]
    pub(super) fn retained_bytes(&self) -> usize {
        self.request_bytes + self.process_bytes
    }
}

fn insert_bounded<K>(
    values: &mut HashMap<K, W3cTraceContext>,
    order: &mut VecDeque<K>,
    retained_bytes: &mut usize,
    key: K,
    value: W3cTraceContext,
    entry_bytes: usize,
) where
    K: Clone + Eq + std::hash::Hash + RetainedKeyBytes,
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

fn remove_tracked<K>(
    values: &mut HashMap<K, W3cTraceContext>,
    order: &mut VecDeque<K>,
    retained_bytes: &mut usize,
    key: &K,
) -> Option<W3cTraceContext>
where
    K: Eq + std::hash::Hash + RetainedKeyBytes,
{
    let removed = values.remove(key);
    if removed.is_some() {
        order.retain(|candidate| candidate != key);
        *retained_bytes = retained_payload_bytes(values);
    }
    removed
}

fn retained_payload_bytes<K>(values: &HashMap<K, W3cTraceContext>) -> usize
where
    K: RetainedKeyBytes,
{
    values
        .iter()
        .map(|(key, trace)| key.retained_bytes() + trace_context_bytes(trace).unwrap_or(0))
        .sum()
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
