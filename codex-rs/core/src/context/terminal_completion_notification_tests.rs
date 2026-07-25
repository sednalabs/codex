use super::ContextualUserFragment;
use super::TerminalCompletionNotification;
use super::TerminalCompletionStatus;

#[test]
fn terminal_completion_fragment_is_metadata_only() {
    let fragment = TerminalCompletionNotification {
        process_id: 42,
        instance_id: uuid::Uuid::nil(),
        status: TerminalCompletionStatus::Failed,
        exit_code: None,
        coalesced_exited: 0,
        coalesced_failed: 0,
    };

    let body = fragment.body();
    assert!(body.starts_with('\n'));
    assert!(body.ends_with('\n'));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body.trim()).unwrap(),
        serde_json::json!({
            "process_id": 42,
            "instance_id": "00000000-0000-0000-0000-000000000000",
            "status": "failed",
            "exit_code": null,
            "coalesced": {
                "exited": 0,
                "failed": 0,
            },
        })
    );
    for forbidden in [
        "stdout",
        "stderr",
        "command",
        "cwd",
        "environment",
        "approval",
    ] {
        assert!(!body.contains(forbidden));
    }
}
