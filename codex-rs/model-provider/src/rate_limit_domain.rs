//! Typed, side-effect-free facts for reasoning about rate-limit domains.
//!
//! This module deliberately does not infer coupling from model or thread
//! identities.  Provider scope and eligibility are observations, and remain
//! unknown until a provider supplies them.

/// The provider's declared coupling domain for a request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RateLimitDomainScope {
    /// Requests with the same opaque key intentionally share a limit domain.
    Shared(String),
    /// The provider states that this request has an independent domain.
    Independent,
    /// No safe scope conclusion is available.
    Unknown,
}

/// Facts known locally when a request was made. These are not provider claims.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalRequestFacts {
    /// Exact request correlation supplied by the caller.
    pub request_id: String,
    /// Optional caller-supplied model identity; it is never used to infer scope.
    pub model: Option<String>,
    /// Optional caller-supplied thread identity; it is never used to infer scope.
    pub thread_id: Option<String>,
}

/// Facts observed from the provider, retained as opaque values for provenance.
#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub struct ProviderObservedFacts {
    pub provider_scope: Option<String>,
    pub eligible: Option<bool>,
    pub deadline: Option<String>,
    pub budget: Option<String>,
    pub freshness: Option<String>,
}

/// Immutable, correlated evidence joining local request facts to observations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RateLimitEvidence {
    pub local: LocalRequestFacts,
    pub observed: ProviderObservedFacts,
    pub scope: RateLimitDomainScope,
}

impl RateLimitEvidence {
    /// G2: enough provider attribution exists to retain evidence, without
    /// asserting that admission is safe. Missing deadline, budget, or
    /// freshness intentionally does not make this helper admission-capable.
    pub fn g2_evidence_only_ready(&self) -> bool {
        !self.is_dormant()
    }

    /// G3: admission-capable only when every required provider fact is present
    /// and the provider explicitly marks the request eligible.
    pub fn g3_admission_capable_ready(&self) -> bool {
        !self.is_dormant()
            && self.observed.eligible == Some(true)
            && self.observed.deadline.is_some()
            && self.observed.budget.is_some()
            && self.observed.freshness.is_some()
    }

    /// Unknown provider scope or eligibility is fail-closed and remains dormant.
    pub fn is_dormant(&self) -> bool {
        matches!(self.scope, RateLimitDomainScope::Unknown)
            || self.observed.provider_scope.is_none()
            || self.observed.eligible.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(request_id: &str, model: Option<&str>, thread_id: Option<&str>) -> LocalRequestFacts {
        LocalRequestFacts {
            request_id: request_id.into(),
            model: model.map(str::into),
            thread_id: thread_id.map(str::into),
        }
    }

    fn observed_complete() -> ProviderObservedFacts {
        ProviderObservedFacts {
            provider_scope: Some("opaque-provider-domain".into()),
            eligible: Some(true),
            deadline: Some("opaque-deadline".into()),
            budget: Some("opaque-budget".into()),
            freshness: Some("opaque-freshness".into()),
        }
    }

    #[test]
    fn scope_equality_is_explicit_and_does_not_infer_from_model_or_thread() {
        assert_eq!(
            RateLimitDomainScope::Shared("same".into()),
            RateLimitDomainScope::Shared("same".into())
        );
        assert_ne!(
            RateLimitDomainScope::Shared("a".into()),
            RateLimitDomainScope::Shared("b".into())
        );
        let a = local("r1", Some("model-a"), Some("thread-a"));
        let b = local("r2", Some("model-a"), Some("thread-b"));
        assert_ne!(a, b);
        assert_eq!(RateLimitDomainScope::Unknown, RateLimitDomainScope::Unknown);
    }

    #[test]
    fn local_and_provider_provenance_remain_separate() {
        let evidence = RateLimitEvidence {
            local: local("request-1", Some("model-a"), None),
            observed: observed_complete(),
            scope: RateLimitDomainScope::Independent,
        };
        assert_eq!(evidence.local.request_id, "request-1");
        assert_eq!(evidence.observed.provider_scope.as_deref(), Some("opaque-provider-domain"));
    }

    #[test]
    fn missing_provider_scope_or_eligibility_is_dormant() {
        for observed in [
            ProviderObservedFacts {
                provider_scope: None,
                eligible: Some(true),
                ..observed_complete()
            },
            ProviderObservedFacts {
                eligible: None,
                ..observed_complete()
            },
        ] {
            let evidence = RateLimitEvidence {
                local: local("r", None, None),
                observed,
                scope: RateLimitDomainScope::Independent,
            };
            assert!(evidence.is_dormant());
            assert!(!evidence.g2_evidence_only_ready());
            assert!(!evidence.g3_admission_capable_ready());
        }
    }

    #[test]
    fn g3_requires_all_provider_facts() {
        let mut evidence = RateLimitEvidence {
            local: local("r", None, None),
            observed: observed_complete(),
            scope: RateLimitDomainScope::Shared("key".into()),
        };
        assert!(evidence.g2_evidence_only_ready());
        assert!(evidence.g3_admission_capable_ready());
        evidence.observed.freshness = None;
        assert!(!evidence.g3_admission_capable_ready());
    }
}
