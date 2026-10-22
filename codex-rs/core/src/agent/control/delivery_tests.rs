use super::*;
use pretty_assertions::assert_eq;

#[test]
fn message_delivery_separates_wait_wake_from_idle_turn_permission() {
    for encrypted in [false, true] {
        for (mode, origin, trigger_turn) in [
            (
                MessageDeliveryMode::QueueOnly,
                AgentCommunicationOrigin::Informational,
                false,
            ),
            (
                MessageDeliveryMode::WakeWait,
                AgentCommunicationOrigin::Actionable,
                false,
            ),
            (
                MessageDeliveryMode::TriggerTurn,
                AgentCommunicationOrigin::Actionable,
                true,
            ),
        ] {
            let message = if encrypted {
                AgentMessage::Encrypted("opaque".to_string())
            } else {
                AgentMessage::Plaintext("progress".to_string())
            };
            let communication = message.into_communication(
                AgentPath::root().join("child").unwrap(),
                AgentPath::root(),
                mode,
            );
            assert_eq!(communication.origin, Some(origin));
            assert_eq!(communication.trigger_turn, trigger_turn);
            assert_eq!(communication.encrypted_content.is_some(), encrypted);
            if !encrypted {
                let kind = if trigger_turn { "NEW_TASK" } else { "MESSAGE" };
                assert!(
                    communication
                        .content
                        .starts_with(&format!("Message Type: {kind}\n"))
                );
            }
        }
    }
}
