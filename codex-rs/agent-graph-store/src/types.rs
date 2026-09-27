use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;

/// Lifecycle status attached to a directional thread-spawn edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadSpawnEdgeStatus {
    /// The child thread is still live or resumable as an open spawned agent.
    Open,
    /// The child thread has been closed from the parent/child graph's perspective.
    Closed,
}

/// Descendants returned by a bounded persisted-graph recovery query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadSpawnDescendants {
    /// Descendant thread identifiers retained by the bounded query.
    pub thread_ids: Vec<ThreadId>,
    /// Whether the recursive safety limit was reached.
    pub relation_limit_reached: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn thread_spawn_edge_status_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&ThreadSpawnEdgeStatus::Open)
                .expect("open status should serialize"),
            "\"open\""
        );
        assert_eq!(
            serde_json::to_string(&ThreadSpawnEdgeStatus::Closed)
                .expect("closed status should serialize"),
            "\"closed\""
        );
        assert_eq!(
            serde_json::from_str::<ThreadSpawnEdgeStatus>("\"open\"")
                .expect("open status should deserialize"),
            ThreadSpawnEdgeStatus::Open
        );
        assert_eq!(
            serde_json::from_str::<ThreadSpawnEdgeStatus>("\"closed\"")
                .expect("closed status should deserialize"),
            ThreadSpawnEdgeStatus::Closed
        );
    }
}
