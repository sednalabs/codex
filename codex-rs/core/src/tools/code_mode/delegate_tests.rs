//! Characterization of independent cell dispatch.

use std::sync::Arc;
use std::time::Duration;

use codex_code_mode::CellId;
use tokio::sync::oneshot;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use super::CodeModeDispatchBroker;
use super::DispatchMessage;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::tools::code_mode::ExecContext;
use crate::tools::registry::ToolRegistry;
use crate::tools::router::ToolRouter;
use crate::turn_diff_tracker::TurnDiffTracker;

#[tokio::test]
async fn unready_cell_blocks_unrelated_ready_notification() {
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let step = StepContext::for_test(Arc::clone(&turn));
    let router = Arc::new(ToolRouter::from_parts(
        ToolRegistry::empty_for_test(),
        Vec::new(),
    ));
    let broker = CodeModeDispatchBroker::new();
    let first = CellId::new("unready".to_string());
    let second = CellId::new("ready".to_string());
    broker.mark_cell_ready_for_dispatch(&second);
    let (first_tx, first_rx) = oneshot::channel();
    let (second_tx, mut second_rx) = oneshot::channel();
    broker
        .dispatch_tx
        .send(DispatchMessage::Notify {
            call_id: "first".to_string(),
            cell_id: first.clone(),
            text: String::new(),
            cancellation_token: CancellationToken::new(),
            response_tx: first_tx,
        })
        .await
        .expect("queue first notification");
    let _worker = broker.start_turn_worker(
        ExecContext { session, turn },
        router,
        step,
        Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
    );

    timeout(Duration::from_secs(5), async {
        while super::dispatch_gate(&broker.dispatch_gates, &first).receiver_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("worker should wait for the first gate");
    broker
        .dispatch_tx
        .send(DispatchMessage::Notify {
            call_id: "second".to_string(),
            cell_id: second,
            text: String::new(),
            cancellation_token: CancellationToken::new(),
            response_tx: second_tx,
        })
        .await
        .expect("queue second notification");

    // The independent ready cell cannot be served while the first gate is held.
    assert!(
        timeout(Duration::from_millis(250), &mut second_rx)
            .await
            .is_err()
    );
    broker.mark_cell_ready_for_dispatch(&first);
    assert!(matches!(
        timeout(Duration::from_secs(5), first_rx).await,
        Ok(Ok(Ok(())))
    ));
    assert!(matches!(
        timeout(Duration::from_secs(5), second_rx).await,
        Ok(Ok(Ok(())))
    ));
}
