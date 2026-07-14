use codex_protocol::AgentPath;
use codex_protocol::protocol::TokenUsage;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::truncate_text;

use super::ContextualUserFragment;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InterAgentCompletionMessage {
    task_name: AgentPath,
    sender: AgentPath,
    provider_receipt: Option<CompletionProviderReceipt>,
    payload: String,
}

pub(crate) const COMPLETION_MESSAGE_MAX_TOKENS: usize = 1_000;
pub(crate) const COMPLETION_MESSAGE_MAX_RENDERED_BYTES: usize =
    (COMPLETION_MESSAGE_MAX_TOKENS - 1) * 4;
const COMPLETION_RECEIPT_MAX_RENDERED_BYTES: usize = 1_024;
const COMPLETION_PATH_MAX_RENDERED_BYTES: usize = 256;
const RECEIPT_IDENTITY_MAX_ESCAPED_BYTES: usize = 256;
const TRUNCATION_MARKER_MAX_BYTES: usize = 64;

/// Runtime-authored provider evidence for a child completion.
///
/// Identity describes only the terminal successful sampling response, while usage aggregates all
/// successful provider response completions observed during the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletionProviderReceipt {
    terminal_response_model: Option<String>,
    terminal_response_snapshot: Option<String>,
    turn_provider_usage: Option<TokenUsage>,
}

impl CompletionProviderReceipt {
    pub(crate) fn new(
        terminal_response_model: Option<String>,
        terminal_response_snapshot: Option<String>,
        turn_provider_usage: Option<TokenUsage>,
    ) -> Option<Self> {
        let terminal_response_model = terminal_response_model.filter(|model| !model.is_empty());
        let terminal_response_snapshot =
            terminal_response_snapshot.filter(|snapshot| !snapshot.is_empty());
        if terminal_response_model.is_none()
            && terminal_response_snapshot.is_none()
            && turn_provider_usage.is_none()
        {
            return None;
        }
        Some(Self {
            terminal_response_model,
            terminal_response_snapshot,
            turn_provider_usage,
        })
    }

    fn render(&self) -> String {
        let mut lines = vec!["<completion_provider_receipt>".to_string()];
        if let Some(model) = self.terminal_response_model.as_deref() {
            lines.push(render_identity_element("terminal_response_model", model));
        }
        if let Some(snapshot) = self.terminal_response_snapshot.as_deref() {
            lines.push(render_identity_element(
                "terminal_response_snapshot",
                snapshot,
            ));
        }
        if let Some(usage) = self.turn_provider_usage.as_ref() {
            lines.push(format!(
                "  <turn_provider_usage input_tokens=\"{}\" cached_input_tokens=\"{}\" output_tokens=\"{}\" reasoning_output_tokens=\"{}\" total_tokens=\"{}\" />",
                usage.input_tokens,
                usage.cached_input_tokens,
                usage.output_tokens,
                usage.reasoning_output_tokens,
                usage.total_tokens,
            ));
        }
        lines.push("</completion_provider_receipt>".to_string());
        let rendered = lines.join("\n");
        if rendered.len() <= COMPLETION_RECEIPT_MAX_RENDERED_BYTES {
            rendered
        } else {
            "<completion_provider_receipt>\n  <omitted reason=\"encoded_budget\" />\n</completion_provider_receipt>"
                .to_string()
        }
    }
}

fn render_identity_element(name: &str, value: &str) -> String {
    let (value, truncated) = bounded_xml_value(value);
    let truncation = if truncated { " truncated=\"true\"" } else { "" };
    format!("  <{name}{truncation}>{value}</{name}>")
}

fn bounded_xml_value(value: &str) -> (String, bool) {
    let mut escaped = String::new();
    let mut utf8 = [0_u8; 4];
    for ch in value.chars() {
        let encoded = ch.encode_utf8(&mut utf8);
        let encoded = match ch {
            '&' => "&amp;",
            '<' => "&lt;",
            '>' => "&gt;",
            '\"' => "&quot;",
            '\'' => "&apos;",
            '\t' | '\n' | '\r' => encoded,
            ch if ch >= ' ' && ch != '\u{fffe}' && ch != '\u{ffff}' => encoded,
            _ => "\u{fffd}",
        };
        if escaped.len().saturating_add(encoded.len()) > RECEIPT_IDENTITY_MAX_ESCAPED_BYTES {
            return (escaped, true);
        }
        escaped.push_str(encoded);
    }
    (escaped, false)
}

fn bounded_text(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    truncate_text(
        value,
        TruncationPolicy::Bytes(max_bytes.saturating_sub(TRUNCATION_MARKER_MAX_BYTES)),
    )
}

impl InterAgentCompletionMessage {
    pub(crate) fn new(task_name: AgentPath, sender: AgentPath, payload: impl Into<String>) -> Self {
        Self {
            task_name,
            sender,
            provider_receipt: None,
            payload: payload.into(),
        }
    }

    pub(crate) fn with_provider_receipt(
        task_name: AgentPath,
        sender: AgentPath,
        provider_receipt: CompletionProviderReceipt,
        payload: impl Into<String>,
    ) -> Self {
        Self {
            task_name,
            sender,
            provider_receipt: Some(provider_receipt),
            payload: payload.into(),
        }
    }
}

impl ContextualUserFragment for InterAgentCompletionMessage {
    fn role(&self) -> &'static str {
        "assistant"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn body(&self) -> String {
        let receipt = self
            .provider_receipt
            .as_ref()
            .map(|receipt| format!("\n{}", receipt.render()))
            .unwrap_or_default();
        let task_name = bounded_text(self.task_name.as_str(), COMPLETION_PATH_MAX_RENDERED_BYTES);
        let sender = bounded_text(self.sender.as_str(), COMPLETION_PATH_MAX_RENDERED_BYTES);
        let prefix = format!(
            "Message Type: FINAL_ANSWER\nTask name: {task_name}\nSender: {sender}{receipt}\nPayload:\n"
        );
        let payload = bounded_text(
            &self.payload,
            COMPLETION_MESSAGE_MAX_RENDERED_BYTES.saturating_sub(prefix.len()),
        );
        let rendered = format!("{prefix}{payload}");
        debug_assert!(rendered.len() <= COMPLETION_MESSAGE_MAX_RENDERED_BYTES);
        rendered
    }
}
