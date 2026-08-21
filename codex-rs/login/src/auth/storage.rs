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
use std::os::unix::fs::DirBuilderExt;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
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

    /// Compare persisted identity material with a live identity without treating an
    /// undecodable JWT as an authorization to delete it. JWTs are intentionally
    /// normalized through the same claims-to-record conversion used at load time.
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

    /// Task registration is deliberately excluded: it is runtime state, not the
    /// credential that authorizes an agent identity. The remaining fields bind the
    /// private key to the issued account identity.
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
    fn codex_home(&self) -> &Path;
    fn load_unlocked(&self) -> std::io::Result<Option<AuthDotJson>>;
    fn save_unlocked(&self, auth: &AuthDotJson) -> std::io::Result<()>;
    fn delete_unlocked(&self) -> std::io::Result<bool>;

    fn load(&self) -> std::io::Result<Option<AuthDotJson>> {
        let _guard = AUTH_STORAGE_TRANSACTION_LOCK
            .lock()
            .map_err(|_| std::io::Error::other("failed to lock auth storage"))?;
        let _file_guard = lock_auth_storage(self.codex_home())?;
        self.load_unlocked()
    }

    fn save(&self, auth: &AuthDotJson) -> std::io::Result<()> {
        let _guard = AUTH_STORAGE_TRANSACTION_LOCK
            .lock()
            .map_err(|_| std::io::Error::other("failed to lock auth storage"))?;
        let _file_guard = lock_auth_storage(self.codex_home())?;
        self.save_unlocked(auth)
    }

    fn delete(&self) -> std::io::Result<bool> {
        let _guard = AUTH_STORAGE_TRANSACTION_LOCK
            .lock()
            .map_err(|_| std::io::Error::other("failed to lock auth storage"))?;
        let _file_guard = lock_auth_storage(self.codex_home())?;
        self.delete_unlocked()
    }

    fn delete_if(&self, should_delete: &dyn Fn(&AuthDotJson) -> bool) -> std::io::Result<bool> {
        let _guard = AUTH_STORAGE_TRANSACTION_LOCK
            .lock()
            .map_err(|_| std::io::Error::other("failed to lock auth storage"))?;
        let _file_guard = lock_auth_storage(self.codex_home())?;
        let Some(auth) = self.load_unlocked()? else {
            return Ok(false);
        };
        if !should_delete(&auth) {
            return Ok(false);
        }
        self.delete_unlocked()
    }
}

// Auth backends do not all expose compare-and-delete primitives. Serialize storage
// transactions both in-process and across cooperating Codex processes so rejection
// cleanup cannot erase a credential saved after the value comparison.
static AUTH_STORAGE_TRANSACTION_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

fn lock_auth_storage(codex_home: &Path) -> std::io::Result<File> {
    // The lock root must not follow TMPDIR; cooperating Codex processes may have
    // different temporary-directory environments while sharing one CODEX_HOME.
    #[cfg(unix)]
    let lock_dir = lock_root_for_uid(
        &private_lock_anchor(codex_home)?,
        // Keep the lock namespace private to the effective user. A different
        // unprivileged user must not be able to pre-create our shared root and
        // deny auth operations before ownership validation runs.
        unsafe { libc::geteuid() as u64 },
    );
    #[cfg(not(unix))]
    let lock_dir = std::env::temp_dir().join("codex-auth-locks");
    lock_auth_storage_at(codex_home, &lock_dir)
}

fn lock_root_for_uid(base: &Path, uid: u64) -> PathBuf {
    base.join(format!("codex-auth-locks-{uid}"))
}

#[cfg(unix)]
fn private_lock_anchor(codex_home: &Path) -> std::io::Result<PathBuf> {
    let absolute = std::path::absolute(codex_home)?;
    let mut anchor = absolute.parent().unwrap_or(absolute.as_path());
    while !anchor.exists() {
        let Some(parent) = anchor.parent() else {
            break;
        };
        anchor = parent;
    }
    if let Ok(private_anchor) = validate_private_lock_anchor(anchor) {
        return Ok(private_anchor);
    }

    if let Some(home) = dirs::home_dir()
        && let Ok(private_anchor) = validate_private_lock_anchor(&home)
    {
        return Ok(private_anchor);
    }
    validate_private_lock_anchor(anchor)
}

#[cfg(unix)]
fn validate_private_lock_anchor(anchor: &Path) -> std::io::Result<PathBuf> {
    let canonical_anchor = std::fs::canonicalize(anchor)?;
    let mut current = canonical_anchor.as_path();
    loop {
        let metadata = std::fs::symlink_metadata(current)?;
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            return Err(std::io::Error::other(format!(
                "auth lock anchor is not a directory: {}",
                current.display()
            )));
        }
        if metadata.permissions().mode() & 0o022 != 0 {
            return Err(std::io::Error::other(format!(
                "auth lock anchor has an unsafe writable parent: {}",
                current.display()
            )));
        }
        if current == Path::new("/") {
            break;
        }
        current = current.parent().ok_or_else(|| {
            std::io::Error::other(format!(
                "auth lock anchor has no canonical parent: {}",
                canonical_anchor.display()
            ))
        })?;
    }

    let metadata = std::fs::symlink_metadata(&canonical_anchor)?;
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::other(format!(
            "auth lock anchor is not owned by the current user: {}",
            canonical_anchor.display()
        )));
    }
    Ok(canonical_anchor)
}

fn lock_auth_storage_at(codex_home: &Path, lock_dir: &Path) -> std::io::Result<File> {
    let canonical_identity = canonical_storage_identity(codex_home)?;
    let mut hasher = Sha256::new();
    hasher.update(canonical_identity.to_string_lossy().as_bytes());
    let lock_key = digest_hex(hasher.finalize());
    ensure_secure_lock_dir(&lock_dir)?;
    let lock_path = lock_dir.join(lock_key);
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
        // The lock root is private, but also refuse a final-component symlink if
        // an attacker manages to replace a lock file between metadata checks.
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let lock_file = options.open(&lock_path)?;
    validate_lock_file(&lock_file, &lock_path)?;
    lock_file.lock()?;
    Ok(lock_file)
}

fn ensure_secure_lock_dir(lock_dir: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(lock_dir) {
        Ok(metadata) => {
            validate_lock_dir(&metadata, lock_dir)?;
            #[cfg(unix)]
            if metadata.permissions().mode() & 0o077 != 0 {
                let mut permissions = metadata.permissions();
                permissions.set_mode(0o700);
                std::fs::set_permissions(lock_dir, permissions)?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            builder.mode(0o700);
            match builder.create(lock_dir) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            let metadata = std::fs::symlink_metadata(lock_dir)?;
            validate_lock_dir(&metadata, lock_dir)?;
            #[cfg(unix)]
            if metadata.permissions().mode() & 0o077 != 0 {
                let mut permissions = metadata.permissions();
                permissions.set_mode(0o700);
                std::fs::set_permissions(lock_dir, permissions)?;
            }
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

#[cfg(unix)]
fn validate_lock_dir(metadata: &std::fs::Metadata, lock_dir: &Path) -> std::io::Result<()> {
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::other(format!(
            "auth lock path is not a directory: {}",
            lock_dir.display()
        )));
    }
    // A shared lock directory must be owned by this process and private from
    // other users. Existing directories are tightened to 0700 below.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::other(format!(
            "auth lock directory has unexpected owner: {}",
            lock_dir.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_lock_dir(metadata: &std::fs::Metadata, lock_dir: &Path) -> std::io::Result<()> {
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::other(format!(
            "auth lock path is not a directory: {}",
            lock_dir.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn validate_lock_file(file: &File, lock_path: &Path) -> std::io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::other(format!(
            "auth lock path is not a private regular file: {}",
            lock_path.display()
        )));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o600);
        file.set_permissions(permissions)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_lock_file(file: &File, lock_path: &Path) -> std::io::Result<()> {
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::other(format!(
            "auth lock path is not a regular file: {}",
            lock_path.display()
        )));
    }
    Ok(())
}

/// Return a stable identity for an existing home, a symlink alias, or an absent
/// configured home. For the latter, canonicalize the nearest existing ancestor
/// and append the missing path suffix instead of failing before a first login.
fn canonical_storage_identity(codex_home: &Path) -> std::io::Result<PathBuf> {
    let absolute = std::path::absolute(codex_home)?;
    let mut existing = absolute.as_path();
    let mut missing = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            return Ok(absolute);
        };
        missing.push(name.to_os_string());
        let Some(parent) = existing.parent() else {
            return Ok(absolute);
        };
        existing = parent;
    }
    let mut identity = std::fs::canonicalize(existing)?;
    for component in missing.iter().rev() {
        identity.push(component);
    }
    Ok(identity)
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
    fn codex_home(&self) -> &Path {
        &self.codex_home
    }

    fn load_unlocked(&self) -> std::io::Result<Option<AuthDotJson>> {
        let auth_file = get_auth_file(&self.codex_home);
        let auth_dot_json = match self.try_read_auth_json(&auth_file) {
            Ok(auth) => auth,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        Ok(Some(auth_dot_json))
    }

    fn save_unlocked(&self, auth_dot_json: &AuthDotJson) -> std::io::Result<()> {
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

    fn delete_unlocked(&self) -> std::io::Result<bool> {
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
    let canonical = canonical_storage_identity(codex_home)?;
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
    fn codex_home(&self) -> &Path {
        &self.codex_home
    }

    fn load_unlocked(&self) -> std::io::Result<Option<AuthDotJson>> {
        let key = compute_store_key(&self.codex_home)?;
        self.load_from_keyring(&key)
    }

    fn save_unlocked(&self, auth: &AuthDotJson) -> std::io::Result<()> {
        let key = compute_store_key(&self.codex_home)?;
        // Simpler error mapping per style: prefer method reference over closure
        let serialized = serde_json::to_string(auth).map_err(std::io::Error::other)?;
        self.save_to_keyring(&key, &serialized)?;
        if let Err(err) = delete_file_if_exists(&self.codex_home) {
            warn!("failed to remove CLI auth fallback file: {err}");
        }
        Ok(())
    }

    fn delete_unlocked(&self) -> std::io::Result<bool> {
        let key = compute_store_key(&self.codex_home)?;
        let keyring_removed = self
            .keyring_store
            .delete(KEYRING_SERVICE, &key)
            .map_err(|err| {
                std::io::Error::other(format!("failed to delete auth from keyring: {err}"))
            })?;
        let file_removed = delete_file_if_exists(&self.codex_home)?;
        Ok(keyring_removed || file_removed)
    }
}

#[derive(Clone)]
struct SecretsKeyringAuthStorage {
    codex_home: PathBuf,
    direct_storage: DirectKeyringAuthStorage,
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
        let direct_storage =
            DirectKeyringAuthStorage::new(codex_home.clone(), Arc::clone(&keyring_store));
        let secrets_manager = SecretsManager::new_with_keyring_store_and_namespace(
            codex_home.clone(),
            SecretsBackendKind::Local,
            keyring_store,
            LocalSecretsNamespace::CodexAuth,
        );
        Self {
            codex_home,
            direct_storage,
            secrets_manager,
        }
    }
}

impl AuthStorageBackend for SecretsKeyringAuthStorage {
    fn codex_home(&self) -> &Path {
        &self.codex_home
    }

    fn load_unlocked(&self) -> std::io::Result<Option<AuthDotJson>> {
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

    fn save_unlocked(&self, auth: &AuthDotJson) -> std::io::Result<()> {
        let serialized = serde_json::to_string(auth).map_err(std::io::Error::other)?;
        self.secrets_manager
            .set(&SecretScope::Global, &CODEX_AUTH_SECRET_NAME, &serialized)
            .map_err(|err| {
                let message =
                    format!("failed to write OAuth tokens to encrypted auth storage: {err}");
                warn!("{message}");
                std::io::Error::other(message)
            })?;
        if let Err(err) = delete_file_if_exists(&self.codex_home) {
            warn!("failed to remove CLI auth fallback file: {err}");
        }
        Ok(())
    }

    fn delete_unlocked(&self) -> std::io::Result<bool> {
        let keyring_removed = self
            .secrets_manager
            .delete(&SecretScope::Global, &CODEX_AUTH_SECRET_NAME)
            .map_err(|err| {
                std::io::Error::other(format!(
                    "failed to delete auth from encrypted auth storage: {err}"
                ))
            })?;
        let file_removed = delete_file_if_exists(&self.codex_home)?;
        let direct_removed = self.direct_storage.delete_unlocked()?;
        Ok(keyring_removed || file_removed || direct_removed)
    }
}

#[derive(Clone, Debug)]
struct AutoAuthStorage {
    keyring_storage: Arc<dyn AuthStorageBackend>,
    file_storage: Arc<FileAuthStorage>,
}

impl AutoAuthStorage {
    fn new(
        codex_home: PathBuf,
        keyring_store: Arc<dyn KeyringStore>,
        keyring_backend_kind: AuthKeyringBackendKind,
    ) -> Self {
        Self {
            keyring_storage: create_keyring_auth_storage(
                codex_home.clone(),
                keyring_store,
                keyring_backend_kind,
            ),
            file_storage: Arc::new(FileAuthStorage::new(codex_home)),
        }
    }
}

impl AuthStorageBackend for AutoAuthStorage {
    fn codex_home(&self) -> &Path {
        self.file_storage.codex_home()
    }

    fn load_unlocked(&self) -> std::io::Result<Option<AuthDotJson>> {
        match self.keyring_storage.load_unlocked() {
            Ok(Some(auth)) => Ok(Some(auth)),
            Ok(None) => self.file_storage.load_unlocked(),
            Err(err) => {
                warn!("failed to load CLI auth from keyring, falling back to file storage: {err}");
                self.file_storage.load_unlocked()
            }
        }
    }

    fn save_unlocked(&self, auth: &AuthDotJson) -> std::io::Result<()> {
        match self.keyring_storage.save_unlocked(auth) {
            Ok(()) => Ok(()),
            Err(err) => {
                warn!("failed to save auth to keyring, falling back to file storage: {err}");
                self.file_storage.save_unlocked(auth)
            }
        }
    }

    fn delete_unlocked(&self) -> std::io::Result<bool> {
        match self.keyring_storage.delete_unlocked() {
            Ok(keyring_removed) => {
                // Keyring backends normally remove the fallback file themselves, but
                // keep Auto semantics aligned with load: a file fallback is never
                // retained merely because keyring deletion returned no value.
                let file_removed = self.file_storage.delete_unlocked()?;
                Ok(keyring_removed || file_removed)
            }
            Err(err) => {
                warn!(
                    "failed to delete CLI auth from keyring, falling back to file storage: {err}"
                );
                self.file_storage.delete_unlocked()
            }
        }
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
    fn codex_home(&self) -> &Path {
        &self.codex_home
    }

    fn load_unlocked(&self) -> std::io::Result<Option<AuthDotJson>> {
        self.with_store(|store, key| Ok(store.get(&key).cloned()))
    }

    fn save_unlocked(&self, auth: &AuthDotJson) -> std::io::Result<()> {
        self.with_store(|store, key| {
            store.insert(key, auth.clone());
            Ok(())
        })
    }

    fn delete_unlocked(&self) -> std::io::Result<bool> {
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
        AuthCredentialsStoreMode::Auto => Arc::new(AutoAuthStorage::new(
            codex_home,
            keyring_store,
            keyring_backend_kind,
        )),
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
