use rmcp::transport::auth::AuthError;

use super::is_authentication_required_error;

#[test]
fn missing_refresh_token_requires_authentication() {
    let error = anyhow::Error::new(AuthError::AuthorizationRequired);

    assert!(is_authentication_required_error(&error));
}

#[test]
fn invalid_grant_refresh_failure_requires_authentication() {
    let error = anyhow::Error::new(AuthError::TokenRefreshRejected(
        "Server returned error response: invalid_grant: refresh token expired or revoked"
            .to_string(),
    ));

    assert!(is_authentication_required_error(&error));
}

#[test]
fn transient_refresh_failure_does_not_require_authentication() {
    let error = anyhow::Error::new(AuthError::TokenRefreshFailed(
        "Server returned error response: temporarily_unavailable: provider is temporarily unavailable"
            .to_string(),
    ));

    assert!(!is_authentication_required_error(&error));
}

#[test]
fn malformed_refresh_failure_does_not_require_authentication() {
    let error = anyhow::Error::new(AuthError::TokenRefreshFailed(
        "Failed to parse server response".to_string(),
    ));

    assert!(!is_authentication_required_error(&error));
}

#[test]
fn untyped_refresh_failure_text_does_not_require_authentication() {
    for message in [
        "Server returned error response: invalid_grant: refresh token expired or revoked",
        "No refresh token available",
    ] {
        let error = anyhow::Error::new(AuthError::TokenRefreshFailed(message.to_string()));

        assert!(!is_authentication_required_error(&error));
    }
}
