use super::*;
use codex_agent_mailbox::Envelope;
use codex_agent_mailbox::OutcomeReceipt;
use codex_agent_mailbox::RequestSnapshot;
use codex_agent_mailbox::WorkRef;
use codex_agent_mailbox::WorkSnapshot;
use codex_protocol::ResponseItemId;

fn communication(id: &str, intent: Option<AgentMessageIntent>) -> InterAgentCommunication {
    let mut message = InterAgentCommunication::new(
        AgentPath::root(), AgentPath::root(), Vec::new(), "body".to_string(), false,
    );
    message.id = Some(ResponseItemId::from_server(id.to_string()));
    message.intent = intent;
    message
}

fn envelope(kind: MessageKind, intent: Option<AgentMessageIntent>) -> Envelope {
    Envelope {
        id: "amsg_request".to_string(),
        invocation_key: "request".to_string(),
        sender: ThreadId::new(),
        recipient: ThreadId::new(),
        communication: communication("amsg_request", intent),
        kind,
        activate_if_idle: false,
        work_ref: None,
        reply_to: None,
        supersedes: None,
        legacy_attention: false,
        canonical: false,
    }
}

fn work(id: &str, ready: bool) -> SnapshotItem {
    let work_ref = WorkRef {
        id: id.to_string(), issuer: ThreadId::new(), agent: ThreadId::new(), submission_id: id.to_string(),
    };
    let outcome = ready.then(|| OutcomeReceipt {
        work_ref: work_ref.clone(), id: format!("amsg_{id}"), outcome: Outcome::Completed,
        continuation: codex_agent_mailbox::Continuation::None,
        response: Some(communication(&format!("amsg_{id}"), Some(AgentMessageIntent::ResultReady))),
        response_message_id: Some(format!("amsg_{id}")), canonical: false, observed: false,
    });
    SnapshotItem::Work(WorkSnapshot {
        work_ref, input: communication("amsg_input", None), dispatch_state: DispatchState::Dispatched,
        outcome, continuations: Vec::new(), observed: false,
    })
}

#[test]
fn exact_any_all_selection_requires_the_requested_ready_set() {
    let selection = vec![work("work_first", true), work("work_second", false)];
    assert!(selection_satisfied(&selection, DurableReturnWhen::Any));
    assert!(!selection_satisfied(&selection, DurableReturnWhen::All));
    assert!(!selection_satisfied(&[], DurableReturnWhen::All));
    assert!(selection_satisfied(&[work("work_first", true), work("work_second", true)], DurableReturnWhen::All));
    assert_eq!(selected_body_ids(&selection), HashSet::from(["amsg_work_first".to_string()]));
}

#[test]
fn reply_only_selection_uses_exact_request_disposition() {
    let request = envelope(MessageKind::Request, None);
    let mut selected = vec![SnapshotItem::Request(RequestSnapshot {
        request_id: request.id.clone(), request, reply: None,
        disposition: RequestDisposition::Open, replacement: None, observed: false,
    })];
    assert!(!selection_satisfied(&selected, DurableReturnWhen::Any));
    let SnapshotItem::Request(request) = &mut selected[0] else { unreachable!() };
    request.disposition = RequestDisposition::Replied;
    request.reply = Some(envelope(MessageKind::Note, None));
    assert!(selection_satisfied(&selected, DurableReturnWhen::All));
}

#[test]
fn explicit_progress_never_wakes_or_consumes_typed_attention() {
    assert!(!actionable_envelope(&envelope(MessageKind::Request, Some(AgentMessageIntent::Progress))));
    assert!(!actionable_envelope(&envelope(MessageKind::Alert, Some(AgentMessageIntent::Progress))));
    assert!(!actionable_envelope(&envelope(MessageKind::Note, None)));
    assert!(actionable_envelope(&envelope(MessageKind::Request, None)));
    assert!(actionable_envelope(&envelope(MessageKind::Note, Some(AgentMessageIntent::ActionRequired))));
}

#[tokio::test]
async fn selected_partial_result_and_quiet_updates_do_not_interrupt_all_join() {
    let (session, _turn) = crate::session::tests::make_session_and_context().await;
    session.input_queue.enqueue_mailbox_communication(communication("amsg_quiet", Some(AgentMessageIntent::Progress))).await;
    session.input_queue.enqueue_mailbox_communication(communication("amsg_selected", Some(AgentMessageIntent::ResultReady))).await;
    let ignored = HashSet::from(["amsg_selected".to_string()]);
    assert_eq!(actionable_queue_activity(&session, &ignored, None).await, None);
    assert_eq!(session.input_queue.snapshot_mailbox_communications().await.len(), 2);
    session.input_queue.enqueue_mailbox_communication(communication("amsg_external", Some(AgentMessageIntent::ActionRequired))).await;
    assert_eq!(actionable_queue_activity(&session, &ignored, None).await, Some(InputQueueActivity::Mailbox));
    assert_eq!(session.input_queue.snapshot_mailbox_communications().await.len(), 3);
    let snapshot = mailbox_snapshot_with_exclusions(&session, "join", &[], "exact_target_set".to_string(), WakeSource::Mailbox, &ignored).await;
    assert_eq!(snapshot.provenance.causal_event_id.as_deref(), Some("amsg_external"));
    assert_eq!(session.input_queue.snapshot_mailbox_communications().await.len(), 3);
}
