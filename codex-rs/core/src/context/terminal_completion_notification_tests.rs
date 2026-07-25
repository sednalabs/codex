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
    assert_eq!(
        body,
        "\n{\"coalesced\":{\"exited\":0,\"failed\":0},\"exit_code\":null,\"instance_id\":\"00000000-0000-0000-0000-000000000000\",\"process_id\":42,\"status\":\"failed\"}\n"
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
