//! This file handles all logic related to managing MCP OAuth credentials.
//! All credentials are stored using the keyring crate which uses os-specific keyring services.
//! https://crates.io/crates/keyring
//! macOS: macOS keychain.
//! Windows: Windows Credential Manager
//! Linux: DBus-based Secret Service, the kernel keyutils, and a combo of the two
//! FreeBSD, OpenBSD: DBus-based Secret Service
//!
//! For Linux, we use linux-native-async-persistent which uses both keyutils and async-secret-service (see below) for storage.
//! See the docs for the keyutils_persistent module for a full explanation of why both are used. Because this store uses the
//! async-secret-service, you must specify the additional features required by that store
//!
//! async-secret-service provides access to the DBus-based Secret Service storage on Linux, FreeBSD, and OpenBSD. This is an asynchronous
//! keystore that always encrypts secrets when they are transferred across the bus. If DBus isn't installed the keystore will fall back to the json
//! file because we don't use the "vendored" feature.
//!
//! If the keyring is not available or fails, we fall back to CODEX_HOME/.credentials.json which is consistent with other coding CLI agents.

mod resolved_store;
mod store_lock;

#[cfg(test)]
#[path = "oauth/test_support.rs"]
mod test_support;

use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_secrets::LocalSecretsNamespace;
use codex_secrets::SecretName;
use codex_secrets::SecretScope;
use codex_secrets::SecretsBackendKind;
use codex_secrets::SecretsManager;
use oauth2::AccessToken;
use oauth2::RefreshToken;
use oauth2::Scope;
use oauth2::TokenResponse;
use oauth2::basic::BasicTokenType;
use rmcp::transport::auth::OAuthTokenResponse;
use rmcp::transport::auth::VendorExtraTokenFields;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::map::Map as JsonMap;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tracing::warn;

use self::store_lock::OAuthStore;
use self::store_lock::OAuthStoreLock;
use self::store_lock::OAuthStoreLockFailure;

use codex_keyring_store::DefaultKeyringStore;
use codex_keyring_store::KeyringStore;
use rmcp::transport::auth::AuthorizationManager;
use rmcp::transport::auth::CredentialStore as _;
use rmcp::transport::auth::InMemoryCredentialStore;
use rmcp::transport::auth::StoredCredentials;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tokio::time::timeout;

use codex_utils_home_dir::find_codex_home;

pub(crate) use self::resolved_store::ResolvedOAuthCredentialStore;
pub(crate) use self::resolved_store::ResolvedOAuthTokens;
pub(crate) use self::resolved_store::resolve_oauth_tokens_from_store_policy;

const KEYRING_SERVICE: &str = "Codex MCP Credentials";
const MCP_OAUTH_SECRET_PREFIX: &str = "MCP_OAUTH";
const REFRESH_SKEW_MILLIS: u64 = 30_000;
const REFRESH_LOCK_DIR: &str = "mcp-oauth-refresh-locks";
const LOCK_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(60);
const CREDENTIAL_LOCK_RETRY_SLEEP: Duration = Duration::from_millis(500);
const REFRESH_TRANSACTION_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredOAuthTokens {
    pub server_name: String,
    pub url: String,
    pub client_id: String,
    pub token_response: WrappedOAuthTokenResponse,
    #[serde(default)]
    pub expires_at: Option<u64>,
}

/// Wrap OAuthTokenResponse to allow for partial equality comparison.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WrappedOAuthTokenResponse(pub OAuthTokenResponse);

impl PartialEq for WrappedOAuthTokenResponse {
    fn eq(&self, other: &Self) -> bool {
        match (serde_json::to_string(self), serde_json::to_string(other)) {
            (Ok(s1), Ok(s2)) => s1 == s2,
            _ => false,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StoredOAuthTokenStatus {
    Missing,
    Usable,
    AuthorizationRequired,
}

pub(crate) fn oauth_token_status(
    server_name: &str,
    url: &str,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<StoredOAuthTokenStatus> {
    let resolved = resolve_oauth_tokens_from_store_policy(
        &DefaultKeyringStore,
        server_name,
        url,
        store_mode,
        keyring_backend_kind,
    )?;
    Ok(match resolved.as_ref().map(|resolved| &resolved.tokens) {
        None => StoredOAuthTokenStatus::Missing,
        Some(tokens) if oauth_tokens_are_usable(tokens) => StoredOAuthTokenStatus::Usable,
        Some(_) => StoredOAuthTokenStatus::AuthorizationRequired,
    })
}

fn oauth_tokens_are_usable(tokens: &StoredOAuthTokens) -> bool {
    if tokens.client_id.trim().is_empty() {
        return false;
    }

    let token_response = &tokens.token_response.0;
    if token_needs_refresh(tokens.expires_at) {
        return token_response
            .refresh_token()
            .is_some_and(|token| !token.secret().trim().is_empty());
    }

    !token_response.access_token().secret().trim().is_empty()
}

fn refresh_expires_in_from_timestamp(tokens: &mut StoredOAuthTokens) {
    let Some(expires_at) = tokens.expires_at else {
        return;
    };

    match expires_in_from_timestamp(expires_at) {
        Some(seconds) => {
            let duration = Duration::from_secs(seconds);
            tokens.token_response.0.set_expires_in(Some(&duration));
        }
        None => {
            // RMCP treats a missing expiry as unknown and uses the access token
            // as-is. Treat a known-expired timestamp as an explicit zero so
            // startup refreshes the token before the first request.
            tokens
                .token_response
                .0
                .set_expires_in(Some(&Duration::ZERO));
        }
    }
}

fn load_oauth_tokens_from_keyring<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    keyring_backend_kind: AuthKeyringBackendKind,
    server_name: &str,
    url: &str,
) -> std::result::Result<Option<StoredOAuthTokens>, OAuthKeyringLoadError> {
    match keyring_backend_kind {
        AuthKeyringBackendKind::Direct => {
            load_oauth_tokens_from_direct_keyring(keyring_store, server_name, url)
                .map_err(OAuthKeyringLoadError::Backend)
        }
        AuthKeyringBackendKind::Secrets => {
            load_oauth_tokens_from_secrets_keyring(keyring_store, server_name, url)
        }
    }
}

fn load_oauth_tokens_from_direct_keyring<K: KeyringStore>(
    keyring_store: &K,
    server_name: &str,
    url: &str,
) -> Result<Option<StoredOAuthTokens>> {
    let key = compute_store_key(server_name, url)?;
    match keyring_store.load(KEYRING_SERVICE, &key) {
        Ok(Some(serialized)) => {
            let mut tokens: StoredOAuthTokens = serde_json::from_str(&serialized)
                .context("failed to deserialize OAuth tokens from keyring")?;
            refresh_expires_in_from_timestamp(&mut tokens);
            Ok(Some(tokens))
        }
        Ok(None) => Ok(None),
        Err(error) => Err(Error::new(error.into_error())),
    }
}

fn load_oauth_tokens_from_secrets_keyring<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    server_name: &str,
    url: &str,
) -> std::result::Result<Option<StoredOAuthTokens>, OAuthKeyringLoadError> {
    let _store_lock = OAuthStoreLock::acquire(OAuthStore::Secrets)?;
    let codex_home = find_codex_home().map_err(anyhow::Error::from)?;
    let manager = SecretsManager::new_with_keyring_store_and_namespace(
        codex_home.to_path_buf(),
        SecretsBackendKind::Local,
        Arc::new(keyring_store.clone()),
        LocalSecretsNamespace::McpOAuth,
    );
    let secret_name = compute_secret_name(server_name, url)?;
    match manager
        .get(&SecretScope::Global, &secret_name)
        .context("failed to load MCP OAuth tokens from encrypted storage")?
    {
        Some(serialized) => {
            let mut tokens: StoredOAuthTokens = serde_json::from_str(&serialized)
                .context("failed to deserialize OAuth tokens from encrypted storage")?;
            refresh_expires_in_from_timestamp(&mut tokens);
            Ok(Some(tokens))
        }
        None => Ok(None),
    }
}

/// Classifies keyring load failures that affect Auto fallback policy.
#[derive(Debug, thiserror::Error)]
enum OAuthKeyringLoadError {
    /// Store coordination failed, so consulting another authority would be unsafe.
    #[error(transparent)]
    StoreLock(#[from] OAuthStoreLockFailure),
    /// The selected keyring backend itself was unavailable or its data was invalid.
    #[error(transparent)]
    Backend(#[from] anyhow::Error),
}

pub fn save_oauth_tokens(
    server_name: &str,
    tokens: &StoredOAuthTokens,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<()> {
    let keyring_store = DefaultKeyringStore;
    match store_mode {
        OAuthCredentialsStoreMode::Auto => save_oauth_tokens_with_keyring_with_fallback_to_file(
            &keyring_store,
            keyring_backend_kind,
            server_name,
            tokens,
        ),
        OAuthCredentialsStoreMode::File => save_oauth_tokens_to_file(tokens),
        OAuthCredentialsStoreMode::Keyring => save_oauth_tokens_with_keyring_and_cleanup_file(
            &keyring_store,
            keyring_backend_kind,
            server_name,
            tokens,
        ),
    }
}

pub async fn save_oauth_tokens_locked(
    server_name: &str,
    tokens: &StoredOAuthTokens,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<()> {
    let _lock = RefreshCredentialLock::acquire_for_server_with_timeout(
        server_name,
        &tokens.url,
        LOCK_ACQUIRE_TIMEOUT,
    )
    .await?;
    save_oauth_tokens(server_name, tokens, store_mode, keyring_backend_kind)
}

fn save_oauth_tokens_with_keyring<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    keyring_backend_kind: AuthKeyringBackendKind,
    server_name: &str,
    tokens: &StoredOAuthTokens,
) -> Result<()> {
    // This exact-store writer is used after a client resolves its authority. Only login-time
    // policy resolution may clean up or update the non-selected store.
    match keyring_backend_kind {
        AuthKeyringBackendKind::Direct => {
            save_oauth_tokens_to_direct_keyring(keyring_store, server_name, tokens)
        }
        AuthKeyringBackendKind::Secrets => {
            save_oauth_tokens_to_secrets_keyring(keyring_store, server_name, tokens)
        }
    }
}

fn save_oauth_tokens_to_direct_keyring<K: KeyringStore>(
    keyring_store: &K,
    server_name: &str,
    tokens: &StoredOAuthTokens,
) -> Result<()> {
    let serialized = serde_json::to_string(tokens).context("failed to serialize OAuth tokens")?;

    let key = compute_store_key(server_name, &tokens.url)?;
    match keyring_store.save(KEYRING_SERVICE, &key, &serialized) {
        Ok(()) => Ok(()),
        Err(error) => {
            let message = format!(
                "failed to write OAuth tokens to keyring: {}",
                error.message()
            );
            warn!("{message}");
            Err(Error::new(error.into_error()).context(message))
        }
    }
}

/// Saves one credential while holding the Secrets aggregate-store lock across the mutation.
fn save_oauth_tokens_to_secrets_keyring<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    server_name: &str,
    tokens: &StoredOAuthTokens,
) -> Result<()> {
    let serialized = serde_json::to_string(tokens).context("failed to serialize OAuth tokens")?;
    let _store_lock = OAuthStoreLock::acquire(OAuthStore::Secrets)?;
    save_oauth_tokens_to_secrets_keyring_with_lock_held(
        keyring_store,
        server_name,
        tokens,
        &serialized,
    )
}

/// Writes one credential to Secrets. The caller must hold the Secrets aggregate-store lock.
fn save_oauth_tokens_to_secrets_keyring_with_lock_held<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    server_name: &str,
    tokens: &StoredOAuthTokens,
    serialized: &str,
) -> Result<()> {
    let codex_home = find_codex_home()?;
    let manager = SecretsManager::new_with_keyring_store_and_namespace(
        codex_home.to_path_buf(),
        SecretsBackendKind::Local,
        Arc::new(keyring_store.clone()),
        LocalSecretsNamespace::McpOAuth,
    );
    let secret_name = compute_secret_name(server_name, &tokens.url)?;
    manager
        .set(&SecretScope::Global, &secret_name, serialized)
        .context("failed to write OAuth tokens to encrypted storage")
}

/// Saves to the selected keyring backend, then best-effort removes the fallback File entry.
fn save_oauth_tokens_with_keyring_and_cleanup_file<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    keyring_backend_kind: AuthKeyringBackendKind,
    server_name: &str,
    tokens: &StoredOAuthTokens,
) -> Result<()> {
    save_oauth_tokens_with_keyring(keyring_store, keyring_backend_kind, server_name, tokens)?;
    let key = compute_store_key(server_name, &tokens.url)?;
    if let Err(error) = delete_oauth_tokens_from_file(&key) {
        warn!(
            server_name,
            keyring_backend = ?keyring_backend_kind,
            error = %error,
            "failed to remove OAuth tokens from fallback storage"
        );
    }
    Ok(())
}

fn save_oauth_tokens_with_keyring_with_fallback_to_file<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    keyring_backend_kind: AuthKeyringBackendKind,
    server_name: &str,
    tokens: &StoredOAuthTokens,
) -> Result<()> {
    match save_oauth_tokens_with_keyring_and_cleanup_file(
        keyring_store,
        keyring_backend_kind,
        server_name,
        tokens,
    ) {
        Ok(()) => Ok(()),
        // As on load, a store lock failure is a coordination failure rather than evidence that
        // the keyring backend is unavailable. Falling back could leave a newer File token hidden
        // behind a stale Secrets entry.
        Err(error) if error.downcast_ref::<OAuthStoreLockFailure>().is_some() => Err(error),
        Err(error) => {
            let message = error.to_string();
            warn!("falling back to file storage for OAuth tokens: {message}");
            save_oauth_tokens_to_file(tokens)
                .with_context(|| format!("failed to write OAuth tokens to keyring: {message}"))
        }
    }
}

pub fn delete_oauth_tokens(
    server_name: &str,
    url: &str,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<bool> {
    let keyring_store = DefaultKeyringStore;
    delete_oauth_tokens_from_keyring_and_file(
        &keyring_store,
        store_mode,
        keyring_backend_kind,
        server_name,
        url,
    )
}

pub async fn delete_oauth_tokens_locked(
    server_name: &str,
    url: &str,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<bool> {
    let _lock = RefreshCredentialLock::acquire_for_server_with_timeout(
        server_name,
        url,
        LOCK_ACQUIRE_TIMEOUT,
    )
    .await?;
    delete_oauth_tokens(server_name, url, store_mode, keyring_backend_kind)
}

fn delete_oauth_tokens_from_keyring_and_file<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    server_name: &str,
    url: &str,
) -> Result<bool> {
    let key = compute_store_key(server_name, url)?;
    let keyring_result =
        delete_oauth_tokens_from_keyring(keyring_store, keyring_backend_kind, server_name, url);
    let keyring_removed = match keyring_result {
        Ok(removed) => removed,
        Err(error) => {
            let message = error.to_string();
            warn!("failed to delete OAuth tokens from keyring: {message}");
            match store_mode {
                OAuthCredentialsStoreMode::Auto | OAuthCredentialsStoreMode::Keyring => {
                    return Err(error).context("failed to delete OAuth tokens from keyring");
                }
                OAuthCredentialsStoreMode::File => false,
            }
        }
    };

    let file_removed = delete_oauth_tokens_from_file(&key)?;
    Ok(keyring_removed || file_removed)
}

fn delete_oauth_tokens_from_keyring<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    keyring_backend_kind: AuthKeyringBackendKind,
    server_name: &str,
    url: &str,
) -> Result<bool> {
    match keyring_backend_kind {
        AuthKeyringBackendKind::Direct => {
            delete_oauth_tokens_from_direct_keyring(keyring_store, server_name, url)
        }
        AuthKeyringBackendKind::Secrets => {
            let direct_removed =
                delete_oauth_tokens_from_direct_keyring(keyring_store, server_name, url)?;
            let secrets_removed =
                delete_oauth_tokens_from_secrets_keyring(keyring_store, server_name, url)?;
            Ok(direct_removed || secrets_removed)
        }
    }
}

fn delete_oauth_tokens_from_direct_keyring<K: KeyringStore>(
    keyring_store: &K,
    server_name: &str,
    url: &str,
) -> Result<bool> {
    let key = compute_store_key(server_name, url)?;
    keyring_store
        .delete(KEYRING_SERVICE, &key)
        .map_err(|error| Error::new(error.into_error()))
}

fn delete_oauth_tokens_from_secrets_keyring<K: KeyringStore + Clone + 'static>(
    keyring_store: &K,
    server_name: &str,
    url: &str,
) -> Result<bool> {
    let _store_lock = OAuthStoreLock::acquire(OAuthStore::Secrets)?;
    let codex_home = find_codex_home()?;
    let manager = SecretsManager::new_with_keyring_store_and_namespace(
        codex_home.to_path_buf(),
        SecretsBackendKind::Local,
        Arc::new(keyring_store.clone()),
        LocalSecretsNamespace::McpOAuth,
    );
    let secret_name = compute_secret_name(server_name, url)?;
    let secrets_removed = manager
        .delete(&SecretScope::Global, &secret_name)
        .context("failed to delete OAuth tokens from encrypted storage")?;
    Ok(secrets_removed)
}

#[derive(Clone)]
pub(crate) struct OAuthPersistor {
    inner: Arc<OAuthPersistorInner>,
}

struct OAuthPersistorInner {
    server_name: String,
    url: String,
    authorization_manager: Arc<Mutex<AuthorizationManager>>,
    credential_store: ResolvedOAuthCredentialStore,
    last_credentials: Mutex<Option<StoredOAuthTokens>>,
    persist_lock: Mutex<()>,
}

impl OAuthPersistor {
    pub(crate) fn new(
        server_name: String,
        url: String,
        authorization_manager: Arc<Mutex<AuthorizationManager>>,
        credential_store: ResolvedOAuthCredentialStore,
        initial_credentials: Option<StoredOAuthTokens>,
    ) -> Self {
        Self {
            inner: Arc::new(OAuthPersistorInner {
                server_name,
                url,
                authorization_manager,
                credential_store,
                last_credentials: Mutex::new(initial_credentials),
                persist_lock: Mutex::new(()),
            }),
        }
    }

    /// Persists RMCP-managed credential changes back to this client's resolved authority.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "AuthorizationManager async access must be serialized through its mutex"
    )]
    pub(crate) async fn persist_if_needed(&self) -> Result<()> {
        let _persist_guard = self.inner.persist_lock.lock().await;
        let (client_id, maybe_credentials) = {
            let manager = self.inner.authorization_manager.clone();
            let guard = manager.lock().await;
            guard.get_credentials().await
        }?;

        let previous = self.inner.last_credentials.lock().await.clone();
        let candidate = match maybe_credentials {
            Some(mut credentials) => {
                if let Some(previous) = previous.as_ref() {
                    if credentials.refresh_token().is_none() {
                        credentials.set_refresh_token(
                            previous.token_response.0.refresh_token().cloned(),
                        );
                    }
                    if credentials.scopes().is_none() {
                        credentials.set_scopes(previous.token_response.0.scopes().cloned());
                    }
                    if credentials.expires_in().is_none() {
                        let expires_in = previous.token_response.0.expires_in();
                        credentials.set_expires_in(expires_in.as_ref());
                    }
                }
                let new_token_response = WrappedOAuthTokenResponse(credentials.clone());
                let same_token = previous
                    .as_ref()
                    .map(|previous| previous.token_response == new_token_response)
                    .unwrap_or(false);
                let expires_at = if same_token {
                    previous
                        .as_ref()
                        .and_then(|previous| previous.expires_at)
                } else {
                    compute_expires_at_millis(&credentials)
                };
                let stored = StoredOAuthTokens {
                    server_name: self.inner.server_name.clone(),
                    url: self.inner.url.clone(),
                    client_id,
                    token_response: new_token_response,
                    expires_at,
                };
                Some(stored)
            }
            None => None,
        };

        if stored_credentials_match_for_reconciliation(candidate.as_ref(), previous.as_ref()) {
            return Ok(());
        }

        let _refresh_lock = RefreshCredentialLock::acquire_for_server_with_timeout(
            &self.inner.server_name,
            &self.inner.url,
            LOCK_ACQUIRE_TIMEOUT,
        )
        .await?;
        let durable = self.inner.credential_store.load(
            &DefaultKeyringStore,
            &self.inner.server_name,
            &self.inner.url,
        )?;

        if !stored_credentials_match_for_reconciliation(durable.as_ref(), previous.as_ref()) {
            match durable {
                Some(latest) => self.adopt_credentials(latest).await?,
                None => {
                    self.clear_manager_credentials().await;
                    *self.inner.last_credentials.lock().await = None;
                }
            }
            return Ok(());
        }

        match candidate {
            Some(stored) => {
                self.inner.credential_store.save(
                    &DefaultKeyringStore,
                    &self.inner.server_name,
                    &stored,
                )?;
                *self.inner.last_credentials.lock().await = Some(stored);
            }
            None => {
                if let Err(error) = self.inner.credential_store.delete(
                    &DefaultKeyringStore,
                    &self.inner.server_name,
                    &self.inner.url,
                ) {
                    warn!(
                        server_name = %self.inner.server_name,
                        error = %error,
                        "failed to remove MCP OAuth credentials from the resolved store"
                    );
                    return Ok(());
                }
                *self.inner.last_credentials.lock().await = None;
            }
        }

        Ok(())
    }

    pub(crate) async fn refresh_if_needed(&self) -> Result<()> {
        self.refresh_if_needed_with_timeout(REFRESH_TRANSACTION_TIMEOUT)
            .await
    }

    pub(crate) async fn refresh_if_needed_with_timeout(
        &self,
        overall_timeout: Duration,
    ) -> Result<()> {
        let expires_at = {
            let guard = self.inner.last_credentials.lock().await;
            guard.as_ref().and_then(|tokens| tokens.expires_at)
        };

        if !token_needs_refresh(expires_at) {
            return Ok(());
        }

        self.refresh_transaction(RefreshReason::Expiry, overall_timeout)
            .await
    }

    pub(crate) async fn refresh_after_unauthorized(&self) -> Result<()> {
        self.refresh_after_unauthorized_with_timeout(REFRESH_TRANSACTION_TIMEOUT)
            .await
    }

    pub(crate) async fn refresh_after_unauthorized_with_timeout(
        &self,
        overall_timeout: Duration,
    ) -> Result<()> {
        self.refresh_transaction(RefreshReason::Unauthorized, overall_timeout)
            .await
    }

    fn save_resolved_credentials(&self, tokens: &StoredOAuthTokens) -> Result<()> {
        self.inner.credential_store.save(
            &DefaultKeyringStore,
            &self.inner.server_name,
            tokens,
        )
    }

    async fn refresh_transaction(
        &self,
        reason: RefreshReason,
        overall_timeout: Duration,
    ) -> Result<()> {
        let keyring_store = DefaultKeyringStore;
        self.refresh_transaction_with_keyring_store(reason, overall_timeout, &keyring_store)
            .await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "AuthorizationManager async access must be serialized through its mutex"
    )]
    async fn refresh_transaction_with_keyring_store<K: KeyringStore + Clone + 'static>(
        &self,
        reason: RefreshReason,
        overall_timeout: Duration,
        keyring_store: &K,
    ) -> Result<()> {
        let local_access_token = {
            let last_credentials = self.inner.last_credentials.lock().await;
            last_credentials
                .as_ref()
                .map(|tokens| tokens.token_response.0.access_token().secret().to_string())
        };

        let deadline = Instant::now() + overall_timeout;
        let lock_timeout =
            remaining_refresh_timeout(deadline, overall_timeout, LOCK_ACQUIRE_TIMEOUT)?;
        let _lock = RefreshCredentialLock::acquire_for_server_with_timeout(
            &self.inner.server_name,
            &self.inner.url,
            lock_timeout,
        )
        .await?;

        // Reread after acquiring the cross-process lock. If another client refreshed first,
        // adopt that newer state instead of replaying a stale rotating refresh token.
        let Some(latest) = self.inner.credential_store.load(
            keyring_store,
            &self.inner.server_name,
            &self.inner.url,
        )?
        else {
            self.clear_manager_credentials().await;
            let mut last_credentials = self.inner.last_credentials.lock().await;
            *last_credentials = None;
            anyhow::bail!(
                "OAuth tokens for server {} were removed before refresh; authorization required",
                self.inner.server_name
            );
        };

        let latest_access_token = latest.token_response.0.access_token().secret();
        let should_adopt = !token_needs_refresh(latest.expires_at)
            && match reason {
                RefreshReason::Expiry => true,
                RefreshReason::Unauthorized => {
                    local_access_token.as_deref() != Some(latest_access_token)
                }
            };
        if should_adopt {
            self.adopt_credentials(latest).await?;
            return Ok(());
        }

        let manager = self.inner.authorization_manager.clone();
        let mut guard = manager.lock().await;
        if let Err(error) =
            install_tokens_in_manager_guard(&mut guard, &latest, CredentialExposure::Refresh).await
        {
            install_tokens_in_manager_guard(&mut guard, &latest, CredentialExposure::Request)
                .await
                .context("failed to restore request-only OAuth credentials")?;
            return Err(error).context("failed to stage OAuth credentials for refresh");
        }

        let refresh_request_timeout =
            match remaining_refresh_timeout(deadline, overall_timeout, REFRESH_TRANSACTION_TIMEOUT)
            {
                Ok(refresh_request_timeout) => refresh_request_timeout,
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

        let refresh_result = match timeout(refresh_request_timeout, guard.refresh_token()).await {
            Ok(Ok(token_response)) => Ok(refreshed_tokens(token_response, &latest, &self.inner)),
            Ok(Err(error)) => Err(error).with_context(|| {
                format!(
                    "failed to refresh OAuth tokens for server {}",
                    self.inner.server_name
                )
            }),
            Err(_) => Err(anyhow::anyhow!(
                "timed out after {overall_timeout:?} refreshing OAuth tokens for server {}",
                self.inner.server_name
            )),
        };

        let request_tokens = refresh_result.as_ref().unwrap_or(&latest);
        install_tokens_in_manager_guard(&mut guard, request_tokens, CredentialExposure::Request)
            .await
            .context("failed to restore request-only OAuth credentials")?;
        drop(guard);

        let refreshed = refresh_result?;
        // Retain the full rotating credential in local durable state before attempting the
        // write. RMCP keeps request-only credentials, so this is the retry source if the
        // first persistence attempt fails.
        {
            let mut last_credentials = self.inner.last_credentials.lock().await;
            *last_credentials = Some(refreshed.clone());
        }
        self.save_resolved_credentials(&refreshed)?;
        Ok(())
    }

    pub(crate) async fn adopt_credentials(&self, tokens: StoredOAuthTokens) -> Result<()> {
        install_tokens_in_manager(&self.inner.authorization_manager, &tokens).await?;
        let mut last_credentials = self.inner.last_credentials.lock().await;
        *last_credentials = Some(tokens);
        Ok(())
    }

    async fn clear_manager_credentials(&self) {
        let manager = self.inner.authorization_manager.clone();
        let mut guard = manager.lock().await;
        guard.set_credential_store(InMemoryCredentialStore::new());
    }
}

#[derive(Clone, Copy)]
enum RefreshReason {
    Expiry,
    Unauthorized,
}

/// Compares durable credentials without treating the derived `expires_in` countdown as a
/// concurrent credential change when `expires_at` remains authoritative.
fn stored_credentials_match_for_reconciliation(
    left: Option<&StoredOAuthTokens>,
    right: Option<&StoredOAuthTokens>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            let mut left = left.clone();
            let mut right = right.clone();
            if left.expires_at.is_some() && right.expires_at.is_some() {
                left.token_response.0.set_expires_in(None);
                right.token_response.0.set_expires_in(None);
            }
            left == right
        }
        _ => false,
    }
}

fn remaining_refresh_timeout(
    deadline: Instant,
    overall_timeout: Duration,
    maximum: Duration,
) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        anyhow::bail!("timed out after {overall_timeout:?} refreshing OAuth tokens");
    }
    Ok(remaining.min(maximum))
}

struct RefreshCredentialLock {
    _file: File,
}

impl RefreshCredentialLock {
    async fn acquire_for_server_with_timeout(
        server_name: &str,
        url: &str,
        acquire_timeout: Duration,
    ) -> Result<Self> {
        let key = compute_store_key(server_name, url)?;
        Self::acquire_with_timeout(&key, acquire_timeout)
            .await
            .with_context(|| format!("failed to acquire OAuth credential lock for {server_name}"))
    }

    async fn acquire_with_timeout(store_key: &str, acquire_timeout: Duration) -> Result<Self> {
        let path = refresh_lock_path(store_key)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("failed to open OAuth refresh lock {}", path.display()))?;

        match timeout(acquire_timeout, async {
            loop {
                match file.try_lock() {
                    Ok(()) => return Ok(()),
                    Err(std::fs::TryLockError::WouldBlock) => {
                        sleep(CREDENTIAL_LOCK_RETRY_SLEEP).await;
                    }
                    Err(error) => return Err(std::io::Error::from(error)),
                }
            }
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                return Err(error).with_context(|| {
                    format!("failed to lock OAuth refresh lock {}", path.display())
                });
            }
            Err(_) => anyhow::bail!(
                "timed out after {acquire_timeout:?} waiting for OAuth refresh lock {}",
                path.display()
            ),
        }

        Ok(Self { _file: file })
    }
}

#[expect(
    clippy::await_holding_invalid_type,
    reason = "AuthorizationManager async access must be serialized through its mutex"
)]
async fn install_tokens_in_manager(
    authorization_manager: &Arc<Mutex<AuthorizationManager>>,
    tokens: &StoredOAuthTokens,
) -> Result<()> {
    let manager = authorization_manager.clone();
    let mut guard = manager.lock().await;
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
    authorization_manager
        .initialize_from_store()
        .await
        .context("failed to adopt refreshed OAuth tokens")?;
    Ok(())
}

#[derive(Clone, Copy)]
enum CredentialExposure {
    Request,
    Refresh,
}

fn stored_credentials_from_tokens(
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

pub(crate) fn request_oauth_token_response(tokens: &StoredOAuthTokens) -> OAuthTokenResponse {
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
    let expires_at = compute_expires_at_millis(&token_response);
    StoredOAuthTokens {
        server_name: inner.server_name.clone(),
        url: inner.url.clone(),
        client_id: previous.client_id.clone(),
        token_response: WrappedOAuthTokenResponse(token_response),
        expires_at,
    }
}

const FALLBACK_FILENAME: &str = ".credentials.json";
const MCP_SERVER_TYPE: &str = "http";

type FallbackFile = BTreeMap<String, FallbackTokenEntry>;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FallbackTokenEntry {
    server_name: String,
    server_url: String,
    client_id: String,
    access_token: String,
    #[serde(default)]
    expires_at: Option<u64>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    scopes: Vec<String>,
}

fn load_oauth_tokens_from_file(server_name: &str, url: &str) -> Result<Option<StoredOAuthTokens>> {
    let _store_lock = OAuthStoreLock::acquire(OAuthStore::File)?;
    let Some(store) = read_fallback_file_unlocked()? else {
        return Ok(None);
    };

    let key = compute_store_key(server_name, url)?;

    for entry in store.values() {
        let entry_key = compute_store_key(&entry.server_name, &entry.server_url)?;
        if entry_key != key {
            continue;
        }

        let mut token_response = OAuthTokenResponse::new(
            AccessToken::new(entry.access_token.clone()),
            BasicTokenType::Bearer,
            VendorExtraTokenFields::default(),
        );

        if let Some(refresh) = entry.refresh_token.clone() {
            token_response.set_refresh_token(Some(RefreshToken::new(refresh)));
        }

        let scopes = entry.scopes.clone();
        if !scopes.is_empty() {
            token_response.set_scopes(Some(scopes.into_iter().map(Scope::new).collect()));
        }

        let mut stored = StoredOAuthTokens {
            server_name: entry.server_name.clone(),
            url: entry.server_url.clone(),
            client_id: entry.client_id.clone(),
            token_response: WrappedOAuthTokenResponse(token_response),
            expires_at: entry.expires_at,
        };
        refresh_expires_in_from_timestamp(&mut stored);

        return Ok(Some(stored));
    }

    Ok(None)
}

/// Saves one credential while holding the File aggregate-store lock across the full
/// read-modify-write operation.
fn save_oauth_tokens_to_file(tokens: &StoredOAuthTokens) -> Result<()> {
    let _store_lock = OAuthStoreLock::acquire(OAuthStore::File)?;
    save_oauth_tokens_to_file_with_lock_held(tokens)
}

/// Updates the fallback File. The caller must hold the File aggregate-store lock.
fn save_oauth_tokens_to_file_with_lock_held(tokens: &StoredOAuthTokens) -> Result<()> {
    let key = compute_store_key(&tokens.server_name, &tokens.url)?;
    let mut store = read_fallback_file_unlocked()?.unwrap_or_default();

    let token_response = &tokens.token_response.0;
    let expires_at = tokens
        .expires_at
        .or_else(|| compute_expires_at_millis(token_response));
    let refresh_token = token_response
        .refresh_token()
        .map(|token| token.secret().to_string());
    let scopes = token_response
        .scopes()
        .map(|s| s.iter().map(|s| s.to_string()).collect())
        .unwrap_or_default();
    let entry = FallbackTokenEntry {
        server_name: tokens.server_name.clone(),
        server_url: tokens.url.clone(),
        client_id: tokens.client_id.clone(),
        access_token: token_response.access_token().secret().to_string(),
        expires_at,
        refresh_token,
        scopes,
    };

    store.insert(key, entry);
    write_fallback_file(&store)
}

fn delete_oauth_tokens_from_file(key: &str) -> Result<bool> {
    let _store_lock = OAuthStoreLock::acquire(OAuthStore::File)?;
    let mut store = match read_fallback_file_unlocked()? {
        Some(store) => store,
        None => return Ok(false),
    };

    let removed = store.remove(key).is_some();

    if removed {
        write_fallback_file(&store)?;
    }

    Ok(removed)
}

pub(crate) fn compute_expires_at_millis(response: &OAuthTokenResponse) -> Option<u64> {
    let expires_in = response.expires_in()?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0));
    let expiry = now.checked_add(expires_in)?;
    let millis = expiry.as_millis();
    if millis > u128::from(u64::MAX) {
        Some(u64::MAX)
    } else {
        Some(millis as u64)
    }
}

fn expires_in_from_timestamp(expires_at: u64) -> Option<u64> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0));
    let now_ms = now.as_millis() as u64;

    if expires_at <= now_ms {
        None
    } else {
        Some((expires_at - now_ms) / 1000)
    }
}

fn token_needs_refresh(expires_at: Option<u64>) -> bool {
    let Some(expires_at) = expires_at else {
        return false;
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis() as u64;

    now.saturating_add(REFRESH_SKEW_MILLIS) >= expires_at
}

fn compute_store_key(server_name: &str, server_url: &str) -> Result<String> {
    let mut payload = JsonMap::new();
    payload.insert(
        "type".to_string(),
        Value::String(MCP_SERVER_TYPE.to_string()),
    );
    payload.insert("url".to_string(), Value::String(server_url.to_string()));
    payload.insert("headers".to_string(), Value::Object(JsonMap::new()));

    let truncated = sha_256_prefix(&Value::Object(payload))?;
    Ok(format!("{server_name}|{truncated}"))
}

fn refresh_lock_path(store_key: &str) -> Result<PathBuf> {
    let mut hasher = Sha256::new();
    hasher.update(store_key.as_bytes());
    let digest = hasher.finalize();
    Ok(find_codex_home()?
        .join(REFRESH_LOCK_DIR)
        .join(format!("{digest:x}.lock"))
        .to_path_buf())
}

/// Derive a valid secret-store name from the MCP OAuth store key.
///
/// `compute_store_key` intentionally includes readable identity components and
/// a pipe separator, but `SecretName` only allows `A-Z`, `0-9`, and `_`.
/// Re-hashing keeps the secret key deterministic while satisfying that
/// restricted alphabet.
fn compute_secret_name(server_name: &str, server_url: &str) -> Result<SecretName> {
    let key = compute_store_key(server_name, server_url)?;
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();
    let hex = format!("{digest:X}");
    SecretName::new(&format!("{MCP_OAUTH_SECRET_PREFIX}_{}", &hex[..32]))
}

fn fallback_file_path() -> Result<PathBuf> {
    Ok(find_codex_home()?.join(FALLBACK_FILENAME).to_path_buf())
}

fn read_fallback_file_unlocked() -> Result<Option<FallbackFile>> {
    let path = fallback_file_path()?;
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err).context(format!(
                "failed to read credentials file at {}",
                path.display()
            ));
        }
    };

    if contents.trim().is_empty() {
        return Ok(None);
    }

    match serde_json::from_str::<FallbackFile>(&contents) {
        Ok(store) => Ok(Some(store)),
        Err(e) => Err(e).context(format!(
            "failed to parse credentials file at {}",
            path.display()
        )),
    }
}

fn write_fallback_file(store: &FallbackFile) -> Result<()> {
    let path = fallback_file_path()?;

    if store.is_empty() {
        if path.exists() {
            fs::remove_file(path)?;
        }
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let serialized = serde_json::to_string(store)?;
    let parent = path
        .parent()
        .context("credentials file path is missing a parent directory")?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_nanos();
    let tmp_path = parent.join(format!(
        ".{FALLBACK_FILENAME}.tmp-{}-{nonce}",
        std::process::id()
    ));

    let write_result = {
        let mut tmp_file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp_path)
            .with_context(|| {
                format!(
                    "failed to create temp credentials file at {}",
                    tmp_path.display()
                )
            })?;

        let result = (|| -> Result<()> {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = fs::Permissions::from_mode(0o600);
                tmp_file.set_permissions(perms).with_context(|| {
                    format!(
                        "failed to set permissions on temp credentials file at {}",
                        tmp_path.display()
                    )
                })?;
            }

            tmp_file.write_all(serialized.as_bytes()).with_context(|| {
                format!(
                    "failed to write temp credentials file at {}",
                    tmp_path.display()
                )
            })?;
            tmp_file.sync_all().with_context(|| {
                format!(
                    "failed to sync temp credentials file at {}",
                    tmp_path.display()
                )
            })?;
            Ok(())
        })();

        drop(tmp_file);
        result
    };

    if let Err(error) = write_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(error);
    }

    if let Err(error) = fs::rename(&tmp_path, &path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(error).with_context(|| {
            format!(
                "failed to atomically replace credentials file at {} with {}",
                path.display(),
                tmp_path.display()
            )
        });
    }

    #[cfg(unix)]
    {
        let dir = fs::File::open(parent)
            .with_context(|| format!("failed to open {}", parent.display()))?;
        dir.sync_all()
            .with_context(|| format!("failed to sync {}", parent.display()))?;
    }

    Ok(())
}

fn sha_256_prefix(value: &Value) -> Result<String> {
    let serialized =
        serde_json::to_string(&value).context("failed to serialize MCP OAuth key payload")?;
    let mut hasher = Sha256::new();
    hasher.update(serialized.as_bytes());
    let digest = hasher.finalize();
    let hex = format!("{digest:x}");
    let truncated = &hex[..16];
    Ok(truncated.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use codex_keyring_store::tests::MockKeyringStore;
    use codex_secrets::compute_keyring_account;
    use keyring::Error as KeyringError;
    use pretty_assertions::assert_eq;
    use std::sync::Arc;
    use tokio::time;

    use super::test_support::TempCodexHome;

    #[test]
    fn resolve_oauth_tokens_from_store_policy_uses_keyring_when_available() -> Result<()> {
        let _env = TempCodexHome::new();
        let store = MockKeyringStore::default();
        let tokens = sample_tokens();
        let expected = tokens.clone();
        let serialized = serde_json::to_string(&tokens)?;
        let key = super::compute_store_key(&tokens.server_name, &tokens.url)?;
        store.save(KEYRING_SERVICE, &key, &serialized)?;

        let resolved = super::resolve_oauth_tokens_from_store_policy(
            &store,
            &tokens.server_name,
            &tokens.url,
            OAuthCredentialsStoreMode::Auto,
            AuthKeyringBackendKind::Direct,
        )?
        .expect("tokens should load from keyring");
        assert_eq!(
            resolved.store,
            ResolvedOAuthCredentialStore::Keyring(AuthKeyringBackendKind::Direct)
        );
        assert_tokens_match_without_expiry(&resolved.tokens, &expected);
        Ok(())
    }

    #[test]
    fn load_oauth_tokens_falls_back_when_missing_in_keyring() -> Result<()> {
        let _env = TempCodexHome::new();
        let store = MockKeyringStore::default();
        let tokens = sample_tokens();
        let expected = tokens.clone();

        super::save_oauth_tokens_to_file(&tokens)?;

        let resolved = super::resolve_oauth_tokens_from_store_policy(
            &store,
            &tokens.server_name,
            &tokens.url,
            OAuthCredentialsStoreMode::Auto,
            AuthKeyringBackendKind::Direct,
        )?
        .expect("tokens should load from fallback");
        assert_eq!(resolved.store, ResolvedOAuthCredentialStore::File);
        assert_tokens_match_without_expiry(&resolved.tokens, &expected);
        Ok(())
    }

    #[test]
    fn load_oauth_tokens_falls_back_when_keyring_errors() -> Result<()> {
        let _env = TempCodexHome::new();
        let store = MockKeyringStore::default();
        let tokens = sample_tokens();
        let expected = tokens.clone();
        let key = super::compute_store_key(&tokens.server_name, &tokens.url)?;
        store.set_error(&key, KeyringError::Invalid("error".into(), "load".into()));

        super::save_oauth_tokens_to_file(&tokens)?;

        let resolved = super::resolve_oauth_tokens_from_store_policy(
            &store,
            &tokens.server_name,
            &tokens.url,
            OAuthCredentialsStoreMode::Auto,
            AuthKeyringBackendKind::Direct,
        )?
        .expect("tokens should load from fallback");
        assert_eq!(resolved.store, ResolvedOAuthCredentialStore::File);
        assert_tokens_match_without_expiry(&resolved.tokens, &expected);
        Ok(())
    }

    #[test]
    fn load_oauth_tokens_ignores_empty_fallback_file() -> Result<()> {
        let _env = TempCodexHome::new();
        let fallback_path = super::fallback_file_path()?;
        fs::write(&fallback_path, "   \n\t")?;

        let loaded = super::load_oauth_tokens_from_file("missing", "https://example.com")?;
        assert!(loaded.is_none());
        Ok(())
    }

    #[test]
    fn load_oauth_tokens_auto_ignores_corrupt_fallback_when_keyring_errors() -> Result<()> {
        let _env = TempCodexHome::new();
        let store = MockKeyringStore::default();
        let tokens = sample_tokens();
        let key = super::compute_store_key(&tokens.server_name, &tokens.url)?;
        store.set_error(&key, KeyringError::Invalid("error".into(), "load".into()));
        fs::write(super::fallback_file_path()?, "{not-json")?;

        let resolved = super::resolve_oauth_tokens_from_store_policy(
            &store,
            &tokens.server_name,
            &tokens.url,
            OAuthCredentialsStoreMode::Auto,
            AuthKeyringBackendKind::Direct,
        )?;
        assert!(resolved.is_none());
        Ok(())
    }

    #[test]
    fn exact_store_operations_do_not_adopt_or_mutate_the_other_store() -> Result<()> {
        let _env = TempCodexHome::new();
        let store = MockKeyringStore::default();
        let file_tokens = sample_tokens();
        let mut keyring_tokens = file_tokens.clone();
        keyring_tokens
            .token_response
            .0
            .set_access_token(AccessToken::new("keyring-access-token".to_string()));

        super::save_oauth_tokens_to_file(&file_tokens)?;
        let fallback_path = super::fallback_file_path()?;
        let fallback_before = fs::read(&fallback_path)?;
        super::save_oauth_tokens_with_keyring(
            &store,
            AuthKeyringBackendKind::Direct,
            &keyring_tokens.server_name,
            &keyring_tokens,
        )?;

        assert_eq!(fs::read(fallback_path)?, fallback_before);
        let loaded = ResolvedOAuthCredentialStore::Keyring(AuthKeyringBackendKind::Direct)
            .load(&store, &keyring_tokens.server_name, &keyring_tokens.url)?
            .expect("tokens should load from the selected keyring store");
        assert_tokens_match_without_expiry(&loaded, &keyring_tokens);
        Ok(())
    }

    #[test]
    fn save_oauth_tokens_prefers_keyring_when_available() -> Result<()> {
        let _env = TempCodexHome::new();
        let store = MockKeyringStore::default();
        let tokens = sample_tokens();
        let key = super::compute_store_key(&tokens.server_name, &tokens.url)?;

        super::save_oauth_tokens_to_file(&tokens)?;

        super::save_oauth_tokens_with_keyring_with_fallback_to_file(
            &store,
            AuthKeyringBackendKind::Direct,
            &tokens.server_name,
            &tokens,
        )?;

        let fallback_path = super::fallback_file_path()?;
        assert!(!fallback_path.exists(), "fallback file should be removed");
        let stored = store.saved_value(&key).expect("value saved to keyring");
        assert_eq!(serde_json::from_str::<StoredOAuthTokens>(&stored)?, tokens);
        Ok(())
    }

    #[test]
    fn save_oauth_tokens_writes_fallback_when_keyring_fails() -> Result<()> {
        let _env = TempCodexHome::new();
        let store = MockKeyringStore::default();
        let tokens = sample_tokens();
        let key = super::compute_store_key(&tokens.server_name, &tokens.url)?;
        store.set_error(&key, KeyringError::Invalid("error".into(), "save".into()));

        super::save_oauth_tokens_with_keyring_with_fallback_to_file(
            &store,
            AuthKeyringBackendKind::Direct,
            &tokens.server_name,
            &tokens,
        )?;

        let fallback_path = super::fallback_file_path()?;
        assert!(fallback_path.exists(), "fallback file should be created");
        let saved = super::read_fallback_file_unlocked()?.expect("fallback file should load");
        let key = super::compute_store_key(&tokens.server_name, &tokens.url)?;
        let entry = saved.get(&key).expect("entry for key");
        assert_eq!(entry.server_name, tokens.server_name);
        assert_eq!(entry.server_url, tokens.url);
        assert_eq!(entry.client_id, tokens.client_id);
        assert_eq!(
            entry.access_token,
            tokens.token_response.0.access_token().secret().as_str()
        );
        assert!(store.saved_value(&key).is_none());
        Ok(())
    }

    #[test]
    fn save_oauth_tokens_with_secrets_backend_writes_encrypted_storage() -> Result<()> {
        let env = TempCodexHome::new();
        let store = MockKeyringStore::default();
        let tokens = sample_tokens();
        let key = super::compute_store_key(&tokens.server_name, &tokens.url)?;
        let serialized = serde_json::to_string(&tokens)?;
        store.save(KEYRING_SERVICE, &key, &serialized)?;
        super::save_oauth_tokens_to_file(&tokens)?;

        super::save_oauth_tokens_with_keyring_with_fallback_to_file(
            &store,
            AuthKeyringBackendKind::Secrets,
            &tokens.server_name,
            &tokens,
        )?;

        let manager = SecretsManager::new_with_keyring_store_and_namespace(
            env.path().to_path_buf(),
            SecretsBackendKind::Local,
            Arc::new(store.clone()),
            LocalSecretsNamespace::McpOAuth,
        );
        let secret_name = super::compute_secret_name(&tokens.server_name, &tokens.url)?;
        let stored = manager
            .get(&SecretScope::Global, &secret_name)?
            .expect("tokens should be saved to encrypted storage");
        assert_eq!(serde_json::from_str::<StoredOAuthTokens>(&stored)?, tokens);
        assert_eq!(store.saved_value(&key), Some(serialized));
        assert!(env.path().join("secrets").join("mcp_oauth.age").exists());
        assert!(!env.path().join("secrets").join("local.age").exists());
        assert!(!super::fallback_file_path()?.exists());
        Ok(())
    }

    #[test]
    fn load_oauth_tokens_with_secrets_backend_reads_encrypted_storage() -> Result<()> {
        let _env = TempCodexHome::new();
        let store = MockKeyringStore::default();
        let tokens = sample_tokens();
        let expected = tokens.clone();

        super::save_oauth_tokens_with_keyring(
            &store,
            AuthKeyringBackendKind::Secrets,
            &tokens.server_name,
            &tokens,
        )?;

        let loaded = super::load_oauth_tokens_from_keyring(
            &store,
            AuthKeyringBackendKind::Secrets,
            &tokens.server_name,
            &tokens.url,
        )?
        .expect("tokens should load from encrypted storage");
        assert_tokens_match_without_expiry(&loaded, &expected);
        Ok(())
    }

    #[test]
    fn load_oauth_tokens_with_secrets_backend_ignores_direct_entry() -> Result<()> {
        let _env = TempCodexHome::new();
        let store = MockKeyringStore::default();
        let tokens = sample_tokens();
        let key = super::compute_store_key(&tokens.server_name, &tokens.url)?;
        let serialized = serde_json::to_string(&tokens)?;
        store.save(KEYRING_SERVICE, &key, &serialized)?;

        let loaded = super::load_oauth_tokens_from_keyring(
            &store,
            AuthKeyringBackendKind::Secrets,
            &tokens.server_name,
            &tokens.url,
        )?;

        assert!(loaded.is_none());
        Ok(())
    }

    #[test]
    fn save_oauth_tokens_with_secrets_backend_falls_back_to_file_when_keyring_fails() -> Result<()>
    {
        let env = TempCodexHome::new();
        let store = MockKeyringStore::default();
        store.set_error(
            &compute_keyring_account(env.path()),
            KeyringError::Invalid("error".into(), "save".into()),
        );
        let tokens = sample_tokens();

        super::save_oauth_tokens_with_keyring_with_fallback_to_file(
            &store,
            AuthKeyringBackendKind::Secrets,
            &tokens.server_name,
            &tokens,
        )?;

        let saved = super::read_fallback_file_unlocked()?.expect("fallback file should load");
        let key = super::compute_store_key(&tokens.server_name, &tokens.url)?;
        assert!(saved.contains_key(&key));
        Ok(())
    }

    #[test]
    fn delete_oauth_tokens_with_secrets_backend_removes_secrets_and_file() -> Result<()> {
        let env = TempCodexHome::new();
        let store = MockKeyringStore::default();
        let tokens = sample_tokens();
        let serialized = serde_json::to_string(&tokens)?;
        let key = super::compute_store_key(&tokens.server_name, &tokens.url)?;
        store.save(KEYRING_SERVICE, &key, &serialized)?;
        super::save_oauth_tokens_with_keyring(
            &store,
            AuthKeyringBackendKind::Secrets,
            &tokens.server_name,
            &tokens,
        )?;
        store.save(KEYRING_SERVICE, &key, &serialized)?;
        super::save_oauth_tokens_to_file(&tokens)?;

        let removed = super::delete_oauth_tokens_from_keyring_and_file(
            &store,
            OAuthCredentialsStoreMode::Auto,
            AuthKeyringBackendKind::Secrets,
            &tokens.server_name,
            &tokens.url,
        )?;

        let manager = SecretsManager::new_with_keyring_store_and_namespace(
            env.path().to_path_buf(),
            SecretsBackendKind::Local,
            Arc::new(store.clone()),
            LocalSecretsNamespace::McpOAuth,
        );
        let secret_name = super::compute_secret_name(&tokens.server_name, &tokens.url)?;
        assert!(removed);
        assert!(manager.get(&SecretScope::Global, &secret_name)?.is_none());
        assert!(store.saved_value(&key).is_none());
        assert!(!super::fallback_file_path()?.exists());
        Ok(())
    }

    #[test]
    fn delete_oauth_tokens_removes_all_storage() -> Result<()> {
        let _env = TempCodexHome::new();
        let store = MockKeyringStore::default();
        let tokens = sample_tokens();
        let serialized = serde_json::to_string(&tokens)?;
        let key = super::compute_store_key(&tokens.server_name, &tokens.url)?;
        store.save(KEYRING_SERVICE, &key, &serialized)?;
        super::save_oauth_tokens_to_file(&tokens)?;

        let removed = super::delete_oauth_tokens_from_keyring_and_file(
            &store,
            OAuthCredentialsStoreMode::Auto,
            AuthKeyringBackendKind::Direct,
            &tokens.server_name,
            &tokens.url,
        )?;
        assert!(removed);
        assert!(!store.contains(&key));
        assert!(!super::fallback_file_path()?.exists());
        Ok(())
    }

    #[test]
    fn delete_oauth_tokens_file_mode_removes_keyring_only_entry() -> Result<()> {
        let _env = TempCodexHome::new();
        let store = MockKeyringStore::default();
        let tokens = sample_tokens();
        let serialized = serde_json::to_string(&tokens)?;
        let key = super::compute_store_key(&tokens.server_name, &tokens.url)?;
        store.save(KEYRING_SERVICE, &key, &serialized)?;
        assert!(store.contains(&key));

        let removed = super::delete_oauth_tokens_from_keyring_and_file(
            &store,
            OAuthCredentialsStoreMode::Auto,
            AuthKeyringBackendKind::Direct,
            &tokens.server_name,
            &tokens.url,
        )?;
        assert!(removed);
        assert!(!store.contains(&key));
        assert!(!super::fallback_file_path()?.exists());
        Ok(())
    }

    #[test]
    fn delete_oauth_tokens_propagates_keyring_errors() -> Result<()> {
        let _env = TempCodexHome::new();
        let store = MockKeyringStore::default();
        let tokens = sample_tokens();
        let key = super::compute_store_key(&tokens.server_name, &tokens.url)?;
        store.set_error(&key, KeyringError::Invalid("error".into(), "delete".into()));
        super::save_oauth_tokens_to_file(&tokens).unwrap();

        let result = super::delete_oauth_tokens_from_keyring_and_file(
            &store,
            OAuthCredentialsStoreMode::Auto,
            AuthKeyringBackendKind::Direct,
            &tokens.server_name,
            &tokens.url,
        );
        assert!(result.is_err());
        assert!(super::fallback_file_path().unwrap().exists());
        Ok(())
    }

    #[test]
    fn refresh_expires_in_from_timestamp_restores_future_durations() {
        let mut tokens = sample_tokens();
        let expires_at = tokens.expires_at.expect("expires_at should be set");

        tokens.token_response.0.set_expires_in(None);
        super::refresh_expires_in_from_timestamp(&mut tokens);

        let actual = tokens
            .token_response
            .0
            .expires_in()
            .expect("expires_in should be restored")
            .as_secs();
        let expected = super::expires_in_from_timestamp(expires_at)
            .expect("expires_at should still be in the future");
        let diff = actual.abs_diff(expected);
        assert!(diff <= 1, "expires_in drift too large: diff={diff}");
    }

    #[test]
    fn refresh_expires_in_from_timestamp_marks_expired_tokens() {
        let mut tokens = sample_tokens();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0));
        let expired_at = now.as_millis() as u64;
        tokens.expires_at = Some(expired_at.saturating_sub(1000));

        let duration = Duration::from_secs(600);
        tokens.token_response.0.set_expires_in(Some(&duration));

        super::refresh_expires_in_from_timestamp(&mut tokens);

        assert_eq!(tokens.token_response.0.expires_in(), Some(Duration::ZERO));
    }

    #[test]
    fn durable_reconciliation_ignores_derived_expiry_drift() {
        let original = sample_tokens();
        let mut reread = original.clone();
        reread
            .token_response
            .0
            .set_expires_in(Some(&Duration::from_secs(1)));

        assert_ne!(original, reread);
        assert!(super::stored_credentials_match_for_reconciliation(
            Some(&original),
            Some(&reread)
        ));
    }

    #[test]
    fn oauth_tokens_are_usable_when_expiry_is_unknown() {
        let mut tokens = sample_tokens();
        tokens.expires_at = None;
        tokens.token_response.0.set_refresh_token(None);

        assert!(super::oauth_tokens_are_usable(&tokens));
    }

    #[test]
    fn oauth_tokens_are_usable_when_unexpired_without_refresh_token() {
        let mut tokens = sample_tokens();
        tokens.token_response.0.set_refresh_token(None);

        assert!(super::oauth_tokens_are_usable(&tokens));
    }

    #[test]
    fn oauth_tokens_are_usable_when_expired_but_refreshable() {
        let mut tokens = sample_tokens();
        tokens.expires_at = Some(0);

        assert!(super::oauth_tokens_are_usable(&tokens));
    }

    #[test]
    fn oauth_tokens_are_not_usable_when_expired_and_unrefreshable() {
        let mut tokens = sample_tokens();
        tokens.expires_at = Some(0);
        tokens.token_response.0.set_refresh_token(None);

        assert!(!super::oauth_tokens_are_usable(&tokens));
    }

    #[test]
    fn oauth_tokens_are_not_usable_when_near_expiry_and_unrefreshable() {
        let mut tokens = sample_tokens();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0))
            .as_millis() as u64;
        tokens.expires_at = Some(now.saturating_add(REFRESH_SKEW_MILLIS - 1));
        tokens.token_response.0.set_refresh_token(None);

        assert!(!super::oauth_tokens_are_usable(&tokens));
    }

    #[test]
    fn oauth_tokens_are_not_usable_when_client_id_is_blank() {
        let mut tokens = sample_tokens();
        tokens.client_id = " ".to_string();

        assert!(!super::oauth_tokens_are_usable(&tokens));
    }

    #[test]
    fn oauth_tokens_are_not_usable_when_access_token_is_blank() {
        let mut tokens = sample_tokens();
        tokens
            .token_response
            .0
            .set_access_token(AccessToken::new(" ".to_string()));

        assert!(!super::oauth_tokens_are_usable(&tokens));
    }

    #[test]
    fn oauth_tokens_are_not_usable_when_required_refresh_token_is_blank() {
        let mut tokens = sample_tokens();
        tokens.expires_at = Some(0);
        tokens
            .token_response
            .0
            .set_refresh_token(Some(RefreshToken::new(" ".to_string())));

        assert!(!super::oauth_tokens_are_usable(&tokens));
    }

    #[test]
    fn request_oauth_token_response_strips_refresh_material() {
        let tokens = sample_tokens();
        let request_tokens = super::request_oauth_token_response(&tokens);

        assert_eq!(
            request_tokens.access_token().secret(),
            tokens.token_response.0.access_token().secret()
        );
        assert!(request_tokens.refresh_token().is_none());
        assert!(request_tokens.expires_in().is_none());
        assert_eq!(request_tokens.scopes(), tokens.token_response.0.scopes());
    }

    #[test]
    fn refresh_credentials_do_not_expose_granted_scopes_to_rmcp() {
        let tokens = sample_tokens();

        let stored_credentials =
            super::stored_credentials_from_tokens(&tokens, super::CredentialExposure::Refresh);

        assert_eq!(stored_credentials.client_id, tokens.client_id);
        let staged_response = stored_credentials
            .token_response
            .as_ref()
            .expect("refresh credentials should stage the full token response");
        let expected_response = &tokens.token_response.0;
        assert_eq!(
            staged_response.access_token().secret(),
            expected_response.access_token().secret()
        );
        assert_eq!(
            staged_response.refresh_token().map(RefreshToken::secret),
            expected_response.refresh_token().map(RefreshToken::secret)
        );
        assert_eq!(
            staged_response.scopes().map(Vec::as_slice),
            expected_response.scopes().map(Vec::as_slice)
        );
        assert_eq!(stored_credentials.granted_scopes, Vec::<String>::new());
        assert!(stored_credentials.token_received_at.is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refresh_transaction_keeps_manager_credentials_on_keyring_reread_error() -> Result<()> {
        let _env = TempCodexHome::new();
        let store = MockKeyringStore::default();
        let mut tokens = sample_tokens();
        tokens.expires_at = Some(0);
        tokens
            .token_response
            .0
            .set_expires_in(Some(&Duration::ZERO));
        let key = super::compute_store_key(&tokens.server_name, &tokens.url)?;
        store.set_error(&key, KeyringError::Invalid("error".into(), "load".into()));

        let manager = authorization_manager_for(&tokens).await?;
        let persistor = OAuthPersistor::new(
            tokens.server_name.clone(),
            tokens.url.clone(),
            manager.clone(),
            ResolvedOAuthCredentialStore::Keyring(AuthKeyringBackendKind::Direct),
            Some(tokens.clone()),
        );
        let refresh_timeout = Duration::from_secs(1);

        let error = persistor
            .refresh_transaction_with_keyring_store(RefreshReason::Expiry, refresh_timeout, &store)
            .await
            .expect_err("keyring reread failure should abort refresh");

        assert!(
            format!("{error:#}")
                .contains("failed to reread OAuth tokens from resolved keyring storage"),
            "unexpected error: {error:#}"
        );
        drop(persistor);
        let manager = Arc::try_unwrap(manager)
            .unwrap_or_else(|_| panic!("persistor should be the only cloned OAuth manager owner"))
            .into_inner();
        let access_token = manager.get_access_token().await?;
        assert_eq!(
            access_token,
            tokens.token_response.0.access_token().secret().as_str()
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn locked_save_waits_for_refresh_transaction_lock() -> Result<()> {
        let _env = TempCodexHome::new();
        let tokens = sample_tokens();
        let key = super::compute_store_key(&tokens.server_name, &tokens.url)?;
        let lock = super::RefreshCredentialLock::acquire_for_server_with_timeout(
            &tokens.server_name,
            &tokens.url,
            Duration::from_secs(/*secs*/ 1),
        )
        .await?;

        let tokens_for_save = tokens.clone();
        let save = tokio::spawn(async move {
            super::save_oauth_tokens_locked(
                &tokens_for_save.server_name,
                &tokens_for_save,
                OAuthCredentialsStoreMode::File,
                AuthKeyringBackendKind::default(),
            )
            .await
        });

        time::sleep(Duration::from_millis(/*millis*/ 50)).await;
        assert!(
            !super::fallback_file_path()?.exists(),
            "login save should wait for the refresh transaction lock"
        );

        drop(lock);
        save.await??;

        let saved = super::read_fallback_file_unlocked()?.expect("fallback file should load");
        assert!(saved.contains_key(&key));
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn persist_if_needed_preserves_durable_fields_from_request_only_credentials() -> Result<()>
    {
        let _env = TempCodexHome::new();
        let tokens = sample_tokens();
        super::save_oauth_tokens_to_file(&tokens)?;
        let manager = authorization_manager_for(&tokens).await?;
        let persistor = OAuthPersistor::new(
            tokens.server_name.clone(),
            tokens.url.clone(),
            manager,
            ResolvedOAuthCredentialStore::File,
            Some(tokens.clone()),
        );

        persistor.persist_if_needed().await?;

        let stored = ResolvedOAuthCredentialStore::File
            .load(
                &DefaultKeyringStore,
                &tokens.server_name,
                &tokens.url,
            )?
            .expect("durable file credentials should remain present");
        assert_tokens_match_without_expiry(&stored, &tokens);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn persist_if_needed_waits_for_refresh_credential_lock() -> Result<()> {
        let _env = TempCodexHome::new();
        let tokens = sample_tokens();
        super::save_oauth_tokens_to_file(&tokens)?;
        let manager = authorization_manager_for(&tokens).await?;
        let persistor = OAuthPersistor::new(
            tokens.server_name.clone(),
            tokens.url.clone(),
            manager.clone(),
            ResolvedOAuthCredentialStore::File,
            Some(tokens.clone()),
        );
        let mut updated = tokens.clone();
        updated
            .token_response
            .0
            .set_access_token(AccessToken::new("persisted-access-token".to_string()));
        super::install_tokens_in_manager(&manager, &updated).await?;

        let lock = super::RefreshCredentialLock::acquire_for_server_with_timeout(
            &tokens.server_name,
            &tokens.url,
            Duration::from_secs(1),
        )
        .await?;
        let persist = tokio::spawn(async move { persistor.persist_if_needed().await });

        time::sleep(Duration::from_millis(50)).await;
        let stored = ResolvedOAuthCredentialStore::File
            .load(
                &DefaultKeyringStore,
                &tokens.server_name,
                &tokens.url,
            )?
            .expect("durable credentials should remain present while persistence waits");
        assert_eq!(
            stored.token_response.0.access_token().secret(),
            tokens.token_response.0.access_token().secret()
        );

        drop(lock);
        persist.await??;
        let stored = ResolvedOAuthCredentialStore::File
            .load(
                &DefaultKeyringStore,
                &tokens.server_name,
                &tokens.url,
            )?
            .expect("durable credentials should be updated after the lock is released");
        assert_eq!(
            stored.token_response.0.access_token().secret(),
            "persisted-access-token"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn persist_if_needed_preserves_newer_durable_credentials() -> Result<()> {
        let _env = TempCodexHome::new();
        let tokens = sample_tokens();
        super::save_oauth_tokens_to_file(&tokens)?;

        let save_manager = authorization_manager_for(&tokens).await?;
        let save_persistor = OAuthPersistor::new(
            tokens.server_name.clone(),
            tokens.url.clone(),
            save_manager.clone(),
            ResolvedOAuthCredentialStore::File,
            Some(tokens.clone()),
        );
        let mut stale_save = tokens.clone();
        stale_save
            .token_response
            .0
            .set_access_token(AccessToken::new("stale-save-token".to_string()));
        super::install_tokens_in_manager(&save_manager, &stale_save).await?;

        let delete_manager = authorization_manager_for(&tokens).await?;
        let delete_persistor = OAuthPersistor::new(
            tokens.server_name.clone(),
            tokens.url.clone(),
            delete_manager.clone(),
            ResolvedOAuthCredentialStore::File,
            Some(tokens.clone()),
        );
        delete_persistor.clear_manager_credentials().await;

        let mut newer = tokens.clone();
        newer
            .token_response
            .0
            .set_access_token(AccessToken::new("newer-durable-token".to_string()));
        super::save_oauth_tokens_to_file(&newer)?;

        save_persistor.persist_if_needed().await?;
        delete_persistor.persist_if_needed().await?;

        let stored = ResolvedOAuthCredentialStore::File
            .load(
                &DefaultKeyringStore,
                &tokens.server_name,
                &tokens.url,
            )?
            .expect("newer durable credentials must not be overwritten or deleted");
        assert_eq!(
            stored.token_response.0.access_token().secret(),
            "newer-durable-token"
        );
        for manager in [&save_manager, &delete_manager] {
            let guard = manager.lock().await;
            let (_client_id, credentials) = guard.get_credentials().await?;
            assert_eq!(
                credentials
                    .as_ref()
                    .map(|credentials| credentials.access_token().secret()),
                Some("newer-durable-token")
            );
        }
        Ok(())
    }

    async fn authorization_manager_for(
        tokens: &StoredOAuthTokens,
    ) -> Result<Arc<Mutex<AuthorizationManager>>> {
        let manager = Arc::new(Mutex::new(AuthorizationManager::new(&tokens.url).await?));
        let store = InMemoryCredentialStore::new();
        store
            .save(super::stored_credentials_from_tokens(
                tokens,
                CredentialExposure::Request,
            ))
            .await?;
        {
            let mut guard = manager.lock().await;
            guard.set_credential_store(store);
            guard.initialize_from_store().await?;
        }
        Ok(manager)
    }

    fn assert_tokens_match_without_expiry(
        actual: &StoredOAuthTokens,
        expected: &StoredOAuthTokens,
    ) {
        assert_eq!(actual.server_name, expected.server_name);
        assert_eq!(actual.url, expected.url);
        assert_eq!(actual.client_id, expected.client_id);
        assert_eq!(actual.expires_at, expected.expires_at);
        assert_token_response_match_without_expiry(
            &actual.token_response,
            &expected.token_response,
        );
    }

    fn assert_token_response_match_without_expiry(
        actual: &WrappedOAuthTokenResponse,
        expected: &WrappedOAuthTokenResponse,
    ) {
        let actual_response = &actual.0;
        let expected_response = &expected.0;

        assert_eq!(
            actual_response.access_token().secret(),
            expected_response.access_token().secret()
        );
        assert_eq!(actual_response.token_type(), expected_response.token_type());
        assert_eq!(
            actual_response.refresh_token().map(RefreshToken::secret),
            expected_response.refresh_token().map(RefreshToken::secret),
        );
        assert_eq!(actual_response.scopes(), expected_response.scopes());
        assert_eq!(
            actual_response.extra_fields().0,
            expected_response.extra_fields().0
        );
        assert_eq!(
            actual_response.expires_in().is_some(),
            expected_response.expires_in().is_some()
        );
    }

    fn sample_tokens() -> StoredOAuthTokens {
        let mut response = OAuthTokenResponse::new(
            AccessToken::new("access-token".to_string()),
            BasicTokenType::Bearer,
            VendorExtraTokenFields::default(),
        );
        response.set_refresh_token(Some(RefreshToken::new("refresh-token".to_string())));
        response.set_scopes(Some(vec![
            Scope::new("scope-a".to_string()),
            Scope::new("scope-b".to_string()),
        ]));
        let expires_in = Duration::from_secs(3600);
        response.set_expires_in(Some(&expires_in));
        let expires_at = super::compute_expires_at_millis(&response);

        StoredOAuthTokens {
            server_name: "test-server".to_string(),
            url: "https://example.test".to_string(),
            client_id: "client-id".to_string(),
            token_response: WrappedOAuthTokenResponse(response),
            expires_at,
        }
    }
}
