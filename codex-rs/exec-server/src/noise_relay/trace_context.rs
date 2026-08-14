use std::collections::HashMap;
use std::collections::VecDeque;

use codex_exec_server_protocol::EXEC_CLOSED_METHOD;
use codex_exec_server_protocol::JSONRPCMessage;
use codex_exec_server_protocol::RequestId;
use codex_protocol::protocol::W3cTraceContext;

const MAX_TRACE_CONTEXT_ENTRIES: usize = 256;

/// Correlates return traffic with the trace carried by its originating request.
///
/// Responses do not repeat the request's W3C carrier, and process notifications
/// are correlated by `processId`. State is scoped to one authenticated virtual
/// stream so telemetry does not change the JSON-RPC wire protocol.
#[derive(Default)]
pub(super) struct NoiseTraceContext {
    requests: HashMap<RequestId, W3cTraceContext>,
    request_order: VecDeque<RequestId>,
    processes: HashMap<String, W3cTraceContext>,
    process_order: VecDeque<String>,
}

impl NoiseTraceContext {
    pub(super) fn observe_request(&mut self, message: &JSONRPCMessage) {
        let JSONRPCMessage::Request(request) = message else {
            return;
        };
        let Some(trace) = request.trace.as_ref() else {
            return;
        };
        insert_bounded(
            &mut self.requests,
            &mut self.request_order,
            request.id.clone(),
            trace.clone(),
        );
        if let Some(process_id) = message_process_id(message) {
            let process_id = process_id.to_string();
            if !self.processes.contains_key(&process_id) {
                insert_bounded(
                    &mut self.processes,
                    &mut self.process_order,
                    process_id,
                    trace.clone(),
                );
            }
        }
    }

    pub(super) fn return_trace(&mut self, message: &JSONRPCMessage) -> Option<W3cTraceContext> {
        match message {
            JSONRPCMessage::Response(response) => {
                remove_tracked(&mut self.requests, &mut self.request_order, &response.id)
            }
            JSONRPCMessage::Error(error) => {
                remove_tracked(&mut self.requests, &mut self.request_order, &error.id)
            }
            JSONRPCMessage::Notification(notification) => {
                let process_id = message_process_id(message)?.to_string();
                let trace = self.processes.get(&process_id).cloned();
                if notification.method == EXEC_CLOSED_METHOD {
                    remove_tracked(&mut self.processes, &mut self.process_order, &process_id);
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
}

fn insert_bounded<K>(
    values: &mut HashMap<K, W3cTraceContext>,
    order: &mut VecDeque<K>,
    key: K,
    value: W3cTraceContext,
) where
    K: Clone + Eq + std::hash::Hash,
{
    if let Some(existing) = values.get_mut(&key) {
        *existing = value;
        return;
    }
    while values.len() >= MAX_TRACE_CONTEXT_ENTRIES {
        let Some(oldest) = order.pop_front() else {
            break;
        };
        values.remove(&oldest);
    }
    values.insert(key.clone(), value);
    order.push_back(key);
}

fn remove_tracked<K>(
    values: &mut HashMap<K, W3cTraceContext>,
    order: &mut VecDeque<K>,
    key: &K,
) -> Option<W3cTraceContext>
where
    K: Eq + std::hash::Hash,
{
    let removed = values.remove(key);
    if removed.is_some() {
        order.retain(|candidate| candidate != key);
    }
    removed
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
