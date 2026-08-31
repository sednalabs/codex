//! Disconnects preserve editable input but never automatically retry queued submissions.

use super::*;

impl ChatWidget {
    pub(crate) fn pause_for_disconnect(&mut self) {
        self.input_queue.recovered_queue = true;
        self.input_queue.suppress_queue_autosend = true;
        self.set_initial_user_message_submit_suppressed(/*suppressed*/ true);
        if let Some(message) = self.initial_user_message.take() {
            self.restore_user_message_to_composer(message);
        }
        self.bottom_pane.ensure_status_indicator();
        self.bottom_pane
            .set_interrupt_hint_visible(/*visible*/ false);
        self.set_status_header("Connection lost — copy your draft, then relaunch".to_string());
        self.set_footer_hint_override(Some(vec![("ctrl+c".into(), "quit".into())]));
        self.add_error_message("Connection lost.".into());
    }

    pub(crate) fn handle_disconnected_view_key(&mut self, key: KeyEvent) {
        self.bottom_pane.handle_key_event(key);
    }

    pub(crate) fn handle_disconnected_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Press && self.chat_keymap.edit_queued_message.is_pressed(key) {
            if let Some(composer) = self.pop_latest_queued_composer_state() {
                self.restore_composer_state(composer);
            } else if let Some(steer) = self.input_queue.pending_steers.pop_back() {
                self.restore_user_message_to_composer(user_message_for_restore(
                    steer.user_message,
                    &steer.history_record,
                ));
                self.input_queue.recovered_queue &= !self.input_queue.pending_steers.is_empty();
            }
            self.bottom_pane
                .handle_disconnected_key(KeyEvent::new(KeyCode::Null, KeyModifiers::NONE));
            self.refresh_pending_input_preview();
            self.request_redraw();
        } else {
            self.bottom_pane.handle_disconnected_key(key);
        }
    }
}
