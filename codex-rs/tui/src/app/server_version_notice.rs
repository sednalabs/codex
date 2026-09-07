//! Keeps the older remote server notice in the overview's shared view state.

use super::*;

impl App {
    pub(super) fn update_server_version_overview_notice(
        &mut self,
        client_version: &str,
        older_server: Option<&str>,
    ) {
        self.agents_overview
            .view_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .server_version_notice =
            older_server.map(|server| format!("Service v{server} < Codex CLI v{client_version}"));
    }
}
