//! Owns synchronous Guardian review policy independently of the host session runtime.
//! The host supplies review attempts and enforces the resulting decision on the bound action.

mod assessment;
mod circuit_breaker;
mod model;
mod outcome;
mod retry;

pub use assessment::GuardianAssessment;
pub use assessment::guardian_output_contract_prompt;
pub use assessment::guardian_output_schema;
pub use assessment::parse_guardian_assessment;
pub use circuit_breaker::AUTO_REVIEW_DENIAL_WINDOW_SIZE;
pub use circuit_breaker::GuardianRejectionCircuitBreaker;
pub use circuit_breaker::GuardianRejectionCircuitBreakerAction;
pub use circuit_breaker::GuardianRejectionCircuitBreakerPolicy;
pub use model::ReviewModel;
pub use model::select_review_model;
pub use outcome::GuardianReviewError;
pub use outcome::GuardianReviewOutcome;
pub use outcome::GuardianReviewSessionOutcome;
pub use retry::GuardianReviewSessionLimits;
pub use retry::run_with_retry;

pub const MAX_REVIEW_ATTEMPTS: i64 = 3;
pub const REVIEW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);
