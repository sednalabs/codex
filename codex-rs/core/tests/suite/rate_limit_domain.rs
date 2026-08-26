//! Contract tests for local request correlation and provider-limit evidence.
//!
//! These tests intentionally keep local request identity separate from provider evidence.
//! They do not infer account scope, shared quota, or independence from parentage, model names,
//! or process-global state.

use codex_extension_api::LocalRequestIdentity;
use codex_extension_api::ProviderEvidenceAuthority;
use codex_extension_api::ProviderLimitEvidence;
use codex_extension_api::RateLimitDomain;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;
use std::time::Duration;

fn domain(
    thread_id: ThreadId,
    provider_key: &str,
    authority: ProviderEvidenceAuthority,
) -> RateLimitDomain {
    RateLimitDomain {
        local_request_identity: LocalRequestIdentity {
            thread_id,
            configured_provider_key: Some(provider_key.to_string()),
            requested_model: Some("requested-model".to_string()),
            resolved_model: Some("resolved-model".to_string()),
        },
        provider_limit_evidence: ProviderLimitEvidence {
            authority,
            snapshot: None,
            reset_at: None,
            retry_after: Some(Duration::from_secs(30)),
        },
    }
}

#[test]
fn local_request_identity_preserves_exact_thread_correlation() {
    let thread_a = ThreadId::new();
    let thread_b = ThreadId::new();
    let domain_a = domain(
        thread_a,
        "provider-a",
        ProviderEvidenceAuthority::UnknownLostProvenance,
    );
    let domain_b = domain(
        thread_b,
        "provider-b",
        ProviderEvidenceAuthority::UnknownLostProvenance,
    );

    assert_eq!(domain_a.local_request_identity.thread_id, thread_a);
    assert_eq!(domain_b.local_request_identity.thread_id, thread_b);
}

#[test]
fn provider_evidence_authority_is_explicit() {
    let thread_a = ThreadId::new();
    let thread_b = ThreadId::new();
    let domain_a = domain(
        thread_a,
        "provider",
        ProviderEvidenceAuthority::UnknownLostProvenance,
    );
    let domain_b = domain(
        thread_b,
        "provider",
        ProviderEvidenceAuthority::UnknownUnsupportedTransport,
    );

    assert_eq!(
        domain_a.provider_limit_evidence.authority,
        ProviderEvidenceAuthority::UnknownLostProvenance
    );
    assert_eq!(
        domain_b.provider_limit_evidence.authority,
        ProviderEvidenceAuthority::UnknownUnsupportedTransport
    );
    assert_ne!(
        domain_a.provider_limit_evidence.authority,
        domain_b.provider_limit_evidence.authority
    );
    assert_ne!(
        domain_a.local_request_identity.thread_id,
        domain_b.local_request_identity.thread_id
    );
}

#[test]
fn local_identity_does_not_claim_provider_scope() {
    let domain = domain(
        ThreadId::new(),
        "provider",
        ProviderEvidenceAuthority::UnknownUnsupportedTransport,
    );

    assert_eq!(
        domain.provider_limit_evidence.authority,
        ProviderEvidenceAuthority::UnknownUnsupportedTransport
    );
    assert_eq!(domain.provider_limit_evidence.snapshot, None);
}
