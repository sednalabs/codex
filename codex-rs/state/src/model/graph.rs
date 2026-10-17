use strum::AsRefStr;
use strum::Display;
use strum::EnumString;

/// Status attached to a directional thread-spawn edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, AsRefStr, Display, EnumString)]
#[strum(serialize_all = "snake_case")]
pub enum DirectionalThreadSpawnEdgeStatus {
    Open,
    Closed,
}

/// Persisted descendants returned by a bounded agent recovery query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadSpawnDescendants {
    /// Descendant thread identifiers retained by the bounded query.
    pub thread_ids: Vec<codex_protocol::ThreadId>,
    /// Whether at least one additional descendant exists beyond the safety limit.
    pub relation_limit_reached: bool,
}
