use codex_protocol::AgentPath;
use codex_protocol::protocol::TokenUsage;

use super::ContextualUserFragment;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InterAgentCompletionMessage {
    task_name: AgentPath,
    sender: AgentPath,
    provider_receipt: Option<CompletionProviderReceipt>,
    payload: String,
}

const RECEIPT_IDENTITY_MAX_CHARS: usize = 256;

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
            lines.push(format!(
                "  <terminal_response_model>{}</terminal_response_model>",
                bounded_xml_value(model)
            ));
        }
        if let Some(snapshot) = self.terminal_response_snapshot.as_deref() {
            lines.push(format!(
                "  <terminal_response_snapshot>{}</terminal_response_snapshot>",
                bounded_xml_value(snapshot)
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
        lines.join("\n")
    }
}

fn bounded_xml_value(value: &str) -> String {
    value
        .chars()
        .take(RECEIPT_IDENTITY_MAX_CHARS)
        .fold(String::new(), |mut escaped, ch| {
            match ch {
                '&' => escaped.push_str("&amp;"),
                '<' => escaped.push_str("&lt;"),
                '>' => escaped.push_str("&gt;"),
                '\"' => escaped.push_str("&quot;"),
                '\'' => escaped.push_str("&apos;"),
                _ => escaped.push(ch),
            }
            escaped
        })
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
        format!(
            "Message Type: FINAL_ANSWER\nTask name: {}\nSender: {}{receipt}\nPayload:\n{}",
            self.task_name, self.sender, self.payload,
        )
    }
}
