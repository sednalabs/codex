use crate::ThreadId;
use serde::Deserialize;
use serde::Serialize;

const AUTOMATIC_TURN_CLIENT_ID_PREFIX: &str = "sedna-auto-turn:v1:";
const AUTOMATIC_TURN_CLIENT_ID_MAX_BYTES: usize = 1024;

/// Model-opaque provenance for a user turn generated automatically by a Codex client.
///
/// This metadata is carried in `client_user_message_id`, which is persisted with the user-message
/// item but is not included in the model-facing conversation payload.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AutomaticTurnOrigin {
    CyberPolicyAutoContinue,
}

impl AutomaticTurnOrigin {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::CyberPolicyAutoContinue => "cyber_policy_auto_continue",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AutomaticTurnProvenance {
    pub origin: AutomaticTurnOrigin,
    pub thread_id: String,
    pub trigger_turn_id: String,
    pub attempt: u8,
    pub max_attempts: u8,
}

impl AutomaticTurnProvenance {
    pub fn cyber_policy_auto_continue(
        thread_id: ThreadId,
        trigger_turn_id: impl Into<String>,
        attempt: u8,
        max_attempts: u8,
    ) -> Option<Self> {
        let provenance = Self {
            origin: AutomaticTurnOrigin::CyberPolicyAutoContinue,
            thread_id: thread_id.to_string(),
            trigger_turn_id: trigger_turn_id.into(),
            attempt,
            max_attempts,
        };
        provenance.is_valid().then_some(provenance)
    }

    pub fn to_client_user_message_id(&self) -> Option<String> {
        if !self.is_valid() {
            return None;
        }
        let payload = serde_json::to_string(self).ok()?;
        let client_id = format!("{AUTOMATIC_TURN_CLIENT_ID_PREFIX}{payload}");
        (client_id.len() <= AUTOMATIC_TURN_CLIENT_ID_MAX_BYTES).then_some(client_id)
    }

    pub fn from_client_user_message_id(client_id: &str) -> Option<Self> {
        if client_id.len() > AUTOMATIC_TURN_CLIENT_ID_MAX_BYTES {
            return None;
        }
        let payload = client_id.strip_prefix(AUTOMATIC_TURN_CLIENT_ID_PREFIX)?;
        let provenance: Self = serde_json::from_str(payload).ok()?;
        provenance.is_valid().then_some(provenance)
    }

    fn is_valid(&self) -> bool {
        !self.thread_id.trim().is_empty()
            && ThreadId::from_string(&self.thread_id).is_ok()
            && !self.trigger_turn_id.trim().is_empty()
            && self.attempt > 0
            && self.max_attempts > 0
            && self.attempt <= self.max_attempts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cyber_policy_provenance_round_trips_without_prompt_text() {
        let thread_id = ThreadId::new();
        let provenance =
            AutomaticTurnProvenance::cyber_policy_auto_continue(thread_id, "turn-trigger", 2, 3)
                .expect("valid provenance");
        let client_id = provenance
            .to_client_user_message_id()
            .expect("bounded client id");

        assert!(client_id.starts_with(AUTOMATIC_TURN_CLIENT_ID_PREFIX));
        assert!(!client_id.contains("\"message\""));
        assert_eq!(
            AutomaticTurnProvenance::from_client_user_message_id(&client_id),
            Some(provenance)
        );
    }

    #[test]
    fn parser_rejects_human_ids_and_invalid_attempts() {
        assert!(AutomaticTurnProvenance::from_client_user_message_id("human-message").is_none());

        let thread_id = ThreadId::new();
        assert!(
            AutomaticTurnProvenance::cyber_policy_auto_continue(thread_id, "turn-trigger", 0, 3)
                .is_none()
        );
        assert!(
            AutomaticTurnProvenance::cyber_policy_auto_continue(thread_id, "turn-trigger", 4, 3)
                .is_none()
        );
    }
}
