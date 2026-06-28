use rmcp::transport::auth::AuthError;

use super::is_authentication_required_error;

#[test]
fn token_refresh_failure_requires_authentication() {
    let error = anyhow::Error::new(AuthError::TokenRefreshFailed(
        "No refresh token available".to_string(),
    ));

    assert!(is_authentication_required_error(&error));
}
