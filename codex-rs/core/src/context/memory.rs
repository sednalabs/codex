//! Typed, bounded v2 memory context. Extraction chunks preserve selected evidence in order.

use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::truncate_text;

/// Memory-owned model context, capped below 10k tokens even for byte-heavy text.
pub enum MemoryContextFragment {
    ReadInstructions(String),
    ExtractionEvidence(String),
}

impl MemoryContextFragment {
    /// Splits already-budgeted extraction input without dropping evidence.
    pub fn extraction_messages(mut text: &str) -> Vec<ResponseItem> {
        let mut messages = Vec::new();
        while !text.is_empty() {
            let end = text.floor_char_boundary(text.len().min(8_900));
            messages.push(ContextualUserFragment::into(Self::ExtractionEvidence(
                text[..end].to_string(),
            )));
            text = &text[end..];
        }
        messages
    }
}

impl ContextualUserFragment for MemoryContextFragment {
    fn role(&self) -> &'static str {
        match self {
            Self::ReadInstructions(_) => "developer",
            Self::ExtractionEvidence(_) => "user",
        }
    }
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind(
            match self {
                Self::ReadInstructions(_) => "memories.instructions",
                Self::ExtractionEvidence(_) => "memories.extraction_evidence",
            }
            .to_string(),
        )
    }
    fn requires_separate_message(&self) -> bool {
        true
    }
    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }
    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }
    fn body(&self) -> String {
        let (Self::ReadInstructions(text) | Self::ExtractionEvidence(text)) = self;
        truncate_text(text, TruncationPolicy::Bytes(8_900))
    }
}

#[cfg(test)]
#[path = "memory_tests.rs"]
mod tests;
