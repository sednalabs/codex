use super::*;

#[tokio::test]
async fn automatic_retry_submission_carries_bounded_transport_metadata() {
    let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.config.notices.auto_continue_on_cyber_policy = Some(true);
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);

    handle_turn_started(&mut chat, "policy-trigger");
    drain_insert_history(&mut rx);
    handle_error(
        &mut chat,
        "server fallback message",
        Some(CodexErrorInfo::CyberPolicy),
    );

    let Op::UserTurn {
        items,
        client_user_message_id,
        ..
    } = next_submit_op(&mut op_rx)
    else {
        panic!("expected automatic retry turn");
    };
    assert_eq!(
        items,
        vec![UserInput::Text {
            text: "continue".to_string(),
            text_elements: Vec::new(),
        }]
    );
    let provenance =
        codex_protocol::automatic_turn::AutomaticTurnProvenance::decode_client_user_message_id(
            client_user_message_id
                .as_deref()
                .expect("automatic retry should carry metadata"),
        )
        .expect("metadata should decode");
    assert_eq!(provenance.thread_id, thread_id.to_string());
    assert_eq!(provenance.trigger_turn_id, "policy-trigger");
    assert_eq!(provenance.attempt, 1);
    assert_eq!(provenance.max_attempts, 3);
    assert_eq!(provenance.capability, "test-capability");
}
