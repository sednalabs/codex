use super::*;
use codex_protocol::AgentPath;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn mailbox_shared_control_does_not_capture_stock_invocations() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config.features.enable(Feature::AgentMailbox).unwrap();
    let control = &session.services.agent_control;
    control.mailbox_initialize(&config).await.unwrap();
    let recipient = ThreadId::new();
    let mut communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::from_string("/root/worker".to_string()).unwrap(),
        Vec::new(),
        "native input".to_string(),
        true,
    );
    let original = communication.clone();
    let mut submission_id = "stock-submission".to_string();
    let context = AgentCommunicationContext::new(AgentCommunicationKind::Followup, session.thread_id);
    assert!(control.mailbox_accept_native_submission(recipient, &context, &mut communication, &mut submission_id).await.unwrap().is_none());
    assert_eq!(communication, original);
    assert_eq!(submission_id, "stock-submission");
    let mailbox = control.mailbox().unwrap();
    assert!(mailbox.issued_work_refs(session.thread_id, None, 100).await.unwrap().is_empty());

    let enabled_context = context.with_native_key("enabled-call".to_string());
    let (accepted, decision) = control.mailbox_accept_native_submission(recipient, &enabled_context, &mut communication, &mut submission_id).await.unwrap().unwrap();
    assert_eq!(decision, DispatchDecision::Dispatch);
    assert_eq!(accepted.issuer, session.thread_id);
    assert_eq!(mailbox.accepted_input(&accepted).await.unwrap(), communication);
    assert!(communication.id.unwrap().as_str().starts_with("amsg_"));
}
