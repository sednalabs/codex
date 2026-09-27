use super::*;
use codex_protocol::automatic_turn::AutomaticTurnProvenance;

pub(super) const AUTOMATIC_TURN_MAX_ATTEMPTS: u8 = 3;

impl ChatWidget {
    pub(super) fn automatic_turn_client_id(
        &self,
        trigger_turn_id: &str,
        capability: &str,
    ) -> Option<String> {
        let thread_id = self.thread_id()?;
        AutomaticTurnProvenance::policy_retry(
            thread_id,
            trigger_turn_id,
            self.cyber_policy_auto_continue_attempts,
            AUTOMATIC_TURN_MAX_ATTEMPTS,
            capability,
        )
        .and_then(|provenance| provenance.to_client_user_message_id())
    }
}
