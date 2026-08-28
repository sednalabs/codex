//! Shared context sections for synchronous Guardian review and asynchronous scoring.
//!
//! Contributor failures abort collection without returning partial context.
//! Sections carry structured transcript evidence without depending on either
//! consumer's rendering, retention, compaction, or request lifecycle.
//! Registered contributors declare their scope once and are collected only for
//! matching context consumers.

use std::sync::Arc;

use codex_protocol::models::ResponseItem;

pub use entry::ConversationTranscriptEntry;
pub use entry::ConversationTranscriptEntryKind;
pub use truncation::truncate_text;

mod entry;
mod truncation;

/// Consumer for which a Guardian context is composed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextTarget {
    /// The reusable synchronous Guardian reviewer.
    Sync,
    /// The asynchronous Guardian action scorer.
    Async,
}

/// Consumers to which a context section contributes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SectionScope {
    /// Include the section in both synchronous review and asynchronous scoring.
    Shared,
    /// Include the section only in synchronous review.
    SyncOnly,
    /// Include the section only in asynchronous scoring.
    AsyncOnly,
}

impl SectionScope {
    /// Whether this section is included for the requested context consumer.
    pub fn includes(self, target: ContextTarget) -> bool {
        match self {
            Self::Shared => true,
            Self::SyncOnly => matches!(target, ContextTarget::Sync),
            Self::AsyncOnly => matches!(target, ContextTarget::Async),
        }
    }
}

/// Borrowed host inputs available while one Guardian context section is built.
#[derive(Clone, Copy, Debug)]
pub struct SectionInput<'a> {
    /// Consumer for which the host is collecting context sections.
    pub target: ContextTarget,
    /// Parent conversation history available to this contribution.
    pub history: &'a [ResponseItem],
}

/// Supplies one independently scoped section to Guardian context assembly.
///
/// Implementations declare whether they apply to synchronous review,
/// asynchronous scoring, or both. The registry filters contributors by scope
/// before invoking them. Contributors distinguish sections that do not apply
/// from required evidence that could not be collected.
pub trait SectionContributor: Send + Sync {
    /// Guardian consumers that should receive this contribution.
    fn scope(&self) -> SectionScope;

    /// Builds this section using the host's current conversation snapshot.
    ///
    /// Return `Ok(None)` only when this section is optional or does not apply.
    /// Missing required evidence must return `Err`; callers must not review a
    /// partial context as though collection succeeded.
    fn contribute(&self, input: &SectionInput<'_>) -> Result<Option<ContextSection>, SectionError>;
}

/// A section could not provide the evidence needed for a valid review context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SectionError {
    /// Evidence required by this contributor for the current input is missing.
    MissingRequiredEvidence { section: &'static str },
}

impl std::fmt::Display for SectionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingRequiredEvidence { section } => {
                write!(formatter, "missing required evidence for section {section}")
            }
        }
    }
}

impl std::error::Error for SectionError {}

/// Ordered collection of independently scoped Guardian section contributors.
#[derive(Clone, Default)]
pub struct SectionRegistry {
    contributors: Vec<Arc<dyn SectionContributor>>,
}

impl SectionRegistry {
    /// Adds a contributor to the end of the section collection order.
    pub fn register(&mut self, contributor: impl SectionContributor + 'static) {
        self.contributors.push(Arc::new(contributor));
    }

    /// Collects applicable sections in their original registration order.
    ///
    /// Stops at the first error without returning any partial context. The host
    /// decides whether to fall back to synchronous review or deny approval.
    pub fn collect(&self, input: &SectionInput<'_>) -> Result<Vec<ContextSection>, SectionError> {
        self.contributors
            .iter()
            .filter(|contributor| contributor.scope().includes(input.target))
            .filter_map(|contributor| contributor.contribute(input).transpose())
            .collect()
    }
}

/// Ordered transcript evidence produced by one section contributor.
#[derive(Clone, Debug, PartialEq)]
pub struct ContextSection {
    /// Structured evidence before consumer-specific selection and rendering.
    pub items: Vec<ConversationTranscriptEntry>,
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod registry_tests;
