use super::OutputHandles;
use super::UnifiedExecProcess;
use crate::session::tests::make_session_and_context_with_rx;
use crate::unified_exec::async_watcher::emit_exec_end_for_unified_exec;
use crate::unified_exec::head_tail_buffer::HeadTailBuffer;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn source_transcript_preserves_exec_end_when_delta_receiver_lags() {
    let output_buffer = Arc::new(Mutex::new(HeadTailBuffer::new(/*max_bytes*/ 8)));
    let aggregated_output = Arc::new(Mutex::new(HeadTailBuffer::new(/*max_bytes*/ 8)));
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(false));
    let output_closed_notify = Arc::new(Notify::new());
    let (output_tx, _) = broadcast::channel(/*capacity*/ 1);
    let mut lagged_receiver = output_tx.subscribe();
    let (stdout_tx, stdout_rx) = mpsc::channel(/*capacity*/ 4);
    let (stderr_tx, stderr_rx) = mpsc::channel(/*capacity*/ 1);

    let source_task = UnifiedExecProcess::spawn_local_output_task(
        stdout_rx,
        stderr_rx,
        OutputHandles {
            output_buffer: Arc::clone(&output_buffer),
            output_notify,
            output_closed: Arc::clone(&output_closed),
            output_closed_notify,
            cancellation_token: CancellationToken::new(),
        },
        Arc::clone(&aggregated_output),
        output_tx,
    );

    for chunk in [b"HEAD".to_vec(), b"MIDDLE".to_vec(), b"TAIL".to_vec()] {
        stdout_tx
            .send(chunk)
            .await
            .expect("source task should stay open");
    }
    drop(stdout_tx);
    drop(stderr_tx);
    source_task.await.expect("source task should complete");

    assert!(output_closed.load(Ordering::Acquire));
    assert!(matches!(
        lagged_receiver.try_recv(),
        Err(broadcast::error::TryRecvError::Lagged(2))
    ));

    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    emit_exec_end_for_unified_exec(
        session,
        Arc::clone(&turn),
        "lagged-output".to_string(),
        vec!["test-command".to_string()],
        #[allow(deprecated)]
        turn.cwd.clone().into(),
        Some("123".to_string()),
        aggregated_output,
        String::new(),
        /*exit_code*/ 0,
        Duration::from_millis(7),
    )
    .await;

    let event = tokio::time::timeout(Duration::from_secs(1), rx_event.recv())
        .await
        .expect("timed out waiting for completed command item")
        .expect("event channel closed");
    let EventMsg::ItemCompleted(completed) = event.msg else {
        panic!("expected ItemCompleted event");
    };
    let TurnItem::CommandExecution(item) = completed.item else {
        panic!("expected CommandExecution item");
    };

    let expected_output = "HEAD\n... 6 bytes omitted ...\nTAIL";
    assert_eq!(
        (
            item.stdout.as_deref(),
            item.stderr.as_deref(),
            item.aggregated_output.as_deref(),
            item.exit_code,
            item.duration,
        ),
        (
            Some(expected_output),
            Some(""),
            Some(expected_output),
            Some(0),
            Some(Duration::from_millis(7)),
        )
    );
}
