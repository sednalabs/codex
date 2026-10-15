//! Cross-process serialization for app-server authentication transitions.
//!
//! The State marker survives a crash; this OS lock distinguishes recovery from a live login
//! running in another app-server process. The lock file contains no credentials or auth data.

use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use codex_login::AuthManager;
use codex_rollout::StateDbHandle;

const LOCK_FILE_NAME: &str = "app-server-auth-transition.lock";
const LOCK_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(60);
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// Held while an app-server account transition is active or its startup recovery is resolved.
pub(crate) struct AuthTransitionLock {
    _file: File,
}

/// Owns the process lock and the State admission marker for one account transition.
/// Dropping before completion leaves the durable marker set; startup recovery will
/// reload auth under the same lock before clearing it.
pub(crate) struct AuthTransition {
    lock: AuthTransitionLock,
    state_db: Option<StateDbHandle>,
    generation: Option<i64>,
    auth_manager: Arc<AuthManager>,
}

impl AuthTransitionLock {
    /// Acquire off the async executor thread. A still-running transition may hold the lock
    /// through OAuth completion; on timeout the caller fails closed without changing auth state.
    pub(crate) async fn acquire(codex_home: PathBuf) -> io::Result<Self> {
        tokio::task::spawn_blocking(move || acquire_blocking(&codex_home))
            .await
            .map_err(io::Error::other)?
    }
}

impl AuthTransition {
    /// Begin a serialized login/logout transition. State-backed continuations are
    /// revoked before the caller can mutate the authenticated account.
    pub(crate) async fn begin(
        state_db: Option<&StateDbHandle>,
        auth_manager: Arc<AuthManager>,
        codex_home: PathBuf,
    ) -> io::Result<Self> {
        let lock_home = auth_transition_lock_home(state_db, &codex_home);
        let lock = AuthTransitionLock::acquire(lock_home).await?;
        let (state_db, generation) = match state_db {
            Some(state_db) => {
                // The cached manager may have been initialized before another app-server
                // completed a transition. Reconcile it under the same State-domain lock before
                // reading or changing the durable epoch.
                let auth_revision = auth_manager.reload_for_auth_transition().await?;
                // A request task can be cancelled after it persisted the pending
                // marker. Recover that generation under the lock before starting
                // another transition in this still-running app-server.
                if let Some(pending_generation) = state_db
                    .pending_automatic_turn_auth_transition()
                    .await
                    .map_err(io::Error::other)?
                {
                    if !state_db
                        .complete_automatic_turn_auth_transition(pending_generation)
                        .await
                        .map_err(io::Error::other)?
                    {
                        return Err(io::Error::other(
                            "automatic-turn auth transition changed during recovery",
                        ));
                    }
                }
                mark_auth_manager_reconciled(state_db, &auth_manager, auth_revision).await?;
                let generation = state_db
                    .begin_automatic_turn_auth_transition()
                    .await
                    .map_err(io::Error::other)?;
                (Some(state_db.clone()), Some(generation))
            }
            None => {
                auth_manager.reload_for_auth_transition().await?;
                (None, None)
            }
        };
        Ok(Self {
            lock,
            state_db,
            generation,
            auth_manager,
        })
    }

    /// Complete only the exact transition this guard began. A stale/missing marker
    /// is an error and remains fail-closed rather than admitting continuation work.
    pub(crate) async fn complete(self) -> io::Result<()> {
        let Self {
            lock,
            state_db,
            generation,
            auth_manager,
        } = self;
        let auth_revision = auth_manager.reload_for_auth_transition().await?;
        if let (Some(state_db), Some(generation)) = (state_db.as_ref(), generation)
            && !state_db
                .complete_automatic_turn_auth_transition(generation)
                .await
                .map_err(io::Error::other)?
        {
            return Err(io::Error::other(
                "automatic-turn auth transition changed before completion",
            ));
        }
        if let Some(state_db) = state_db {
            mark_auth_manager_reconciled(&state_db, &auth_manager, auth_revision).await?;
        }
        drop(lock);
        Ok(())
    }
}

/// Reconcile an interrupted State marker only while no app-server login/logout can be active.
/// The AuthManager is reloaded under the same cross-process lock before the marker is cleared.
pub(crate) async fn recover_pending_auth_transition(
    state_db: Option<&StateDbHandle>,
    auth_manager: &AuthManager,
    codex_home: &Path,
) -> io::Result<()> {
    let Some(state_db) = state_db else {
        return Ok(());
    };
    let _transition_lock =
        AuthTransitionLock::acquire(auth_transition_lock_home(Some(state_db), codex_home)).await?;
    // AuthManager is constructed before this lock is acquired. Always reload under
    // the lock so a process that started during another process's login/logout does
    // not serve with the pre-transition snapshot after it eventually acquires it.
    let auth_revision = auth_manager.reload_for_auth_transition().await?;
    if let Some(generation) = state_db
        .pending_automatic_turn_auth_transition()
        .await
        .map_err(io::Error::other)?
    {
        if !state_db
            .complete_automatic_turn_auth_transition(generation)
            .await
            .map_err(io::Error::other)?
        {
            return Err(io::Error::other(
                "automatic-turn auth transition changed during startup recovery",
            ));
        }
    }
    mark_auth_manager_reconciled(state_db, auth_manager, auth_revision).await?;
    Ok(())
}

async fn mark_auth_manager_reconciled(
    state_db: &StateDbHandle,
    auth_manager: &AuthManager,
    auth_revision: u64,
) -> io::Result<()> {
    let auth_epoch = state_db
        .automatic_turn_current_auth_epoch()
        .await
        .ok_or_else(|| io::Error::other("automatic-turn auth epoch is unavailable or pending"))?;
    auth_manager.mark_automatic_turn_auth_epoch_reconciled(auth_epoch, auth_revision)
}

fn auth_transition_lock_home(state_db: Option<&StateDbHandle>, codex_home: &Path) -> PathBuf {
    state_db
        .map(|state_db| state_db.sqlite().home().to_path_buf())
        .unwrap_or_else(|| codex_home.to_path_buf())
}

fn acquire_blocking(lock_home: &Path) -> io::Result<AuthTransitionLock> {
    fs::create_dir_all(lock_home)?;
    let lock_home = fs::canonicalize(lock_home)?;
    let path = lock_home.join(LOCK_FILE_NAME);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    let started = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(AuthTransitionLock { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) if started.elapsed() >= LOCK_ACQUIRE_TIMEOUT => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for app-server auth transition lock",
                ));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                std::thread::sleep(LOCK_RETRY_INTERVAL);
            }
            Err(error) => return Err(io::Error::from(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AuthTransition;
    use super::AuthTransitionLock;
    use super::auth_transition_lock_home;
    use super::recover_pending_auth_transition;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    use base64::Engine as _;
    use codex_login::AuthCredentialsStoreMode;
    use codex_login::AuthKeyringBackendKind;
    use codex_login::AuthManager;
    use codex_login::CodexAuth;
    use codex_login::test_support::transport_default_auth_route_config;
    use codex_state::SqliteConfig;
    use codex_utils_absolute_path::test_support::PathExt;
    use tempfile::tempdir;

    async fn auth_manager(codex_home: &Path) -> Arc<AuthManager> {
        Arc::new(
            AuthManager::new(
                codex_home.to_path_buf(),
                /*enable_codex_api_key_env*/ false,
                AuthCredentialsStoreMode::File,
                /*forced_chatgpt_workspace_id*/ None,
                /*chatgpt_base_url*/ None,
                AuthKeyringBackendKind::default(),
                transport_default_auth_route_config(),
            )
            .await,
        )
    }

    #[tokio::test]
    async fn auth_transition_lock_serializes_startup_and_account_changes() {
        let home = tempfile::tempdir().expect("temporary codex home");
        let first = AuthTransitionLock::acquire(home.path().to_path_buf())
            .await
            .expect("first transition lock");
        let second_home = home.path().to_path_buf();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let second = tokio::spawn(async move {
            let _ = started_tx.send(());
            AuthTransitionLock::acquire(second_home)
                .await
                .expect("second transition lock")
        });
        started_rx.await.expect("second acquisition started");
        tokio::task::yield_now().await;
        assert!(
            !second.is_finished(),
            "a second app-server must not pass an active auth transition"
        );

        drop(first);
        let _second = tokio::time::timeout(Duration::from_secs(2), second)
            .await
            .expect("second transition proceeds after the first releases")
            .expect("second acquisition task");
    }

    #[tokio::test]
    async fn distinct_codex_homes_share_the_state_database_lock_domain() -> anyhow::Result<()> {
        let state_home = tempdir()?;
        let first_codex_home = tempdir()?;
        let second_codex_home = tempdir()?;
        let state_db = codex_state::StateRuntime::init(
            SqliteConfig::new_for_testing(state_home.path().abs()),
            "test-provider".to_string(),
        )
        .await?;

        let first_home = auth_transition_lock_home(Some(&state_db), first_codex_home.path());
        let second_home = auth_transition_lock_home(Some(&state_db), second_codex_home.path());
        assert_eq!(first_home, second_home);
        assert_ne!(first_home.as_path(), first_codex_home.path());

        let first = AuthTransitionLock::acquire(first_home).await?;
        let second_home = second_home.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let second = tokio::spawn(async move {
            let _ = started_tx.send(());
            AuthTransitionLock::acquire(second_home)
                .await
                .expect("second State-domain lock")
        });
        started_rx.await?;
        tokio::task::yield_now().await;
        assert!(!second.is_finished());
        drop(first);
        tokio::time::timeout(Duration::from_secs(2), second).await??;
        Ok(())
    }

    #[tokio::test]
    async fn startup_recovery_reconciles_auth_before_reopening_admission() -> anyhow::Result<()> {
        let state_home = tempdir()?;
        let codex_home = tempdir()?;
        let state_db = codex_state::StateRuntime::init(
            SqliteConfig::new_for_testing(state_home.path().abs()),
            "test-provider".to_string(),
        )
        .await?;
        let manager = auth_manager(codex_home.path()).await;
        let pending_generation = state_db.begin_automatic_turn_auth_transition().await?;

        let held_lock = AuthTransitionLock::acquire(auth_transition_lock_home(
            Some(&state_db),
            codex_home.path(),
        ))
        .await?;
        let recovery_state_db = state_db.clone();
        let recovery_manager = Arc::clone(&manager);
        let recovery_home = codex_home.path().to_path_buf();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let recovery = tokio::spawn(async move {
            let _ = started_tx.send(());
            recover_pending_auth_transition(
                Some(&recovery_state_db),
                recovery_manager.as_ref(),
                &recovery_home,
            )
            .await
        });
        started_rx.await?;
        tokio::task::yield_now().await;
        assert!(
            !recovery.is_finished(),
            "startup recovery must wait for an account transition sharing the State DB"
        );
        drop(held_lock);
        recovery.await??;

        assert_eq!(
            state_db.pending_automatic_turn_auth_transition().await?,
            None
        );
        let reconciled_auth_epoch = state_db
            .automatic_turn_current_auth_epoch()
            .await
            .expect("recovery completed the durable auth epoch");
        let (auth_revision, _provider_guard) = manager.current_provider_request_guard().await;
        assert_eq!(reconciled_auth_epoch, pending_generation + 1);
        assert!(
            manager.is_automatic_turn_auth_epoch_reconciled(reconciled_auth_epoch, auth_revision)
        );
        Ok(())
    }

    fn write_chatgpt_auth_file(
        codex_home: &Path,
        user_id: &str,
        account_id: &str,
    ) -> anyhow::Result<CodexAuth> {
        std::fs::create_dir_all(codex_home)?;
        let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let header = encode(&serde_json::to_vec(&serde_json::json!({
            "alg": "none",
            "typ": "JWT"
        }))?);
        let payload = encode(&serde_json::to_vec(&serde_json::json!({
            "email": format!("{user_id}@example.test"),
            "email_verified": true,
            "https://api.openai.com/auth": {
                "chatgpt_user_id": user_id,
                "user_id": user_id,
                "chatgpt_account_id": account_id,
                "chatgpt_plan_type": "pro"
            }
        }))?);
        let signature = encode(b"test-signature");
        let access_token = format!("access-{user_id}");
        let refresh_token = format!("refresh-{user_id}");
        let token = format!("{header}.{payload}.{signature}");
        std::fs::write(
            codex_home.join("auth.json"),
            serde_json::to_vec(&serde_json::json!({
                "tokens": {
                    "id_token": token,
                    "access_token": access_token,
                    "refresh_token": refresh_token
                }
            }))?,
        )?;
        Ok(CodexAuth::from_external_chatgpt_tokens(
            &token,
            account_id,
            Some("pro"),
        )?)
    }

    fn auth_manager_with_chatgpt_identity(
        codex_home: &Path,
        user_id: &str,
        account_id: &str,
    ) -> anyhow::Result<Arc<AuthManager>> {
        let auth = write_chatgpt_auth_file(codex_home, user_id, account_id)?;
        Ok(AuthManager::from_auth_for_testing_with_home(
            auth,
            codex_home.to_path_buf(),
        ))
    }

    fn cached_principal(manager: &AuthManager) -> Option<String> {
        let auth = manager.auth_cached()?;
        Some(format!(
            "{}:{}",
            auth.get_account_id()?,
            auth.get_chatgpt_user_id()?
        ))
    }

    async fn current_auth_revision(manager: &Arc<AuthManager>) -> u64 {
        let (revision, _provider_guard) = manager.current_provider_request_guard().await;
        revision
    }

    fn policy_error(turn_id: &str) -> codex_protocol::protocol::Event {
        use codex_protocol::protocol::CodexErrorInfo;
        use codex_protocol::protocol::ErrorEvent;
        use codex_protocol::protocol::Event;
        use codex_protocol::protocol::EventMsg;

        Event {
            id: turn_id.to_string(),
            msg: EventMsg::Error(ErrorEvent {
                message: "blocked".to_string(),
                codex_error_info: Some(CodexErrorInfo::CyberPolicy),
                misalignment: None,
            }),
        }
    }

    #[tokio::test]
    async fn automatic_turn_auth_epoch_witness_is_manager_and_cache_bound() -> anyhow::Result<()> {
        let state_home = tempdir()?;
        let codex_home_a = tempdir()?;
        let codex_home_b = tempdir()?;
        let state_db = Arc::new(
            codex_state::StateRuntime::init(
                SqliteConfig::new_for_testing(state_home.path().abs()),
                "test-provider".to_string(),
            )
            .await?,
        );
        // Two embedded servers can share this cloned StateDbHandle while owning distinct cached
        // authenticated identities and provider request revisions.
        let state_db_a = Arc::clone(&state_db);
        let state_db_b = Arc::clone(&state_db);
        let manager_a = auth_manager_with_chatgpt_identity(
            codex_home_a.path(),
            "user-a-stale",
            "account-a-stale",
        )?;
        let manager_b = auth_manager_with_chatgpt_identity(
            codex_home_b.path(),
            "user-b-current",
            "account-b-current",
        )?;
        let stale_auth_revision_a = current_auth_revision(&manager_a).await;
        let stale_principal_a = cached_principal(&manager_a).expect("A starts authenticated");
        let thread_id = codex_protocol::ThreadId::new();

        // B reloads its own cache under the shared State-domain lock, completes the durable
        // transition, and receives only its own manager-local reconciliation witness.
        let transition_b = AuthTransition::begin(
            Some(&state_db_b),
            Arc::clone(&manager_b),
            codex_home_b.path().to_path_buf(),
        )
        .await?;
        transition_b.complete().await?;
        let auth_revision_b = current_auth_revision(&manager_b).await;
        let epochs_b = state_db_b
            .automatic_turn_current_epochs(thread_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("State epoch is available after transition"))?;
        assert!(manager_b.is_automatic_turn_auth_epoch_reconciled(epochs_b.1, auth_revision_b));
        assert!(
            !manager_a.is_automatic_turn_auth_epoch_reconciled(epochs_b.1, stale_auth_revision_a)
        );
        assert_eq!(
            cached_principal(&manager_a).as_deref(),
            Some(stale_principal_a.as_str()),
            "B's transition must not reload or certify A's separate auth cache"
        );

        // This is the same principal gate used by ModelClient after it captures the exact
        // ProviderAuthority/AuthManager pair. A stale manager gets no principal for either issue
        // or redeem, despite sharing State with the manager that just reconciled.
        let principal_a_at_b_epoch = manager_a.automatic_turn_principal_if_auth_epoch_reconciled(
            epochs_b.1,
            stale_auth_revision_a,
            Some(stale_principal_a.clone()),
        );
        let expected_epochs_a = principal_a_at_b_epoch.as_ref().map(|_| epochs_b);
        assert!(expected_epochs_a.is_none());
        assert!(
            state_db_a
                .record_automatic_turn_event_with_submission_context(
                    thread_id,
                    &policy_error("stale-manager-a-issue"),
                    principal_a_at_b_epoch.as_deref(),
                    None,
                    None,
                    expected_epochs_a,
                )
                .await
                .is_none(),
            "a stale manager cannot issue a ticket against B's reconciled epoch"
        );
        assert!(
            manager_a
                .automatic_turn_principal_if_auth_epoch_reconciled(
                    epochs_b.1,
                    stale_auth_revision_a,
                    Some(stale_principal_a.clone()),
                )
                .is_none(),
            "the stale manager cannot reach State reservation for redemption"
        );
        let principal_b = manager_b
            .automatic_turn_principal_if_auth_epoch_reconciled(
                epochs_b.1,
                auth_revision_b,
                cached_principal(&manager_b),
            )
            .expect("B's current authenticated identity is reconciled");
        let ticket_b = state_db_b
            .record_automatic_turn_event_with_submission_context(
                thread_id,
                &policy_error("manager-b-issue"),
                Some(&principal_b),
                None,
                None,
                Some(epochs_b),
            )
            .await
            .expect("the reconciled manager can issue its principal-bound ticket");
        assert!(
            state_db_b
                .reserve_automatic_turn_continuation(
                    thread_id,
                    &ticket_b,
                    &principal_b,
                    "manager-b-client",
                    "start",
                    None,
                )
                .await?
        );

        // A's own transition changes its persisted identity. The new cache revision and epoch
        // witness must replace, not inherit, its previously stale cached principal.
        let transition_a = AuthTransition::begin(
            Some(&state_db_a),
            Arc::clone(&manager_a),
            codex_home_a.path().to_path_buf(),
        )
        .await?;
        write_chatgpt_auth_file(codex_home_a.path(), "user-a-current", "account-a-current")?;
        transition_a.complete().await?;
        let current_auth_revision_a = current_auth_revision(&manager_a).await;
        assert_ne!(current_auth_revision_a, stale_auth_revision_a);
        let epochs_a = state_db_a
            .automatic_turn_current_epochs(thread_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("State epoch is available after A transition"))?;
        let principal_a_current = cached_principal(&manager_a).expect("A has its new identity");
        assert_ne!(principal_a_current, stale_principal_a);
        assert_eq!(
            manager_a.automatic_turn_principal_if_auth_epoch_reconciled(
                epochs_a.1,
                current_auth_revision_a,
                Some(principal_a_current.clone()),
            ),
            Some(principal_a_current.clone())
        );
        assert!(
            manager_a
                .automatic_turn_principal_if_auth_epoch_reconciled(
                    epochs_a.1,
                    stale_auth_revision_a,
                    Some(stale_principal_a),
                )
                .is_none(),
            "A's pre-reload cached identity cannot redeem under the new epoch"
        );
        assert!(
            manager_b
                .automatic_turn_principal_if_auth_epoch_reconciled(
                    epochs_a.1,
                    auth_revision_b,
                    Some(principal_b),
                )
                .is_none(),
            "B's older reconciliation cannot authorize A's new State epoch"
        );

        let ticket_a = state_db_a
            .record_automatic_turn_event_with_submission_context(
                thread_id,
                &policy_error("manager-a-current-issue"),
                Some(&principal_a_current),
                None,
                None,
                Some(epochs_a),
            )
            .await
            .expect("A's reconciled cache can issue its own ticket");
        assert!(
            state_db_a
                .reserve_automatic_turn_continuation(
                    thread_id,
                    &ticket_a,
                    &principal_a_current,
                    "manager-a-client-2",
                    "start",
                    None,
                )
                .await?
        );
        Ok(())
    }

    #[tokio::test]
    async fn truncated_auth_save_keeps_the_durable_transition_gate_closed() -> anyhow::Result<()> {
        let state_home = tempdir()?;
        let codex_home = tempdir()?;
        let state_db = codex_state::StateRuntime::init(
            SqliteConfig::new_for_testing(state_home.path().abs()),
            "test-provider".to_string(),
        )
        .await?;
        let manager = auth_manager(codex_home.path()).await;
        let transition = AuthTransition::begin(
            Some(&state_db),
            Arc::clone(&manager),
            codex_home.path().to_path_buf(),
        )
        .await?;
        let generation = transition.generation.expect("State transition generation");

        // Model a credential writer that replaced/truncated the file before returning an error.
        // Recovery must not clear the gate based on the manager's previous cached identity.
        std::fs::write(codex_home.path().join("auth.json"), b"{ truncated")?;
        assert!(transition.complete().await.is_err());
        assert_eq!(
            state_db.pending_automatic_turn_auth_transition().await?,
            Some(generation),
            "a malformed persisted auth value cannot reopen continuation admission"
        );
        Ok(())
    }
}
