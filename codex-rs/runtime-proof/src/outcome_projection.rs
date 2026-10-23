//! Bounded, allowlisted projection of the fixed launcher task's exec JSONL.
//!
//! The child stdout is a private protocol stream. Only canonical request IDs,
//! fixed tool names, closed outcomes, and validated Ops state fields are sent to
//! the existing launcher stderr/journal path.

use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Read;
use std::io::Write;
use uuid::Uuid;

const MAX_LINE_BYTES: usize = 64 * 1024;
const MAX_PENDING_CALLS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tool {
    Claim,
    CurrentWork,
    ReceiptRead,
    Release,
}

impl Tool {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "work_item_claim" => Some(Self::Claim),
            "work_item_current_work" => Some(Self::CurrentWork),
            "work_item_execution_receipt_read" => Some(Self::ReceiptRead),
            "work_item_release" => Some(Self::Release),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Claim => "work_item_claim",
            Self::CurrentWork => "work_item_current_work",
            Self::ReceiptRead => "work_item_execution_receipt_read",
            Self::Release => "work_item_release",
        }
    }

    fn is_mutation(self) -> bool {
        matches!(self, Self::Claim | Self::Release)
    }
}

#[derive(Clone, Debug)]
struct PendingCall {
    tool: Tool,
    work_item_ref: String,
    request_id: Option<String>,
    expected_generation: Option<u64>,
    expected_updated_at: Option<String>,
}

pub(super) fn project_child_stdout<R: Read, W: Write>(
    reader: R,
    work_item_ref: &str,
    principal: Uuid,
    writer: &mut W,
) {
    let mut projector = Projector::new(work_item_ref, principal, writer);
    let mut reader = reader;
    let mut chunk = [0_u8; 4096];
    let mut line = Vec::with_capacity(4096);
    let mut overlong = false;

    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        for byte in &chunk[..read] {
            if *byte == b'\n' {
                if !overlong && !line.is_empty() {
                    projector.accept_line(&line);
                }
                line.clear();
                overlong = false;
            } else if line.len() < MAX_LINE_BYTES {
                line.push(*byte);
            } else {
                overlong = true;
            }
        }
    }

    if !overlong && !line.is_empty() {
        projector.accept_line(&line);
    }
    projector.finish();
}

struct Projector<'a, W: Write> {
    work_item_ref: &'a str,
    principal: Uuid,
    primary_thread: Option<String>,
    pending: BTreeMap<String, PendingCall>,
    writer: &'a mut W,
}

impl<'a, W: Write> Projector<'a, W> {
    fn new(work_item_ref: &'a str, principal: Uuid, writer: &'a mut W) -> Self {
        Self {
            work_item_ref,
            principal,
            primary_thread: None,
            pending: BTreeMap::new(),
            writer,
        }
    }

    fn accept_line(&mut self, line: &[u8]) {
        let Ok(event) = serde_json::from_slice::<Value>(line) else {
            return;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("thread.started") => self.accept_thread_start(&event),
            Some("item.started") => self.accept_item_start(&event),
            Some("item.completed") => self.accept_item_completion(&event),
            _ => {}
        }
    }

    fn accept_thread_start(&mut self, event: &Value) {
        let Some(thread_id) = event
            .get("thread_id")
            .and_then(Value::as_str)
            .filter(|value| is_safe_identifier(value, /*max_len*/ 128))
        else {
            return;
        };
        if self.primary_thread.is_none() {
            self.primary_thread = Some(thread_id.to_string());
        }
    }

    fn accept_item_start(&mut self, event: &Value) {
        if !self.is_primary_event(event) {
            return;
        }
        let Some(item) = event.get("item") else {
            return;
        };
        if item.get("type").and_then(Value::as_str) != Some("mcp_tool_call") {
            return;
        }
        let (Some(item_id), Some(server), Some(tool_name)) = (
            item.get("id").and_then(Value::as_str),
            item.get("server").and_then(Value::as_str),
            item.get("tool").and_then(Value::as_str),
        ) else {
            return;
        };
        if server != "ops" || !is_safe_identifier(item_id, /*max_len*/ 128) {
            return;
        }
        let Some(tool) = Tool::parse(tool_name) else {
            return;
        };
        if self.pending.len() >= MAX_PENDING_CALLS || self.pending.contains_key(item_id) {
            return;
        }
        let Some(arguments) = item.get("arguments").and_then(Value::as_object) else {
            return;
        };
        let Some(work_item_ref) = arguments
            .get("work_item_ref")
            .and_then(Value::as_str)
            .filter(|value| *value == self.work_item_ref && is_safe_work_ref(value))
        else {
            return;
        };
        let request_id = if tool.is_mutation() {
            arguments
                .get("claim_precondition")
                .and_then(Value::as_object)
                .and_then(|precondition| precondition.get("request_id"))
                .and_then(Value::as_str)
                .and_then(canonical_uuid)
        } else if tool == Tool::ReceiptRead {
            arguments
                .get("request_id")
                .and_then(Value::as_str)
                .and_then(canonical_uuid)
        } else {
            None
        };
        if (tool.is_mutation() || tool == Tool::ReceiptRead) && request_id.is_none() {
            return;
        }
        let precondition = arguments
            .get("claim_precondition")
            .and_then(Value::as_object);
        let expected_generation = precondition
            .and_then(|value| value.get("expected_generation"))
            .and_then(Value::as_u64)
            .filter(|value| *value <= ((1_u64 << 53) - 1));
        let expected_updated_at = precondition
            .and_then(|value| value.get("expected_updated_at"))
            .and_then(Value::as_str)
            .and_then(canonical_timestamp);
        if tool.is_mutation() && (expected_generation.is_none() || expected_updated_at.is_none()) {
            return;
        }

        let pending = PendingCall {
            tool,
            work_item_ref: work_item_ref.to_string(),
            request_id,
            expected_generation,
            expected_updated_at,
        };
        if tool.is_mutation() {
            self.emit_record(&serde_json::json!({
                "event": "runtime_proof_operation",
                "tool": tool.as_str(),
                "state": "started",
                "outcome": "unknown",
                "work_item_ref": pending.work_item_ref.as_str(),
                "request_id": pending.request_id.as_deref(),
                "expected_generation": pending.expected_generation,
                "expected_updated_at": pending.expected_updated_at.as_deref(),
            }));
        }
        self.pending.insert(item_id.to_string(), pending);
    }

    fn accept_item_completion(&mut self, event: &Value) {
        if !self.is_primary_event(event) {
            return;
        }
        let Some(item) = event.get("item") else {
            return;
        };
        if item.get("type").and_then(Value::as_str) != Some("mcp_tool_call") {
            return;
        }
        let Some(item_id) = item.get("id").and_then(Value::as_str) else {
            return;
        };
        let Some(pending) = self.pending.remove(item_id) else {
            return;
        };
        let error = item
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str);
        if let Some(error) = error {
            let outcome = if error == crate::HOLDER_CONFLICT_MESSAGE {
                "holder_conflict"
            } else {
                "unknown"
            };
            self.emit_outcome(&pending, outcome, /*details*/ None);
            return;
        }

        let status = item.get("status").and_then(Value::as_str);
        if status != Some("completed") {
            self.emit_outcome(&pending, "unknown", /*details*/ None);
            return;
        }
        let structured = item
            .get("result")
            .and_then(|result| result.get("structured_content"));
        match pending.tool {
            Tool::CurrentWork => self.emit_current_work(&pending, structured),
            Tool::ReceiptRead => self.emit_receipt(&pending, structured),
            Tool::Claim => self.emit_claim_or_release(&pending, structured, "claimed"),
            Tool::Release => self.emit_claim_or_release(&pending, structured, "released"),
        }
    }

    fn emit_current_work(&mut self, pending: &PendingCall, value: Option<&Value>) {
        let Some(value) = value else {
            self.emit_outcome(pending, "unresolved", /*details*/ None);
            return;
        };
        if value.get("work_item_ref").and_then(Value::as_str) != Some(self.work_item_ref) {
            self.emit_outcome(pending, "unresolved", /*details*/ None);
            return;
        }
        let Some(claim) = value.get("current_claim") else {
            self.emit_outcome(pending, "unresolved", /*details*/ None);
            return;
        };
        let updated_at = value
            .get("work_revision")
            .and_then(|revision| revision.get("updated_at"))
            .and_then(Value::as_str)
            .and_then(canonical_timestamp);
        if claim.is_null() {
            if let Some(updated_at) = updated_at {
                self.emit_outcome(
                    pending,
                    "read",
                    Some(serde_json::json!({
                        "current_claim": null,
                        "expected_generation": 0,
                        "updated_at": updated_at,
                    })),
                );
            } else {
                self.emit_outcome(pending, "unresolved", /*details*/ None);
            }
            return;
        }
        let Some(generation) = claim
            .get("generation")
            .and_then(Value::as_u64)
            .filter(|value| *value <= ((1_u64 << 53) - 1))
        else {
            self.emit_outcome(pending, "unresolved", /*details*/ None);
            return;
        };
        let Some(instance_id) = claim
            .get("execution_instance_id")
            .and_then(Value::as_str)
            .and_then(canonical_uuid)
        else {
            self.emit_outcome(pending, "unresolved", /*details*/ None);
            return;
        };
        let Some(principal_id) = claim
            .get("principal_id")
            .and_then(Value::as_str)
            .and_then(canonical_uuid)
        else {
            self.emit_outcome(pending, "unresolved", /*details*/ None);
            return;
        };
        let released = claim.get("released").and_then(Value::as_bool);
        let Some(released) = released else {
            self.emit_outcome(pending, "unresolved", /*details*/ None);
            return;
        };
        let Some(updated_at) = updated_at else {
            self.emit_outcome(pending, "unresolved", /*details*/ None);
            return;
        };
        let principal_matches = principal_id == self.principal.to_string();
        self.emit_outcome(
            pending,
            "read",
            Some(serde_json::json!({
                "current_claim": {
                    "generation": generation,
                    "execution_instance_id": instance_id,
                    "principal_id": principal_id,
                    "principal_matches": principal_matches,
                    "released": released,
                },
                "updated_at": updated_at,
            })),
        );
    }

    fn emit_receipt(&mut self, pending: &PendingCall, value: Option<&Value>) {
        let Some(value) = value else {
            self.emit_outcome(pending, "unresolved", /*details*/ None);
            return;
        };
        if value.get("work_item_ref").and_then(Value::as_str) != Some(self.work_item_ref)
            || value.get("request_id").and_then(Value::as_str) != pending.request_id.as_deref()
        {
            self.emit_outcome(pending, "unresolved", /*details*/ None);
            return;
        }
        if value.is_null() {
            self.emit_outcome(
                pending,
                "unresolved",
                Some(serde_json::json!({"receipt": null})),
            );
            return;
        }
        let historical = value.get("historical");
        let Some(historical) = historical.filter(|value| !value.is_null()) else {
            self.emit_outcome(pending, "unresolved", /*details*/ None);
            return;
        };
        let generation = historical
            .get("generation")
            .and_then(Value::as_u64)
            .filter(|value| *value <= ((1_u64 << 53) - 1));
        let instance_id = historical
            .get("execution_instance_id")
            .and_then(Value::as_str)
            .and_then(canonical_uuid);
        let Some(generation) = generation else {
            self.emit_outcome(pending, "unresolved", /*details*/ None);
            return;
        };
        let Some(instance_id) = instance_id else {
            self.emit_outcome(pending, "unresolved", /*details*/ None);
            return;
        };
        let outcome = historical
            .get("outcome")
            .and_then(Value::as_str)
            .filter(|value| matches!(*value, "claimed" | "released"));
        let Some(outcome) = outcome else {
            self.emit_outcome(pending, "unresolved", /*details*/ None);
            return;
        };
        self.emit_outcome(
            pending,
            "receipt_found",
            Some(serde_json::json!({
                "historical": {
                    "generation": generation,
                    "execution_instance_id": instance_id,
                    "outcome": outcome,
                }
            })),
        );
    }

    fn emit_claim_or_release(
        &mut self,
        pending: &PendingCall,
        value: Option<&Value>,
        expected: &str,
    ) {
        let result = value.and_then(|value| {
            if value.get("work_item_ref").and_then(Value::as_str) != Some(self.work_item_ref) {
                return None;
            }
            if expected == "claimed" {
                return value
                    .as_object()
                    .is_some_and(|object| object.len() == 1)
                    .then_some(None);
            }
            let request_matches =
                value.get("request_id").and_then(Value::as_str) == pending.request_id.as_deref();
            let outcome_matches = value.get("outcome").and_then(Value::as_str) == Some(expected);
            if !request_matches || !outcome_matches {
                return None;
            }
            let generation = value.get("generation").and_then(Value::as_u64)?;
            if generation > ((1_u64 << 53) - 1) || Some(generation) != pending.expected_generation {
                return None;
            }
            let instance_id = value
                .get("execution_instance_id")
                .and_then(Value::as_str)
                .and_then(canonical_uuid)?;
            let released = value.get("released").and_then(Value::as_bool)?;
            if !released {
                return None;
            }
            Some(Some(serde_json::json!({
                "generation": generation,
                "execution_instance_id": instance_id,
                "released": released,
                "historical_present": value.get("historical").is_some_and(|value| !value.is_null()),
            })))
        });
        self.emit_outcome(
            pending,
            if result.is_some() {
                expected
            } else {
                "unresolved"
            },
            result.flatten(),
        );
    }

    fn emit_outcome(&mut self, pending: &PendingCall, outcome: &str, details: Option<Value>) {
        let mut record = serde_json::json!({
            "event": "runtime_proof_operation",
            "tool": pending.tool.as_str(),
            "state": "completed",
            "outcome": outcome,
            "work_item_ref": pending.work_item_ref.as_str(),
            "request_id": pending.request_id.as_deref(),
            "expected_generation": pending.expected_generation,
            "expected_updated_at": pending.expected_updated_at.as_deref(),
        });
        if let (Some(record), Some(details)) = (record.as_object_mut(), details)
            && let Some(details) = details.as_object()
        {
            for (key, value) in details {
                record.insert(key.clone(), value.clone());
            }
        }
        self.emit_record(&record);
    }

    fn is_primary_event(&self, event: &Value) -> bool {
        match (
            self.primary_thread.as_deref(),
            event.get("thread_id").and_then(Value::as_str),
        ) {
            (Some(primary), Some(thread)) => primary == thread,
            _ => false,
        }
    }

    fn finish(&mut self) {
        for (_, pending) in std::mem::take(&mut self.pending) {
            self.emit_outcome(&pending, "unknown", /*details*/ None);
        }
    }

    fn emit_record(&mut self, value: &Value) {
        let _ = serde_json::to_writer(&mut *self.writer, value);
        let _ = self.writer.write_all(b"\n");
        let _ = self.writer.flush();
    }
}

fn canonical_uuid(value: &str) -> Option<String> {
    let uuid = Uuid::parse_str(value).ok()?;
    (uuid.to_string() == value).then(|| value.to_string())
}

fn canonical_timestamp(value: &str) -> Option<String> {
    if value.len() > 64 || value.chars().any(char::is_control) {
        return None;
    }
    let timestamp = chrono::DateTime::parse_from_rfc3339(value).ok()?;
    Some(timestamp.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true))
}

fn is_safe_identifier(value: &str, max_len: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn is_safe_work_ref(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b':' | b'.'))
}

#[cfg(test)]
#[path = "outcome_projection_tests.rs"]
mod tests;
