//! Reduction for payload-free, provenance-bearing inference observations.
//!
//! These events are deliberately separate from the legacy model-visible
//! `InferenceCall` object. They describe the physical-request lifecycle and
//! retain the complete bounded identity envelope without treating provider
//! execution as equivalent to a conversation response.

use anyhow::Result;
use anyhow::bail;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InferenceCallEvent;
use codex_protocol::protocol::InferenceCallStatus;

use super::TraceReducer;
use crate::model::ExecutionStatus;
use crate::model::ExecutionWindow;
use crate::model::InferenceCallObservation;
use crate::payload::RawPayloadRef;
use crate::raw_event::RawEventSeq;

impl TraceReducer {
    pub(super) fn reduce_inference_observation(
        &mut self,
        seq: RawEventSeq,
        wall_time_unix_ms: i64,
        event_type: String,
        event_payload: RawPayloadRef,
    ) -> Result<()> {
        if event_type != "inference_call" {
            // ProtocolEventObserved is intentionally an open envelope. Unknown
            // event types remain represented by their raw payload reference.
            return Ok(());
        }

        let value = self.read_payload_json(&event_payload)?;
        let event = match serde_json::from_value::<EventMsg>(value)? {
            EventMsg::InferenceCall(event) => event
                .into_durable()
                .ok_or_else(|| anyhow::anyhow!("inference observation exceeded durable bounds"))?,
            _ => bail!("inference_call protocol payload was not an inference event"),
        };

        if event.status == InferenceCallStatus::Started {
            if self
                .rollout
                .inference_observations
                .contains_key(&event.inference_call_id)
            {
                bail!(
                    "duplicate inference observation start for {}",
                    event.inference_call_id
                );
            }
            let inference_call_id = event.inference_call_id.clone();
            self.rollout.inference_observations.insert(
                inference_call_id.clone(),
                InferenceCallObservation {
                    inference_call_id,
                    started_event: event.clone(),
                    execution: ExecutionWindow {
                        started_at_unix_ms: wall_time_unix_ms,
                        started_seq: seq,
                        ended_at_unix_ms: None,
                        ended_seq: None,
                        status: ExecutionStatus::Running,
                    },
                    event,
                    raw_event_payload_ids: vec![event_payload.raw_payload_id],
                },
            );
            return Ok(());
        }

        let inference_call_id = event.inference_call_id.clone();
        if let Some(observation) = self
            .rollout
            .inference_observations
            .get_mut(&inference_call_id)
        {
            if observation.execution.status != ExecutionStatus::Running {
                bail!(
                    "duplicate inference observation terminal for {}",
                    inference_call_id
                );
            }
            if event.status == InferenceCallStatus::LocalDenied {
                bail!(
                    "inference observation local denial followed physical open for {}",
                    inference_call_id
                );
            }
            ensure_same_attempt_identity(&observation.started_event, &event)?;
            observation.execution.ended_at_unix_ms = Some(wall_time_unix_ms);
            observation.execution.ended_seq = Some(seq);
            observation.execution.status = execution_status(event.status);
            observation.event = event;
            observation
                .raw_event_payload_ids
                .push(event_payload.raw_payload_id);
        } else {
            // The writer is best-effort, so a failed start-payload write can
            // leave a durable terminal event. Preserve that evidence as a
            // terminal-only observation instead of dropping the event or
            // claiming that a start was observed.
            self.rollout.inference_observations.insert(
                inference_call_id.clone(),
                InferenceCallObservation {
                    inference_call_id,
                    started_event: event.clone(),
                    execution: ExecutionWindow {
                        started_at_unix_ms: wall_time_unix_ms,
                        started_seq: seq,
                        ended_at_unix_ms: Some(wall_time_unix_ms),
                        ended_seq: Some(seq),
                        status: execution_status(event.status),
                    },
                    event,
                    raw_event_payload_ids: vec![event_payload.raw_payload_id],
                },
            );
        }
        Ok(())
    }
}

fn execution_status(status: InferenceCallStatus) -> ExecutionStatus {
    match status {
        InferenceCallStatus::Completed => ExecutionStatus::Completed,
        InferenceCallStatus::Cancelled => ExecutionStatus::Cancelled,
        InferenceCallStatus::Started => ExecutionStatus::Running,
        InferenceCallStatus::Failed
        | InferenceCallStatus::UsageLimitReached
        | InferenceCallStatus::LocalDenied
        | InferenceCallStatus::AdmissionFailed
        | InferenceCallStatus::TransportUncertain
        | InferenceCallStatus::Unknown => ExecutionStatus::Failed,
    }
}

fn ensure_same_attempt_identity(
    started: &InferenceCallEvent,
    terminal: &InferenceCallEvent,
) -> Result<()> {
    let same = started.inference_call_id == terminal.inference_call_id
        && started.thread_id == terminal.thread_id
        && started.turn_id == terminal.turn_id
        && started.spawn_request_id == terminal.spawn_request_id
        && started.transport == terminal.transport
        && started.source == terminal.source
        && started.configured_provider == terminal.configured_provider
        && started.configured_model == terminal.configured_model
        && started.requested_model == terminal.requested_model
        && started.effective_provider == terminal.effective_provider
        && started.effective_model == terminal.effective_model
        && started.requested_service_tier == terminal.requested_service_tier
        && started.request_started_at_ms == terminal.request_started_at_ms;
    if !same {
        bail!(
            "inference observation terminal changed immutable attempt identity for {}",
            started.inference_call_id
        );
    }
    Ok(())
}
