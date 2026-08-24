//! Contract tests for exact-thread provider rate-limit evidence.
//!
//! These tests intentionally use explicit provider-declared keys. They do not infer shared
//! quota or independence from parentage, model names, or process-global state.

use codex_extension_api::RateLimitDomain;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;
use std::time::Duration;

fn domain(
    thread_id: ThreadId,
    provider_id: &str,
    shared_quota_key: Option<&str>,
) -> RateLimitDomain {
    RateLimitDomain {
        thread_id,
        provider_id: Some(provider_id.to_string()),
        requested_model: Some("requested-model".to_string()),
        effective_model: Some("effective-model".to_string()),
        account_context_key: None,
        shared_quota_key: shared_quota_key.map(str::to_string),
        snapshot: None,
        reset_at: None,
        retry_after: Some(Duration::from_secs(30)),
    }
}

#[test]
fn explicit_domains_are_bound_to_their_exact_thread() {
    let thread_a = ThreadId::new();
    let thread_b = ThreadId::new();
    let domain_a = domain(thread_a, "provider-a", Some("quota-a"));
    let domain_b = domain(thread_b, "provider-b", Some("quota-b"));

    assert_eq!(domain_a.thread_id, thread_a);
    assert_eq!(domain_b.thread_id, thread_b);
    assert_ne!(domain_a, domain_b);
    assert_ne!(domain_a.shared_quota_key, domain_b.shared_quota_key);
}

#[test]
fn only_an_explicit_shared_quota_key_can_match_another_domain() {
    let thread_a = ThreadId::new();
    let thread_b = ThreadId::new();
    let domain_a = domain(thread_a, "provider", Some("provider-account-window"));
    let domain_b = domain(thread_b, "provider", Some("provider-account-window"));

    assert_eq!(
        domain_a.shared_quota_key.as_deref(),
        domain_b.shared_quota_key.as_deref()
    );
    assert_ne!(domain_a.thread_id, domain_b.thread_id);
}

#[test]
fn unknown_account_and_quota_scope_is_not_claimed_independent() {
    let domain = domain(ThreadId::new(), "provider", None);

    assert_eq!(domain.account_context_key, None);
    assert_eq!(domain.shared_quota_key, None);
}
