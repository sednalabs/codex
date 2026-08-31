//! Typed, side-effect-free facts for reasoning about rate-limit domains.
//!
//! This module deliberately does not infer coupling from model or thread
//! identities. Provider scope and eligibility are observations, and remain
//! unknown until a provider supplies them.

use std::error::Error;
use std::fmt;

const MAX_OPAQUE_VALUE_LENGTH: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OpaqueValueError {
    Empty,
    TooLong,
    InvalidCharacter,
}

fn validate_opaque_value(value: &str) -> Result<(), OpaqueValueError> {
    if value.is_empty() {
        return Err(OpaqueValueError::Empty);
    }
    if value.len() > MAX_OPAQUE_VALUE_LENGTH {
        return Err(OpaqueValueError::TooLong);
    }
    if value.chars().any(char::is_control) {
        return Err(OpaqueValueError::InvalidCharacter);
    }
    Ok(())
}

/// An opaque, provider-issued non-secret identity for a rate-limit domain.
///
/// The value is intentionally not exposed after construction. Callers must
/// provide a bounded, control-character-free provider-domain identifier and
/// remain responsible for withholding secrets.
#[derive(Clone, Eq, PartialEq)]
pub struct ProviderDomainId(String);

impl ProviderDomainId {
    /// Construct an opaque provider-domain identity after validating its shape.
    ///
    /// This crate-private boundary keeps admission evidence construction under
    /// provider integration control; shape validation alone does not attest
    /// provider authority.
    pub(crate) fn try_new(value: impl AsRef<str>) -> Result<Self, ProviderDomainIdError> {
        let value = value.as_ref();
        validate_opaque_value(value).map_err(ProviderDomainIdError::from)?;
        Ok(Self(value.to_owned()))
    }
}

impl fmt::Debug for ProviderDomainId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ProviderDomainId")
            .field(&"<redacted>")
            .finish()
    }
}

/// Why an opaque provider-domain identity was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderDomainIdError {
    /// The supplied identity was empty.
    Empty,
    /// The supplied identity exceeded the bounded representation.
    TooLong,
    /// The supplied identity contained a control character.
    InvalidCharacter,
}

impl From<OpaqueValueError> for ProviderDomainIdError {
    fn from(error: OpaqueValueError) -> Self {
        match error {
            OpaqueValueError::Empty => Self::Empty,
            OpaqueValueError::TooLong => Self::TooLong,
            OpaqueValueError::InvalidCharacter => Self::InvalidCharacter,
        }
    }
}

impl fmt::Display for ProviderDomainIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "provider domain identity is empty",
            Self::TooLong => "provider domain identity is too long",
            Self::InvalidCharacter => "provider domain identity contains control characters",
        })
    }
}

impl Error for ProviderDomainIdError {}

#[derive(Clone, Eq, PartialEq)]
struct OpaqueProviderFact(String);

impl OpaqueProviderFact {
    fn try_new(value: &str) -> Result<Self, ProviderFactError> {
        validate_opaque_value(value).map_err(ProviderFactError::from)?;
        Ok(Self(value.to_owned()))
    }
}

/// Why an opaque provider observation was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderFactError {
    /// The supplied fact was empty.
    Empty,
    /// The supplied fact exceeded the bounded representation.
    TooLong,
    /// The supplied fact contained a control character.
    InvalidCharacter,
}

impl From<OpaqueValueError> for ProviderFactError {
    fn from(error: OpaqueValueError) -> Self {
        match error {
            OpaqueValueError::Empty => Self::Empty,
            OpaqueValueError::TooLong => Self::TooLong,
            OpaqueValueError::InvalidCharacter => Self::InvalidCharacter,
        }
    }
}

impl fmt::Display for ProviderFactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "provider fact is empty",
            Self::TooLong => "provider fact is too long",
            Self::InvalidCharacter => "provider fact contains control characters",
        })
    }
}

impl Error for ProviderFactError {}

#[derive(Clone, Eq, PartialEq)]
enum DomainScopeKind {
    Shared(ProviderDomainId),
    Independent,
    Unknown,
}

/// The provider's declared coupling domain for a request.
#[derive(Clone, Eq, PartialEq)]
pub struct RateLimitDomainScope(DomainScopeKind);

impl RateLimitDomainScope {
    /// Construct a shared scope bound to one opaque provider-domain identity.
    pub(crate) fn shared(provider_domain: ProviderDomainId) -> Self {
        Self(DomainScopeKind::Shared(provider_domain))
    }

    /// Construct the provider-declared independent scope.
    pub(crate) fn independent() -> Self {
        Self(DomainScopeKind::Independent)
    }

    /// Construct the fail-closed unknown scope.
    pub fn unknown() -> Self {
        Self(DomainScopeKind::Unknown)
    }
}

impl fmt::Debug for RateLimitDomainScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            DomainScopeKind::Shared(_) => formatter
                .debug_tuple("RateLimitDomainScope::Shared")
                .field(&"<redacted>")
                .finish(),
            DomainScopeKind::Independent => {
                formatter.write_str("RateLimitDomainScope::Independent")
            }
            DomainScopeKind::Unknown => formatter.write_str("RateLimitDomainScope::Unknown"),
        }
    }
}

/// Facts known locally when a request was made. These are not provider claims.
#[derive(Clone, Eq, PartialEq)]
pub struct LocalRequestFacts {
    request_id: String,
    model: Option<String>,
    thread_id: Option<String>,
}

impl LocalRequestFacts {
    /// Capture caller facts without making any scope inference from them.
    pub fn new(
        request_id: impl Into<String>,
        model: Option<String>,
        thread_id: Option<String>,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            model,
            thread_id,
        }
    }
}

impl fmt::Debug for LocalRequestFacts {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalRequestFacts")
            .field("request_id", &"<redacted>")
            .field("model", &self.model.as_ref().map(|_| "<redacted>"))
            .field("thread_id", &self.thread_id.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Facts observed from the provider, retained as opaque values for provenance.
#[derive(Clone, Eq, PartialEq, Default)]
pub struct ProviderObservedFacts {
    provider_scope: Option<ProviderDomainId>,
    eligible: Option<bool>,
    deadline: Option<OpaqueProviderFact>,
    budget: Option<OpaqueProviderFact>,
    freshness: Option<OpaqueProviderFact>,
}

impl ProviderObservedFacts {
    /// Construct provider observations after validating opaque fact values.
    ///
    /// A missing provider scope or eligibility is valid evidence, but remains
    /// dormant at the admission-capable boundary. This constructor preserves
    /// caller-supplied observations but does not itself attest their source;
    /// the crate-private boundary is reserved for provider integrations.
    pub(crate) fn try_from_provider(
        provider_scope: Option<ProviderDomainId>,
        eligible: Option<bool>,
        deadline: Option<&str>,
        budget: Option<&str>,
        freshness: Option<&str>,
    ) -> Result<Self, ProviderFactError> {
        Ok(Self {
            provider_scope,
            eligible,
            deadline: deadline.map(OpaqueProviderFact::try_new).transpose()?,
            budget: budget.map(OpaqueProviderFact::try_new).transpose()?,
            freshness: freshness.map(OpaqueProviderFact::try_new).transpose()?,
        })
    }
}

impl fmt::Debug for ProviderObservedFacts {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderObservedFacts")
            .field(
                "provider_scope",
                &self.provider_scope.as_ref().map(|_| "<redacted>"),
            )
            .field("eligible", &self.eligible)
            .field("deadline", &self.deadline.as_ref().map(|_| "<redacted>"))
            .field("budget", &self.budget.as_ref().map(|_| "<redacted>"))
            .field("freshness", &self.freshness.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Immutable, provider-correlated evidence joining local request facts to observations.
#[derive(Clone, Eq, PartialEq)]
pub struct RateLimitEvidence {
    local: LocalRequestFacts,
    observed: ProviderObservedFacts,
    scope: RateLimitDomainScope,
}

impl RateLimitEvidence {
    /// Join facts only when a shared scope is bound to the same provider identity.
    pub fn try_new(
        local: LocalRequestFacts,
        observed: ProviderObservedFacts,
        scope: RateLimitDomainScope,
    ) -> Result<Self, RateLimitEvidenceError> {
        match &scope.0 {
            DomainScopeKind::Shared(expected_domain)
                if observed.provider_scope.as_ref() != Some(expected_domain) =>
            {
                return Err(RateLimitEvidenceError::SharedScopeMismatch);
            }
            DomainScopeKind::Independent if observed.provider_scope.is_none() => {
                return Err(RateLimitEvidenceError::IndependentScopeMissingProviderObservation);
            }
            _ => {}
        }

        Ok(Self {
            local,
            observed,
            scope,
        })
    }

    /// G2: enough provider attribution exists to retain evidence, without
    /// asserting that admission is safe. Unknown provider scope or eligibility
    /// is deliberately retainable evidence; use `is_dormant` and G3 for the
    /// fail-closed admission boundary.
    pub fn g2_evidence_only_ready(&self) -> bool {
        true
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
        matches!(&self.scope.0, DomainScopeKind::Unknown)
            || self.observed.provider_scope.is_none()
            || self.observed.eligible.is_none()
    }
}

impl fmt::Debug for RateLimitEvidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RateLimitEvidence")
            .field("local", &self.local)
            .field("observed", &self.observed)
            .field("scope", &self.scope)
            .finish()
    }
}

/// Why provider-correlated evidence could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RateLimitEvidenceError {
    /// A shared scope did not carry the exact provider identity observed for the request.
    SharedScopeMismatch,
    /// An independent scope was asserted without a provider scope observation.
    IndependentScopeMissingProviderObservation,
}

impl fmt::Display for RateLimitEvidenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SharedScopeMismatch => {
                "shared rate-limit scope does not match provider observation"
            }
            Self::IndependentScopeMissingProviderObservation => {
                "independent rate-limit scope requires provider scope observation"
            }
        })
    }
}

impl Error for RateLimitEvidenceError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(request_id: &str, model: Option<&str>, thread_id: Option<&str>) -> LocalRequestFacts {
        LocalRequestFacts::new(
            request_id,
            model.map(str::to_owned),
            thread_id.map(str::to_owned),
        )
    }

    fn provider_domain() -> ProviderDomainId {
        ProviderDomainId::try_new("opaque-provider-domain").unwrap()
    }

    fn observed_complete() -> ProviderObservedFacts {
        ProviderObservedFacts::try_from_provider(
            Some(provider_domain()),
            Some(true),
            Some("opaque-deadline"),
            Some("opaque-budget"),
            Some("opaque-freshness"),
        )
        .unwrap()
    }

    fn evidence(
        observed: ProviderObservedFacts,
        scope: RateLimitDomainScope,
    ) -> Result<RateLimitEvidence, RateLimitEvidenceError> {
        RateLimitEvidence::try_new(
            local("request-1", Some("model-a"), Some("thread-a")),
            observed,
            scope,
        )
    }

    #[test]
    fn scope_equality_is_explicit_and_does_not_infer_from_model_or_thread() {
        let same = provider_domain();
        assert_eq!(
            RateLimitDomainScope::shared(same.clone()),
            RateLimitDomainScope::shared(same)
        );
        assert_ne!(
            RateLimitDomainScope::shared(ProviderDomainId::try_new("a").unwrap()),
            RateLimitDomainScope::shared(ProviderDomainId::try_new("b").unwrap())
        );
        let a = local("r1", Some("model-a"), Some("thread-a"));
        let b = local("r2", Some("model-a"), Some("thread-b"));
        assert_ne!(a, b);
        assert_eq!(
            RateLimitDomainScope::unknown(),
            RateLimitDomainScope::unknown()
        );
    }

    #[test]
    fn local_and_provider_provenance_remain_separate() {
        let evidence = evidence(observed_complete(), RateLimitDomainScope::independent()).unwrap();
        assert!(evidence.g2_evidence_only_ready());
        assert!(evidence.g3_admission_capable_ready());
    }

    #[test]
    fn mismatched_provider_domains_are_rejected() {
        let mismatched_scope = RateLimitDomainScope::shared(
            ProviderDomainId::try_new("other-provider-domain").unwrap(),
        );
        assert_eq!(
            evidence(observed_complete(), mismatched_scope),
            Err(RateLimitEvidenceError::SharedScopeMismatch)
        );
    }

    #[test]
    fn malformed_opaque_values_are_rejected() {
        assert_eq!(
            ProviderDomainId::try_new(""),
            Err(ProviderDomainIdError::Empty)
        );
        assert_eq!(
            ProviderDomainId::try_new("provider\n-domain"),
            Err(ProviderDomainIdError::InvalidCharacter)
        );
        assert_eq!(
            ProviderDomainId::try_new("x".repeat(MAX_OPAQUE_VALUE_LENGTH + 1)),
            Err(ProviderDomainIdError::TooLong)
        );

        assert_eq!(
            ProviderObservedFacts::try_from_provider(
                /*provider_scope*/ None,
                /*eligible*/ None,
                Some("provider\n-fact"),
                /*budget*/ None,
                /*freshness*/ None,
            ),
            Err(ProviderFactError::InvalidCharacter)
        );
        assert_eq!(
            ProviderObservedFacts::try_from_provider(
                /*provider_scope*/ None,
                /*eligible*/ None,
                Some(&"x".repeat(MAX_OPAQUE_VALUE_LENGTH + 1)),
                /*budget*/ None,
                /*freshness*/ None,
            ),
            Err(ProviderFactError::TooLong)
        );
    }

    #[test]
    fn printable_provider_punctuation_is_accepted() {
        for value in [
            "provider domain / shared+scope = test@example",
            "2026-08-30T12:34:56+10:00",
            "arn:aws:iam::123456789012:role/provider+reader@example",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ==+/",
        ] {
            assert!(
                ProviderDomainId::try_new(value).is_ok(),
                "rejected {value:?}"
            );
        }

        ProviderObservedFacts::try_from_provider(
            Some(ProviderDomainId::try_new("provider / scope+1@example").unwrap()),
            Some(true),
            Some("2026-08-30T12:34:56+10:00"),
            Some("budget=100 / minute"),
            Some("freshness@example +00:00"),
        )
        .unwrap();
    }

    #[test]
    fn shared_scope_requires_provider_scope_observation() {
        let observed = ProviderObservedFacts::try_from_provider(
            /*provider_scope*/ None,
            Some(true),
            Some("opaque-deadline"),
            Some("opaque-budget"),
            Some("opaque-freshness"),
        )
        .unwrap();
        assert_eq!(
            evidence(observed, RateLimitDomainScope::shared(provider_domain())),
            Err(RateLimitEvidenceError::SharedScopeMismatch)
        );
    }

    #[test]
    fn secret_safe_debug_does_not_render_raw_values() {
        let provider_domain = provider_domain();
        let provider_domain_debug = format!("{provider_domain:?}");
        assert!(!provider_domain_debug.contains("opaque-provider-domain"));
        assert!(provider_domain_debug.contains("<redacted>"));

        let shared_scope_debug = format!("{:?}", RateLimitDomainScope::shared(provider_domain));
        assert!(!shared_scope_debug.contains("opaque-provider-domain"));
        assert!(shared_scope_debug.contains("<redacted>"));

        let evidence = evidence(observed_complete(), RateLimitDomainScope::independent()).unwrap();
        let rendered = format!("{evidence:?}");
        for secret in [
            "request-1",
            "model-a",
            "thread-a",
            "opaque-provider-domain",
            "opaque-deadline",
            "opaque-budget",
            "opaque-freshness",
        ] {
            assert!(
                !rendered.contains(secret),
                "debug output leaked a sentinel value"
            );
        }
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn missing_provider_scope_or_eligibility_is_dormant() {
        let missing_scope = ProviderObservedFacts::try_from_provider(
            /*provider_scope*/ None,
            Some(true),
            Some("opaque-deadline"),
            Some("opaque-budget"),
            Some("opaque-freshness"),
        )
        .unwrap();
        let missing_scope_evidence =
            evidence(missing_scope, RateLimitDomainScope::unknown()).unwrap();
        assert!(missing_scope_evidence.is_dormant());
        assert!(missing_scope_evidence.g2_evidence_only_ready());
        assert!(!missing_scope_evidence.g3_admission_capable_ready());

        let missing_scope = ProviderObservedFacts::try_from_provider(
            /*provider_scope*/ None,
            Some(true),
            Some("opaque-deadline"),
            Some("opaque-budget"),
            Some("opaque-freshness"),
        )
        .unwrap();
        assert_eq!(
            evidence(missing_scope, RateLimitDomainScope::independent()),
            Err(RateLimitEvidenceError::IndependentScopeMissingProviderObservation)
        );

        let missing_eligibility = ProviderObservedFacts::try_from_provider(
            Some(provider_domain()),
            /*eligible*/ None,
            Some("opaque-deadline"),
            Some("opaque-budget"),
            Some("opaque-freshness"),
        )
        .unwrap();
        let missing_eligibility_evidence =
            evidence(missing_eligibility, RateLimitDomainScope::independent()).unwrap();
        assert!(missing_eligibility_evidence.is_dormant());
        assert!(missing_eligibility_evidence.g2_evidence_only_ready());
        assert!(!missing_eligibility_evidence.g3_admission_capable_ready());

        let unknown_scope = evidence(observed_complete(), RateLimitDomainScope::unknown()).unwrap();
        assert!(unknown_scope.is_dormant());
        assert!(unknown_scope.g2_evidence_only_ready());
        assert!(!unknown_scope.g3_admission_capable_ready());
    }

    #[test]
    fn g3_requires_all_provider_facts_and_true_eligibility() {
        let missing_freshness = ProviderObservedFacts::try_from_provider(
            Some(provider_domain()),
            Some(true),
            Some("opaque-deadline"),
            Some("opaque-budget"),
            /*freshness*/ None,
        )
        .unwrap();
        let missing_freshness_evidence =
            evidence(missing_freshness, RateLimitDomainScope::independent()).unwrap();
        assert!(missing_freshness_evidence.g2_evidence_only_ready());
        assert!(!missing_freshness_evidence.g3_admission_capable_ready());

        let ineligible = ProviderObservedFacts::try_from_provider(
            Some(provider_domain()),
            Some(false),
            Some("opaque-deadline"),
            Some("opaque-budget"),
            Some("opaque-freshness"),
        )
        .unwrap();
        let ineligible_evidence =
            evidence(ineligible, RateLimitDomainScope::independent()).unwrap();
        assert!(!ineligible_evidence.g3_admission_capable_ready());
    }

    #[test]
    fn valid_provider_correlated_shared_construction_is_admission_ready() {
        let domain = provider_domain();
        let observed = ProviderObservedFacts::try_from_provider(
            Some(domain.clone()),
            Some(true),
            Some("opaque-deadline"),
            Some("opaque-budget"),
            Some("opaque-freshness"),
        )
        .unwrap();
        let evidence = evidence(observed, RateLimitDomainScope::shared(domain)).unwrap();
        assert!(evidence.g2_evidence_only_ready());
        assert!(evidence.g3_admission_capable_ready());
    }
}
