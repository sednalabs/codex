use super::*;

#[tokio::test]
async fn cyber_policy_auto_continue_carries_exact_turn_provenance() {
    let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.config.notices.auto_continue_on_cyber_policy = Some(true);
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);

    handle_turn_started(&mut chat, "turn-policy-trigger");
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
        panic!("expected automatic continue turn");
    };
    assert_eq!(
        items,
        vec![UserInput::Text {
            text: "continue".to_string(),
            text_elements: Vec::new(),
        }]
    );

    let provenance =
        codex_protocol::automatic_turn::AutomaticTurnProvenance::from_client_user_message_id(
            client_user_message_id
                .as_deref()
                .expect("automatic continuation should carry provenance"),
        )
        .expect("automatic continuation provenance should decode");
    assert_eq!(provenance.thread_id, thread_id.to_string());
    assert_eq!(provenance.trigger_turn_id, "turn-policy-trigger");
    assert_eq!(provenance.attempt, 1);
    assert_eq!(provenance.max_attempts, 3);
}
