use super::MailboxWaitReport;
use super::MailboxWaitReturnWhen;
use super::MailboxWaitSelectionItem;
use super::MailboxWakeCause;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn mailbox_wait_report_serializes_exact_selection_and_nullable_wake_identity() {
    let report = MailboxWaitReport {
        requested_targets: vec!["work_exact".to_string()],
        requested_replies_to: vec!["amsg_request".to_string()],
        return_when: MailboxWaitReturnWhen::All,
        completed: vec![MailboxWaitSelectionItem {
            work_ref: Some("work_exact".to_string()),
            message_id: None,
            agent: Some("/root/worker".to_string()),
            status: "completed".to_string(),
        }],
        pending: vec![MailboxWaitSelectionItem {
            work_ref: None,
            message_id: Some("amsg_request".to_string()),
            agent: None,
            status: "pending".to_string(),
        }],
        completion_reason: Some(MailboxWakeCause::RuntimeFault),
        condition_satisfied: false,
        quiet_count: 2,
        wake_sender: None,
        wake_ref: None,
    };
    let serialized = serde_json::to_value(&report).expect("serialize durable mailbox report");
    assert_eq!(
        serialized,
        json!({
            "requestedTargets": ["work_exact"],
            "requestedRepliesTo": ["amsg_request"],
            "returnWhen": "all",
            "completed": [{
                "workRef": "work_exact",
                "messageId": null,
                "agent": "/root/worker",
                "status": "completed"
            }],
            "pending": [{
                "workRef": null,
                "messageId": "amsg_request",
                "agent": null,
                "status": "pending"
            }],
            "completionReason": "runtime_fault",
            "conditionSatisfied": false,
            "quietCount": 2,
            "wakeSender": null,
            "wakeRef": null
        })
    );
    assert_eq!(
        serde_json::from_value::<MailboxWaitReport>(serialized)
            .expect("deserialize durable mailbox report"),
        report
    );
}

#[test]
fn mailbox_wait_report_accepts_source_selection_aliases() {
    let report: MailboxWaitReport = serde_json::from_value(json!({
        "requested_targets": ["work_exact"],
        "requested_replies_to": ["amsg_request"],
        "returnWhen": "any",
        "completed": [],
        "pending": [{
            "work_ref": "work_exact",
            "message_id": "amsg_request",
            "status": "pending"
        }],
        "conditionSatisfied": false,
        "quietCount": 0
    }))
    .expect("source mailbox selection aliases should remain readable");

    assert_eq!(
        report,
        MailboxWaitReport {
            requested_targets: vec!["work_exact".to_string()],
            requested_replies_to: vec!["amsg_request".to_string()],
            return_when: MailboxWaitReturnWhen::Any,
            completed: Vec::new(),
            pending: vec![MailboxWaitSelectionItem {
                work_ref: Some("work_exact".to_string()),
                message_id: Some("amsg_request".to_string()),
                agent: None,
                status: "pending".to_string(),
            }],
            completion_reason: None,
            condition_satisfied: false,
            quiet_count: 0,
            wake_sender: None,
            wake_ref: None,
        }
    );
}
