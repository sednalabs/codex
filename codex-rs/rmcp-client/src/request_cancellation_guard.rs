use rmcp::model::CancelledNotification;
use rmcp::model::CancelledNotificationParam;
use rmcp::model::RequestId;
use rmcp::service::Peer;
use rmcp::service::RoleClient;

/// Cancels the matching server request when the caller abandons its response future.
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
        let Some(peer) = self.peer.take() else { return };
        let request_id = self.request_id.clone();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let task = runtime.spawn(async move {
            let _ = peer
                .send_notification(
                    CancelledNotification::new(CancelledNotificationParam::new(
                        Some(request_id),
                        Some("request cancelled".to_string()),
                    ))
                    .into(),
                )
                .await;
        });
        drop(task);
    }
}
