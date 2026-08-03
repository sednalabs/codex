use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_app_server_protocol::FuzzyFileSearchSessionUpdatedNotification;
use codex_app_server_protocol::ServerNotification;
use codex_file_search::FileSearchSnapshot;
use pretty_assertions::assert_eq;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::outgoing_message::OutgoingEnvelope;
use crate::outgoing_message::OutgoingMessage;
use crate::outgoing_message::OutgoingMessageSender;
use super::PendingNotification;
use super::PendingNotifications;
use super::SessionReporterImpl;
use super::SessionShared;
use super::forward_notifications;

#[derive(Debug, Eq, PartialEq)]
enum ObservedNotification {
    Updated(String),
    Completed,
}

fn report_snapshot(
    reporter: &SessionReporterImpl,
    shared: &SessionShared,
    query: &str,
) {
    {
        #[expect(clippy::unwrap_used)]
        let mut latest_query = shared.latest_query.lock().unwrap();
        *latest_query = query.to_string();
    }
    codex_file_search::SessionReporter::on_update(
        reporter,
        &FileSearchSnapshot {
            query: query.to_string(),
            ..Default::default()
        },
    );
}

async fn recv_notification(
    outgoing_rx: &mut mpsc::Receiver<OutgoingEnvelope>,
) -> ObservedNotification {
    let envelope = timeout(Duration::from_secs(1), outgoing_rx.recv())
        .await
        .expect("notification should arrive before timeout")
        .expect("outgoing channel should remain open");
    let OutgoingEnvelope::Broadcast { message } = envelope else {
        panic!("expected broadcast notification");
    };
    let OutgoingMessage::AppServerNotification(envelope) = message else {
        panic!("expected app-server notification");
    };
    match envelope.notification {
        ServerNotification::FuzzyFileSearchSessionUpdated(notification) => {
            ObservedNotification::Updated(notification.query)
        }
        ServerNotification::FuzzyFileSearchSessionCompleted(notification) => {
            assert_eq!(notification.session_id, "session");
            ObservedNotification::Completed
        }
        other => panic!("unexpected notification: {other:?}"),
    }
}

#[test]
fn pending_notifications_replace_latest_and_preserve_completion_order() {
    let mut pending = PendingNotifications::new();

    assert!(pending.replace_update("first"));
    assert!(pending.push_completion());
    assert!(pending.push_completion());
    assert!(pending.replace_update("latest"));
    assert!(pending.push_completion());

    let drained = [
        pending.take_next(),
        pending.take_next(),
        pending.take_next(),
        pending.take_next(),
        pending.take_next(),
    ];
    assert_eq!(
        drained,
        [
            Some(PendingNotification::Complete),
            Some(PendingNotification::Complete),
            Some(PendingNotification::Update("latest")),
            Some(PendingNotification::Complete),
            None,
        ]
    );
}

#[tokio::test]
async fn forwarder_serializes_replace_latest_completion_and_cancellation_under_backpressure()
{
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel(1);
    let outgoing = Arc::new(OutgoingMessageSender::new(
        outgoing_tx,
        codex_analytics::AnalyticsEventsClient::disabled(),
    ));
    outgoing
        .send_server_notification(ServerNotification::FuzzyFileSearchSessionUpdated(
            FuzzyFileSearchSessionUpdatedNotification {
                session_id: "session".to_string(),
                query: "outbound-blocker".to_string(),
                files: Vec::new(),
            },
        ))
        .await;

    let shared = Arc::new(SessionShared {
        session_id: "session".to_string(),
        latest_query: Mutex::new(String::new()),
        outgoing: outgoing.clone(),
        canceled: Arc::new(AtomicBool::new(false)),
        pending_notifications: Mutex::new(PendingNotifications::new()),
        notification_ready: Notify::new(),
        notification_dequeued: Notify::new(),
        notification_waiting: Notify::new(),
    });
    let reporter = SessionReporterImpl {
        shared: shared.clone(),
    };
    let forwarder_waiting = shared.notification_waiting.notified();
    let forwarder = tokio::spawn(forward_notifications(shared.clone()));
    timeout(Duration::from_secs(1), forwarder_waiting)
        .await
        .expect("forwarder should park on the notification wakeup");

    let in_flight_dequeued = shared.notification_dequeued.notified();
    report_snapshot(&reporter, &shared, "in-flight");
    timeout(Duration::from_secs(1), in_flight_dequeued)
        .await
        .expect("forwarder should dequeue the in-flight snapshot");

    report_snapshot(&reporter, &shared, "replaced");
    codex_file_search::SessionReporter::on_complete(&reporter);
    report_snapshot(&reporter, &shared, "latest");
    codex_file_search::SessionReporter::on_complete(&reporter);

    let completion_before_dequeued = shared.notification_dequeued.notified();
    assert_eq!(
        recv_notification(&mut outgoing_rx).await,
        ObservedNotification::Updated("outbound-blocker".to_string())
    );
    timeout(Duration::from_secs(1), completion_before_dequeued)
        .await
        .expect("forwarder should dequeue completion before the latest snapshot");

    let latest_dequeued = shared.notification_dequeued.notified();
    assert_eq!(
        recv_notification(&mut outgoing_rx).await,
        ObservedNotification::Updated("in-flight".to_string())
    );
    timeout(Duration::from_secs(1), latest_dequeued)
        .await
        .expect("forwarder should dequeue the latest snapshot");

    let completion_after_dequeued = shared.notification_dequeued.notified();
    assert_eq!(
        recv_notification(&mut outgoing_rx).await,
        ObservedNotification::Completed
    );
    timeout(Duration::from_secs(1), completion_after_dequeued)
        .await
        .expect("forwarder should dequeue completion after the latest snapshot");

    assert_eq!(
        recv_notification(&mut outgoing_rx).await,
        ObservedNotification::Updated("latest".to_string())
    );
    assert_eq!(
        recv_notification(&mut outgoing_rx).await,
        ObservedNotification::Completed
    );

    outgoing
        .send_server_notification(ServerNotification::FuzzyFileSearchSessionUpdated(
            FuzzyFileSearchSessionUpdatedNotification {
                session_id: "session".to_string(),
                query: "cancel-blocker".to_string(),
                files: Vec::new(),
            },
        ))
        .await;
    let canceled_in_flight_dequeued = shared.notification_dequeued.notified();
    report_snapshot(&reporter, &shared, "canceled-in-flight");
    timeout(Duration::from_secs(1), canceled_in_flight_dequeued)
        .await
        .expect("forwarder should dequeue the snapshot canceled in flight");
    report_snapshot(&reporter, &shared, "canceled-pending");
    codex_file_search::SessionReporter::on_complete(&reporter);

    shared.cancel();
    forwarder.abort();
    let join_error = forwarder
        .await
        .expect_err("blocked forwarder should be canceled");
    assert!(join_error.is_cancelled());

    report_snapshot(&reporter, &shared, "ignored-after-cancel");
    codex_file_search::SessionReporter::on_complete(&reporter);
    assert_eq!(
        recv_notification(&mut outgoing_rx).await,
        ObservedNotification::Updated("cancel-blocker".to_string())
    );
    assert!(matches!(
        outgoing_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn stopping_pending_notifications_releases_payload_and_rejects_callbacks() {
    struct DropMarker(Arc<AtomicUsize>);

    impl Drop for DropMarker {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    let drops = Arc::new(AtomicUsize::new(0));
    let mut pending = PendingNotifications::new();
    assert!(pending.replace_update(DropMarker(drops.clone())));

    pending.stop();
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert!(!pending.replace_update(DropMarker(drops.clone())));
    assert!(!pending.push_completion());
    assert_eq!(drops.load(Ordering::Relaxed), 2);
    assert!(pending.take_next().is_none());
}
