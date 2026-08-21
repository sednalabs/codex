use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::fmt::Debug;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use tracing::warn;

use super::BedrockApiKeyAuth;
use crate::token_data::TokenData;
use codex_agent_identity::AgentIdentityJwtClaims;
use codex_agent_identity::decode_agent_identity_jwt;
use codex_config::types::AuthCredentialsStoreMode;
pub use codex_config::types::AuthKeyringBackendKind;
use codex_keyring_store::DefaultKeyringStore;
use codex_keyring_store::KeyringStore;
use codex_protocol::account::PlanType as AccountPlanType;
use codex_protocol::auth::AuthMode;
use codex_secrets::LocalSecretsNamespace;
use codex_secrets::SecretName;
use codex_secrets::SecretScope;
use codex_secrets::SecretsBackendKind;
use codex_secrets::SecretsManager;
use once_cell::sync::Lazy;

/// Expected structure for $CODEX_HOME/auth.json.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct AuthDotJson {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<AuthMode>,

    #[serde(rename = "OPENAI_API_KEY")]
    pub openai_api_key: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenData>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<DateTime<Utc>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_identity: Option<AgentIdentityStorage>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub personal_access_token: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bedrock_api_key: Option<BedrockApiKeyAuth>,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(untagged)]
pub enum AgentIdentityStorage {
    Jwt(String),
    Record(AgentIdentityAuthRecord),
}

impl AgentIdentityStorage {
    pub fn has_auth_material(&self) -> bool {
        match self {
            Self::Jwt(jwt) => !jwt.trim().is_empty(),
            Self::Record(record) => {
                !record.agent_runtime_id.trim().is_empty()
                    && !record.agent_private_key.trim().is_empty()
            }
        }
    }

    pub(crate) fn as_record(&self) -> Option<&AgentIdentityAuthRecord> {
        match self {
            Self::Jwt(_) => None,
            Self::Record(record) => Some(record),
        }
    }

    pub(crate) fn matches_record(&self, record: &AgentIdentityAuthRecord) -> bool {
        match self {
            Self::Jwt(jwt) => AgentIdentityAuthRecord::from_agent_identity_jwt(jwt)
                .ok()
                .is_some_and(|stored| stored.same_credential(record)),
            Self::Record(stored) => stored.same_credential(record),
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct AgentIdentityAuthRecord {
    pub agent_runtime_id: String,
    pub agent_private_key: String,
    pub account_id: String,
    pub chatgpt_user_id: String,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_non_empty_string",
        serialize_with = "serialize_optional_string_as_empty"
    )]
    pub email: Option<String>,
    pub plan_type: AccountPlanType,
    pub chatgpt_account_is_fedramp: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
}

fn deserialize_optional_non_empty_string<'de, D>(
    deserializer: D,
) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(|value| value.filter(|value| !value.is_empty()))
}

fn serialize_optional_string_as_empty<S>(
    value: &Option<String>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    value.as_deref().unwrap_or_default().serialize(serializer)
}

impl AgentIdentityAuthRecord {
    pub(crate) fn from_agent_identity_jwt(jwt: &str) -> std::io::Result<Self> {
        let claims =
            decode_agent_identity_jwt(jwt, /*jwks*/ None).map_err(std::io::Error::other)?;

        Ok(claims.into())
    }

    pub(crate) fn same_credential(&self, other: &Self) -> bool {
        self.agent_runtime_id == other.agent_runtime_id
            && self.agent_private_key == other.agent_private_key
            && self.account_id == other.account_id
            && self.chatgpt_user_id == other.chatgpt_user_id
    }
}

impl From<AgentIdentityJwtClaims> for AgentIdentityAuthRecord {
    fn from(claims: AgentIdentityJwtClaims) -> Self {
        Self {
            agent_runtime_id: claims.agent_runtime_id,
            agent_private_key: claims.agent_private_key,
            account_id: claims.account_id,
            chatgpt_user_id: claims.chatgpt_user_id,
            email: claims.email,
            plan_type: claims.plan_type.into(),
            chatgpt_account_is_fedramp: claims.chatgpt_account_is_fedramp,
            task_id: None,
        }
    }
}

pub(super) fn get_auth_file(codex_home: &Path) -> PathBuf {
    codex_home.join("auth.json")
}

pub(super) fn delete_file_if_exists(codex_home: &Path) -> std::io::Result<bool> {
    let auth_file = get_auth_file(codex_home);
    match std::fs::remove_file(&auth_file) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

pub(super) trait AuthStorageBackend: Debug + Send + Sync {
    /// This is a physical store operation. Implementations must not consult or
    /// mutate another auth representation.
    fn load(&self) -> std::io::Result<Option<AuthDotJson>>;
    /// This is a physical store operation. Policy belongs to AuthRepository.
    fn save(&self, auth: &AuthDotJson) -> std::io::Result<()>;
    /// This is a physical store operation. Policy belongs to AuthRepository.
    fn delete(&self) -> std::io::Result<bool>;
}

/// The exact durable representation selected by a repository read.
///
/// `auth` is deliberately part of this snapshot: write-side callers must use
/// it as a compare-and-update precondition rather than resolve Auto again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AuthStorageSource {
    File,
    DirectKeyring,
    SecretsKeyring,
    Ephemeral,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct LoadedAuth {
    pub(super) source: AuthStorageSource,
    pub(super) auth: AuthDotJson,
}

/// Coordinates configured auth policy over strictly source-local stores.
///
/// The lock is intentionally held across policy selection and compare/update
/// operations. Individual stores never use it and never perform cross-store
/// cleanup, which keeps a captured `LoadedAuth` a real authority boundary.
#[derive(Clone, Debug)]
pub(super) struct AuthRepository {
    mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    file: Arc<FileAuthStorage>,
    direct: Arc<DirectKeyringAuthStorage>,
    secrets: Arc<SecretsKeyringAuthStorage>,
    ephemeral: Arc<EphemeralAuthStorage>,
    lock: Arc<Mutex<()>>,
}

static AUTH_REPOSITORY_LOCK: Lazy<Arc<Mutex<()>>> = Lazy::new(|| Arc::new(Mutex::new(())));

impl AuthRepository {
    fn new(
        codex_home: PathBuf,
        mode: AuthCredentialsStoreMode,
        keyring_store: Arc<dyn KeyringStore>,
        keyring_backend_kind: AuthKeyringBackendKind,
    ) -> Self {
        Self {
            mode,
            keyring_backend_kind,
            file: Arc::new(FileAuthStorage::new(codex_home.clone())),
            direct: Arc::new(DirectKeyringAuthStorage::new(
                codex_home.clone(),
                Arc::clone(&keyring_store),
            )),
            secrets: Arc::new(SecretsKeyringAuthStorage::new(
                codex_home.clone(),
                keyring_store,
            )),
            ephemeral: Arc::new(EphemeralAuthStorage::new(codex_home)),
            lock: Arc::clone(&AUTH_REPOSITORY_LOCK),
        }
    }

    fn selected_keyring_source(&self) -> AuthStorageSource {
        match self.keyring_backend_kind {
            AuthKeyringBackendKind::Direct => AuthStorageSource::DirectKeyring,
            AuthKeyringBackendKind::Secrets => AuthStorageSource::SecretsKeyring,
        }
    }

    fn storage(&self, source: AuthStorageSource) -> &dyn AuthStorageBackend {
        match source {
            AuthStorageSource::File => self.file.as_ref(),
            AuthStorageSource::DirectKeyring => self.direct.as_ref(),
            AuthStorageSource::SecretsKeyring => self.secrets.as_ref(),
            AuthStorageSource::Ephemeral => self.ephemeral.as_ref(),
        }
    }

    fn load_active_locked(&self) -> std::io::Result<Option<LoadedAuth>> {
        let source = match self.mode {
            AuthCredentialsStoreMode::File => AuthStorageSource::File,
            AuthCredentialsStoreMode::Keyring => self.selected_keyring_source(),
            AuthCredentialsStoreMode::Ephemeral => AuthStorageSource::Ephemeral,
            AuthCredentialsStoreMode::Auto => {
                let source = self.selected_keyring_source();
                // A keyring read error is not evidence that no credential is
                // present, so Auto must not silently select a shadow file.
                return match self.storage(source).load()? {
                    Some(auth) => Ok(Some(LoadedAuth { source, auth })),
                    None => self.storage(AuthStorageSource::File).load().map(|auth| {
                        auth.map(|auth| LoadedAuth {
                            source: AuthStorageSource::File,
                            auth,
                        })
                    }),
                };
            }
        };
        self.storage(source)
            .load()
            .map(|auth| auth.map(|auth| LoadedAuth { source, auth }))
    }

    pub(super) fn load_active(&self) -> std::io::Result<Option<LoadedAuth>> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| std::io::Error::other("failed to lock auth repository"))?;
        self.load_active_locked()
    }

    pub(super) fn replace_for_login(&self, auth: &AuthDotJson) -> std::io::Result<LoadedAuth> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| std::io::Error::other("failed to lock auth repository"))?;
        let source = match self.mode {
            AuthCredentialsStoreMode::File => AuthStorageSource::File,
            AuthCredentialsStoreMode::Keyring => self.selected_keyring_source(),
            AuthCredentialsStoreMode::Ephemeral => AuthStorageSource::Ephemeral,
            AuthCredentialsStoreMode::Auto => {
                let keyring_source = self.selected_keyring_source();
                // Fallback is safe only after the same locked observation
                // positively established that the keyring is empty.
                match self.storage(keyring_source).load()? {
                    Some(_) => {
                        self.storage(keyring_source).save(auth)?;
                        keyring_source
                    }
                    None => match self.storage(keyring_source).save(auth) {
                        Ok(()) => keyring_source,
                        Err(err) => {
                            warn!("failed to save auth to empty keyring; using file store: {err}");
                            self.storage(AuthStorageSource::File).save(auth)?;
                            AuthStorageSource::File
                        }
                    },
                }
            }
        };
        let loaded = self
            .load_active_locked()?
            .ok_or_else(|| std::io::Error::other("auth disappeared after save"))?;
        if loaded.auth != *auth {
            return Err(std::io::Error::other(
                "auth storage selected a different credential after save",
            ));
        }
        Ok(LoadedAuth {
            source,
            auth: loaded.auth,
        })
    }

    pub(super) fn update_if_unchanged(
        &self,
        expected: &LoadedAuth,
        updated: &AuthDotJson,
    ) -> std::io::Result<LoadedAuth> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| std::io::Error::other("failed to lock auth repository"))?;
        if self.storage(expected.source).load()?.as_ref() != Some(&expected.auth) {
            return Err(std::io::Error::other("auth changed before update"));
        }
        self.storage(expected.source).save(updated)?;
        Ok(LoadedAuth {
            source: expected.source,
            auth: updated.clone(),
        })
    }

    pub(super) fn delete_if_matches(&self, expected: &LoadedAuth) -> std::io::Result<bool> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| std::io::Error::other("failed to lock auth repository"))?;
        if self.storage(expected.source).load()?.as_ref() != Some(&expected.auth) {
            return Ok(false);
        }
        self.storage(expected.source).delete()
    }

    /// User-initiated logout is deliberately independent of the configured
    /// policy. All stores are attempted even after one fails.
    pub(super) fn logout_all(&self) -> std::io::Result<bool> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| std::io::Error::other("failed to lock auth repository"))?;
        let mut removed = false;
        let mut failures = Vec::new();
        for source in [
            AuthStorageSource::Ephemeral,
            AuthStorageSource::File,
            AuthStorageSource::DirectKeyring,
            AuthStorageSource::SecretsKeyring,
        ] {
            match self.storage(source).delete() {
                Ok(value) => removed |= value,
                Err(err) => failures.push(format!("{source:?}: {err}")),
            }
        }
        if failures.is_empty() {
            Ok(removed)
        } else {
            Err(std::io::Error::other(format!(
                "partial auth logout; attempted all stores; failures: {}",
                failures.join("; ")
            )))
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct FileAuthStorage {
    codex_home: PathBuf,
}

impl FileAuthStorage {
    pub(super) fn new(codex_home: PathBuf) -> Self {
        Self { codex_home }
    }

    /// Attempt to read and parse the `auth.json` file in the given `CODEX_HOME` directory.
    /// Returns the full AuthDotJson structure.
    pub(super) fn try_read_auth_json(&self, auth_file: &Path) -> std::io::Result<AuthDotJson> {
        let mut file = File::open(auth_file)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        let auth_dot_json: AuthDotJson = serde_json::from_str(&contents)?;

        Ok(auth_dot_json)
    }
}

impl AuthStorageBackend for FileAuthStorage {
    fn load(&self) -> std::io::Result<Option<AuthDotJson>> {
        let auth_file = get_auth_file(&self.codex_home);
        let auth_dot_json = match self.try_read_auth_json(&auth_file) {
            Ok(auth) => auth,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        Ok(Some(auth_dot_json))
    }

    fn save(&self, auth_dot_json: &AuthDotJson) -> std::io::Result<()> {
        let auth_file = get_auth_file(&self.codex_home);

        if let Some(parent) = auth_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json_data = serde_json::to_string_pretty(auth_dot_json)?;
        let mut options = OpenOptions::new();
        options.truncate(true).write(true).create(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
        }
        let mut file = options.open(auth_file)?;
        file.write_all(json_data.as_bytes())?;
        file.flush()?;
        Ok(())
    }

    fn delete(&self) -> std::io::Result<bool> {
        delete_file_if_exists(&self.codex_home)
    }
}

static CODEX_AUTH_SECRET_NAME: Lazy<SecretName> =
    Lazy::new(|| match SecretName::new("CODEX_AUTH") {
        Ok(name) => name,
        Err(err) => unreachable!("CODEX_AUTH should be a valid secret name: {err}"),
    });
const KEYRING_SERVICE: &str = "Codex Auth";

// turns codex_home path into a stable, short key string
fn compute_store_key(codex_home: &Path) -> std::io::Result<String> {
    let canonical = codex_home
        .canonicalize()
        .unwrap_or_else(|_| codex_home.to_path_buf());
    let path_str = canonical.to_string_lossy();
    let mut hasher = Sha256::new();
    hasher.update(path_str.as_bytes());
    let digest = hasher.finalize();
    let hex = digest_hex(digest);
    let truncated = hex.get(..16).unwrap_or(&hex);
    Ok(format!("cli|{truncated}"))
}

fn digest_hex(digest: impl AsRef<[u8]>) -> String {
    use std::fmt::Write as _;

    let digest = digest.as_ref();
    let mut hex = String::with_capacity(digest.len() * 2);
    for &byte in digest {
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

#[derive(Clone, Debug)]
struct DirectKeyringAuthStorage {
    codex_home: PathBuf,
    keyring_store: Arc<dyn KeyringStore>,
}

impl DirectKeyringAuthStorage {
    fn new(codex_home: PathBuf, keyring_store: Arc<dyn KeyringStore>) -> Self {
        Self {
            codex_home,
            keyring_store,
        }
    }

    fn load_from_keyring(&self, key: &str) -> std::io::Result<Option<AuthDotJson>> {
        match self.keyring_store.load(KEYRING_SERVICE, key) {
            Ok(Some(serialized)) => serde_json::from_str(&serialized).map(Some).map_err(|err| {
                std::io::Error::other(format!(
                    "failed to deserialize CLI auth from keyring: {err}"
                ))
            }),
            Ok(None) => Ok(None),
            Err(error) => Err(std::io::Error::other(format!(
                "failed to load CLI auth from keyring: {}",
                error.message()
            ))),
        }
    }

    fn save_to_keyring(&self, key: &str, value: &str) -> std::io::Result<()> {
        match self.keyring_store.save(KEYRING_SERVICE, key, value) {
            Ok(()) => Ok(()),
            Err(error) => {
                let message = format!(
                    "failed to write OAuth tokens to keyring: {}",
                    error.message()
                );
                warn!("{message}");
                Err(std::io::Error::other(message))
            }
        }
    }
}

impl AuthStorageBackend for DirectKeyringAuthStorage {
    fn load(&self) -> std::io::Result<Option<AuthDotJson>> {
        let key = compute_store_key(&self.codex_home)?;
        self.load_from_keyring(&key)
    }

    fn save(&self, auth: &AuthDotJson) -> std::io::Result<()> {
        let key = compute_store_key(&self.codex_home)?;
        // Simpler error mapping per style: prefer method reference over closure
        let serialized = serde_json::to_string(auth).map_err(std::io::Error::other)?;
        self.save_to_keyring(&key, &serialized)?;
        Ok(())
    }

    fn delete(&self) -> std::io::Result<bool> {
        let key = compute_store_key(&self.codex_home)?;
        self.keyring_store
            .delete(KEYRING_SERVICE, &key)
            .map_err(|err| {
                std::io::Error::other(format!("failed to delete auth from keyring: {err}"))
            })
    }
}

#[derive(Clone)]
struct SecretsKeyringAuthStorage {
    codex_home: PathBuf,
    secrets_manager: SecretsManager,
}

impl Debug for SecretsKeyringAuthStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretsKeyringAuthStorage")
            .field("codex_home", &self.codex_home)
            .finish_non_exhaustive()
    }
}

impl SecretsKeyringAuthStorage {
    fn new(codex_home: PathBuf, keyring_store: Arc<dyn KeyringStore>) -> Self {
        let secrets_manager = SecretsManager::new_with_keyring_store_and_namespace(
            codex_home.clone(),
            SecretsBackendKind::Local,
            keyring_store,
            LocalSecretsNamespace::CodexAuth,
        );
        Self {
            codex_home,
            secrets_manager,
        }
    }
}

impl AuthStorageBackend for SecretsKeyringAuthStorage {
    fn load(&self) -> std::io::Result<Option<AuthDotJson>> {
        match self
            .secrets_manager
            .get(&SecretScope::Global, &CODEX_AUTH_SECRET_NAME)
            .map_err(|err| {
                std::io::Error::other(format!(
                    "failed to load CLI auth from encrypted auth storage: {err}"
                ))
            })? {
            Some(serialized) => serde_json::from_str(&serialized).map(Some).map_err(|err| {
                std::io::Error::other(format!(
                    "failed to deserialize CLI auth from encrypted auth storage: {err}"
                ))
            }),
            None => Ok(None),
        }
    }

    fn save(&self, auth: &AuthDotJson) -> std::io::Result<()> {
        let serialized = serde_json::to_string(auth).map_err(std::io::Error::other)?;
        self.secrets_manager
            .set(&SecretScope::Global, &CODEX_AUTH_SECRET_NAME, &serialized)
            .map_err(|err| {
                let message =
                    format!("failed to write OAuth tokens to encrypted auth storage: {err}");
                warn!("{message}");
                std::io::Error::other(message)
            })?;
        Ok(())
    }

    fn delete(&self) -> std::io::Result<bool> {
        self.secrets_manager
            .delete(&SecretScope::Global, &CODEX_AUTH_SECRET_NAME)
            .map_err(|err| {
                std::io::Error::other(format!(
                    "failed to delete auth from encrypted auth storage: {err}"
                ))
            })
    }
}

// A global in-memory store for mapping codex_home -> AuthDotJson.
static EPHEMERAL_AUTH_STORE: Lazy<Mutex<HashMap<String, AuthDotJson>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Debug)]
struct EphemeralAuthStorage {
    codex_home: PathBuf,
}

impl EphemeralAuthStorage {
    fn new(codex_home: PathBuf) -> Self {
        Self { codex_home }
    }

    fn with_store<F, T>(&self, action: F) -> std::io::Result<T>
    where
        F: FnOnce(&mut HashMap<String, AuthDotJson>, String) -> std::io::Result<T>,
    {
        let key = compute_store_key(&self.codex_home)?;
        let mut store = EPHEMERAL_AUTH_STORE
            .lock()
            .map_err(|_| std::io::Error::other("failed to lock ephemeral auth storage"))?;
        action(&mut store, key)
    }
}

impl AuthStorageBackend for EphemeralAuthStorage {
    fn load(&self) -> std::io::Result<Option<AuthDotJson>> {
        self.with_store(|store, key| Ok(store.get(&key).cloned()))
    }

    fn save(&self, auth: &AuthDotJson) -> std::io::Result<()> {
        self.with_store(|store, key| {
            store.insert(key, auth.clone());
            Ok(())
        })
    }

    fn delete(&self) -> std::io::Result<bool> {
        self.with_store(|store, key| Ok(store.remove(&key).is_some()))
    }
}

pub(super) fn create_auth_storage(
    codex_home: PathBuf,
    mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Arc<dyn AuthStorageBackend> {
    let keyring_store: Arc<dyn KeyringStore> = Arc::new(DefaultKeyringStore);
    create_auth_storage_with_store(codex_home, mode, keyring_store, keyring_backend_kind)
}

pub(super) fn create_auth_repository(
    codex_home: PathBuf,
    mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Arc<AuthRepository> {
    let keyring_store: Arc<dyn KeyringStore> = Arc::new(DefaultKeyringStore);
    Arc::new(create_auth_repository_with_store(
        codex_home,
        mode,
        keyring_store,
        keyring_backend_kind,
    ))
}

fn create_auth_repository_with_store(
    codex_home: PathBuf,
    mode: AuthCredentialsStoreMode,
    keyring_store: Arc<dyn KeyringStore>,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> AuthRepository {
    AuthRepository::new(codex_home, mode, keyring_store, keyring_backend_kind)
}

fn create_auth_storage_with_store(
    codex_home: PathBuf,
    mode: AuthCredentialsStoreMode,
    keyring_store: Arc<dyn KeyringStore>,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Arc<dyn AuthStorageBackend> {
    match mode {
        AuthCredentialsStoreMode::File => Arc::new(FileAuthStorage::new(codex_home)),
        AuthCredentialsStoreMode::Keyring => {
            create_keyring_auth_storage(codex_home, keyring_store, keyring_backend_kind)
        }
        AuthCredentialsStoreMode::Auto => {
            unreachable!("Auto policy must be accessed through AuthRepository")
        }
        AuthCredentialsStoreMode::Ephemeral => Arc::new(EphemeralAuthStorage::new(codex_home)),
    }
}

fn create_keyring_auth_storage(
    codex_home: PathBuf,
    keyring_store: Arc<dyn KeyringStore>,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Arc<dyn AuthStorageBackend> {
    match keyring_backend_kind {
        AuthKeyringBackendKind::Direct => {
            Arc::new(DirectKeyringAuthStorage::new(codex_home, keyring_store))
        }
        AuthKeyringBackendKind::Secrets => {
            Arc::new(SecretsKeyringAuthStorage::new(codex_home, keyring_store))
        }
    }
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
