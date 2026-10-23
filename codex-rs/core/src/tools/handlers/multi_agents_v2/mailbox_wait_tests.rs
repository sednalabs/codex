use super::*;
use crate::session::step_context::StepContext;
use crate::tools::context::ToolCallSource;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_agent_mailbox::Continuation;
use codex_agent_mailbox::MessageDraft;
use codex_agent_mailbox::MessageKind;
use codex_agent_mailbox::NativeWork;
use codex_agent_mailbox::WorkRef;
use codex_features::Feature;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::ResponseInputItem;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

async fn fixture() -> (Arc<Session>, Arc<TurnContext>, Arc<Mailbox>, ThreadId) {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config.features.enable(Feature::AgentMailbox).unwrap();
    turn.multi_agent_version = config.multi_agent_version_from_features();
    turn.config = Arc::new(config);
    session.services.agent_control.mailbox_initialize(&turn.config).await.unwrap();
    let mailbox = session.services.agent_control.mailbox().unwrap();
    (Arc::new(session), Arc::new(turn), mailbox, ThreadId::new())
}

async fn work(mailbox: &Mailbox, issuer: ThreadId, agent: ThreadId, key: &str, outcome: Option<Outcome>) -> WorkRef {
    let work = mailbox.accept_work(NativeWork {
        native_key: key.to_string(), issuer, agent, submission_id: key.to_string(),
        input: InterAgentCommunication::new(AgentPath::root(), AgentPath::from_string("/root/worker".to_string()).unwrap(), Vec::new(), key.to_string(), true),
    }).await.unwrap();
    if let Some(outcome) = outcome {
        let result = mailbox.record_outcome(&work, outcome, None, Continuation::None).await.unwrap();
        // This fixture represents a previously surfaced result. Writer custody has
        // separate Session tests; this exercises replay through the actual wait facade.
        mailbox.mark_canonical(issuer, &result.id).await.unwrap();
        mailbox.acknowledge_observed(issuer, &[ObservationTarget::Work(work.clone())]).await.unwrap();
    }
    work
}

async fn wait(session: Arc<Session>, turn: Arc<TurnContext>, targets: Vec<String>, mode: &str) -> MailboxWaitReport {
    let payload = ToolPayload::Function { arguments: serde_json::json!({ "targets": targets, "return_when": mode, "timeout_ms": 0 }).to_string() };
    let output = handle_wait(ToolInvocation {
        session, step_context: StepContext::for_test(Arc::clone(&turn)), turn,
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
        call_id: format!("replay-{mode}"), tool_name: ToolName::plain("wait_agent"),
        source: ToolCallSource::Direct, payload: payload.clone(),
    }).await.unwrap();
    let ResponseInputItem::FunctionCallOutput { output, .. } = output.to_response_item("replay", &payload) else { panic!("expected a function result") };
    let FunctionCallOutputBody::Text(body) = output.body else { panic!("expected report text") };
    serde_json::from_str(&body).unwrap()
}

#[tokio::test]
async fn observed_unsuccessful_work_remains_non_success_in_exact_and_mixed_replays() {
    for outcome in [Outcome::Failed, Outcome::Cancelled, Outcome::NeedsAttention] {
        let (session, turn, mailbox, agent) = fixture().await;
        let failed = work(&mailbox, session.thread_id, agent, "unsuccessful", Some(outcome)).await;
        let pending = work(&mailbox, session.thread_id, agent, "pending", None).await;
        for mode in ["any", "all"] {
            for targets in [vec![failed.id.clone()], vec![failed.id.clone(), pending.id.clone()]] {
                let report = wait(Arc::clone(&session), Arc::clone(&turn), targets.clone(), mode).await;
                assert_eq!(report.completion_reason, Some(MailboxWakeCause::NeedsAttention));
                assert!(!report.condition_satisfied);
                assert_eq!(report.pending.len(), targets.len() - 1);
            }
        }
    }
}

#[tokio::test]
async fn unrelated_attention_does_not_erase_a_successful_selected_join() {
    let (session, turn, mailbox, agent) = fixture().await;
    let successful = work(&mailbox, session.thread_id, agent, "successful", Some(Outcome::Completed)).await;
    let attention = mailbox.accept_message(MessageDraft {
        invocation_key: "unrelated-request".to_string(), sender: agent, recipient: session.thread_id,
        communication: InterAgentCommunication::new(AgentPath::from_string("/root/worker".to_string()).unwrap(), AgentPath::root(), Vec::new(), "independent request".to_string(), false),
        kind: MessageKind::Request, activate_if_idle: false, work_ref: None, reply_to: None, supersedes: None, legacy_attention: false,
    }).await.unwrap();
    for mode in ["any", "all"] {
        let report = wait(Arc::clone(&session), Arc::clone(&turn), vec![successful.id.clone()], mode).await;
        assert_eq!(report.completion_reason, Some(MailboxWakeCause::AlreadySatisfied));
        assert!(report.condition_satisfied);
        let pending_attention = mailbox.attention(session.thread_id, 100).await.unwrap();
        assert_eq!(pending_attention.len(), 1);
        assert_eq!(pending_attention[0].id, format!("attention:{}", attention.id));
    }
}
