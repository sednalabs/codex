//! Distinguishes completed assessments from failures without assigning risk to errors.

use crate::GuardianAssessment;
use codex_analytics::GuardianReviewFailureReason;
use codex_protocol::protocol::CodexErrorInfo;

#[derive(Debug)]
pub enum GuardianReviewOutcome {
    Completed(GuardianAssessment),
    Error(GuardianReviewError),
}

#[derive(Debug)]
pub enum GuardianReviewError {
    PromptBuild {
        message: String,
    },
    Session {
        message: String,
        error_info: Option<CodexErrorInfo>,
    },
    Parse {
        message: String,
    },
    Timeout,
    Cancelled,
}

impl GuardianReviewError {
    pub fn prompt_build(err: anyhow::Error) -> Self {
        Self::PromptBuild {
            message: err.to_string(),
        }
    }

    pub fn session(err: anyhow::Error) -> Self {
        Self::Session {
            message: err.to_string(),
            error_info: None,
        }
    }

    pub fn session_with_error_info(err: anyhow::Error, error_info: CodexErrorInfo) -> Self {
        Self::Session {
            message: err.to_string(),
            error_info: Some(error_info),
        }
    }

    pub fn parse(err: anyhow::Error) -> Self {
        Self::Parse {
            message: err.to_string(),
        }
    }

    pub fn failure_reason(&self) -> GuardianReviewFailureReason {
        match self {
            Self::PromptBuild { .. } => GuardianReviewFailureReason::PromptBuildError,
            Self::Session { .. } => GuardianReviewFailureReason::SessionError,
            Self::Parse { .. } => GuardianReviewFailureReason::ParseError,
            Self::Timeout => GuardianReviewFailureReason::Timeout,
            Self::Cancelled => GuardianReviewFailureReason::Cancelled,
        }
    }
}

#[derive(Debug)]
pub enum GuardianReviewSessionOutcome {
    Completed(anyhow::Result<Option<String>>),
    PromptBuildFailed(anyhow::Error),
    SessionFailed {
        error: anyhow::Error,
        error_info: Option<CodexErrorInfo>,
    },
    TimedOut,
    Aborted,
}

impl From<GuardianReviewSessionOutcome> for GuardianReviewOutcome {
    fn from(outcome: GuardianReviewSessionOutcome) -> Self {
        match outcome {
            GuardianReviewSessionOutcome::Completed(Ok(Some(message))) => {
                match crate::parse_guardian_assessment(Some(&message)) {
                    Ok(assessment) => Self::Completed(assessment),
                    Err(error) => Self::Error(GuardianReviewError::parse(error)),
                }
            }
            GuardianReviewSessionOutcome::Completed(Ok(None)) => {
                Self::Error(GuardianReviewError::session(anyhow::anyhow!(
                    "guardian review completed without an assessment payload"
                )))
            }
            GuardianReviewSessionOutcome::Completed(Err(error)) => {
                Self::Error(GuardianReviewError::session(error))
            }
            GuardianReviewSessionOutcome::PromptBuildFailed(error) => {
                Self::Error(GuardianReviewError::prompt_build(error))
            }
            GuardianReviewSessionOutcome::SessionFailed { error, error_info } => {
                Self::Error(match error_info {
                    Some(error_info) => {
                        GuardianReviewError::session_with_error_info(error, error_info)
                    }
                    None => GuardianReviewError::session(error),
                })
            }
            GuardianReviewSessionOutcome::TimedOut => Self::Error(GuardianReviewError::Timeout),
            GuardianReviewSessionOutcome::Aborted => Self::Error(GuardianReviewError::Cancelled),
        }
    }
}
