//! Preserve local work after an external connection is lost. Offline input and stale
//! async completions are quarantined; recovery requires relaunching the TUI.

use super::*;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum ReconnectPresentation {
    #[default]
    Conversation,
    Overview,
}

#[derive(Default)]
pub(super) struct ReconnectState {
    pub(super) offline: bool,
    pub(super) presentation: ReconnectPresentation,
}

impl App {
    pub(super) fn recover_transport_error(&mut self, error: &color_eyre::Report) -> bool {
        let disconnected = matches!(
            error.downcast_ref::<TypedRequestError>(),
            Some(TypedRequestError::Transport { .. })
        );
        disconnected && self.begin_reconnect()
    }

    pub(super) fn begin_reconnect(&mut self) -> bool {
        if matches!(self.app_server_target, AppServerTarget::Embedded) {
            return false;
        }
        if !self.reconnect.offline {
            self.reconnect.offline = true;
            self.cancel_pending_key_chord();
            self.overlay = None;
            self.commit_animation = None;
            self.clear_recap_request(crate::app_event::RecapTrigger::Manual);
            self.reconnect.presentation = if self
                .chat_widget
                .selected_index_for_active_view(agents_overview::AGENTS_OVERVIEW_VIEW_ID)
                .is_some()
            {
                if let Ok(mut state) = self.agents_overview.view_state.lock() {
                    state.connection_notice = Some("Disconnected — agent list is stale");
                    if let Some(task) = state.refresh_task.take() {
                        task.abort();
                    }
                }
                self.agents_overview.request_id = None;
                self.agents_overview.refresh_pending = false;
                ReconnectPresentation::Overview
            } else {
                self.chat_widget
                    .handle_disconnected_key(KeyEvent::new(KeyCode::Null, KeyModifiers::NONE));
                ReconnectPresentation::Conversation
            };
            self.chat_widget.pause_for_disconnect();
            self.startup_pending_protected_request = false;
            self.abort_all_thread_event_listeners();
            for (_, (_, task)) in self.dynamic_tool_tasks.drain() {
                task.abort();
            }
        }
        true
    }
}
