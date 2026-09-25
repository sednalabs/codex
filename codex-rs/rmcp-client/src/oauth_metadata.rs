//! Require advertised OAuth metadata before configuring any credential-bearing flow.

use rmcp::transport::auth::AuthError;
use rmcp::transport::auth::AuthorizationManager;
use rmcp::transport::auth::AuthorizationMetadata;

pub(crate) async fn discover_metadata(
    manager: &AuthorizationManager,
) -> Result<AuthorizationMetadata, AuthError> {
    let resolution = manager.resolve_metadata().await?;
    // The SDK also resolves unpublished metadata to guessed legacy endpoints.
    // Preserve Codex's discovery-only contract for login and restored credentials.
    if !resolution.source.is_discovered() {
        return Err(AuthError::NoAuthorizationSupport);
    }
    Ok(resolution.metadata)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use axum::http::Response;
    use pretty_assertions::assert_eq;
    use rmcp::transport::auth::OAuthHttpClient;
    use rmcp::transport::auth::OAuthHttpClientFuture;
    use rmcp::transport::auth::OAuthHttpRequest;
    use serde_json::json;

    use super::*;

    struct MetadataClient {
        metadata: Option<serde_json::Value>,
        requests: Mutex<Vec<(String, String)>>,
    }

    impl OAuthHttpClient for MetadataClient {
        fn execute(&self, request: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
            Box::pin(async move {
                let path = request.request.uri().path().to_string();
                self.requests
                    .lock()
                    .unwrap()
                    .push((request.request.method().to_string(), path.clone()));
                if path == "/.well-known/oauth-authorization-server"
                    && let Some(metadata) = &self.metadata
                {
                    return Ok(Response::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(serde_json::to_vec(metadata).unwrap())
                        .unwrap());
                }
                Ok(Response::builder().status(404).body(Vec::new()).unwrap())
            })
        }
    }

    #[tokio::test]
    async fn unpublished_metadata_is_refused_without_contacting_guessed_endpoints() {
        let client = Arc::new(MetadataClient {
            metadata: None,
            requests: Mutex::new(Vec::new()),
        });
        let manager = AuthorizationManager::new_with_oauth_http_client(
            "https://oauth.example/mcp",
            client.clone(),
        )
        .await
        .unwrap();

        assert!(matches!(
            discover_metadata(&manager).await,
            Err(AuthError::NoAuthorizationSupport)
        ));
        let requests = client.requests.lock().unwrap();
        assert!(!requests.is_empty());
        assert!(requests.iter().all(|(method, path)| method == "GET"
            && !["/authorize", "/token", "/register"].contains(&path.as_str())));
    }

    #[tokio::test]
    async fn advertised_metadata_is_preserved() {
        let metadata = json!({
            "authorization_endpoint": "https://oauth.example/published/authorize",
            "token_endpoint": "https://oauth.example/published/token",
            "registration_endpoint": "https://oauth.example/published/register",
            "issuer": "https://oauth.example",
            "scopes_supported": ["read"],
            "custom_field": "preserved",
        });
        let client = Arc::new(MetadataClient {
            metadata: Some(metadata.clone()),
            requests: Mutex::new(Vec::new()),
        });
        let manager =
            AuthorizationManager::new_with_oauth_http_client("https://oauth.example/mcp", client)
                .await
                .unwrap();
        let actual = discover_metadata(&manager).await.unwrap();
        let expected: AuthorizationMetadata = serde_json::from_value(metadata).unwrap();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }
}
