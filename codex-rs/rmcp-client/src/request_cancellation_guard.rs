use rmcp::model::CancelledNotification;
use rmcp::model::CancelledNotificationParam;
use rmcp::model::RequestId;
use rmcp::service::Peer;
use rmcp::service::RoleClient;

/// Sends a protocol cancellation when an in-flight request is dropped.
///
/// rmcp request handles intentionally do not cancel from `Drop`. The client
/// uses this guard around the response future so caller cancellation and active
/// timeout cancellation both reclaim the peer's pending request registration.
pub(crate) struct RequestCancellationGuard {
    peer: Option<Peer<RoleClient>>,
    request_id: RequestId,
}

impl RequestCancellationGuard {
    pub(crate) fn new(peer: Peer<RoleClient>, request_id: RequestId) -> Self {
        Self {
            peer: Some(peer),
            request_id,
        }
    }

    pub(crate) fn disarm(mut self) {
        self.peer = None;
    }
}

impl Drop for RequestCancellationGuard {
    fn drop(&mut self) {
        let Some(peer) = self.peer.take() else {
            return;
        };
        let request_id = self.request_id.clone();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let task = runtime.spawn(async move {
            let _ = peer
                .send_notification(
                    CancelledNotification {
                        params: CancelledNotificationParam {
                            request_id: Some(request_id),
                            reason: Some("request cancelled".to_string()),
                            meta: None,
                        },
                        method: rmcp::model::CancelledNotificationMethod,
                        extensions: Default::default(),
                    }
                    .into(),
                )
                .await;
        });
        drop(task);
    }
}
