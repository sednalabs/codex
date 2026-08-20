use super::*;
use codex_protocol::automatic_turn::AutomaticTurnProvenance;

pub(super) const CYBER_POLICY_AUTO_CONTINUE_MAX_ATTEMPTS: u8 = 3;

impl ChatWidget {
    pub(super) fn cyber_policy_auto_continue_client_id(
        &self,
        trigger_turn_id: &str,
    ) -> Option<String> {
        let thread_id = self.thread_id()?;
        AutomaticTurnProvenance::cyber_policy_auto_continue(
            thread_id,
            trigger_turn_id,
            self.cyber_policy_auto_continue_attempts,
            CYBER_POLICY_AUTO_CONTINUE_MAX_ATTEMPTS,
        )
        .and_then(|provenance| provenance.to_client_user_message_id())
    }
}
