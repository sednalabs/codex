//! Map only supported owned provider-completion receipts to usage identities.

use super::{
    DiagnosticCallReference, UsageAccountScope, UsageJoinKey, MAX_USAGE_ID_BYTES,
};
use super::super::types::{
    EventKind, ObservationQuality, ProviderLedgerWriteOutcome, RecordedEvent, SourcePlane,
    PROVIDER_COMPLETION_PRODUCER_BOUNDARY,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderReferenceMapError {
    InvalidSelectedUsageNamespace,
    InvalidReceipt,
}

/// Maps only the supported owned provider-completion event to its explicit
/// ledger identity. Host operation IDs and event IDs are never provider IDs.
pub fn map_provider_completion_reference(
    event: &RecordedEvent,
    usage_source_namespace: &str,
) -> Result<Option<DiagnosticCallReference>, ProviderReferenceMapError> {
    if !bounded_identifier(usage_source_namespace) {
        return Err(ProviderReferenceMapError::InvalidSelectedUsageNamespace);
    }
    let input = &event.input;
    if event.truncated
        || input.source_plane != SourcePlane::RustCollab
        || input.kind != EventKind::ProviderCompletionObserved
        || input.quality != ObservationQuality::Owned
        || input.producer_boundary != PROVIDER_COMPLETION_PRODUCER_BOUNDARY
        || input.external_operation.is_some()
    {
        return Ok(None);
    }
    let Some(provider_call) = input.provider_call.as_ref() else {
        return Err(ProviderReferenceMapError::InvalidReceipt);
    };
    let Some(thread_id) = input.thread_id.as_deref() else {
        return Ok(None);
    };
    if !bounded_identifier(thread_id)
        || !bounded_identifier(&provider_call.provider)
        || !bounded_identifier(&provider_call.response_id)
        || input.turn_id.as_deref().is_some_and(|value| !bounded_identifier(value))
        || !bounded_identifier(&event.identity.capture_instance_id)
        || event.identity.sequence == 0
    {
        return Err(ProviderReferenceMapError::InvalidReceipt);
    }
    let account_scope = match &provider_call.ledger_response_scope {
        UsageAccountScope::KnownScope(value) if bounded_identifier(value) => {
            Some(UsageAccountScope::KnownScope(value.clone()))
        }
        UsageAccountScope::WriterUnscoped => Some(UsageAccountScope::WriterUnscoped),
        UsageAccountScope::Unknown => None,
        UsageAccountScope::KnownScope(_) => return Err(ProviderReferenceMapError::InvalidReceipt),
    };
    let call_id = match provider_call.ledger_write_outcome {
        ProviderLedgerWriteOutcome::Inserted => Some(
            provider_call
                .persisted_provider_call_id
                .as_deref()
                .filter(|value| bounded_identifier(value))
                .ok_or(ProviderReferenceMapError::InvalidReceipt)?
                .to_string(),
        ),
        ProviderLedgerWriteOutcome::Duplicate => {
            if provider_call.persisted_provider_call_id.is_some() {
                return Err(ProviderReferenceMapError::InvalidReceipt);
            }
            None
        }
        ProviderLedgerWriteOutcome::FailedUnknown | ProviderLedgerWriteOutcome::NotConfigured => {
            if provider_call.persisted_provider_call_id.is_some() {
                return Err(ProviderReferenceMapError::InvalidReceipt);
            }
            return Ok(None);
        }
    };
    let Some(account_scope) = account_scope else { return Ok(None) };
    Ok(Some(DiagnosticCallReference {
        key: UsageJoinKey {
            source_namespace: usage_source_namespace.to_string(),
            thread_id: thread_id.to_string(),
            turn_id: input.turn_id.clone(),
            provider: Some(provider_call.provider.clone()),
            account_scope,
            call_id,
            response_id: Some(provider_call.response_id.clone()),
        },
        source_plane: input.source_plane,
        event: event.identity.clone(),
    }))
}

fn bounded_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_USAGE_ID_BYTES
        && !value.chars().any(char::is_control)
}
