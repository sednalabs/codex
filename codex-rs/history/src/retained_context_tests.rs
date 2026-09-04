use super::*;
use pretty_assertions::assert_eq;

fn publish_answer() -> RetainedContextEvent {
    RetainedContextEvent::VerifiedAnswer(VerifiedAnswer {
        turn_id: "turn-1".to_owned(),
        call_id: "ask-1".to_owned(),
        questions: vec![VerifiedQuestionAnswer {
            question: "Publish?".to_owned(),
            answer: "Yes, but never publicly.".to_owned(),
        }],
    })
}

#[test]
fn retained_evidence_preserves_order_through_checkpoint_and_rollback() {
    let mut context = RetainedContext::default();
    let first = publish_answer();
    assert!(context.record(&first));
    let before_restriction = context.clone();
    context.record_user_message(RetainedUserMessage {
        turn_id: "revocation-turn".to_owned(),
        message_id: Some("revocation".to_owned()),
        text: "Do not publish after all.".to_owned(),
        complete: true,
    });
    let snapshot = context.clone();
    assert!(!context.record(&first));
    assert_eq!(context, snapshot);
    let checkpoint =
        serde_json::from_str(&serde_json::to_string(&context).expect("retained answer fixture"))
            .expect("retained answer fixture");
    let mut restored = RetainedContext::default();
    restored.restore(Some(&checkpoint));
    assert_eq!(restored, snapshot);
    assert_eq!(
        restored
            .ordered_entries()
            .map(|entry| match entry {
                RetainedContextEntry::VerifiedAnswer(answer) => answer.questions[0].answer.as_str(),
                RetainedContextEntry::UserMessage(message) => message.text.as_str(),
            })
            .collect::<Vec<_>>(),
        vec!["Yes, but never publicly.", "Do not publish after all."]
    );
    restored.rollback(&["revocation-turn"], Some("revocation"));
    assert_eq!(
        restored,
        RetainedContext {
            next_order: snapshot.next_order,
            ..before_restriction
        }
    );
}

#[test]
fn retained_families_enforce_storage_limits_without_changing_snapshots() {
    let mut context = RetainedContext::default();
    let first = publish_answer();
    context.record(&first);
    let snapshot = context.clone();

    for index in 2..=10 {
        context.record(&RetainedContextEvent::VerifiedAnswer(VerifiedAnswer {
            turn_id: "turn-2".to_owned(),
            call_id: format!("ask-{index}"),
            questions: vec![VerifiedQuestionAnswer {
                question: "Continue?".to_owned(),
                answer: "Yes".to_owned(),
            }],
        }));
    }
    assert!(!context.verified_answers_complete());
    assert_eq!(context.verified_answers().count(), MAX_FAMILY_RECORDS);
    context.rollback(&["turn-2"], /*first_removed_message_id*/ None);
    assert_eq!(context.verified_answers().count(), 0);
    assert!(!context.verified_answers_complete());

    let mut restored = snapshot.clone();
    let mut oversized = first.clone();
    let RetainedContextEvent::VerifiedAnswer(answer) = &mut oversized;
    answer.questions[0].answer = "a".repeat(MAX_RECORD_BYTES);
    restored.record(&oversized);
    assert!(!restored.verified_answers_complete());
    assert!(
        restored
            .verified_answers()
            .next()
            .expect("retained answer fixture")
            .questions
            .is_empty()
    );
    assert_eq!(
        snapshot
            .verified_answers()
            .next()
            .expect("retained answer fixture")
            .questions[0]
            .answer,
        "Yes, but never publicly."
    );
    for index in 0..=MAX_FAMILY_RECORDS {
        restored.record_user_message(RetainedUserMessage {
            turn_id: "later-turn".to_owned(),
            message_id: Some(format!("message-{index}")),
            text: "Keep the repository private.".to_owned(),
            complete: true,
        });
    }
    assert!(!restored.user_messages_complete());
    assert_eq!(
        restored
            .ordered_entries()
            .filter(|entry| matches!(entry, RetainedContextEntry::UserMessage(_)))
            .count(),
        MAX_FAMILY_RECORDS
    );
    restored.record_user_message(RetainedUserMessage {
        turn_id: "oversized-turn".to_owned(),
        message_id: Some("oversized-message".to_owned()),
        text: "restriction ".repeat(MAX_RECORD_BYTES),
        complete: true,
    });
    let Some(RetainedContextEntry::UserMessage(message)) = restored.ordered_entries().next_back()
    else {
        panic!("latest user evidence");
    };
    assert_eq!((&message.text, message.complete), (&String::new(), false));
}

#[test]
fn legacy_checkpoints_mark_user_messages_incomplete() {
    let RetainedContextEvent::VerifiedAnswer(answer) = publish_answer();
    let mut wire = serde_json::json!({
        "verified_answers": [answer], "incomplete": false
    });
    let legacy: RetainedContext =
        serde_json::from_value(wire.clone()).expect("legacy retained-answer checkpoint");
    assert!(legacy.verified_answers_complete());
    assert!(!legacy.user_messages_complete());

    // Retain the original wire key even though the internal field names its family.
    wire["verified_answers"][0]["order"] = serde_json::json!(0);
    wire["user_messages"] = serde_json::json!([]);
    wire["user_messages_incomplete"] = serde_json::json!(true);
    wire["next_order"] = serde_json::json!(0);
    assert_eq!(serde_json::to_value(&legacy).unwrap(), wire);

    let mut restored = RetainedContext::default();
    restored.restore(/*checkpoint*/ None);
    assert!(!restored.user_messages_complete());
}
