use super::*;
use crate::token_data::IdTokenInfo;
use anyhow::Context;
use base64::Engine;
use codex_secrets::LocalSecretsNamespace;
use codex_secrets::SecretScope;
use codex_secrets::SecretsBackendKind;
use codex_secrets::SecretsManager;
use codex_secrets::compute_keyring_account;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::fs::OpenOptions;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use tempfile::tempdir;

use codex_keyring_store::CredentialStoreError;
use codex_keyring_store::KeyringStore;
use codex_keyring_store::tests::MockKeyringStore;
use keyring::Error as KeyringError;

#[derive(Debug)]
struct FailingSaveKeyringStore {
    inner: MockKeyringStore,
}

#[derive(Debug)]
struct WriteThenFailKeyringStore {
    inner: MockKeyringStore,
}

impl KeyringStore for WriteThenFailKeyringStore {
    fn load(&self, service: &str, account: &str) -> Result<Option<String>, CredentialStoreError> {
        self.inner.load(service, account)
    }

    fn save(&self, service: &str, account: &str, value: &str) -> Result<(), CredentialStoreError> {
        self.inner.save(service, account, value)?;
        Err(CredentialStoreError::new(KeyringError::Invalid(
            "error".into(),
            "save-after-write".into(),
        )))
    }

    fn delete(&self, service: &str, account: &str) -> Result<bool, CredentialStoreError> {
        self.inner.delete(service, account)
    }
}

#[derive(Debug)]
struct FailingDeleteKeyringStore {
    inner: MockKeyringStore,
}

impl KeyringStore for FailingDeleteKeyringStore {
    fn load(&self, service: &str, account: &str) -> Result<Option<String>, CredentialStoreError> {
        self.inner.load(service, account)
    }

    fn save(&self, service: &str, account: &str, value: &str) -> Result<(), CredentialStoreError> {
        self.inner.save(service, account, value)
    }

    fn delete(&self, _service: &str, _account: &str) -> Result<bool, CredentialStoreError> {
        Err(CredentialStoreError::new(KeyringError::Invalid(
            "error".into(),
            "delete".into(),
        )))
    }
}

impl KeyringStore for FailingSaveKeyringStore {
    fn load(&self, service: &str, account: &str) -> Result<Option<String>, CredentialStoreError> {
        self.inner.load(service, account)
    }

    fn save(
        &self,
        _service: &str,
        _account: &str,
        _value: &str,
    ) -> Result<(), CredentialStoreError> {
        Err(CredentialStoreError::new(KeyringError::Invalid(
            "error".into(),
            "save".into(),
        )))
    }

    fn delete(&self, service: &str, account: &str) -> Result<bool, CredentialStoreError> {
        self.inner.delete(service, account)
    }
}

#[tokio::test]
async fn file_storage_load_returns_auth_dot_json() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let storage = FileAuthStorage::new(codex_home.path().to_path_buf());
    let auth_dot_json = AuthDotJson {
        auth_mode: Some(AuthMode::ApiKey),
        openai_api_key: Some("test-key".to_string()),
        tokens: None,
        last_refresh: Some(Utc::now()),
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
    };

    storage
        .save(&auth_dot_json)
        .context("failed to save auth file")?;

    let loaded = storage.load().context("failed to load auth file")?;
    assert_eq!(Some(auth_dot_json), loaded);
    Ok(())
}

#[tokio::test]
async fn file_storage_save_persists_auth_dot_json() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let storage = FileAuthStorage::new(codex_home.path().to_path_buf());
    let auth_dot_json = AuthDotJson {
        auth_mode: Some(AuthMode::ApiKey),
        openai_api_key: Some("test-key".to_string()),
        tokens: None,
        last_refresh: Some(Utc::now()),
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
    };

    let file = get_auth_file(codex_home.path());
    storage
        .save(&auth_dot_json)
        .context("failed to save auth file")?;

    let same_auth_dot_json = storage
        .try_read_auth_json(&file)
        .context("failed to read auth file after save")?;
    assert_eq!(auth_dot_json, same_auth_dot_json);
    Ok(())
}

#[tokio::test]
async fn file_storage_round_trips_agent_identity_auth() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let storage = FileAuthStorage::new(codex_home.path().to_path_buf());
    let agent_identity = jwt_with_payload(json!({
        "agent_runtime_id": "agent-runtime-id",
        "agent_private_key": "private-key",
        "account_id": "account-id",
        "chatgpt_user_id": "user-id",
        "email": "user@example.com",
        "plan_type": "pro",
        "chatgpt_account_is_fedramp": false,
    }));
    let auth_dot_json = AuthDotJson {
        auth_mode: Some(AuthMode::AgentIdentity),
        openai_api_key: None,
        tokens: None,
        last_refresh: None,
        agent_identity: Some(AgentIdentityStorage::Jwt(agent_identity)),
        personal_access_token: None,
        bedrock_api_key: None,
    };

    storage.save(&auth_dot_json)?;

    let loaded = storage.load()?;
    assert_eq!(Some(auth_dot_json), loaded);
    Ok(())
}

#[tokio::test]
async fn file_storage_round_trips_registered_agent_identity_auth() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let storage = FileAuthStorage::new(codex_home.path().to_path_buf());
    let record = AgentIdentityAuthRecord {
        agent_runtime_id: "agent-runtime-id".to_string(),
        agent_private_key: "private-key".to_string(),
        account_id: "account-id".to_string(),
        chatgpt_user_id: "user-id".to_string(),
        email: Some("user@example.com".to_string()),
        plan_type: AccountPlanType::Pro,
        chatgpt_account_is_fedramp: false,
        task_id: Some("task-id".to_string()),
    };
    let auth_dot_json = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        openai_api_key: None,
        tokens: None,
        last_refresh: None,
        agent_identity: Some(AgentIdentityStorage::Record(record)),
        personal_access_token: None,
        bedrock_api_key: None,
    };

    storage.save(&auth_dot_json)?;

    let loaded = storage.load()?;
    assert_eq!(Some(auth_dot_json), loaded);
    Ok(())
}

#[tokio::test]
async fn file_storage_loads_empty_agent_identity_email_as_none() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let storage = FileAuthStorage::new(codex_home.path().to_path_buf());
    let auth_file = get_auth_file(codex_home.path());
    std::fs::write(
        &auth_file,
        serde_json::to_string_pretty(&json!({
            "auth_mode": "chatgpt",
            "agent_identity": {
                "agent_runtime_id": "agent-runtime-id",
                "agent_private_key": "private-key",
                "account_id": "account-id",
                "chatgpt_user_id": "user-id",
                "email": "",
                "plan_type": "pro",
                "chatgpt_account_is_fedramp": false,
            },
        }))?,
    )?;

    let loaded = storage.load()?;

    assert_eq!(
        loaded,
        Some(AuthDotJson {
            auth_mode: Some(AuthMode::Chatgpt),
            openai_api_key: None,
            tokens: None,
            last_refresh: None,
            agent_identity: Some(AgentIdentityStorage::Record(AgentIdentityAuthRecord {
                agent_runtime_id: "agent-runtime-id".to_string(),
                agent_private_key: "private-key".to_string(),
                account_id: "account-id".to_string(),
                chatgpt_user_id: "user-id".to_string(),
                email: None,
                plan_type: AccountPlanType::Pro,
                chatgpt_account_is_fedramp: false,
                task_id: None,
            })),
            personal_access_token: None,
            bedrock_api_key: None,
        })
    );
    Ok(())
}

#[tokio::test]
async fn file_storage_writes_missing_agent_identity_email_as_empty_string() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let storage = FileAuthStorage::new(codex_home.path().to_path_buf());
    let auth_dot_json = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        openai_api_key: None,
        tokens: None,
        last_refresh: None,
        agent_identity: Some(AgentIdentityStorage::Record(AgentIdentityAuthRecord {
            agent_runtime_id: "agent-runtime-id".to_string(),
            agent_private_key: "private-key".to_string(),
            account_id: "account-id".to_string(),
            chatgpt_user_id: "user-id".to_string(),
            email: None,
            plan_type: AccountPlanType::Pro,
            chatgpt_account_is_fedramp: false,
            task_id: None,
        })),
        personal_access_token: None,
        bedrock_api_key: None,
    };

    storage.save(&auth_dot_json)?;

    let auth_file = get_auth_file(codex_home.path());
    let saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(auth_file)?)?;
    assert_eq!(saved["agent_identity"]["email"], "");
    assert_eq!(storage.load()?, Some(auth_dot_json));
    Ok(())
}

#[tokio::test]
async fn file_storage_round_trips_personal_access_token_auth() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let storage = FileAuthStorage::new(codex_home.path().to_path_buf());
    let auth_dot_json = AuthDotJson {
        auth_mode: Some(AuthMode::PersonalAccessToken),
        openai_api_key: None,
        tokens: None,
        last_refresh: None,
        agent_identity: None,
        personal_access_token: Some("at-example".to_string()),
        bedrock_api_key: None,
    };

    storage.save(&auth_dot_json)?;

    let loaded = storage.load()?;
    assert_eq!(Some(auth_dot_json), loaded);
    Ok(())
}

#[tokio::test]
async fn file_storage_loads_agent_identity_as_jwt() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let storage = FileAuthStorage::new(codex_home.path().to_path_buf());
    let agent_identity_jwt = jwt_with_payload(json!({
        "agent_runtime_id": "agent-runtime-id",
        "agent_private_key": "private-key",
        "account_id": "account-id",
        "chatgpt_user_id": "user-id",
        "email": "user@example.com",
        "plan_type": "pro",
        "chatgpt_account_is_fedramp": false,
    }));
    let auth_file = get_auth_file(codex_home.path());
    std::fs::write(
        &auth_file,
        serde_json::to_string_pretty(&json!({
            "auth_mode": "agentIdentity",
            "agent_identity": agent_identity_jwt,
        }))?,
    )?;

    let loaded = storage.load()?;

    assert_eq!(
        loaded.expect("auth should load").agent_identity,
        Some(AgentIdentityStorage::Jwt(agent_identity_jwt))
    );
    Ok(())
}

#[test]
fn file_storage_delete_removes_auth_file() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let auth_dot_json = AuthDotJson {
        auth_mode: Some(AuthMode::ApiKey),
        openai_api_key: Some("sk-test-key".to_string()),
        tokens: None,
        last_refresh: None,
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
    };
    let storage = create_auth_storage(
        dir.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    );
    storage.save(&auth_dot_json)?;
    assert!(dir.path().join("auth.json").exists());
    let storage = FileAuthStorage::new(dir.path().to_path_buf());
    let removed = storage.delete()?;
    assert!(removed);
    assert!(!dir.path().join("auth.json").exists());
    Ok(())
}

#[test]
fn ephemeral_storage_save_load_delete_is_in_memory_only() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let storage = create_auth_storage(
        dir.path().to_path_buf(),
        AuthCredentialsStoreMode::Ephemeral,
        AuthKeyringBackendKind::default(),
    );
    let auth_dot_json = AuthDotJson {
        auth_mode: Some(AuthMode::ApiKey),
        openai_api_key: Some("sk-ephemeral".to_string()),
        tokens: None,
        last_refresh: Some(Utc::now()),
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
    };

    storage.save(&auth_dot_json)?;
    let loaded = storage.load()?;
    assert_eq!(Some(auth_dot_json), loaded);

    let removed = storage.delete()?;
    assert!(removed);
    let loaded = storage.load()?;
    assert_eq!(None, loaded);
    assert!(!get_auth_file(dir.path()).exists());
    Ok(())
}

fn seed_secrets_backend_and_fallback_auth_file_for_delete(
    mock_keyring: &MockKeyringStore,
    codex_home: &Path,
    auth: &AuthDotJson,
) -> anyhow::Result<PathBuf> {
    let manager = SecretsManager::new_with_keyring_store_and_namespace(
        codex_home.to_path_buf(),
        SecretsBackendKind::Local,
        Arc::new(mock_keyring.clone()),
        LocalSecretsNamespace::CodexAuth,
    );
    manager.set(
        &SecretScope::Global,
        &CODEX_AUTH_SECRET_NAME,
        &serde_json::to_string(auth)?,
    )?;
    let auth_file = get_auth_file(codex_home);
    std::fs::write(&auth_file, "stale")?;
    Ok(auth_file)
}

fn seed_secrets_backend_with_auth(
    mock_keyring: &MockKeyringStore,
    codex_home: &Path,
    auth: &AuthDotJson,
) -> anyhow::Result<()> {
    let manager = SecretsManager::new_with_keyring_store_and_namespace(
        codex_home.to_path_buf(),
        SecretsBackendKind::Local,
        Arc::new(mock_keyring.clone()),
        LocalSecretsNamespace::CodexAuth,
    );
    manager.set(
        &SecretScope::Global,
        &CODEX_AUTH_SECRET_NAME,
        &serde_json::to_string(auth)?,
    )?;
    Ok(())
}

fn assert_keyring_saved_auth_and_removed_fallback(
    mock_keyring: &MockKeyringStore,
    codex_home: &Path,
    expected: &AuthDotJson,
) -> anyhow::Result<()> {
    let manager = SecretsManager::new_with_keyring_store_and_namespace(
        codex_home.to_path_buf(),
        SecretsBackendKind::Local,
        Arc::new(mock_keyring.clone()),
        LocalSecretsNamespace::CodexAuth,
    );
    let saved_value = manager
        .get(&SecretScope::Global, &CODEX_AUTH_SECRET_NAME)?
        .context("encrypted auth entry should exist")?;
    let expected_serialized = serde_json::to_string(expected)?;
    assert_eq!(saved_value, expected_serialized);
    let old_key = compute_store_key(codex_home)?;
    assert!(
        mock_keyring.saved_value(&old_key).is_none(),
        "legacy keyring auth entry should not be used"
    );
    let secrets_key = compute_keyring_account(codex_home);
    assert!(
        mock_keyring.saved_value(&secrets_key).is_some(),
        "secrets backend should persist an encryption passphrase in the keyring"
    );
    assert!(encrypted_auth_file(codex_home).exists());
    let auth_file = get_auth_file(codex_home);
    assert!(
        !auth_file.exists(),
        "fallback auth.json should be removed after keyring save"
    );
    Ok(())
}

fn encrypted_auth_file(codex_home: &Path) -> PathBuf {
    codex_home.join("secrets").join("codex_auth.age")
}

fn id_token_with_prefix(prefix: &str) -> IdTokenInfo {
    #[derive(Serialize)]
    struct Header {
        alg: &'static str,
        typ: &'static str,
    }

    let header = Header {
        alg: "none",
        typ: "JWT",
    };
    let payload = json!({
        "email": format!("{prefix}@example.com"),
        "https://api.openai.com/auth": {
            "chatgpt_account_id": format!("{prefix}-account"),
        },
    });
    let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let header_b64 = encode(&serde_json::to_vec(&header).expect("serialize header"));
    let payload_b64 = encode(&serde_json::to_vec(&payload).expect("serialize payload"));
    let signature_b64 = encode(b"sig");
    let fake_jwt = format!("{header_b64}.{payload_b64}.{signature_b64}");

    crate::token_data::parse_chatgpt_jwt_claims(&fake_jwt).expect("fake JWT should parse")
}

fn auth_with_prefix(prefix: &str) -> AuthDotJson {
    AuthDotJson {
        auth_mode: Some(AuthMode::ApiKey),
        openai_api_key: Some(format!("{prefix}-api-key")),
        tokens: Some(TokenData {
            id_token: id_token_with_prefix(prefix),
            access_token: format!("{prefix}-access"),
            refresh_token: format!("{prefix}-refresh"),
            account_id: Some(format!("{prefix}-account-id")),
        }),
        last_refresh: None,
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
    }
}

fn jwt_with_payload(payload: serde_json::Value) -> String {
    let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let header_b64 = encode(br#"{"alg":"EdDSA","typ":"JWT"}"#);
    let payload_b64 = encode(&serde_json::to_vec(&payload).expect("payload should serialize"));
    let signature_b64 = encode(b"sig");
    format!("{header_b64}.{payload_b64}.{signature_b64}")
}

#[test]
fn secrets_keyring_auth_storage_load_returns_deserialized_auth() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let mock_keyring = MockKeyringStore::default();
    let storage = SecretsKeyringAuthStorage::new(
        codex_home.path().to_path_buf(),
        Arc::new(mock_keyring.clone()),
    );
    let expected = AuthDotJson {
        auth_mode: Some(AuthMode::ApiKey),
        openai_api_key: Some("sk-test".to_string()),
        tokens: None,
        last_refresh: None,
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
    };
    seed_secrets_backend_with_auth(&mock_keyring, codex_home.path(), &expected)?;

    let loaded = storage.load()?;
    assert_eq!(Some(expected), loaded);
    Ok(())
}

#[test]
fn keyring_auth_storage_compute_store_key_for_home_directory() -> anyhow::Result<()> {
    let codex_home = PathBuf::from("~/.codex");

    let key = compute_store_key(codex_home.as_path())?;

    assert_eq!(key, "cli|940db7b1d0e4eb40");
    Ok(())
}

#[test]
fn direct_keyring_auth_storage_is_source_local() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let mock_keyring = MockKeyringStore::default();
    let storage = DirectKeyringAuthStorage::new(
        codex_home.path().to_path_buf(),
        Arc::new(mock_keyring.clone()),
    );
    let auth_file = get_auth_file(codex_home.path());
    std::fs::write(&auth_file, "stale")?;
    let auth = auth_with_prefix("direct");

    storage.save(&auth)?;

    let legacy_key = compute_store_key(codex_home.path())?;
    let saved_value = mock_keyring
        .saved_value(&legacy_key)
        .context("direct keyring auth entry should exist")?;
    assert_eq!(saved_value, serde_json::to_string(&auth)?);
    assert!(!encrypted_auth_file(codex_home.path()).exists());
    assert!(
        auth_file.exists(),
        "direct keyring save must not touch auth.json"
    );
    assert_eq!(storage.load()?, Some(auth));
    Ok(())
}

#[test]
fn direct_keyring_auth_storage_delete_is_source_local() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let mock_keyring = MockKeyringStore::default();
    let storage = DirectKeyringAuthStorage::new(
        codex_home.path().to_path_buf(),
        Arc::new(mock_keyring.clone()),
    );
    let auth = auth_with_prefix("direct-delete");
    storage.save(&auth)?;
    let auth_file = get_auth_file(codex_home.path());
    std::fs::write(&auth_file, "stale")?;

    let removed = storage.delete()?;

    assert!(removed, "delete should report removal");
    assert_eq!(storage.load()?, None, "keyring auth should be removed");
    assert!(
        mock_keyring
            .saved_value(&compute_store_key(codex_home.path())?)
            .is_none(),
        "legacy keyring auth entry should be removed"
    );
    assert!(
        auth_file.exists(),
        "direct keyring delete must not touch auth.json"
    );
    assert!(!encrypted_auth_file(codex_home.path()).exists());
    Ok(())
}

#[test]
fn factory_uses_secrets_backend_only_when_requested() -> anyhow::Result<()> {
    let direct_home = tempdir()?;
    let direct_keyring = MockKeyringStore::default();
    let direct_storage = create_auth_storage_with_store(
        direct_home.path().to_path_buf(),
        AuthCredentialsStoreMode::Keyring,
        Arc::new(direct_keyring.clone()),
        AuthKeyringBackendKind::Direct,
    );
    let direct_auth = auth_with_prefix("factory-direct");
    direct_storage.save(&direct_auth)?;
    assert!(
        direct_keyring
            .saved_value(&compute_store_key(direct_home.path())?)
            .is_some()
    );
    assert!(!encrypted_auth_file(direct_home.path()).exists());

    let secrets_home = tempdir()?;
    let secrets_keyring = MockKeyringStore::default();
    let secrets_storage = create_auth_storage_with_store(
        secrets_home.path().to_path_buf(),
        AuthCredentialsStoreMode::Keyring,
        Arc::new(secrets_keyring.clone()),
        AuthKeyringBackendKind::Secrets,
    );
    let secrets_auth = auth_with_prefix("factory-secrets");
    secrets_storage.save(&secrets_auth)?;
    assert!(
        secrets_keyring
            .saved_value(&compute_keyring_account(secrets_home.path()))
            .is_some()
    );
    assert!(encrypted_auth_file(secrets_home.path()).exists());
    Ok(())
}

#[test]
fn secrets_keyring_auth_storage_is_source_local() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let mock_keyring = MockKeyringStore::default();
    let storage = SecretsKeyringAuthStorage::new(
        codex_home.path().to_path_buf(),
        Arc::new(mock_keyring.clone()),
    );
    let auth_file = get_auth_file(codex_home.path());
    std::fs::write(&auth_file, "stale")?;
    let auth = AuthDotJson {
        auth_mode: Some(AuthMode::Chatgpt),
        openai_api_key: None,
        tokens: Some(TokenData {
            id_token: Default::default(),
            access_token: "access".to_string(),
            refresh_token: "refresh".to_string(),
            account_id: Some("account".to_string()),
        }),
        last_refresh: Some(Utc::now()),
        agent_identity: None,
        personal_access_token: None,
        bedrock_api_key: None,
    };

    storage.save(&auth)?;

    let manager = SecretsManager::new_with_keyring_store_and_namespace(
        codex_home.path().to_path_buf(),
        SecretsBackendKind::Local,
        Arc::new(mock_keyring),
        LocalSecretsNamespace::CodexAuth,
    );
    assert_eq!(
        manager.get(&SecretScope::Global, &CODEX_AUTH_SECRET_NAME)?,
        Some(serde_json::to_string(&auth)?)
    );
    assert!(auth_file.exists(), "secrets save must not touch auth.json");
    Ok(())
}

#[test]
fn secrets_keyring_auth_storage_delete_is_source_local() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let mock_keyring = MockKeyringStore::default();
    let storage = SecretsKeyringAuthStorage::new(
        codex_home.path().to_path_buf(),
        Arc::new(mock_keyring.clone()),
    );
    let auth = auth_with_prefix("to-delete");
    let auth_file = seed_secrets_backend_and_fallback_auth_file_for_delete(
        &mock_keyring,
        codex_home.path(),
        &auth,
    )?;

    let removed = storage.delete()?;

    assert!(removed, "delete should report removal");
    assert_eq!(storage.load()?, None, "encrypted auth should be removed");
    assert!(
        auth_file.exists(),
        "secrets delete must not touch auth.json"
    );
    Ok(())
}

#[test]
fn secrets_keyring_auth_storage_delete_does_not_remove_direct_keyring_entry() -> anyhow::Result<()>
{
    let codex_home = tempdir()?;
    let mock_keyring = MockKeyringStore::default();
    let direct_storage = DirectKeyringAuthStorage::new(
        codex_home.path().to_path_buf(),
        Arc::new(mock_keyring.clone()),
    );
    direct_storage.save(&auth_with_prefix("legacy-direct"))?;
    let storage = SecretsKeyringAuthStorage::new(
        codex_home.path().to_path_buf(),
        Arc::new(mock_keyring.clone()),
    );
    let auth = auth_with_prefix("to-delete");
    let auth_file = seed_secrets_backend_and_fallback_auth_file_for_delete(
        &mock_keyring,
        codex_home.path(),
        &auth,
    )?;

    let removed = storage.delete()?;

    assert!(removed, "delete should report removal");
    assert_eq!(storage.load()?, None, "encrypted auth should be removed");
    assert_eq!(
        direct_storage.load()?,
        Some(auth_with_prefix("legacy-direct"))
    );
    assert!(
        auth_file.exists(),
        "secrets delete must not touch auth.json"
    );
    Ok(())
}

#[test]
fn auto_repository_prefers_keyring_and_returns_its_exact_source() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let mock_keyring = MockKeyringStore::default();
    let repository = create_auth_repository_with_store(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::Auto,
        Arc::new(mock_keyring.clone()),
        AuthKeyringBackendKind::Direct,
    );
    let keyring_auth = auth_with_prefix("keyring");
    DirectKeyringAuthStorage::new(codex_home.path().to_path_buf(), Arc::new(mock_keyring))
        .save(&keyring_auth)?;
    FileAuthStorage::new(codex_home.path().to_path_buf()).save(&auth_with_prefix("file"))?;

    assert_eq!(
        repository.load_active()?,
        Some(LoadedAuth {
            source: AuthStorageSource::DirectKeyring,
            auth: keyring_auth,
        })
    );
    Ok(())
}

#[test]
fn auto_repository_falls_back_only_after_a_positive_empty_keyring_read() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let repository = create_auth_repository_with_store(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::Auto,
        Arc::new(FailingSaveKeyringStore {
            inner: MockKeyringStore::default(),
        }),
        AuthKeyringBackendKind::Direct,
    );
    let replacement = auth_with_prefix("replacement");

    assert_eq!(
        repository.replace_for_login(&replacement)?.source,
        AuthStorageSource::File
    );
    assert_eq!(
        repository.load_active()?.map(|loaded| loaded.auth),
        Some(replacement)
    );
    Ok(())
}

#[test]
fn auto_repository_refuses_shadow_file_when_keyring_contains_auth_and_write_fails()
-> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let mock_keyring = MockKeyringStore::default();
    let keyring = DirectKeyringAuthStorage::new(
        codex_home.path().to_path_buf(),
        Arc::new(mock_keyring.clone()),
    );
    let original = auth_with_prefix("original");
    keyring.save(&original)?;
    let repository = create_auth_repository_with_store(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::Auto,
        Arc::new(FailingSaveKeyringStore {
            inner: mock_keyring,
        }),
        AuthKeyringBackendKind::Direct,
    );

    assert!(
        repository
            .replace_for_login(&auth_with_prefix("replacement"))
            .is_err()
    );
    assert_eq!(
        repository.load_active()?.map(|loaded| loaded.auth),
        Some(original)
    );
    assert!(!get_auth_file(codex_home.path()).exists());
    Ok(())
}

#[test]
fn auto_repository_fails_closed_when_keyring_writes_then_returns_an_error() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let replacement = auth_with_prefix("written-then-error");
    let repository = create_auth_repository_with_store(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::Auto,
        Arc::new(WriteThenFailKeyringStore {
            inner: MockKeyringStore::default(),
        }),
        AuthKeyringBackendKind::Direct,
    );

    assert!(repository.replace_for_login(&replacement).is_err());
    assert_eq!(
        repository.load_active()?,
        Some(LoadedAuth {
            source: AuthStorageSource::DirectKeyring,
            auth: replacement,
        })
    );
    assert!(!get_auth_file(codex_home.path()).exists());
    Ok(())
}

#[test]
fn compare_delete_and_logout_keep_source_and_user_policy_separate() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let mock_keyring = MockKeyringStore::default();
    let repository = create_auth_repository_with_store(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::Keyring,
        Arc::new(mock_keyring.clone()),
        AuthKeyringBackendKind::Direct,
    );
    let direct = DirectKeyringAuthStorage::new(
        codex_home.path().to_path_buf(),
        Arc::new(mock_keyring.clone()),
    );
    let secrets =
        SecretsKeyringAuthStorage::new(codex_home.path().to_path_buf(), Arc::new(mock_keyring));
    let ephemeral = EphemeralAuthStorage::new(codex_home.path().to_path_buf());
    let direct_auth = auth_with_prefix("direct");
    direct.save(&direct_auth)?;
    FileAuthStorage::new(codex_home.path().to_path_buf()).save(&auth_with_prefix("file"))?;
    secrets.save(&auth_with_prefix("secrets"))?;
    ephemeral.save(&auth_with_prefix("ephemeral"))?;

    assert!(repository.delete_if_matches(&LoadedAuth {
        source: AuthStorageSource::DirectKeyring,
        auth: direct_auth,
    })?);
    assert!(
        FileAuthStorage::new(codex_home.path().to_path_buf())
            .load()?
            .is_some()
    );
    assert!(secrets.load()?.is_some());
    assert!(ephemeral.load()?.is_some());
    assert!(repository.logout_all()?);
    assert!(
        FileAuthStorage::new(codex_home.path().to_path_buf())
            .load()?
            .is_none()
    );
    assert!(secrets.load()?.is_none());
    assert!(ephemeral.load()?.is_none());
    Ok(())
}

#[test]
fn logout_all_reports_partial_failure_after_continuing_other_stores() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let mock_keyring = MockKeyringStore::default();
    let repository = create_auth_repository_with_store(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        Arc::new(FailingDeleteKeyringStore {
            inner: mock_keyring,
        }),
        AuthKeyringBackendKind::Direct,
    );
    FileAuthStorage::new(codex_home.path().to_path_buf()).save(&auth_with_prefix("file"))?;
    EphemeralAuthStorage::new(codex_home.path().to_path_buf())
        .save(&auth_with_prefix("ephemeral"))?;

    let error = repository
        .logout_all()
        .expect_err("keyring delete should fail");
    assert!(error.to_string().contains("partial auth logout"));
    assert!(error.to_string().contains("DirectKeyring"));
    assert!(
        FileAuthStorage::new(codex_home.path().to_path_buf())
            .load()?
            .is_none()
    );
    assert!(
        EphemeralAuthStorage::new(codex_home.path().to_path_buf())
            .load()?
            .is_none()
    );
    Ok(())
}

#[test]
fn repository_uses_an_os_lock_for_authority_transactions() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let repository = create_auth_repository_with_store(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        Arc::new(MockKeyringStore::default()),
        AuthKeyringBackendKind::Direct,
    );
    let lock_path = auth_repository_lock_path(codex_home.path())?;
    let _transaction = repository.transaction()?;
    let contender = OpenOptions::new().read(true).write(true).open(lock_path)?;
    let error = contender
        .try_lock()
        .expect_err("a second authority transaction must wait for the OS lock");
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    Ok(())
}

#[test]
fn source_bound_compare_update_rejects_a_mismatch_without_mutating() -> anyhow::Result<()> {
    let codex_home = tempdir()?;
    let repository = create_auth_repository_with_store(
        codex_home.path().to_path_buf(),
        AuthCredentialsStoreMode::Keyring,
        Arc::new(MockKeyringStore::default()),
        AuthKeyringBackendKind::Direct,
    );
    let original = auth_with_prefix("original");
    let loaded = repository.replace_for_login(&original)?;
    let replacement = auth_with_prefix("replacement");
    repository.replace_for_login(&replacement)?;

    assert!(
        repository
            .update_if_unchanged(&loaded, &auth_with_prefix("unexpected"))
            .is_err()
    );
    assert_eq!(
        repository.load_active()?.map(|loaded| loaded.auth),
        Some(replacement)
    );
    Ok(())
}

const AUTH_LOCK_TEST_ROLE: &str = "CODEX_AUTH_LOCK_TEST_ROLE";
const AUTH_LOCK_TEST_HOME: &str = "CODEX_AUTH_LOCK_TEST_HOME";
const AUTH_LOCK_TEST_SYNC: &str = "CODEX_AUTH_LOCK_TEST_SYNC";

fn lock_test_path(sync: &Path, name: &str) -> PathBuf {
    sync.join(name)
}

fn write_lock_test_signal(sync: &Path, name: &str) -> anyhow::Result<()> {
    std::fs::write(lock_test_path(sync, name), "ready")?;
    Ok(())
}

fn wait_for_lock_test_signal(sync: &Path, name: &str) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !lock_test_path(sync, name).exists() {
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for lock-test signal {name}");
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn lock_test_repository(codex_home: PathBuf) -> AuthRepository {
    create_auth_repository_with_store(
        codex_home,
        AuthCredentialsStoreMode::File,
        Arc::new(MockKeyringStore::default()),
        AuthKeyringBackendKind::Direct,
    )
}

fn run_lock_test_holder(codex_home: PathBuf, sync: PathBuf) -> anyhow::Result<()> {
    let repository = lock_test_repository(codex_home);
    std::fs::write(
        lock_test_path(&sync, "holder-tmpdir"),
        std::env::var("TMPDIR")?,
    )?;
    let stale = repository
        .load_active()?
        .context("holder must capture the original credential")?;
    write_lock_test_signal(&sync, "stale-snapshot")?;
    let transaction = repository.transaction()?;
    write_lock_test_signal(&sync, "holder-locked")?;
    wait_for_lock_test_signal(&sync, "release-holder")?;
    drop(transaction);
    wait_for_lock_test_signal(&sync, "writer-finished")?;
    assert!(
        repository
            .update_if_unchanged(&stale, &auth_with_prefix("stale-overwrite"))
            .is_err()
    );
    assert_eq!(
        repository.load_active()?.map(|loaded| loaded.auth),
        Some(auth_with_prefix("newer"))
    );
    write_lock_test_signal(&sync, "stale-update-rejected")
}

fn run_lock_test_writer(codex_home: PathBuf, sync: PathBuf) -> anyhow::Result<()> {
    let repository = lock_test_repository(codex_home);
    std::fs::write(
        lock_test_path(&sync, "writer-tmpdir"),
        std::env::var("TMPDIR")?,
    )?;
    write_lock_test_signal(&sync, "writer-started")?;
    repository.replace_for_login(&auth_with_prefix("newer"))?;
    write_lock_test_signal(&sync, "writer-finished")
}

fn spawn_lock_test_child(
    test_name: &str,
    role: &str,
    codex_home: &Path,
    sync: &Path,
    tmpdir: &Path,
) -> anyhow::Result<std::process::Child> {
    Ok(Command::new(std::env::current_exe()?)
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .env(AUTH_LOCK_TEST_ROLE, role)
        .env(AUTH_LOCK_TEST_HOME, codex_home)
        .env(AUTH_LOCK_TEST_SYNC, sync)
        .env("TMPDIR", tmpdir)
        .spawn()?)
}

#[test]
fn repository_lock_is_stable_across_process_tmpdirs_and_blocks_stale_update() -> anyhow::Result<()>
{
    let role = std::env::var(AUTH_LOCK_TEST_ROLE).ok();
    if let Some(role) = role.as_deref() {
        let codex_home = PathBuf::from(std::env::var(AUTH_LOCK_TEST_HOME)?);
        let sync = PathBuf::from(std::env::var(AUTH_LOCK_TEST_SYNC)?);
        return match role {
            "holder" => run_lock_test_holder(codex_home, sync),
            "writer" => run_lock_test_writer(codex_home, sync),
            _ => anyhow::bail!("unknown auth-lock test role {role}"),
        };
    }

    let codex_home = tempdir()?;
    let sync = tempdir()?;
    let tmpdir_a = tempdir()?;
    let tmpdir_b = tempdir()?;
    let repository = lock_test_repository(codex_home.path().to_path_buf());
    repository.replace_for_login(&auth_with_prefix("original"))?;
    let test_name = thread::current()
        .name()
        .context("libtest must name the current test")?
        .to_owned();

    let mut holder = spawn_lock_test_child(
        &test_name,
        "holder",
        codex_home.path(),
        sync.path(),
        tmpdir_a.path(),
    )?;
    wait_for_lock_test_signal(sync.path(), "stale-snapshot")?;
    wait_for_lock_test_signal(sync.path(), "holder-locked")?;
    assert_eq!(
        std::fs::read_to_string(lock_test_path(sync.path(), "holder-tmpdir"))?,
        tmpdir_a.path().to_string_lossy().to_string(),
        "holder must run with its assigned TMPDIR"
    );

    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(auth_repository_lock_path(codex_home.path())?)?;
    let error = lock_file
        .try_lock()
        .expect_err("the holder process must own the shared authority lock");
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);

    let mut writer = spawn_lock_test_child(
        &test_name,
        "writer",
        codex_home.path(),
        sync.path(),
        tmpdir_b.path(),
    )?;
    wait_for_lock_test_signal(sync.path(), "writer-started")?;
    assert_eq!(
        std::fs::read_to_string(lock_test_path(sync.path(), "writer-tmpdir"))?,
        tmpdir_b.path().to_string_lossy().to_string(),
        "writer must run with a distinct TMPDIR"
    );
    assert_ne!(tmpdir_a.path(), tmpdir_b.path());
    thread::sleep(Duration::from_millis(100));
    assert!(
        !lock_test_path(sync.path(), "writer-finished").exists(),
        "writer must remain excluded while the distinct holder process owns the lock"
    );
    assert!(writer.try_wait()?.is_none(), "writer must still be blocked");

    write_lock_test_signal(sync.path(), "release-holder")?;
    wait_for_lock_test_signal(sync.path(), "writer-finished")?;
    wait_for_lock_test_signal(sync.path(), "stale-update-rejected")?;
    assert!(holder.wait()?.success(), "holder child failed");
    assert!(writer.wait()?.success(), "writer child failed");
    assert_eq!(
        repository.load_active()?.map(|loaded| loaded.auth),
        Some(auth_with_prefix("newer"))
    );
    Ok(())
}
