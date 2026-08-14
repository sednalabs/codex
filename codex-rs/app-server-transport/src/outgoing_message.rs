use std::fmt;

use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::Result;
use codex_app_server_protocol::ServerNotificationEnvelope;
use codex_app_server_protocol::ServerRequest;
use serde::Serialize;
use tokio::sync::oneshot;

/// Stable identifier for a transport connection.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ConnectionId(pub u64);

impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Outgoing message from the server to the client.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum OutgoingMessage {
    Request(ServerRequest),
    /// A thread-bound server request with its immutable, connection-local
    /// subscription identity. Serialization stays JSON-RPC-compatible by
    /// flattening the ordinary request and adding one extension property.
    ThreadScopedRequest(ThreadScopedServerRequest),
    /// A thread-bound app-server notification with its immutable,
    /// connection-local subscription identity. It serializes to the normal
    /// notification envelope plus one extension property.
    ThreadScopedNotification(ThreadScopedServerNotification),
    /// AppServerNotification is specific to the case where this is run as an
    /// "app server" as opposed to an MCP server.
    AppServerNotification(ServerNotificationEnvelope),
    Response(OutgoingResponse),
    Error(OutgoingError),
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadScopedServerRequest {
    #[serde(flatten)]
    pub request: ServerRequest,
    #[serde(rename = "threadSubscriptionId")]
    pub thread_subscription_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadScopedServerNotification {
    #[serde(flatten)]
    pub envelope: ServerNotificationEnvelope,
    #[serde(rename = "threadSubscriptionId")]
    pub thread_subscription_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OutgoingResponse {
    pub id: RequestId,
    pub result: Result,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OutgoingError {
    pub error: JSONRPCErrorError,
    pub id: RequestId,
}

#[derive(Debug)]
pub struct QueuedOutgoingMessage {
    pub message: OutgoingMessage,
    pub write_complete_tx: Option<oneshot::Sender<()>>,
}

impl QueuedOutgoingMessage {
    pub fn new(message: OutgoingMessage) -> Self {
        Self {
            message,
            write_complete_tx: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use codex_app_server_protocol::AccountLoginCompletedNotification;
    use codex_app_server_protocol::JSONRPCMessage;
    use codex_app_server_protocol::ServerNotification;

    use super::*;

    #[test]
    fn thread_subscription_extension_preserves_jsonrpc_notification_shape() {
        let message = OutgoingMessage::ThreadScopedNotification(ThreadScopedServerNotification {
            envelope: ServerNotificationEnvelope {
                notification: ServerNotification::AccountLoginCompleted(
                    AccountLoginCompletedNotification {
                        login_id: None,
                        success: true,
                        error: None,
                    },
                ),
                emitted_at_ms: Some(123),
            },
            thread_subscription_id: "subscription-123".to_string(),
        });

        let value = serde_json::to_value(message).expect("thread notification should serialize");
        assert_eq!(
            value.get("threadSubscriptionId"),
            Some(&serde_json::json!("subscription-123"))
        );
        assert!(matches!(
            serde_json::from_value::<JSONRPCMessage>(value)
                .expect("older JSON-RPC decoder should ignore the extension"),
            JSONRPCMessage::Notification(_)
        ));
    }
}
