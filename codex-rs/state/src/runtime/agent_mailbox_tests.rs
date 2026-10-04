use super::*;
use crate::StateRuntime;
use crate::runtime::test_support::unique_temp_dir;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

async fn runtime() -> Arc<StateRuntime> {
    let home = unique_temp_dir();
    StateRuntime::init(
        crate::SqliteConfig::new_for_testing(home.as_path().abs()),
        "test-provider".to_string(),
    )
    .await
    .expect("state runtime")
}

fn request(intent: MailboxIntent, key: &str, payload: &str) -> MailboxSendRequest {
    MailboxSendRequest {
        sender_instance_id: "child-instance".to_string(),
        sender_task_generation: "child-generation-1".to_string(),
        recipient_instance_id: "root-instance".to_string(),
        recipient_task_generation: "root-generation-1".to_string(),
        intent,
        encrypted_payload: payload.to_string(),
        idempotency_key: key.to_string(),
        reply_to: None,
        supersedes: Vec::new(),
    }
}

fn accepted(outcome: MailboxAcceptOutcome) -> MailboxMessage {
    match outcome {
        MailboxAcceptOutcome::Accepted(message) => message,
        MailboxAcceptOutcome::AlreadyAccepted(_) => panic!("expected fresh acceptance"),
    }
}

#[tokio::test]
async fn progress_supersession_is_atomic_and_originals_remain_readable() {
    let runtime = runtime().await;
    let store = runtime.agent_mailbox();
    let first = accepted(
        store
            .accept(request(MailboxIntent::Progress, "p-1", "opaque-1"))
            .await
            .unwrap(),
    );
    let second = accepted(
        store
            .accept(request(MailboxIntent::Progress, "p-2", "opaque-2"))
            .await
            .unwrap(),
    );
    let mut replacement = request(MailboxIntent::ResultReady, "result-1", "opaque-result");
    replacement.supersedes = vec![second.message_id.clone(), first.message_id.clone()];
    let result = accepted(store.accept(replacement).await.unwrap());

    assert_eq!(
        vec![result.message_id.clone()],
        store
            .list_pending("root-instance", "root-generation-1", 10)
            .await
            .unwrap()
            .iter()
            .map(|message| message.message_id.clone())
            .collect::<Vec<_>>()
    );
    let original = store.get(&first.message_id).await.unwrap().unwrap();
    assert_eq!(vec![result.message_id.clone()], original.superseded_by);
    let loaded = store.get(&result.message_id).await.unwrap().unwrap();
    let mut ids = loaded.supersedes;
    ids.sort();
    let mut expected = vec![first.message_id.clone(), second.message_id.clone()];
    expected.sort();
    assert_eq!(expected, ids);
    assert_eq!(None, result.delivered_at_ms);
    runtime.close().await;
}

#[tokio::test]
async fn invalid_mixed_supersession_does_not_accept_or_cover_any_message() {
    let runtime = runtime().await;
    let store = runtime.agent_mailbox();
    let progress = accepted(
        store
            .accept(request(MailboxIntent::Progress, "p", "opaque-progress"))
            .await
            .unwrap(),
    );
    let action = accepted(
        store
            .accept(request(MailboxIntent::ActionRequired, "a", "opaque-action"))
            .await
            .unwrap(),
    );
    let mut invalid = request(MailboxIntent::ResultReady, "mixed", "opaque-result");
    invalid.supersedes = vec![progress.message_id.clone(), action.message_id.clone()];
    assert!(store.accept(invalid).await.is_err());
    assert!(store.get("mixed").await.unwrap().is_none());
    assert_eq!(
        Vec::<String>::new(),
        store
            .get(&progress.message_id)
            .await
            .unwrap()
            .unwrap()
            .superseded_by
    );
    assert_eq!(
        vec![progress.message_id, action.message_id],
        store
            .list_pending("root-instance", "root-generation-1", 10)
            .await
            .unwrap()
            .into_iter()
            .map(|message| message.message_id)
            .collect::<Vec<_>>()
    );
    runtime.close().await;
}

#[tokio::test]
async fn idempotency_returns_original_and_rejects_a_conflicting_payload() {
    let runtime = runtime().await;
    let store = runtime.agent_mailbox();
    let original = accepted(
        store
            .accept(request(
                MailboxIntent::ActionRequired,
                "same-key",
                "opaque-original",
            ))
            .await
            .unwrap(),
    );
    let duplicate = store
        .accept(request(
            MailboxIntent::ActionRequired,
            "same-key",
            "opaque-original",
        ))
        .await
        .unwrap();
    assert_eq!(
        MailboxAcceptOutcome::AlreadyAccepted(original.clone()),
        duplicate
    );
    assert!(
        store
            .accept(request(
                MailboxIntent::ActionRequired,
                "same-key",
                "opaque-replacement"
            ))
            .await
            .is_err()
    );
    assert_eq!(
        1,
        store
            .list_pending("root-instance", "root-generation-1", 10)
            .await
            .unwrap()
            .len()
    );
    runtime.close().await;
}

#[tokio::test]
async fn reply_acknowledgement_is_scoped_idempotent_and_quiet() {
    let runtime = runtime().await;
    let store = runtime.agent_mailbox();
    let original = accepted(
        store
            .accept(request(
                MailboxIntent::ActionRequired,
                "request",
                "opaque-action",
            ))
            .await
            .unwrap(),
    );
    let mut acknowledgement = MailboxSendRequest {
        sender_instance_id: original.recipient_instance_id.clone(),
        sender_task_generation: original.recipient_task_generation.clone(),
        recipient_instance_id: original.sender_instance_id.clone(),
        recipient_task_generation: original.sender_task_generation.clone(),
        intent: MailboxIntent::Acknowledgement,
        encrypted_payload: "opaque-ack".to_string(),
        idempotency_key: "ack-key".to_string(),
        reply_to: Some(original.message_id.clone()),
        supersedes: Vec::new(),
    };
    let ack_message = accepted(store.accept(acknowledgement.clone()).await.unwrap());
    let acknowledged = store.get(&original.message_id).await.unwrap().unwrap();
    assert_eq!(
        Some(ack_message.message_id.clone()),
        acknowledged.acknowledgement_message_id
    );
    assert!(acknowledged.acknowledged_at_ms.is_some());
    assert_eq!(None, ack_message.wait_signalled_at_ms);
    assert!(matches!(
        store.accept(acknowledgement.clone()).await.unwrap(),
        MailboxAcceptOutcome::AlreadyAccepted(_)
    ));
    acknowledgement.idempotency_key = "different-ack".to_string();
    assert!(store.accept(acknowledgement).await.is_err());
    runtime.close().await;
}

#[tokio::test]
async fn lifecycle_markers_are_separate_and_survive_reopen() {
    let runtime = runtime().await;
    let sqlite = runtime.sqlite().clone();
    let message = accepted(
        runtime
            .agent_mailbox()
            .accept(request(MailboxIntent::ActionRequired, "wake", "opaque"))
            .await
            .unwrap(),
    );
    let store = runtime.agent_mailbox();
    for stage in [
        MailboxStage::WaitSignalled,
        MailboxStage::WaitReturned,
        MailboxStage::ContextCommitted,
    ] {
        assert!(
            store
                .record_stage(&message.message_id, stage)
                .await
                .unwrap()
        );
    }
    let observed = store.get(&message.message_id).await.unwrap().unwrap();
    assert!(observed.wait_signalled_at_ms.is_some());
    assert!(observed.wait_returned_at_ms.is_some());
    assert!(observed.context_committed_at_ms.is_some());
    assert_eq!(None, observed.delivered_at_ms);
    assert!(
        store
            .list_pending("root-instance", "root-generation-1", 10)
            .await
            .unwrap()
            .is_empty()
    );
    runtime.close().await;

    let reopened = StateRuntime::init(sqlite, "test-provider".to_string())
        .await
        .expect("reopen state runtime");
    assert_eq!(
        observed,
        reopened
            .agent_mailbox()
            .get(&message.message_id)
            .await
            .unwrap()
            .unwrap()
    );
    reopened
        .agent_mailbox()
        .record_stage(&message.message_id, MailboxStage::Delivered)
        .await
        .unwrap();
    assert!(
        reopened
            .agent_mailbox()
            .list_pending("root-instance", "root-generation-1", 10)
            .await
            .unwrap()
            .is_empty()
    );
    reopened.close().await;
}

#[tokio::test]
async fn progress_coverage_rejects_foreign_generation_and_nonprogress_intents() {
    let runtime = runtime().await;
    let store = runtime.agent_mailbox();
    let progress = accepted(
        store
            .accept(request(
                MailboxIntent::Progress,
                "progress",
                "opaque-progress",
            ))
            .await
            .unwrap(),
    );
    let mut foreign = request(MailboxIntent::Progress, "foreign", "opaque-foreign");
    foreign.sender_task_generation = "child-generation-2".to_string();
    let foreign = accepted(store.accept(foreign).await.unwrap());
    let mut covering = request(MailboxIntent::ActionRequired, "cover", "opaque-cover");
    covering.supersedes = vec![foreign.message_id.clone()];
    assert!(store.accept(covering).await.is_err());
    let covered = store.get(&progress.message_id).await.unwrap().unwrap();
    assert!(covered.superseded_by.is_empty());

    let action = accepted(
        store
            .accept(request(
                MailboxIntent::ActionRequired,
                "action",
                "opaque-action",
            ))
            .await
            .unwrap(),
    );
    let result = accepted(
        store
            .accept(request(
                MailboxIntent::ResultReady,
                "result",
                "opaque-result",
            ))
            .await
            .unwrap(),
    );
    let inbound = accepted(
        store
            .accept(MailboxSendRequest {
                sender_instance_id: "root-instance".to_string(),
                sender_task_generation: "root-generation-1".to_string(),
                recipient_instance_id: "child-instance".to_string(),
                recipient_task_generation: "child-generation-1".to_string(),
                intent: MailboxIntent::Progress,
                encrypted_payload: "opaque-inbound".to_string(),
                idempotency_key: "inbound".to_string(),
                reply_to: None,
                supersedes: Vec::new(),
            })
            .await
            .unwrap(),
    );
    let acknowledgement = accepted(
        store
            .accept(MailboxSendRequest {
                sender_instance_id: "child-instance".to_string(),
                sender_task_generation: "child-generation-1".to_string(),
                recipient_instance_id: "root-instance".to_string(),
                recipient_task_generation: "root-generation-1".to_string(),
                intent: MailboxIntent::Acknowledgement,
                encrypted_payload: "opaque-ack".to_string(),
                idempotency_key: "ack".to_string(),
                reply_to: Some(inbound.message_id),
                supersedes: Vec::new(),
            })
            .await
            .unwrap(),
    );
    for (key, target) in [
        ("cover-action", action.message_id),
        ("cover-result", result.message_id),
        ("cover-ack", acknowledgement.message_id),
    ] {
        let mut invalid = request(MailboxIntent::ResultReady, key, "opaque-cover");
        invalid.supersedes = vec![target.clone()];
        assert!(store.accept(invalid).await.is_err(), "{key}");
        assert!(
            store
                .get(&target)
                .await
                .unwrap()
                .unwrap()
                .superseded_by
                .is_empty()
        );
    }
    runtime.close().await;
}
