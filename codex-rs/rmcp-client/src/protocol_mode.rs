use rmcp::model::ProtocolVersion;
use rmcp::service::ClientLifecycleMode;

/// MCP compatibility policy selected once for a configured server connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum McpProtocolMode {
    /// Preserve legacy MCP initialization and stateful request semantics.
    #[default]
    Legacy,
    /// Prefer MCP 2026-07-28 discovery, falling back to legacy initialization
    /// when the peer does not support discovery.
    V20260728,
}

impl McpProtocolMode {
    pub(crate) fn client_lifecycle(self) -> ClientLifecycleMode {
        match self {
            Self::Legacy => ClientLifecycleMode::Initialize,
            Self::V20260728 => ClientLifecycleMode::Auto {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
                legacy_version: Some(ProtocolVersion::V_2025_06_18),
            },
        }
    }
}
