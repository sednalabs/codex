#![allow(clippy::expect_used)]
// The combined integration target nests the MCP and code-mode async fixtures.
#![recursion_limit = "256"]

// Single integration test binary that aggregates all test modules.
// The submodules live in `tests/all/`.
pub use codex_protocol::error;

mod suite;
