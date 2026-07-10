//! Serialized read-refresh-write transactions for MCP OAuth credentials.

use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use codex_keyring_store::DefaultKeyringStore;
use codex_keyring_store::KeyringStore;
use oauth2::TokenResponse;
use rmcp::transport::auth::AuthError;
use rmcp::transport::auth::AuthorizationManager;
use rmcp::transport::auth::CredentialStore as _;
use rmcp::transport::auth::InMemoryCredentialStore;
use rmcp::transport::auth::OAuthTokenResponse;
use rmcp::transport::auth::StoredCredentials;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::debug;
use tracing::warn;

use super::OAuthPersistor;
use super::OAuthPersistorInner;
use super::StoredOAuthTokens;
use super::WrappedOAuthTokenResponse;
use super::compute_expires_at_millis;
use super::refresh_lock::RefreshCredentialLock;
use super::token_needs_refresh;

const REFRESH_REQUEST_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Clone, Debug)]
pub(super) enum RefreshReason {
    Expiry,
    Unauthorized {
        rejected_access_token: Option<String>,
    },
}

impl RefreshReason {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Expiry => "expiry",
            Self::Unauthorized { .. } => "unauthorized",
        }
    }
}

impl OAuthPersistor {
    pub(crate) async fn refresh_if_needed(&self) -> Result<()> {
        self.refresh_if_needed_in(&DefaultKeyringStore, REFRESH_REQUEST_TIMEOUT)
            .await
    }

    /// Injects the credential backend and provider timeout for deterministic failure-path tests.
    pub(super) async fn refresh_if_needed_in<K: KeyringStore + Clone + 'static>(
        &self,
        keyring_store: &K,
        refresh_request_timeout: Duration,
    ) -> Result<()> {
        let expires_at = {
            let guard = self.inner.last_credentials.lock().await;
            guard.as_ref().and_then(|tokens| tokens.expires_at)
        };

        if !token_needs_refresh(expires_at) {
            return Ok(());
        }

        self.spawn_refresh_transaction(
            RefreshReason::Expiry,
            keyring_store,
            refresh_request_timeout,
        )
        .await
    }

    pub(crate) async fn refresh_after_unauthorized(
        &self,
        rejected_access_token: Option<&str>,
    ) -> Result<()> {
        self.spawn_refresh_transaction(
            RefreshReason::Unauthorized {
                rejected_access_token: rejected_access_token.map(str::to_owned),
            },
            &DefaultKeyringStore,
            REFRESH_REQUEST_TIMEOUT,
        )
        .await
    }

    async fn spawn_refresh_transaction<K: KeyringStore + Clone + 'static>(
        &self,
        reason: RefreshReason,
        keyring_store: &K,
        refresh_request_timeout: Duration,
    ) -> Result<()> {
        let persistor = self.clone();
        let keyring_store = keyring_store.clone();
        let reason_label = reason.as_str();
        // Once the provider can consume a rotating token, caller cancellation must not cancel
        // persistence. The owned task continues with independently bounded lock and request waits.
        let transaction_task = tokio::spawn(async move {
            let result = persistor
                .refresh_transaction_with_keyring_store(
                    reason,
                    refresh_request_timeout,
                    &keyring_store,
                )
                .await;

            if let Err(error) = &result {
                warn!(
                    server_name = %persistor.inner.server_name,
                    refresh_reason = reason_label,
                    error = %error,
                    "MCP OAuth refresh transaction failed"
                );
            }

            result
        });
        transaction_task.await.with_context(|| {
            format!(
                "OAuth refresh task failed for server {}",
                self.inner.server_name
            )
        })?
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "AuthorizationManager async access must be serialized through its Tokio mutex"
    )]
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(
            server_name = %self.inner.server_name,
            refresh_reason = reason.as_str(),
        ),
        err
    )]
    pub(super) async fn refresh_transaction_with_keyring_store<
        K: KeyringStore + Clone + 'static,
    >(
        &self,
        reason: RefreshReason,
        refresh_request_timeout: Duration,
        keyring_store: &K,
    ) -> Result<()> {
        debug!("waiting for the MCP OAuth credential transaction lock");
        let _lock =
            RefreshCredentialLock::acquire_for_server(&self.inner.server_name, &self.inner.url)
                .await?;
        debug!("acquired the MCP OAuth credential transaction lock");

        // Stay on the lifecycle-pinned store. A failure is surfaced rather than falling back and
        // possibly replaying an older rotating refresh token from the other store.
        debug!("rereading authoritative MCP OAuth credentials");
        let latest = self.inner.credential_store.load(
            keyring_store,
            &self.inner.server_name,
            &self.inner.url,
        )?;

        let Some(latest) = latest else {
            self.clear_manager_credentials().await;
            *self.inner.last_credentials.lock().await = None;
            return Err(AuthError::AuthorizationRequired).with_context(|| {
                format!(
                    "OAuth tokens for server {} were removed before refresh; authorization required",
                    self.inner.server_name
                )
            });
        };

        // Expiry refreshes can adopt any fresh winner. A 401 can only adopt a fresh winner whose
        // access token changed; the same token must be refreshed even if its expiry is in the future.
        let latest_access_token = latest.token_response.0.access_token().secret();
        let should_adopt = !token_needs_refresh(latest.expires_at)
            && match &reason {
                RefreshReason::Expiry => true,
                RefreshReason::Unauthorized {
                    rejected_access_token,
                } => rejected_access_token.as_deref() != Some(latest_access_token),
            };
        if should_adopt {
            debug!("adopting newer MCP OAuth credentials without contacting the provider");
            self.adopt_credentials(latest).await?;
            return Ok(());
        }

        if latest
            .token_response
            .0
            .refresh_token()
            .is_none_or(|refresh_token| refresh_token.secret().trim().is_empty())
        {
            return Err(AuthError::AuthorizationRequired).with_context(|| {
                format!(
                    "OAuth tokens for server {} cannot be refreshed; authorization required",
                    self.inner.server_name
                )
            });
        }

        let manager = self.inner.authorization_manager.clone();
        // The provider uses a separate HTTP client and cannot re-enter AuthClient. Retain this
        // guard so requests cannot observe credentials while they are staged and committed.
        let mut guard = manager.lock().await;
        if let Err(error) =
            install_tokens_in_manager_guard(&mut guard, &latest, CredentialExposure::Refresh).await
        {
            install_tokens_in_manager_guard(&mut guard, &latest, CredentialExposure::Request)
                .await
                .context("failed to restore request-only OAuth credentials")?;
            return Err(error).context("failed to stage OAuth credentials for refresh");
        }

        debug!(
            timeout_ms = refresh_request_timeout.as_millis(),
            "requesting refreshed MCP OAuth credentials from the provider"
        );
        let refresh_result = match timeout(refresh_request_timeout, guard.refresh_token()).await {
            Ok(Ok(token_response)) => {
                debug!("received refreshed MCP OAuth credentials from the provider");
                Ok(refreshed_tokens(token_response, &latest, &self.inner))
            }
            Ok(Err(error @ AuthError::TokenRefreshFailed(_))) => {
                warn!(
                    error = %error,
                    "MCP OAuth refresh failed; reauthorization required by RMCP compatibility policy"
                );
                Err(AuthError::AuthorizationRequired).with_context(|| {
                    format!(
                        "failed to refresh OAuth tokens for server {}: {error}",
                        self.inner.server_name
                    )
                })
            }
            Ok(Err(error)) => {
                warn!(error = %error, "MCP OAuth provider refresh failed");
                Err(error).with_context(|| {
                    format!(
                        "failed to refresh OAuth tokens for server {}",
                        self.inner.server_name
                    )
                })
            }
            Err(_) => {
                warn!(
                    timeout_ms = refresh_request_timeout.as_millis(),
                    "MCP OAuth provider refresh timed out; the outcome is unknown and a later serialized retry is permitted"
                );
                Err(anyhow::anyhow!(
                    "timed out after {refresh_request_timeout:?} refreshing OAuth tokens for server {}",
                    self.inner.server_name
                ))
            }
        };

        let refreshed = match refresh_result {
            Ok(refreshed) => refreshed,
            Err(error) => {
                install_tokens_in_manager_guard(
                    &mut guard,
                    &latest,
                    CredentialExposure::Request,
                )
                .await
                .context("failed to restore request-only OAuth credentials")?;
                return Err(error);
            }
        };

        // Commit to the pinned store before exposing the new access token. If persistence fails,
        // restore the prior request credential so no request can use state that will vanish on
        // restart.
        debug!("persisting refreshed MCP OAuth credentials to the resolved store");
        if let Err(error) =
            self.inner
                .credential_store
                .save(keyring_store, &self.inner.server_name, &refreshed)
        {
            warn!(
                error = %error,
                "failed to persist refreshed MCP OAuth credentials; restoring the previous in-process credentials"
            );
            install_tokens_in_manager_guard(&mut guard, &latest, CredentialExposure::Request)
                .await
                .context(
                    "failed to restore previous OAuth credentials after refresh persistence failed",
                )?;
            return Err(error);
        }

        install_tokens_in_manager_guard(&mut guard, &refreshed, CredentialExposure::Request)
            .await
            .context(
                "refreshed OAuth tokens were persisted but could not be installed in the authorization manager",
            )?;
        *self.inner.last_credentials.lock().await = Some(refreshed);
        drop(guard);
        debug!("persisted refreshed MCP OAuth credentials and completed the transaction");
        Ok(())
    }
}

#[expect(
    clippy::await_holding_invalid_type,
    reason = "AuthorizationManager async access must be serialized through its Tokio mutex"
)]
pub(super) async fn install_tokens_in_manager(
    authorization_manager: &Arc<Mutex<AuthorizationManager>>,
    tokens: &StoredOAuthTokens,
) -> Result<()> {
    let mut guard = authorization_manager.lock().await;
    install_tokens_in_manager_guard(&mut guard, tokens, CredentialExposure::Request).await
}

async fn install_tokens_in_manager_guard(
    authorization_manager: &mut AuthorizationManager,
    tokens: &StoredOAuthTokens,
    exposure: CredentialExposure,
) -> Result<()> {
    let store = InMemoryCredentialStore::new();
    store
        .save(stored_credentials_from_tokens(tokens, exposure))
        .await
        .context("failed to stage OAuth tokens for authorization manager")?;

    authorization_manager.set_credential_store(store);
    // TODO(stevenlee): Add an RMCP adoption API that atomically updates credentials, client ID,
    // and private current_scopes; this path cannot synchronize RMCP's scope-upgrade state.
    authorization_manager
        .initialize_from_store()
        .await
        .context("failed to adopt refreshed OAuth tokens")?;
    Ok(())
}

#[derive(Clone, Copy)]
pub(super) enum CredentialExposure {
    Request,
    Refresh,
}

pub(super) fn stored_credentials_from_tokens(
    tokens: &StoredOAuthTokens,
    exposure: CredentialExposure,
) -> StoredCredentials {
    let token_response = match exposure {
        CredentialExposure::Request => request_oauth_token_response(tokens),
        CredentialExposure::Refresh => tokens.token_response.0.clone(),
    };
    let granted_scopes = match exposure {
        CredentialExposure::Request => token_response
            .scopes()
            .map(|scopes| scopes.iter().map(|scope| scope.to_string()).collect())
            .unwrap_or_default(),
        // RFC 6749 treats omitted refresh scopes as the originally granted scope set.
        CredentialExposure::Refresh => Vec::new(),
    };
    let token_received_at = match exposure {
        CredentialExposure::Request => None,
        CredentialExposure::Refresh => SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_secs()),
    };

    StoredCredentials::new(
        tokens.client_id.clone(),
        Some(token_response),
        granted_scopes,
        token_received_at,
    )
}

pub(super) fn request_oauth_token_response(tokens: &StoredOAuthTokens) -> OAuthTokenResponse {
    let mut token_response = tokens.token_response.0.clone();
    token_response.set_refresh_token(None);
    token_response.set_expires_in(None);
    token_response
}

fn refreshed_tokens(
    mut token_response: OAuthTokenResponse,
    previous: &StoredOAuthTokens,
    inner: &OAuthPersistorInner,
) -> StoredOAuthTokens {
    if token_response.refresh_token().is_none() {
        token_response.set_refresh_token(previous.token_response.0.refresh_token().cloned());
    }
    if token_response.scopes().is_none() {
        token_response.set_scopes(previous.token_response.0.scopes().cloned());
    }
    StoredOAuthTokens {
        server_name: inner.server_name.clone(),
        url: inner.url.clone(),
        client_id: previous.client_id.clone(),
        expires_at: compute_expires_at_millis(&token_response),
        token_response: WrappedOAuthTokenResponse(token_response),
    }
}
