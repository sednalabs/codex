mod config;
mod signing;

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::hash::Hasher;
use std::sync::OnceLock;
use std::time::SystemTime;

use aws_credential_types::Credentials;
use aws_credential_types::provider::ProvideCredentials;
use aws_credential_types::provider::SharedCredentialsProvider;
use bytes::Bytes;
use http::HeaderMap;
use http::Method;
use thiserror::Error;

/// AWS auth configuration used to resolve credentials and sign requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsAuthConfig {
    pub profile: Option<String>,
    pub region: Option<String>,
    pub service: String,
}

/// Generic HTTP request shape consumed by SigV4 signing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsRequestToSign {
    pub method: Method,
    pub url: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// Signed request parts returned to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsSignedRequest {
    pub url: String,
    pub headers: HeaderMap,
}

/// Errors returned by credential loading or SigV4 signing.
#[derive(Debug, Error)]
pub enum AwsAuthError {
    #[error("AWS service name must not be empty")]
    EmptyService,
    #[error("AWS SDK config did not resolve a credentials provider")]
    MissingCredentialsProvider,
    #[error("AWS SDK config did not resolve a region")]
    MissingRegion,
    #[error("failed to load AWS credentials: {0}")]
    Credentials(#[from] aws_credential_types::provider::error::CredentialsError),
    #[error("request URL is not a valid URI: {0}")]
    InvalidUri(#[source] http::uri::InvalidUri),
    #[error("failed to construct HTTP request for signing: {0}")]
    BuildHttpRequest(#[source] http::Error),
    #[error("request contains a non-UTF8 header value: {0}")]
    InvalidHeaderValue(#[source] http::header::ToStrError),
    #[error("failed to build signable request: {0}")]
    SigningRequest(#[source] aws_sigv4::http_request::SigningError),
    #[error("failed to build SigV4 signing params: {0}")]
    SigningParams(String),
    #[error("SigV4 signing failed: {0}")]
    SigningFailure(#[source] aws_sigv4::http_request::SigningError),
}

/// Loaded AWS auth context that can sign outbound HTTP requests.
#[derive(Clone)]
pub struct AwsAuthContext {
    credentials_provider: SharedCredentialsProvider,
    profile: Option<String>,
    region: String,
    service: String,
}

/// A request-authority snapshot whose AWS credentials cannot change between admission and
/// signing. Its debug representation deliberately excludes all credential material.
#[derive(Clone)]
pub struct FrozenAwsAuthContext {
    credentials: Credentials,
    region: String,
    service: String,
    identity: AwsCredentialIdentity,
}

/// Process-local, secret-safe identity for one frozen AWS signer.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct AwsCredentialIdentity([u64; 2]);

impl std::fmt::Debug for AwsCredentialIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AwsCredentialIdentity(<redacted>)")
    }
}

impl AwsCredentialIdentity {
    /// Adds this already-keyed opaque identity to a wider process-local authority fingerprint.
    pub fn write_to(&self, state: &mut dyn Hasher) {
        state.write_u64(self.0[0]);
        state.write_u64(self.0[1]);
    }
}

impl std::fmt::Debug for FrozenAwsAuthContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrozenAwsAuthContext")
            .field("region", &self.region)
            .field("service", &self.service)
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

fn credential_identity(
    credentials: &Credentials,
    profile: Option<&str>,
    region: &str,
    service: &str,
) -> AwsCredentialIdentity {
    fn write_bytes(state: &mut dyn Hasher, value: &[u8]) {
        state.write_usize(value.len());
        state.write(value);
    }

    fn write_optional_bytes(state: &mut dyn Hasher, value: Option<&[u8]>) {
        match value {
            Some(value) => {
                state.write_u8(1);
                write_bytes(state, value);
            }
            None => state.write_u8(0),
        }
    }

    static KEYS: OnceLock<[RandomState; 2]> = OnceLock::new();
    let keys = KEYS.get_or_init(|| [RandomState::new(), RandomState::new()]);
    let mut values = [0; 2];
    for (index, key) in keys.iter().enumerate() {
        let mut state = key.build_hasher();
        state.write(b"aws-sigv4\0");
        write_bytes(&mut state, credentials.access_key_id().as_bytes());
        write_bytes(&mut state, credentials.secret_access_key().as_bytes());
        write_optional_bytes(&mut state, credentials.session_token().map(str::as_bytes));
        match credentials.expiry() {
            None => state.write_u8(0),
            Some(expiry) => match expiry.duration_since(std::time::UNIX_EPOCH) {
                Ok(duration) => {
                    state.write_u8(1);
                    state.write_u64(duration.as_secs());
                    state.write_u32(duration.subsec_nanos());
                }
                Err(error) => {
                    state.write_u8(2);
                    state.write_u64(error.duration().as_secs());
                    state.write_u32(error.duration().subsec_nanos());
                }
            },
        }
        write_optional_bytes(
            &mut state,
            credentials
                .account_id()
                .map(|account_id| account_id.as_str().as_bytes()),
        );
        write_optional_bytes(&mut state, profile.map(str::as_bytes));
        write_bytes(&mut state, region.as_bytes());
        write_bytes(&mut state, service.as_bytes());
        values[index] = state.finish();
    }
    AwsCredentialIdentity(values)
}

impl std::fmt::Debug for AwsAuthContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsAuthContext")
            .field("region", &self.region)
            .field("service", &self.service)
            .finish_non_exhaustive()
    }
}

impl AwsAuthContext {
    pub async fn load(config: AwsAuthConfig) -> Result<Self, AwsAuthError> {
        let sdk_config = config::load_sdk_config(&config).await?;
        let credentials_provider = config::credentials_provider(&sdk_config)?;
        let region = config::resolved_region(&sdk_config)?;

        Ok(Self {
            credentials_provider,
            profile: config.profile,
            region,
            service: config.service.trim().to_string(),
        })
    }

    pub fn region(&self) -> &str {
        &self.region
    }

    pub fn service(&self) -> &str {
        &self.service
    }

    pub async fn sign(&self, request: AwsRequestToSign) -> Result<AwsSignedRequest, AwsAuthError> {
        self.sign_at(request, SystemTime::now()).await
    }

    /// Resolves the credential provider exactly once for a request-authority snapshot.
    pub async fn freeze(&self) -> Result<FrozenAwsAuthContext, AwsAuthError> {
        let credentials = self.credentials_provider.provide_credentials().await?;
        Ok(FrozenAwsAuthContext::new(
            credentials,
            self.profile.clone(),
            self.region.clone(),
            self.service.clone(),
        ))
    }

    async fn sign_at(
        &self,
        request: AwsRequestToSign,
        time: SystemTime,
    ) -> Result<AwsSignedRequest, AwsAuthError> {
        let credentials = self.credentials_provider.provide_credentials().await?;
        signing::sign_request(&credentials, &self.region, &self.service, request, time)
    }
}

impl FrozenAwsAuthContext {
    fn new(
        credentials: Credentials,
        profile: Option<String>,
        region: String,
        service: String,
    ) -> Self {
        let identity = credential_identity(&credentials, profile.as_deref(), &region, &service);
        Self {
            credentials,
            region,
            service,
            identity,
        }
    }

    /// Creates a frozen context from caller-provided credentials.
    ///
    /// This is primarily useful to custom credential providers and deterministic tests; callers
    /// should normally use [`AwsAuthContext::freeze`].
    pub fn from_credentials(
        credentials: Credentials,
        region: impl Into<String>,
        service: impl Into<String>,
    ) -> Self {
        Self::new(credentials, None, region.into(), service.into())
    }

    pub fn identity(&self) -> AwsCredentialIdentity {
        self.identity
    }

    pub fn region(&self) -> &str {
        &self.region
    }

    pub fn service(&self) -> &str {
        &self.service
    }

    pub async fn sign(&self, request: AwsRequestToSign) -> Result<AwsSignedRequest, AwsAuthError> {
        self.sign_at(request, SystemTime::now())
    }

    fn sign_at(
        &self,
        request: AwsRequestToSign,
        time: SystemTime,
    ) -> Result<AwsSignedRequest, AwsAuthError> {
        signing::sign_request(
            &self.credentials,
            &self.region,
            &self.service,
            request,
            time,
        )
    }
}

impl AwsAuthError {
    /// Returns whether retrying the outbound request can reasonably recover from this auth error.
    pub fn is_retryable(&self) -> bool {
        match self {
            AwsAuthError::Credentials(error) => matches!(
                error,
                aws_credential_types::provider::error::CredentialsError::ProviderTimedOut(_)
                    | aws_credential_types::provider::error::CredentialsError::ProviderError(_)
            ),
            AwsAuthError::EmptyService
            | AwsAuthError::MissingCredentialsProvider
            | AwsAuthError::MissingRegion
            | AwsAuthError::InvalidUri(_)
            | AwsAuthError::BuildHttpRequest(_)
            | AwsAuthError::InvalidHeaderValue(_)
            | AwsAuthError::SigningRequest(_)
            | AwsAuthError::SigningParams(_)
            | AwsAuthError::SigningFailure(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    use std::time::UNIX_EPOCH;

    use aws_credential_types::Credentials;
    use aws_credential_types::provider::error::CredentialsError;
    use pretty_assertions::assert_eq;

    use super::*;

    fn test_context(session_token: Option<&str>) -> AwsAuthContext {
        AwsAuthContext {
            credentials_provider: SharedCredentialsProvider::new(Credentials::new(
                "AKIDEXAMPLE",
                "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
                session_token.map(str::to_string),
                /*expires_after*/ None,
                "unit-test",
            )),
            profile: None,
            region: "us-east-1".to_string(),
            service: "bedrock".to_string(),
        }
    }

    fn frozen_context(
        access_key_id: &str,
        secret_access_key: &str,
        session_token: Option<&str>,
    ) -> FrozenAwsAuthContext {
        FrozenAwsAuthContext::from_credentials(
            Credentials::new(
                access_key_id,
                secret_access_key,
                session_token.map(str::to_string),
                /*expires_after*/ None,
                "unit-test",
            ),
            "us-east-1",
            "bedrock-mantle",
        )
    }

    fn test_request() -> AwsRequestToSign {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        headers.insert("x-test-header", http::HeaderValue::from_static("present"));
        AwsRequestToSign {
            method: Method::POST,
            url: "https://bedrock-runtime.us-east-1.amazonaws.com/v1/responses".to_string(),
            headers,
            body: Bytes::from_static(br#"{"model":"openai.gpt-oss-120b-1:0"}"#),
        }
    }

    #[tokio::test]
    async fn sign_adds_sigv4_headers_and_preserves_existing_headers() {
        let signed = test_context(/*session_token*/ None)
            .sign_at(
                test_request(),
                UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            )
            .await
            .expect("request should sign");

        assert_eq!(
            signing::header_value(&signed.headers, http::header::CONTENT_TYPE.as_str()),
            Some("application/json".to_string())
        );
        assert_eq!(
            signing::header_value(&signed.headers, "x-test-header"),
            Some("present".to_string())
        );
        assert_eq!(
            signed.url,
            "https://bedrock-runtime.us-east-1.amazonaws.com/v1/responses"
        );
        assert!(
            signing::header_value(&signed.headers, http::header::AUTHORIZATION.as_str())
                .is_some_and(|value| value.starts_with("AWS4-HMAC-SHA256 "))
        );
        assert!(signing::header_value(&signed.headers, "x-amz-date").is_some());
    }

    #[test]
    fn credentials_provider_failures_are_retryable() {
        assert!(
            AwsAuthError::Credentials(CredentialsError::provider_error("temporarily unavailable"))
                .is_retryable()
        );
        assert!(
            AwsAuthError::Credentials(CredentialsError::provider_timed_out(Duration::from_secs(1)))
                .is_retryable()
        );
    }

    #[test]
    fn deterministic_aws_auth_errors_are_not_retryable() {
        assert!(!AwsAuthError::EmptyService.is_retryable());
        assert!(
            !AwsAuthError::Credentials(CredentialsError::not_loaded_no_source()).is_retryable()
        );
        assert!(
            !AwsAuthError::Credentials(CredentialsError::invalid_configuration("bad profile"))
                .is_retryable()
        );
        assert!(
            !AwsAuthError::Credentials(CredentialsError::unhandled("unexpected response"))
                .is_retryable()
        );
    }

    #[tokio::test]
    async fn sign_includes_session_token_when_credentials_have_one() {
        let signed = test_context(Some("session-token"))
            .sign_at(
                test_request(),
                UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            )
            .await
            .expect("request should sign");

        assert_eq!(
            signing::header_value(&signed.headers, "x-amz-security-token"),
            Some("session-token".to_string())
        );
    }

    #[test]
    fn frozen_signer_identity_is_stable_distinct_and_secret_safe() {
        let signer_a = frozen_context("AKID-A", "secret-a", Some("session-a"));
        let signer_a_again = frozen_context("AKID-A", "secret-a", Some("session-a"));
        let signer_b = frozen_context("AKID-A", "secret-b", Some("session-a"));

        assert_eq!(signer_a.identity(), signer_a_again.identity());
        assert_ne!(signer_a.identity(), signer_b.identity());
        let credentials = Credentials::new(
            "AKID-A",
            "secret-a",
            Some("session-a".to_string()),
            /*expires_after*/ None,
            "unit-test",
        );
        assert_ne!(
            credential_identity(
                &credentials,
                Some("profile-a"),
                "us-east-1",
                "bedrock-mantle",
            ),
            credential_identity(
                &credentials,
                Some("profile-b"),
                "us-east-1",
                "bedrock-mantle",
            )
        );
        assert_ne!(
            credential_identity(
                &credentials,
                Some("profile-a"),
                "us-east-1",
                "bedrock-mantle",
            ),
            credential_identity(
                &credentials,
                Some("profile-a"),
                "us-west-2",
                "bedrock-mantle",
            )
        );
        let debug = format!("{signer_a:?} {:?}", signer_a.identity());
        for secret in ["AKID-A", "secret-a", "session-a"] {
            assert!(!debug.contains(secret), "debug leaked credential material");
        }
    }

    #[test]
    fn frozen_signer_reuses_exact_credentials_for_multiple_requests() {
        let signer = frozen_context("AKID-REUSED", "secret-reused", None);
        let signed_a = signer
            .sign_at(
                test_request(),
                UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            )
            .expect("first request should sign");
        let signed_b = signer
            .sign_at(
                test_request(),
                UNIX_EPOCH + Duration::from_secs(1_700_000_001),
            )
            .expect("second request should sign");

        for signed in [signed_a, signed_b] {
            assert!(
                signing::header_value(&signed.headers, http::header::AUTHORIZATION.as_str())
                    .is_some_and(|value| value.contains("Credential=AKID-REUSED/"))
            );
        }
    }

    #[tokio::test]
    async fn load_rejects_empty_service_name() {
        let err = AwsAuthContext::load(AwsAuthConfig {
            profile: None,
            region: None,
            service: "   ".to_string(),
        })
        .await
        .expect_err("empty service should be rejected");

        assert_eq!(err.to_string(), "AWS service name must not be empty");
    }
}
