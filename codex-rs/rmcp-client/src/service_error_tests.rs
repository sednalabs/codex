//! Verify caller error classification preserves typed causes through context.

use super::*;
use pretty_assertions::assert_eq;

#[test]
fn request_error_codes_preserve_typed_causes_through_context() {
    for (error, expected) in [
        (
            anyhow::Error::from(ClientOperationError::OAuthTokenRefreshed {
                source: ServiceError::TransportClosed,
            }),
            Some("oauth_token_refreshed_no_replay"),
        ),
        (
            request_timeout_error("tools/call", Duration::from_secs(1)),
            Some("request_timeout"),
        ),
        (
            anyhow::Error::from(ClientOperationError::Service(ServiceError::TransportClosed)),
            Some("transport_closed"),
        ),
        (anyhow::anyhow!("timed out awaiting tools/call"), None),
    ] {
        assert_eq!(
            request_error_code(&error.context("tool call failed")),
            expected
        );
    }
}
