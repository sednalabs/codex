//! Measures the complete synchronous request after image admission and wire
//! prefix assembly. Includes reused history, tool definitions and output format.

use codex_guardian_context::REQUEST_TOKENS_METRIC;
use codex_protocol::protocol::TruncationPolicy;

use crate::context_manager::estimate_item_token_count;
use codex_api::ResponsesApiRequest;
use codex_otel::SessionTelemetry;

pub(crate) fn observe(telemetry: &SessionTelemetry, request: &ResponsesApiRequest) -> usize {
    let input = request
        .input
        .iter()
        .map(estimate_item_token_count)
        .fold(0i64, i64::saturating_add);
    let instructions = TruncationPolicy::Bytes(request.instructions.len()).token_budget();
    let metadata = serde_json::to_vec(&(&request.tools, &request.text))
        .map(|bytes| TruncationPolicy::Bytes(bytes.len()).token_budget())
        .unwrap_or(usize::MAX);
    let total = usize::try_from(input)
        .unwrap_or(usize::MAX)
        .saturating_add(instructions)
        .saturating_add(metadata);
    // The assembled input already includes inherited history and the current
    // review. Do not report a guessed old/new split after context injection.
    telemetry.histogram(
        REQUEST_TOKENS_METRIC,
        i64::try_from(total).unwrap_or(i64::MAX),
        &[("target", "sync"), ("component", "total")],
    );
    total
}
