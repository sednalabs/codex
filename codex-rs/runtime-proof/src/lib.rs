//! Host-held, per-execution MCP proof for the protected claim cohort.

mod auth;
mod provider_auth;
mod signer;
pub mod startup;
mod wire;

#[cfg(test)]
mod tests;

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

pub fn redact_mcp_bearer(
    server: &str,
    recipient: &str,
    value: &mut serde_json::Value,
) -> anyhow::Result<()> {
    auth::redact_mcp_bearer(server, recipient, value)
}
