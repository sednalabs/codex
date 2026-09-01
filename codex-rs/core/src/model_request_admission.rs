//! Provider-authoritative admission for one model request attempt.
//!
//! This module deliberately has no knowledge of endpoints, accounts, models, or
//! service tiers.  A caller supplies a trusted opaque correlation value and the
//! provider's decision; no alternate provider or route is selected here.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Opaque correlation material trusted by the caller that owns request identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrustedCorrelation(Vec<u8>);

impl TrustedCorrelation {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }
}

/// A generation/lease pair is intentionally non-Clone: it belongs to one attempt.
#[derive(Debug, Eq, PartialEq)]
pub struct AttemptLease {
    generation: u64,
    lease: u64,
}

impl AttemptLease {
    pub fn new(generation: u64, lease: u64) -> Self {
        Self { generation, lease }
    }
}

/// Provider evidence.  The provider response is the authority for this decision.
#[derive(Debug, Eq, PartialEq)]
pub enum ProviderAdmission {
    Admitted,
    Denied,
    Deferred { retry_after: Duration },
    UnknownDomain,
}

#[derive(Debug, Eq, PartialEq)]
pub enum AdmissionOutcome {
    Admitted(PhysicalSendPermit),
    ProviderDenied,
    ProviderDeferred { retry_after: Duration },
    DormantDomain,
    CancelledBeforeSend,
    StaleGenerationOrLease,
    AlreadyDecided,
}

/// Non-Clone, single-use capability establishing ownership of a future physical send.
/// It intentionally exposes no endpoint, account, model, or authentication data.
#[derive(Debug, Eq, PartialEq)]
pub struct PhysicalSendPermit {
    correlation: TrustedCorrelation,
}

impl PhysicalSendPermit {
    /// Consume the permit at the transport-owned boundary.
    pub fn into_transport_ownership(self) -> PhysicalSendOwnership {
        PhysicalSendOwnership {
            correlation: self.correlation,
        }
    }
}

/// Opaque marker handed to the transport after the permit is consumed.
#[derive(Debug, Eq, PartialEq)]
pub struct PhysicalSendOwnership {
    correlation: TrustedCorrelation,
}

/// One exact attempt.  Admission is immutable and can succeed at most once.
#[derive(Debug)]
pub struct ModelRequestAttempt {
    correlation: TrustedCorrelation,
    lease: AttemptLease,
    decided: AtomicBool,
}

impl ModelRequestAttempt {
    pub fn new(correlation: TrustedCorrelation, lease: AttemptLease) -> Self {
        Self {
            correlation,
            lease,
            decided: AtomicBool::new(false),
        }
    }

    /// Evaluate exactly one provider decision.  No denial or deferral is rerouted.
    pub fn admit(
        &self,
        provider: ProviderAdmission,
        current_generation: u64,
        current_lease: u64,
        cancelled_before_send: bool,
    ) -> AdmissionOutcome {
        if self.decided.swap(true, Ordering::AcqRel) {
            return AdmissionOutcome::AlreadyDecided;
        }
        if cancelled_before_send {
            return AdmissionOutcome::CancelledBeforeSend;
        }
        if self.lease.generation != current_generation || self.lease.lease != current_lease {
            return AdmissionOutcome::StaleGenerationOrLease;
        }
        match provider {
            ProviderAdmission::Admitted => AdmissionOutcome::Admitted(PhysicalSendPermit {
                correlation: self.correlation.clone(),
            }),
            ProviderAdmission::Denied => AdmissionOutcome::ProviderDenied,
            ProviderAdmission::Deferred { retry_after } => {
                AdmissionOutcome::ProviderDeferred { retry_after }
            }
            ProviderAdmission::UnknownDomain => AdmissionOutcome::DormantDomain,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt() -> ModelRequestAttempt {
        ModelRequestAttempt::new(TrustedCorrelation::new([1, 2, 3]), AttemptLease::new(4, 5))
    }

    #[test]
    fn provider_outcomes_are_terminal_and_not_rerouted() {
        assert_eq!(
            attempt().admit(ProviderAdmission::Denied, 4, 5, false),
            AdmissionOutcome::ProviderDenied
        );
        assert_eq!(
            attempt().admit(ProviderAdmission::UnknownDomain, 4, 5, false),
            AdmissionOutcome::DormantDomain
        );
    }

    #[test]
    fn deferral_preserves_deadline() {
        assert_eq!(
            attempt().admit(
                ProviderAdmission::Deferred {
                    retry_after: Duration::from_secs(7)
                },
                4,
                5,
                false
            ),
            AdmissionOutcome::ProviderDeferred {
                retry_after: Duration::from_secs(7)
            }
        );
    }

    #[test]
    fn cancellation_and_stale_lease_fail_closed() {
        assert_eq!(
            attempt().admit(ProviderAdmission::Admitted, 4, 5, true),
            AdmissionOutcome::CancelledBeforeSend
        );
        assert_eq!(
            attempt().admit(ProviderAdmission::Admitted, 9, 5, false),
            AdmissionOutcome::StaleGenerationOrLease
        );
    }

    #[test]
    fn duplicate_admission_and_permit_consumption_are_impossible() {
        let a = attempt();
        let permit = match a.admit(ProviderAdmission::Admitted, 4, 5, false) {
            AdmissionOutcome::Admitted(p) => p,
            other => panic!("unexpected outcome: {other:?}"),
        };
        assert_eq!(
            a.admit(ProviderAdmission::Admitted, 4, 5, false),
            AdmissionOutcome::AlreadyDecided
        );
        let _ownership = permit.into_transport_ownership();
    }

    #[test]
    fn exact_attempt_is_at_most_once() {
        let a = attempt();
        assert!(matches!(
            a.admit(ProviderAdmission::Admitted, 4, 5, false),
            AdmissionOutcome::Admitted(_)
        ));
        assert_eq!(
            a.admit(ProviderAdmission::Denied, 4, 5, false),
            AdmissionOutcome::AlreadyDecided
        );
    }

    #[test]
    fn correlation_is_opaque_and_does_not_infer_authority() {
        let first = ModelRequestAttempt::new(TrustedCorrelation::new([0]), AttemptLease::new(1, 1));
        let second =
            ModelRequestAttempt::new(TrustedCorrelation::new([255, 254]), AttemptLease::new(1, 1));
        assert!(matches!(
            first.admit(ProviderAdmission::Admitted, 1, 1, false),
            AdmissionOutcome::Admitted(_)
        ));
        assert!(matches!(
            second.admit(ProviderAdmission::Admitted, 1, 1, false),
            AdmissionOutcome::Admitted(_)
        ));
    }
}
