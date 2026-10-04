//! Host-held, per-execution MCP proof for the protected claim cohort.

mod auth;
mod provider_auth;
mod signer;
pub mod startup;
mod wire;

#[cfg(test)]
mod tests;

pub use auth::McpRedactionContext;
pub use signer::ExecutionNonce;
pub use signer::sign_claim_proof;
pub use wire::RESERVED_META_KEY;

pub fn bearer_for_mcp(
    server: &str,
    recipient: &str,
) -> anyhow::Result<Option<zeroize::Zeroizing<String>>> {
    auth::bearer_for_mcp(server, recipient)
}

pub fn is_protected_mcp_target(server: &str, recipient: &str) -> anyhow::Result<bool> {
    auth::is_protected_mcp_target(server, recipient)
}

pub fn protected_mcp_target() -> anyhow::Result<Option<(String, String)>> {
    auth::protected_mcp_target()
}

pub fn protected_provider_recipient() -> anyhow::Result<Option<String>> {
    auth::protected_provider_recipient()
}

pub fn capture_mcp_redaction_context(
    server: &str,
    recipient: &str,
    proof: Option<&serde_json::Value>,
) -> anyhow::Result<Option<McpRedactionContext>> {
    auth::capture_mcp_redaction_context(server, recipient, proof)
}

pub fn protected_runtime_active_or_failed() -> bool {
    auth::protected_runtime_active_or_failed()
}

pub fn erase_secret_string(value: &mut String) {
    zeroize::Zeroize::zeroize(value);
}
