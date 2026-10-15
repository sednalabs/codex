use crate::runtime::StateRuntime;
use codex_protocol::ThreadId;
use codex_protocol::automatic_turn::AutomaticTurnContinuationRef;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use log::warn;
use uuid::Uuid;

const OUTCOME_STARTED: &str = "started";
const OUTCOME_RECOVERED: &str = "recovered";
const OUTCOME_REBLOCKED: &str = "policy_blocked_again";
const OUTCOME_EXHAUSTED: &str = "exhausted";
const OUTCOME_FAILED: &str = "failed";
const OUTCOME_ABORTED: &str = "aborted";
const OUTCOME_ACCEPTED: &str = "accepted";
const OUTCOME_SUPPRESSED: &str = "suppressed";
const PROVENANCE_SOURCE: &str = "server_side_one_time_ticket";
const EXPECTED_POLICY_RETRY_TEXT: &str = "continue";
const EXPECTED_TRIGGER_CONTEXT_FINGERPRINT: &str = "start:no-expected-turn";
const EXPECTED_RETRY_REASON: &str = "cyber_policy";
const MAX_ATTEMPTS: i64 = 3;

#[derive(Clone, Copy, Debug)]
struct CompletedAutomaticTurn;

impl StateRuntime {
    /// Project automatic-turn metadata against this runtime's actual thread and a preceding
    /// policy error recorded for that thread. This unbound entry point never returns a reference
    /// that an app-server client could redeem.
    ///
    /// The return value is only an opaque continuation reference when an authenticated principal
    /// is present; the authoritative capability remains in State.
    pub async fn record_automatic_turn_event(
        &self,
        thread_id: ThreadId,
        event: &Event,
    ) -> Option<String> {
        self.record_automatic_turn_event_with_principal(thread_id, event, /*principal*/ None)
            .await
    }

    /// Project one event with a server-authenticated principal associated with its submission.
    /// The principal identity is optional for legacy callers, direct state-runtime tests, and
    /// authenticated providers that do not expose a stable non-secret identity.
    pub async fn record_automatic_turn_event_with_principal(
        &self,
        thread_id: ThreadId,
        event: &Event,
        principal: Option<&str>,
    ) -> Option<String> {
        if self.usage_ledger_pool().is_closed() {
            return None;
        }
        match self
            .record_automatic_turn_event_inner(
                thread_id, event, principal, /*client_user_message_id*/ None,
                /*continuation_ref*/ None, /*expected_epochs*/ None,
            )
            .await
        {
            Ok(reference) => reference,
            Err(err) => {
                warn!("automatic turn usage projection: {err}");
                None
            }
        }
    }

    /// Project one event with both its authenticated principal and the client message identity
    /// retained by the session while the operation is in flight. The latter lets an abort clean
    /// up an admission even if the user item was not emitted before interruption.
    pub async fn record_automatic_turn_event_with_principal_and_client_user_message_id(
        &self,
        thread_id: ThreadId,
        event: &Event,
        principal: Option<&str>,
        client_user_message_id: Option<&str>,
    ) -> Option<String> {
        if self.usage_ledger_pool().is_closed() {
            return None;
        }
        match self
            .record_automatic_turn_event_inner(
                thread_id,
                event,
                principal,
                client_user_message_id,
                /*continuation_ref*/ None,
                /*expected_epochs*/ None,
            )
            .await
        {
            Ok(reference) => reference,
            Err(err) => {
                warn!("automatic turn usage projection: {err}");
                None
            }
        }
    }

    /// Project an event with authenticated in-memory submission context. `continuation_ref` is
    /// never read from the UserMessage item; it is supplied only by Core's server-owned turn
    /// context after the reference was redeemed and the State ticket was reserved.
    pub async fn record_automatic_turn_event_with_submission_context(
        &self,
        thread_id: ThreadId,
        event: &Event,
        principal: Option<&str>,
        client_user_message_id: Option<&str>,
        continuation_ref: Option<&str>,
        expected_epochs: Option<(i64, i64)>,
    ) -> Option<String> {
        if self.usage_ledger_pool().is_closed() {
            return None;
        }
        match self
            .record_automatic_turn_event_inner(
                thread_id,
                event,
                principal,
                client_user_message_id,
                continuation_ref,
                expected_epochs,
            )
            .await
        {
            Ok(reference_details) => reference_details,
            Err(err) => {
                warn!("automatic turn usage projection: {err}");
                None
            }
        }
    }

    /// Read the capability for a currently pending policy retry. This is intentionally a
    /// read-only accessor: the ticket is consumed only when the corresponding user message is
    /// projected by `insert_validated_automatic_turn`.
    pub async fn automatic_turn_capability(
        &self,
        thread_id: ThreadId,
        trigger_turn_id: &str,
    ) -> Option<String> {
        if trigger_turn_id.is_empty() || self.usage_ledger_pool().is_closed() {
            return None;
        }
        sqlx::query_scalar(
            "SELECT capability FROM usage_automatic_turn_eligibility WHERE thread_id = ? AND trigger_turn_id = ?",
        )
        .bind(thread_id.to_string())
        .bind(trigger_turn_id)
        .fetch_optional(self.usage_ledger_pool().as_ref())
        .await
        .ok()
            .flatten()
    }

    /// Return the opaque transport reference for a pending ticket. The reference is not itself
    /// authorization: every redemption is bound to the current thread, stable authenticated
    /// principal identity, operation, epochs, and a one-time State update.
    pub async fn automatic_turn_continuation_reference(
        &self,
        thread_id: ThreadId,
        trigger_turn_id: &str,
    ) -> Option<AutomaticTurnContinuationRef> {
        if trigger_turn_id.is_empty() || self.usage_ledger_pool().is_closed() {
            return None;
        }
        let value: Option<String> = sqlx::query_scalar(
            "SELECT continuation_ref FROM usage_automatic_turn_eligibility WHERE thread_id = ? AND trigger_turn_id = ? AND principal_identity IS NOT NULL AND admitted_client_user_message_id IS NULL",
        )
        .bind(thread_id.to_string())
        .bind(trigger_turn_id)
        .fetch_optional(self.usage_ledger_pool().as_ref())
        .await
        .ok()
        .flatten();
        value.and_then(AutomaticTurnContinuationRef::new)
    }

    /// Resolve the trigger identity for an opaque reference without exposing its capability.
    pub async fn automatic_turn_trigger_for_continuation_ref(
        &self,
        thread_id: ThreadId,
        continuation_ref: &str,
    ) -> Option<String> {
        if continuation_ref.is_empty() || self.usage_ledger_pool().is_closed() {
            return None;
        }
        sqlx::query_scalar(
            "SELECT trigger_turn_id FROM usage_automatic_turn_eligibility WHERE thread_id = ? AND continuation_ref = ? AND admitted_client_user_message_id IS NULL",
        )
        .bind(thread_id.to_string())
        .bind(continuation_ref)
        .fetch_optional(self.usage_ledger_pool().as_ref())
        .await
        .ok()
        .flatten()
    }

    /// Return the principal bound to a pending capability. `None` in the outer option means no
    /// pending capability; a row with a null principal is a legacy unbound ticket.
    pub async fn automatic_turn_capability_principal(
        &self,
        thread_id: ThreadId,
        trigger_turn_id: &str,
        capability: &str,
    ) -> Option<Option<String>> {
        if trigger_turn_id.is_empty()
            || capability.is_empty()
            || self.usage_ledger_pool().is_closed()
        {
            return None;
        }
        sqlx::query_scalar(
            "SELECT principal_identity FROM usage_automatic_turn_eligibility WHERE thread_id = ? AND trigger_turn_id = ? AND capability = ?",
        )
        .bind(thread_id.to_string())
        .bind(trigger_turn_id)
        .bind(capability)
        .fetch_optional(self.usage_ledger_pool().as_ref())
        .await
        .ok()
    }

    /// Return the complete server-bound contract for a pending capability. The operation and
    /// context fields are written when the policy event is associated with its authenticated
    /// submission; a ticket without them is intentionally not consumable by app-server requests.
    pub async fn automatic_turn_capability_contract(
        &self,
        thread_id: ThreadId,
        trigger_turn_id: &str,
        capability: &str,
    ) -> Option<(
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    )> {
        if trigger_turn_id.is_empty()
            || capability.is_empty()
            || self.usage_ledger_pool().is_closed()
        {
            return None;
        }
        sqlx::query_as(
            "SELECT principal_identity, allowed_operation_kind, allowed_expected_turn_id, trigger_context_fingerprint FROM usage_automatic_turn_eligibility WHERE thread_id = ? AND trigger_turn_id = ? AND capability = ?",
        )
        .bind(thread_id.to_string())
        .bind(trigger_turn_id)
        .bind(capability)
        .fetch_optional(self.usage_ledger_pool().as_ref())
        .await
        .ok()
        .flatten()
    }

    /// Return the settings and authentication epochs captured when a ticket was issued.
    /// Epochs are fail-closed: settings/auth transitions remove the pending ticket before the
    /// changed state is acknowledged, so a ticket from an earlier observed state cannot execute.
    pub async fn automatic_turn_capability_epochs(
        &self,
        thread_id: ThreadId,
        trigger_turn_id: &str,
        capability: &str,
    ) -> Option<(i64, i64)> {
        if trigger_turn_id.is_empty()
            || capability.is_empty()
            || self.usage_ledger_pool().is_closed()
        {
            return None;
        }
        sqlx::query_as(
            "SELECT settings_generation, auth_generation FROM usage_automatic_turn_eligibility WHERE thread_id = ? AND trigger_turn_id = ? AND capability = ?",
        )
        .bind(thread_id.to_string())
        .bind(trigger_turn_id)
        .bind(capability)
        .fetch_optional(self.usage_ledger_pool().as_ref())
        .await
        .ok()
        .flatten()
    }

    /// Return the current app-server-observed settings and authentication epochs for a thread.
    pub async fn automatic_turn_current_epochs(&self, thread_id: ThreadId) -> Option<(i64, i64)> {
        if self.usage_ledger_pool().is_closed() {
            return None;
        }
        sqlx::query_as(
            "SELECT COALESCE(c.settings_generation, 0), a.generation FROM usage_automatic_turn_auth_epochs AS a LEFT JOIN usage_automatic_turn_chains AS c ON c.thread_id = ? WHERE a.id = 1 AND a.transition_pending = 0",
        )
        .bind(thread_id.to_string())
        .fetch_optional(self.usage_ledger_pool().as_ref())
        .await
        .ok()
        .flatten()
    }

    /// Return the current auth epoch only when no account transition is pending. The owning
    /// AuthManager records its own reconciliation witness after a successful reload.
    pub async fn automatic_turn_current_auth_epoch(&self) -> Option<i64> {
        if self.usage_ledger_pool().is_closed() {
            return None;
        }
        sqlx::query_scalar::<_, i64>(
            "SELECT generation FROM usage_automatic_turn_auth_epochs WHERE id = 1 AND transition_pending = 0",
        )
        .fetch_optional(self.usage_ledger_pool().as_ref())
        .await
        .ok()
        .flatten()
    }

    /// Return the epochs captured by a pending continuation reference. This value is context,
    /// not authority; `reserve_automatic_turn_continuation` revalidates it atomically.
    pub async fn automatic_turn_continuation_epochs(
        &self,
        thread_id: ThreadId,
        continuation_ref: &str,
    ) -> Option<(i64, i64)> {
        if continuation_ref.is_empty() || self.usage_ledger_pool().is_closed() {
            return None;
        }
        sqlx::query_as(
            "SELECT settings_generation, auth_generation FROM usage_automatic_turn_eligibility WHERE thread_id = ? AND continuation_ref = ? AND principal_identity IS NOT NULL AND admitted_client_user_message_id IS NULL",
        )
        .bind(thread_id.to_string())
        .bind(continuation_ref)
        .fetch_optional(self.usage_ledger_pool().as_ref())
        .await
        .ok()
        .flatten()
    }

    /// Advance the settings epoch and invalidate any pending retry before a settings update is
    /// queued. The core may apply the queued operation later, but no old ticket remains valid.
    pub async fn invalidate_automatic_turn_capabilities_for_thread(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<()> {
        if self.usage_ledger_pool().is_closed() {
            return Ok(());
        }
        let thread_id = thread_id.to_string();
        let mut tx = self
            .usage_ledger_pool()
            .begin_with("BEGIN IMMEDIATE")
            .await?;
        sqlx::query(
            "INSERT INTO usage_automatic_turn_chains (thread_id) VALUES (?) ON CONFLICT(thread_id) DO NOTHING",
        )
        .bind(&thread_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE usage_automatic_turn_chains SET settings_generation = settings_generation + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE thread_id = ?",
        )
        .bind(&thread_id)
        .execute(&mut *tx)
        .await?;
        // Advance the retry generation as well so a new policy occurrence that reuses a regular
        // turn id is not mistaken for the old occurrence in the pre-update generation.
        reset_chain_in_transaction(&mut tx, &thread_id).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Advance the process-wide authentication epoch and invalidate all pending retries before
    /// a new account/auth state can serve a request.
    pub async fn invalidate_all_automatic_turn_capabilities(&self) -> anyhow::Result<()> {
        if self.usage_ledger_pool().is_closed() {
            return Ok(());
        }
        let mut tx = self
            .usage_ledger_pool()
            .begin_with("BEGIN IMMEDIATE")
            .await?;
        sqlx::query(
            "UPDATE usage_automatic_turn_auth_epochs SET generation = generation + 1 WHERE id = 1",
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE usage_automatic_turn_chains SET generation = generation + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM usage_automatic_turn_eligibility")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Begin a durable app-server authentication transition before changing persisted or cached
    /// account state. This revokes existing tickets and prevents new ticket issue/redemption
    /// until the matching transition is completed after auth has been reloaded.
    pub async fn begin_automatic_turn_auth_transition(&self) -> anyhow::Result<i64> {
        let pool = self.usage_ledger_pool();
        anyhow::ensure!(!pool.is_closed(), "automatic-turn state is unavailable");
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "UPDATE usage_automatic_turn_auth_epochs SET generation = generation + 1, transition_pending = 1 WHERE id = 1",
        )
        .execute(&mut *tx)
        .await?;
        let generation: i64 = sqlx::query_scalar(
            "SELECT generation FROM usage_automatic_turn_auth_epochs WHERE id = 1 AND transition_pending = 1",
        )
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE usage_automatic_turn_chains SET generation = generation + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM usage_automatic_turn_eligibility")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(generation)
    }

    /// Return the persisted transition token, if startup found an interrupted auth change.
    /// The caller must first establish the current AuthManager state, and must serialize startup
    /// recovery with account transitions before completing this token.
    pub async fn pending_automatic_turn_auth_transition(&self) -> anyhow::Result<Option<i64>> {
        let generation = sqlx::query_scalar::<_, i64>(
            "SELECT generation FROM usage_automatic_turn_auth_epochs WHERE id = 1 AND transition_pending = 1",
        )
        .fetch_optional(self.usage_ledger_pool().as_ref())
        .await?;
        Ok(generation)
    }

    /// Complete a transition only if it still owns the pending generation. A stale completion
    /// cannot clear a newer login/logout transition. Call only after the current AuthManager has
    /// reloaded and established its current authenticated state.
    pub async fn complete_automatic_turn_auth_transition(
        &self,
        transition_generation: i64,
    ) -> anyhow::Result<bool> {
        let pool = self.usage_ledger_pool();
        anyhow::ensure!(!pool.is_closed(), "automatic-turn state is unavailable");
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let updated = sqlx::query(
            "UPDATE usage_automatic_turn_auth_epochs SET generation = generation + 1, transition_pending = 0 WHERE id = 1 AND generation = ? AND transition_pending = 1",
        )
        .bind(transition_generation)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            tx.rollback().await?;
            return Ok(false);
        }
        sqlx::query(
            "UPDATE usage_automatic_turn_chains SET generation = generation + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM usage_automatic_turn_eligibility")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Bind the operation, target precondition, and current thread context to a pending ticket.
    /// Re-delivery of the exact same policy event is idempotent; a conflicting binding fails
    /// closed and leaves the original contract intact.
    pub async fn bind_automatic_turn_capability_contract(
        &self,
        thread_id: ThreadId,
        trigger_turn_id: &str,
        capability: &str,
        operation_kind: &str,
        expected_turn_id: Option<&str>,
        context_fingerprint: &str,
    ) -> anyhow::Result<bool> {
        if trigger_turn_id.is_empty()
            || capability.is_empty()
            || operation_kind.is_empty()
            || context_fingerprint.is_empty()
            || operation_kind != "start"
            || expected_turn_id.is_some()
            || self.usage_ledger_pool().is_closed()
        {
            return Ok(false);
        }
        let result = sqlx::query(
            "UPDATE usage_automatic_turn_eligibility SET allowed_operation_kind = ?, allowed_expected_turn_id = ?, trigger_context_fingerprint = ? WHERE thread_id = ? AND trigger_turn_id = ? AND capability = ? AND (allowed_operation_kind IS NULL OR (allowed_operation_kind = ? AND allowed_expected_turn_id IS ? AND trigger_context_fingerprint = ?))",
        )
        .bind(operation_kind)
        .bind(expected_turn_id)
        .bind(context_fingerprint)
        .bind(thread_id.to_string())
        .bind(trigger_turn_id)
        .bind(capability)
        .bind(operation_kind)
        .bind(expected_turn_id)
        .bind(context_fingerprint)
        .execute(self.usage_ledger_pool().as_ref())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Invalidate a ticket after its trigger-bound context or operation contract no longer
    /// matches. Resetting the chain advances the generation so a stale request cannot consume a
    /// replacement ticket minted from the old event.
    pub async fn invalidate_automatic_turn_capability(
        &self,
        thread_id: ThreadId,
        trigger_turn_id: &str,
        capability: &str,
    ) -> anyhow::Result<bool> {
        if trigger_turn_id.is_empty()
            || capability.is_empty()
            || self.usage_ledger_pool().is_closed()
        {
            return Ok(false);
        }
        let mut tx = self
            .usage_ledger_pool()
            .begin_with("BEGIN IMMEDIATE")
            .await?;
        let result = sqlx::query(
            "DELETE FROM usage_automatic_turn_eligibility WHERE thread_id = ? AND trigger_turn_id = ? AND capability = ?",
        )
        .bind(thread_id.to_string())
        .bind(trigger_turn_id)
        .bind(capability)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 1 {
            reset_chain_in_transaction(&mut tx, &thread_id.to_string()).await?;
        }
        tx.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    /// Atomically admit one validated automatic operation. The capability remains in the
    /// eligibility row until the corresponding user item is projected, but the admission marker
    /// makes concurrent requests for the same ticket mutually exclusive.
    // Keep persistence, provenance, authorization, and replay identities explicit and separate.
    #[allow(clippy::too_many_arguments)]
    pub async fn reserve_automatic_turn_capability(
        &self,
        thread_id: ThreadId,
        trigger_turn_id: &str,
        capability: &str,
        principal: &str,
        client_user_message_id: &str,
        operation_kind: &str,
        expected_turn_id: Option<&str>,
    ) -> anyhow::Result<bool> {
        if trigger_turn_id.is_empty()
            || capability.is_empty()
            || principal.is_empty()
            || client_user_message_id.is_empty()
            || operation_kind != "start"
            || expected_turn_id.is_some()
            || self.usage_ledger_pool().is_closed()
        {
            return Ok(false);
        }
        let result = sqlx::query(
            "UPDATE usage_automatic_turn_eligibility SET admitted_client_user_message_id = ?, admitted_operation_kind = ?, admitted_expected_turn_id = ? WHERE thread_id = ? AND trigger_turn_id = ? AND capability = ? AND principal_identity = ? AND allowed_operation_kind = ? AND allowed_expected_turn_id IS ? AND admitted_client_user_message_id IS NULL AND EXISTS (SELECT 1 FROM usage_automatic_turn_auth_epochs WHERE id = 1 AND transition_pending = 0)",
        )
        .bind(client_user_message_id)
        .bind(operation_kind)
        .bind(expected_turn_id)
        .bind(thread_id.to_string())
        .bind(trigger_turn_id)
        .bind(capability)
        .bind(principal)
        .bind(operation_kind)
        .bind(expected_turn_id)
        .execute(self.usage_ledger_pool().as_ref())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Atomically reserve a ticket using only its opaque transport reference. The ticket's
    /// capability stays server-side; the exact principal-identity/thread/operation/epoch checks happen in
    /// this transaction before the in-memory Core submission receives the reference.
    pub async fn reserve_automatic_turn_continuation(
        &self,
        thread_id: ThreadId,
        continuation_ref: &str,
        principal: &str,
        client_user_message_id: &str,
        operation_kind: &str,
        expected_turn_id: Option<&str>,
    ) -> anyhow::Result<bool> {
        if continuation_ref.is_empty()
            || principal.is_empty()
            || client_user_message_id.is_empty()
            || operation_kind != "start"
            || expected_turn_id.is_some()
            || self.usage_ledger_pool().is_closed()
        {
            return Ok(false);
        }
        let mut tx = self
            .usage_ledger_pool()
            .begin_with("BEGIN IMMEDIATE")
            .await?;
        let transition_pending: i64 = sqlx::query_scalar(
            "SELECT transition_pending FROM usage_automatic_turn_auth_epochs WHERE id = 1",
        )
        .fetch_one(&mut *tx)
        .await?;
        if transition_pending != 0 {
            tx.rollback().await?;
            return Ok(false);
        }
        let ticket = sqlx::query_as::<_, (String, Option<String>, i64, i64, Option<String>, Option<String>, Option<String>, Option<String>, String)>(
            "SELECT trigger_turn_id, principal_identity, settings_generation, auth_generation, allowed_operation_kind, allowed_expected_turn_id, admitted_client_user_message_id, trigger_context_fingerprint, retry_reason FROM usage_automatic_turn_eligibility WHERE thread_id = ? AND continuation_ref = ?",
        )
        .bind(thread_id.to_string())
        .bind(continuation_ref)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((
            trigger_turn_id,
            expected_principal,
            settings_generation,
            auth_generation,
            allowed_operation_kind,
            allowed_expected_turn_id,
            admitted_client_user_message_id,
            trigger_context_fingerprint,
            retry_reason,
        )) = ticket
        else {
            tx.rollback().await?;
            return Ok(false);
        };
        let current_epochs: Option<(i64, i64)> = sqlx::query_as(
            "SELECT c.settings_generation, a.generation FROM usage_automatic_turn_chains AS c CROSS JOIN usage_automatic_turn_auth_epochs AS a WHERE c.thread_id = ? AND a.id = 1",
        )
        .bind(thread_id.to_string())
        .fetch_optional(&mut *tx)
        .await?;
        if expected_principal.as_deref() != Some(principal)
            || allowed_operation_kind.as_deref() != Some(operation_kind)
            || allowed_expected_turn_id != expected_turn_id.map(str::to_owned)
            || admitted_client_user_message_id.is_some()
            || trigger_context_fingerprint.as_deref() != Some(EXPECTED_TRIGGER_CONTEXT_FINGERPRINT)
            || retry_reason != EXPECTED_RETRY_REASON
            || current_epochs != Some((settings_generation, auth_generation))
        {
            tx.rollback().await?;
            return Ok(false);
        }
        let result = sqlx::query(
            "UPDATE usage_automatic_turn_eligibility SET admitted_client_user_message_id = ?, admitted_operation_kind = ?, admitted_expected_turn_id = ? WHERE thread_id = ? AND trigger_turn_id = ? AND continuation_ref = ? AND principal_identity = ? AND allowed_operation_kind = ? AND allowed_expected_turn_id IS ? AND trigger_context_fingerprint = ? AND retry_reason = ? AND admitted_client_user_message_id IS NULL",
        )
        .bind(client_user_message_id)
        .bind(operation_kind)
        .bind(expected_turn_id)
        .bind(thread_id.to_string())
        .bind(&trigger_turn_id)
        .bind(continuation_ref)
        .bind(principal)
        .bind(operation_kind)
        .bind(expected_turn_id)
        .bind(EXPECTED_TRIGGER_CONTEXT_FINGERPRINT)
        .bind(EXPECTED_RETRY_REASON)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    /// Release an admission if the core submission failed before the operation entered the
    /// session loop. A successful submission keeps the reservation until projection consumes it.
    pub async fn release_automatic_turn_capability(
        &self,
        thread_id: ThreadId,
        capability: &str,
        client_user_message_id: &str,
    ) -> anyhow::Result<bool> {
        if capability.is_empty()
            || client_user_message_id.is_empty()
            || self.usage_ledger_pool().is_closed()
        {
            return Ok(false);
        }
        let result = sqlx::query(
            "UPDATE usage_automatic_turn_eligibility SET admitted_client_user_message_id = NULL, admitted_operation_kind = NULL, admitted_expected_turn_id = NULL WHERE thread_id = ? AND capability = ? AND admitted_client_user_message_id = ?",
        )
        .bind(thread_id.to_string())
        .bind(capability)
        .bind(client_user_message_id)
        .execute(self.usage_ledger_pool().as_ref())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn release_automatic_turn_continuation(
        &self,
        thread_id: ThreadId,
        continuation_ref: &str,
        client_user_message_id: &str,
    ) -> anyhow::Result<bool> {
        if continuation_ref.is_empty()
            || client_user_message_id.is_empty()
            || self.usage_ledger_pool().is_closed()
        {
            return Ok(false);
        }
        let result = sqlx::query(
            "UPDATE usage_automatic_turn_eligibility SET admitted_client_user_message_id = NULL, admitted_operation_kind = NULL, admitted_expected_turn_id = NULL WHERE thread_id = ? AND continuation_ref = ? AND admitted_client_user_message_id = ?",
        )
        .bind(thread_id.to_string())
        .bind(continuation_ref)
        .bind(client_user_message_id)
        .execute(self.usage_ledger_pool().as_ref())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Return the server-selected trigger identity and capability for the current policy event.
    /// Trigger identities may carry an occurrence suffix when a regular turn id is reused.
    pub async fn automatic_turn_capability_for_turn(
        &self,
        thread_id: ThreadId,
        turn_id: &str,
    ) -> Option<(String, String)> {
        self.automatic_turn_capability_for_turn_with_principal(thread_id, turn_id)
            .await
            .map(|(trigger_turn_id, capability, _)| (trigger_turn_id, capability))
    }

    /// Return the server-selected trigger identity, capability, and bound principal for a
    /// current policy event.
    pub async fn automatic_turn_capability_for_turn_with_principal(
        &self,
        thread_id: ThreadId,
        turn_id: &str,
    ) -> Option<(String, String, Option<String>)> {
        if turn_id.is_empty() || self.usage_ledger_pool().is_closed() {
            return None;
        }
        sqlx::query_as(
            "SELECT trigger_turn_id, capability, principal_identity FROM usage_automatic_turn_eligibility WHERE thread_id = ? AND (trigger_turn_id = ? OR trigger_turn_id LIKE ?) LIMIT 1",
        )
        .bind(thread_id.to_string())
        .bind(turn_id)
        .bind(format!("{turn_id}#%"))
        .fetch_optional(self.usage_ledger_pool().as_ref())
        .await
        .ok()
        .flatten()
    }

    async fn record_automatic_turn_event_inner(
        &self,
        thread_id: ThreadId,
        event: &Event,
        principal: Option<&str>,
        client_user_message_id: Option<&str>,
        continuation_ref: Option<&str>,
        expected_epochs: Option<(i64, i64)>,
    ) -> anyhow::Result<Option<String>> {
        match &event.msg {
            EventMsg::ItemCompleted(item_event) => {
                if let TurnItem::UserMessage(item) = &item_event.item {
                    self.record_user_message(
                        thread_id,
                        &event.id,
                        &item_event.turn_id,
                        item,
                        principal,
                        continuation_ref,
                        expected_epochs,
                    )
                    .await?;
                }
                Ok(None)
            }
            // This is the legacy projection of ItemCompleted. The canonical item event above is
            // preferred. Legacy events do not carry structured content, so an envelope in this
            // shape is intentionally not eligible for projection.
            EventMsg::UserMessage(_) => Ok(None),
            EventMsg::TurnComplete(turn_complete) => {
                let outcome = turn_complete
                    .error
                    .as_ref()
                    .map(Self::automatic_turn_error_outcome)
                    .unwrap_or(OUTCOME_RECOVERED);
                let completed = self
                    .complete_automatic_turn(
                        thread_id,
                        event.id.as_str(),
                        turn_complete.turn_id.as_str(),
                        outcome,
                        client_user_message_id,
                    )
                    .await?;
                if let Some(error) = turn_complete.error.as_ref()
                    && error
                        .codex_error_info
                        .as_ref()
                        .is_some_and(|info| matches!(info, &CodexErrorInfo::CyberPolicy))
                {
                    return self
                        .record_policy_trigger(
                            thread_id,
                            event.id.as_str(),
                            turn_complete.turn_id.as_str(),
                            completed,
                            principal,
                            expected_epochs,
                        )
                        .await;
                }
                Ok(None)
            }
            EventMsg::Error(error) => {
                let outcome = Self::automatic_turn_error_outcome(error);
                let completed = self
                    .complete_automatic_turn(
                        thread_id,
                        event.id.as_str(),
                        event.id.as_str(),
                        outcome,
                        client_user_message_id,
                    )
                    .await?;
                if error
                    .codex_error_info
                    .as_ref()
                    .is_some_and(|info| matches!(info, &CodexErrorInfo::CyberPolicy))
                {
                    return self
                        .record_policy_trigger(
                            thread_id,
                            event.id.as_str(),
                            event.id.as_str(),
                            completed,
                            principal,
                            expected_epochs,
                        )
                        .await;
                }
                Ok(None)
            }
            EventMsg::TurnAborted(turn_aborted) => {
                let turn_id = turn_aborted.turn_id.as_deref().unwrap_or(event.id.as_str());
                self.complete_automatic_turn(
                    thread_id,
                    event.id.as_str(),
                    turn_id,
                    OUTCOME_ABORTED,
                    client_user_message_id,
                )
                .await?;
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    async fn record_user_message(
        &self,
        thread_id: ThreadId,
        event_occurrence_id: &str,
        turn_id: &str,
        item: &codex_protocol::items::UserMessageItem,
        principal: Option<&str>,
        continuation_ref: Option<&str>,
        expected_epochs: Option<(i64, i64)>,
    ) -> anyhow::Result<()> {
        let Some(client_id) = item.client_id.as_deref() else {
            return Ok(());
        };
        let Some(continuation_ref) = continuation_ref else {
            return Ok(());
        };
        let _ = self
            .insert_validated_automatic_turn(
                thread_id,
                event_occurrence_id,
                turn_id,
                client_id,
                continuation_ref,
                expected_epochs,
                item,
                principal,
            )
            .await?;
        Ok(())
    }

    // Keep persistence, provenance, authorization, and replay identities explicit and separate.
    #[allow(clippy::too_many_arguments)]
    async fn insert_validated_automatic_turn(
        &self,
        thread_id: ThreadId,
        event_occurrence_id: &str,
        turn_id: &str,
        client_id: &str,
        continuation_ref: &str,
        expected_epochs: Option<(i64, i64)>,
        item: &codex_protocol::items::UserMessageItem,
        principal: Option<&str>,
    ) -> anyhow::Result<bool> {
        if !is_expected_policy_retry_content(item) {
            return Ok(false);
        }
        let pool = self.usage_ledger_pool();
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let actual_thread_id = thread_id.to_string();

        // Idempotent delivery of the exact same client id is a no-op. This check occurs before
        // consulting eligibility so a replay cannot consume or replace a later ticket.
        let already_projected: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM usage_automatic_turns WHERE thread_id = ? AND client_user_message_id = ?",
        )
        .bind(&actual_thread_id)
        .bind(client_id)
        .fetch_optional(&mut *tx)
        .await?;
        if already_projected.is_some() {
            tx.commit().await?;
            return Ok(true);
        }

        let transition_pending: i64 = sqlx::query_scalar(
            "SELECT transition_pending FROM usage_automatic_turn_auth_epochs WHERE id = 1",
        )
        .fetch_one(&mut *tx)
        .await?;
        if transition_pending != 0 {
            tx.rollback().await?;
            return Ok(false);
        }

        let Some((
            trigger_turn_id,
            trigger_occurrence_id,
            generation,
            expected_attempt,
            max_attempts,
            capability,
            expected_continuation_ref,
            expected_principal,
            admitted_client_user_message_id,
            admitted_operation_kind,
            admitted_expected_turn_id,
            settings_generation,
            auth_generation,
            trigger_context_fingerprint,
            retry_reason,
        )) = sqlx::query_as::<_, (
            String,
            Option<String>,
            i64,
            i64,
            i64,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            i64,
            i64,
            Option<String>,
            String,
        )>(
            "SELECT trigger_turn_id, event_occurrence_id, generation, next_attempt, max_attempts, capability, continuation_ref, principal_identity, admitted_client_user_message_id, admitted_operation_kind, admitted_expected_turn_id, settings_generation, auth_generation, trigger_context_fingerprint, retry_reason FROM usage_automatic_turn_eligibility WHERE thread_id = ?",
            )
            .bind(&actual_thread_id)
            .fetch_optional(&mut *tx)
            .await?
        else {
            return Ok(false);
        };
        let trigger_occurrence_id =
            trigger_occurrence_id.unwrap_or_else(|| trigger_turn_id.clone());
        let current_epochs: Option<(i64, i64)> = sqlx::query_as(
            "SELECT c.settings_generation, a.generation FROM usage_automatic_turn_chains AS c CROSS JOIN usage_automatic_turn_auth_epochs AS a WHERE c.thread_id = ? AND a.id = 1",
        )
        .bind(&actual_thread_id)
        .fetch_optional(&mut *tx)
        .await?;

        // The transport reference is matched against the durable server-side ticket. All
        // attribution fields come from State, not from user content or a serialized envelope.
        if principal.is_none()
            || expected_epochs != Some((settings_generation, auth_generation))
            || current_epochs != Some((settings_generation, auth_generation))
            || continuation_ref != expected_continuation_ref
            || expected_principal.as_deref() != principal
            || admitted_client_user_message_id.as_deref() != Some(client_id)
            || admitted_client_user_message_id
                .as_deref()
                .is_some_and(|admitted| admitted != client_id)
            || admitted_operation_kind
                .as_deref()
                .is_some_and(|kind| kind != "start")
            || admitted_expected_turn_id.is_some()
            || trigger_context_fingerprint.as_deref() != Some(EXPECTED_TRIGGER_CONTEXT_FINGERPRINT)
            || retry_reason != EXPECTED_RETRY_REASON
        {
            return Ok(false);
        }

        sqlx::query(
            r#"
INSERT INTO usage_automatic_turns (
    thread_id,
    client_user_message_id,
    trigger_turn_id,
    event_occurrence_id,
    turn_id,
    principal_identity,
    origin,
    generation,
    capability,
    attempt,
    max_attempts,
    provenance_source,
    outcome
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(thread_id, client_user_message_id) DO NOTHING
"#,
        )
        .bind(&actual_thread_id)
        .bind(client_id)
        .bind(&trigger_turn_id)
        .bind(event_occurrence_id)
        .bind(turn_id)
        .bind(principal)
        .bind("policy_retry")
        .bind(generation)
        .bind(&capability)
        .bind(expected_attempt)
        .bind(max_attempts)
        .bind(PROVENANCE_SOURCE)
        .bind(OUTCOME_STARTED)
        .execute(&mut *tx)
        .await?;

        // Consume the eligibility ticket atomically. A replay after this point has no ticket and
        // therefore cannot mint a second attempt or steal a later chain generation.
        sqlx::query(
            "DELETE FROM usage_automatic_turn_eligibility WHERE thread_id = ? AND trigger_turn_id = ? AND event_occurrence_id = ? AND generation = ? AND next_attempt = ? AND capability = ?",
        )
        .bind(&actual_thread_id)
        .bind(&trigger_turn_id)
        .bind(&trigger_occurrence_id)
        .bind(generation)
        .bind(expected_attempt)
        .bind(&capability)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE usage_automatic_turn_triggers SET outcome = ? WHERE thread_id = ? AND trigger_turn_id = ? AND event_occurrence_id = ? AND generation = ? AND attempt = ?",
        )
        .bind(OUTCOME_ACCEPTED)
        .bind(&actual_thread_id)
        .bind(&trigger_turn_id)
        .bind(&trigger_occurrence_id)
        .bind(generation)
        .bind(expected_attempt)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    async fn record_policy_trigger(
        &self,
        thread_id: ThreadId,
        event_occurrence_id: &str,
        trigger_turn_id: &str,
        completed: Option<CompletedAutomaticTurn>,
        principal: Option<&str>,
        expected_epochs: Option<(i64, i64)>,
    ) -> anyhow::Result<Option<String>> {
        if trigger_turn_id.is_empty() {
            return Ok(None);
        }
        let pool = self.usage_ledger_pool();
        let actual_thread_id = thread_id.to_string();
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;

        // An abort is a durable terminal boundary. Late or reordered policy terminals for that
        // exact turn/event must remain idempotent after reset and may not mint a ticket in the
        // next generation or reopen the aborted chain.
        let aborted_tombstone: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM usage_automatic_turns WHERE thread_id = ? AND outcome = ? AND abort_event_occurrence_id = ? AND generation >= 0 AND attempt >= 1)",
        )
        .bind(&actual_thread_id)
        .bind(OUTCOME_ABORTED)
        .bind(event_occurrence_id)
        .fetch_one(&mut *tx)
        .await?;
        if aborted_tombstone {
            tx.commit().await?;
            return Ok(None);
        }

        sqlx::query(
            "INSERT INTO usage_automatic_turn_chains (thread_id) VALUES (?) ON CONFLICT(thread_id) DO NOTHING",
        )
        .bind(&actual_thread_id)
        .execute(&mut *tx)
        .await?;
        let generation: i64 = sqlx::query_scalar(
            "SELECT generation FROM usage_automatic_turn_chains WHERE thread_id = ?",
        )
        .bind(&actual_thread_id)
        .fetch_one(&mut *tx)
        .await?;
        let settings_generation: i64 = sqlx::query_scalar(
            "SELECT settings_generation FROM usage_automatic_turn_chains WHERE thread_id = ?",
        )
        .bind(&actual_thread_id)
        .fetch_one(&mut *tx)
        .await?;
        let auth_generation: i64 = sqlx::query_scalar(
            "SELECT generation FROM usage_automatic_turn_auth_epochs WHERE id = 1",
        )
        .fetch_one(&mut *tx)
        .await?;
        let transition_pending: i64 = sqlx::query_scalar(
            "SELECT transition_pending FROM usage_automatic_turn_auth_epochs WHERE id = 1",
        )
        .fetch_one(&mut *tx)
        .await?;
        let ticket_principal = if transition_pending == 0
            && expected_epochs == Some((settings_generation, auth_generation))
        {
            principal
        } else {
            None
        };

        // A pending ticket is authoritative evidence that a later/reordered event must not
        // replace the intended operation. It remains available until the intended envelope is
        // accepted or an explicit manual successful turn advances the generation.
        let pending = sqlx::query_scalar::<_, i64>(
            "SELECT 1 FROM usage_automatic_turn_eligibility WHERE thread_id = ? LIMIT 1",
        )
        .bind(&actual_thread_id)
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if pending && completed.is_none() {
            let _ = insert_suppressed_trigger(
                &mut tx,
                &actual_thread_id,
                trigger_turn_id,
                event_occurrence_id,
                generation,
                /*attempt*/ 1,
            )
            .await?;
            tx.commit().await?;
            return Ok(None);
        }

        let previous = sqlx::query_as::<_, (i64, String, String, i64)>(
            "SELECT attempt, outcome, trigger_turn_id, generation FROM usage_automatic_turns WHERE thread_id = ? AND generation = ? ORDER BY id DESC LIMIT 1",
        )
        .bind(&actual_thread_id)
        .bind(generation)
        .fetch_optional(&mut *tx)
        .await?;
        let next_attempt = match previous.as_ref() {
            Some((attempt, outcome, _, _)) if outcome == OUTCOME_REBLOCKED => *attempt + 1,
            Some((_, outcome, _, _)) if outcome == OUTCOME_EXHAUSTED => MAX_ATTEMPTS + 1,
            _ => 1,
        };
        if previous
            .as_ref()
            .is_some_and(|(_, outcome, _, _)| outcome == OUTCOME_EXHAUSTED)
            && completed.is_none()
        {
            tx.commit().await?;
            return Ok(None);
        }

        // Reusing a regular turn id is valid across attempts. Once the automatic row for that
        // turn has just completed, issue an occurrence-qualified trigger identity; otherwise the
        // exact previously seen identity is treated as a duplicate/stale event.
        let raw_trigger_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM usage_automatic_turn_triggers WHERE thread_id = ? AND trigger_turn_id = ? AND generation = ?)",
        )
        .bind(&actual_thread_id)
        .bind(trigger_turn_id)
        .bind(generation)
        .fetch_one(&mut *tx)
        .await?;
        if raw_trigger_exists && completed.is_none() {
            let _ = insert_suppressed_trigger(
                &mut tx,
                &actual_thread_id,
                trigger_turn_id,
                event_occurrence_id,
                generation,
                next_attempt,
            )
            .await?;
            tx.commit().await?;
            return Ok(None);
        }
        let trigger_identity = if completed.is_some() && raw_trigger_exists {
            format!("{trigger_turn_id}#{event_occurrence_id}")
        } else {
            trigger_turn_id.to_string()
        };

        // A trigger is seen before a ticket is issued. This closes the duplicate/stale path,
        // including events which arrive after a prior ticket was consumed.
        let trigger_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM usage_automatic_turn_triggers WHERE thread_id = ? AND trigger_turn_id = ? AND event_occurrence_id = ? AND generation = ? AND attempt = ?)",
        )
        .bind(&actual_thread_id)
        .bind(&trigger_identity)
        .bind(event_occurrence_id)
        .bind(generation)
        .bind(next_attempt)
        .fetch_one(&mut *tx)
        .await?;
        if trigger_exists && completed.is_none() {
            tx.commit().await?;
            return Ok(None);
        }

        let capability_and_reference = if next_attempt <= MAX_ATTEMPTS && ticket_principal.is_some()
        {
            let capability = Uuid::new_v4().to_string();
            let continuation_ref = Uuid::new_v4().simple().to_string();
            sqlx::query(
            "INSERT INTO usage_automatic_turn_eligibility (thread_id, trigger_turn_id, event_occurrence_id, generation, next_attempt, max_attempts, capability, continuation_ref, principal_identity, settings_generation, auth_generation, allowed_operation_kind, allowed_expected_turn_id, trigger_context_fingerprint, retry_reason) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'start', NULL, ?, ?)"
            )
            .bind(&actual_thread_id)
            .bind(&trigger_identity)
            .bind(event_occurrence_id)
            .bind(generation)
            .bind(next_attempt)
            .bind(MAX_ATTEMPTS)
            .bind(&capability)
            .bind(&continuation_ref)
            .bind(ticket_principal)
            .bind(settings_generation)
            .bind(auth_generation)
            .bind(EXPECTED_TRIGGER_CONTEXT_FINGERPRINT)
            .bind(EXPECTED_RETRY_REASON)
            .execute(&mut *tx)
            .await?;
            Some((capability, continuation_ref))
        } else {
            None
        };

        let outcome = if capability_and_reference.is_some() {
            "pending"
        } else if next_attempt <= MAX_ATTEMPTS {
            OUTCOME_SUPPRESSED
        } else {
            OUTCOME_EXHAUSTED
        };
        sqlx::query(
            "INSERT INTO usage_automatic_turn_triggers (thread_id, trigger_turn_id, event_occurrence_id, generation, attempt, outcome) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(thread_id, trigger_turn_id, event_occurrence_id, generation, attempt) DO UPDATE SET outcome = excluded.outcome",
        )
        .bind(&actual_thread_id)
        .bind(&trigger_identity)
        .bind(event_occurrence_id)
        .bind(generation)
        .bind(next_attempt)
        .bind(outcome)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(capability_and_reference
            .and_then(|(_, continuation_ref)| ticket_principal.map(|_| continuation_ref)))
    }

    fn automatic_turn_error_outcome(error: &ErrorEvent) -> &'static str {
        if error
            .codex_error_info
            .as_ref()
            .is_some_and(|info| matches!(info, &CodexErrorInfo::CyberPolicy))
        {
            OUTCOME_REBLOCKED
        } else {
            OUTCOME_FAILED
        }
    }

    async fn complete_automatic_turn(
        &self,
        thread_id: ThreadId,
        event_occurrence_id: &str,
        turn_id: &str,
        outcome: &str,
        client_user_message_id: Option<&str>,
    ) -> anyhow::Result<Option<CompletedAutomaticTurn>> {
        if turn_id.is_empty() {
            return Ok(None);
        }
        let pool = self.usage_ledger_pool();
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query_as::<_, (i64, i64, i64, i64, String, String)>(
                "SELECT id, attempt, max_attempts, generation, trigger_turn_id, event_occurrence_id FROM usage_automatic_turns WHERE thread_id = ? AND turn_id = ? AND event_occurrence_id = ? AND outcome = 'started' LIMIT 1",
            )
            .bind(thread_id.to_string())
            .bind(turn_id)
            .bind(event_occurrence_id)
            .fetch_optional(&mut *tx)
            .await?;
        let row = if row.is_some() {
            row
        } else if let Some(client_id) = client_user_message_id.filter(|id| !id.is_empty()) {
            sqlx::query_as::<_, (i64, i64, i64, i64, String, String)>(
                "SELECT id, attempt, max_attempts, generation, trigger_turn_id, event_occurrence_id FROM usage_automatic_turns WHERE thread_id = ? AND client_user_message_id = ? AND outcome = 'started' LIMIT 1",
            )
            .bind(thread_id.to_string())
            .bind(client_id)
            .fetch_optional(&mut *tx)
            .await?
        } else {
            None
        };
        let Some((id, attempt, max_attempts, generation, trigger_turn_id, trigger_occurrence_id)) =
            row
        else {
            // A successful/non-automatic turn is the explicit reset boundary for a stale pending
            // ticket. Policy errors are handled by the caller after this no-op completion. Avoid
            // advancing the generation again when a duplicate terminal event is replayed after
            // the first terminal projection already cleared the pending ticket.
            let aborted_admission = if outcome == OUTCOME_ABORTED {
                if let Some(client_id) = client_user_message_id.filter(|id| !id.is_empty()) {
                    sqlx::query_as::<_, (
                        String,
                        String,
                        Option<String>,
                        i64,
                        i64,
                        i64,
                        Option<String>,
                        String,
                    )>(
                        "SELECT trigger_turn_id, capability, event_occurrence_id, generation, next_attempt, max_attempts, principal_identity, admitted_client_user_message_id FROM usage_automatic_turn_eligibility WHERE thread_id = ? AND admitted_client_user_message_id = ? LIMIT 1",
                    )
                    .bind(thread_id.to_string())
                    .bind(client_id)
                    .fetch_optional(&mut *tx)
                    .await?
                } else {
                    None
                }
            } else {
                None
            };
            let had_aborted_admission = aborted_admission.is_some();
            let has_pending_eligibility: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM usage_automatic_turn_eligibility WHERE thread_id = ?)",
            )
            .bind(thread_id.to_string())
            .fetch_one(&mut *tx)
            .await?;
            if outcome == OUTCOME_ABORTED
                && let Some((
                    trigger_turn_id,
                    capability,
                    trigger_occurrence_id,
                    generation,
                    attempt,
                    max_attempts,
                    principal_identity,
                    admitted_client_user_message_id,
                )) = aborted_admission
            {
                sqlx::query(
                    "INSERT INTO usage_automatic_turns (thread_id, client_user_message_id, trigger_turn_id, event_occurrence_id, turn_id, principal_identity, origin, generation, capability, attempt, max_attempts, provenance_source, outcome, abort_event_occurrence_id, completed_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ','now')) ON CONFLICT(thread_id, client_user_message_id) DO NOTHING",
                )
                .bind(thread_id.to_string())
                .bind(admitted_client_user_message_id)
                .bind(&trigger_turn_id)
                .bind(event_occurrence_id)
                .bind(turn_id)
                .bind(principal_identity)
                .bind("policy_retry")
                .bind(generation)
                .bind(&capability)
                .bind(attempt)
                .bind(max_attempts)
                .bind(PROVENANCE_SOURCE)
                .bind(OUTCOME_ABORTED)
                .bind(event_occurrence_id)
                .execute(&mut *tx)
                .await?;
                let trigger_occurrence_id =
                    trigger_occurrence_id.unwrap_or_else(|| trigger_turn_id.clone());
                sqlx::query(
                    "UPDATE usage_automatic_turn_triggers SET outcome = ? WHERE thread_id = ? AND trigger_turn_id = ? AND event_occurrence_id = ? AND generation = ? AND attempt = ?",
                )
                .bind(OUTCOME_ABORTED)
                .bind(thread_id.to_string())
                .bind(&trigger_turn_id)
                .bind(trigger_occurrence_id)
                .bind(generation)
                .bind(attempt)
                .execute(&mut *tx)
                .await?;
            }
            let should_reset = if outcome == OUTCOME_ABORTED {
                had_aborted_admission
            } else {
                outcome != OUTCOME_REBLOCKED && has_pending_eligibility
            };
            if should_reset {
                let thread_id = thread_id.to_string();
                reset_chain_in_transaction(&mut tx, &thread_id).await?;
            }
            tx.commit().await?;
            return Ok(None);
        };
        let outcome = if outcome == OUTCOME_REBLOCKED && attempt >= max_attempts {
            OUTCOME_EXHAUSTED
        } else {
            outcome
        };
        if outcome == OUTCOME_ABORTED {
            sqlx::query(
                "UPDATE usage_automatic_turns SET outcome = ?, abort_event_occurrence_id = ?, completed_at = COALESCE(completed_at, strftime('%Y-%m-%dT%H:%M:%fZ','now')) WHERE id = ? AND outcome = 'started'",
            )
            .bind(outcome)
            .bind(event_occurrence_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        } else {
            sqlx::query(
                "UPDATE usage_automatic_turns SET outcome = ?, completed_at = COALESCE(completed_at, strftime('%Y-%m-%dT%H:%M:%fZ','now')) WHERE id = ? AND outcome = 'started'",
            )
            .bind(outcome)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "UPDATE usage_automatic_turn_triggers SET outcome = ? WHERE thread_id = ? AND trigger_turn_id = ? AND event_occurrence_id = ? AND generation = ? AND attempt = ?",
        )
        .bind(outcome)
        .bind(thread_id.to_string())
        .bind(trigger_turn_id)
        .bind(trigger_occurrence_id)
        .bind(generation)
        .bind(attempt)
        .execute(&mut *tx)
        .await?;
        if matches!(
            outcome,
            OUTCOME_RECOVERED | OUTCOME_FAILED | OUTCOME_ABORTED
        ) {
            let thread_id = thread_id.to_string();
            reset_chain_in_transaction(&mut tx, &thread_id).await?;
        }
        tx.commit().await?;
        Ok(Some(CompletedAutomaticTurn))
    }
}

fn is_expected_policy_retry_content(item: &codex_protocol::items::UserMessageItem) -> bool {
    matches!(
        item.content.as_slice(),
        [UserInput::Text { text, text_elements }] if text == EXPECTED_POLICY_RETRY_TEXT && text_elements.is_empty()
    )
}

async fn insert_suppressed_trigger(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    thread_id: &str,
    trigger_turn_id: &str,
    event_occurrence_id: &str,
    generation: i64,
    attempt: i64,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "INSERT INTO usage_automatic_turn_triggers (thread_id, trigger_turn_id, event_occurrence_id, generation, attempt, outcome) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(thread_id, trigger_turn_id, event_occurrence_id, generation, attempt) DO NOTHING",
    )
    .bind(thread_id)
    .bind(trigger_turn_id)
    .bind(event_occurrence_id)
    .bind(generation)
    .bind(attempt)
    .bind(OUTCOME_SUPPRESSED)
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected() == 1)
}

async fn reset_chain_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    thread_id: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO usage_automatic_turn_chains (thread_id) VALUES (?) ON CONFLICT(thread_id) DO NOTHING",
    )
    .bind(thread_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE usage_automatic_turn_chains SET generation = generation + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE thread_id = ?",
    )
    .bind(thread_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM usage_automatic_turn_eligibility WHERE thread_id = ?")
        .bind(thread_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use codex_protocol::items::UserMessageItem;
    use codex_protocol::protocol::ItemCompletedEvent;
    use codex_protocol::protocol::TurnCompleteEvent;
    use codex_utils_absolute_path::test_support::PathExt;
    use tempfile::TempDir;
    use tempfile::tempdir;

    #[derive(Debug, sqlx_macros::FromRow)]
    struct AutomaticTurnRow {
        attempt: i64,
        max_attempts: i64,
        outcome: String,
    }

    async fn init_runtime() -> Result<(std::sync::Arc<StateRuntime>, TempDir)> {
        let tmp_dir = tempdir()?;
        let runtime = StateRuntime::init(
            crate::SqliteConfig::new_for_testing(tmp_dir.path().abs()),
            "test-provider".to_string(),
        )
        .await?;
        Ok((runtime, tmp_dir))
    }

    async fn capability(runtime: &StateRuntime, thread_id: ThreadId, trigger: &str) -> String {
        runtime
            .automatic_turn_capability(thread_id, trigger)
            .await
            .expect("policy trigger should issue capability")
    }

    async fn bind_contract(
        runtime: &StateRuntime,
        thread_id: ThreadId,
        trigger: &str,
        capability: &str,
        operation_kind: &str,
        expected_turn_id: Option<&str>,
    ) -> Result<()> {
        assert!(
            runtime
                .bind_automatic_turn_capability_contract(
                    thread_id,
                    trigger,
                    capability,
                    operation_kind,
                    expected_turn_id,
                    "test-context",
                )
                .await?
        );
        Ok(())
    }

    fn automatic_user_message(thread_id: ThreadId, turn_id: &str, client_id: &str) -> Event {
        let mut item = UserMessageItem::new(&[UserInput::Text {
            text: EXPECTED_POLICY_RETRY_TEXT.to_string(),
            text_elements: Vec::new(),
        }]);
        item.client_id = Some(client_id.to_string());
        Event {
            id: turn_id.to_string(),
            msg: EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id,
                turn_id: turn_id.to_string(),
                item: TurnItem::UserMessage(item),
                completed_at_ms: 0,
            }),
        }
    }

    async fn issue_ticket(
        runtime: &StateRuntime,
        thread_id: ThreadId,
        trigger_turn_id: &str,
    ) -> AutomaticTurnContinuationRef {
        issue_ticket_for_principal(runtime, thread_id, trigger_turn_id, "principal:stable-test")
            .await
    }

    async fn issue_ticket_for_principal(
        runtime: &StateRuntime,
        thread_id: ThreadId,
        trigger_turn_id: &str,
        principal: &str,
    ) -> AutomaticTurnContinuationRef {
        let expected_epochs = runtime
            .automatic_turn_current_epochs(thread_id)
            .await
            .expect("state runtime exposes current automatic-turn epochs");
        runtime
            .record_automatic_turn_event_with_submission_context(
                thread_id,
                &policy_error(trigger_turn_id),
                Some(principal),
                None,
                None,
                Some(expected_epochs),
            )
            .await
            .and_then(AutomaticTurnContinuationRef::new)
            .expect("authenticated policy event issues an opaque reference")
    }

    async fn reserve_retry(
        runtime: &StateRuntime,
        thread_id: ThreadId,
        reference: &AutomaticTurnContinuationRef,
        client_id: &str,
        principal: &str,
    ) -> Result<bool> {
        Ok(runtime
            .reserve_automatic_turn_continuation(
                thread_id,
                reference.as_str(),
                principal,
                client_id,
                "start",
                /*expected_turn_id*/ None,
            )
            .await?)
    }

    async fn record_retry(
        runtime: &StateRuntime,
        thread_id: ThreadId,
        event: &Event,
        reference: &AutomaticTurnContinuationRef,
        principal: &str,
    ) {
        let client_id = match &event.msg {
            EventMsg::ItemCompleted(ItemCompletedEvent {
                item: TurnItem::UserMessage(item),
                ..
            }) => item.client_id.as_deref(),
            _ => None,
        };
        let expected_epochs = runtime.automatic_turn_current_epochs(thread_id).await;
        runtime
            .record_automatic_turn_event_with_submission_context(
                thread_id,
                event,
                Some(principal),
                client_id,
                Some(reference.as_str()),
                expected_epochs,
            )
            .await;
    }

    async fn rows(
        runtime: &StateRuntime,
        thread_id: ThreadId,
        turn_id: &str,
    ) -> Result<Vec<AutomaticTurnRow>> {
        Ok(sqlx::query_as(
            "SELECT attempt, max_attempts, outcome FROM usage_automatic_turns WHERE thread_id = ? AND turn_id = ? ORDER BY id",
        )
        .bind(thread_id.to_string())
        .bind(turn_id)
        .fetch_all(runtime.usage_ledger_pool().as_ref())
        .await?)
    }

    fn policy_error(turn_id: &str) -> Event {
        Event {
            id: turn_id.to_string(),
            msg: EventMsg::Error(ErrorEvent {
                message: "blocked".to_string(),
                codex_error_info: Some(CodexErrorInfo::CyberPolicy),
            }),
        }
    }

    #[tokio::test]
    async fn trusted_projection_requires_capability_and_recovers() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let thread_id = ThreadId::new();
        let forged = automatic_user_message(thread_id, "turn", "forged-client-id");
        runtime
            .record_automatic_turn_event(thread_id, &forged)
            .await;
        assert!(rows(runtime.as_ref(), thread_id, "turn").await?.is_empty());
        let reference = issue_ticket(runtime.as_ref(), thread_id, "trigger").await;
        let client_id = "admitted-client-id";
        let valid = automatic_user_message(thread_id, "turn", client_id);
        assert!(
            reserve_retry(
                runtime.as_ref(),
                thread_id,
                &reference,
                client_id,
                "principal:stable-test",
            )
            .await?
        );
        record_retry(
            runtime.as_ref(),
            thread_id,
            &valid,
            &reference,
            "principal:stable-test",
        )
        .await;
        let projected = rows(runtime.as_ref(), thread_id, "turn").await?;
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].attempt, 1);
        assert_eq!(projected[0].max_attempts, 3);
        assert_eq!(projected[0].outcome, OUTCOME_STARTED);
        runtime
            .record_automatic_turn_event(
                thread_id,
                &Event {
                    id: "turn".to_string(),
                    msg: EventMsg::TurnComplete(TurnCompleteEvent {
                        turn_id: "turn".to_string(),
                        last_agent_message: Some("done".to_string()),
                        error: None,
                        started_at: None,
                        compaction_events_in_turn: 0,
                        final_model: None,
                        model_snapshot: None,
                        provider_usage: None,
                        completed_at: None,
                        duration_ms: None,
                        time_to_first_token_ms: None,
                    }),
                },
            )
            .await;
        assert_eq!(
            rows(runtime.as_ref(), thread_id, "turn").await?[0].outcome,
            OUTCOME_RECOVERED
        );
        Ok(())
    }

    #[tokio::test]
    async fn trusted_projection_requires_the_server_principal() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let thread_id = ThreadId::new();
        let reference = issue_ticket_for_principal(
            runtime.as_ref(),
            thread_id,
            "trigger",
            "principal:account-1",
        )
        .await;
        let client_id = "bound-client-id";
        let sibling_attempt = automatic_user_message(thread_id, "turn", client_id);
        assert!(
            reserve_retry(
                runtime.as_ref(),
                thread_id,
                &reference,
                client_id,
                "principal:account-1"
            )
            .await?
        );
        record_retry(
            runtime.as_ref(),
            thread_id,
            &sibling_attempt,
            &reference,
            "principal:account-2",
        )
        .await;
        assert!(rows(runtime.as_ref(), thread_id, "turn").await?.is_empty());

        record_retry(
            runtime.as_ref(),
            thread_id,
            &sibling_attempt,
            &reference,
            "principal:account-1",
        )
        .await;
        assert_eq!(rows(runtime.as_ref(), thread_id, "turn").await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn unbound_policy_event_does_not_issue_a_continuation_reference() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let thread_id = ThreadId::new();
        let expected_epochs = runtime.automatic_turn_current_epochs(thread_id).await;

        let reference = runtime
            .record_automatic_turn_event_with_submission_context(
                thread_id,
                &policy_error("unbound-trigger"),
                /*principal*/ None,
                /*client_user_message_id*/ None,
                /*continuation_ref*/ None,
                expected_epochs,
            )
            .await;

        assert!(reference.is_none());
        assert!(
            runtime
                .automatic_turn_capability(thread_id, "unbound-trigger")
                .await
                .is_none()
        );
        assert!(
            runtime
                .automatic_turn_continuation_reference(thread_id, "unbound-trigger")
                .await
                .is_none()
        );
        let forged_shape = AutomaticTurnContinuationRef::new("well-shaped-but-unissued")
            .expect("wire shape does not establish authority");
        assert!(
            !reserve_retry(
                runtime.as_ref(),
                thread_id,
                &forged_shape,
                "forged-client-id",
                "principal:stable-test",
            )
            .await?,
            "a caller-created opaque reference has no matching durable State ticket"
        );
        Ok(())
    }

    #[tokio::test]
    async fn continuation_ticket_survives_state_reopen_and_revalidates_stable_identity()
    -> Result<()> {
        let tmp_dir = tempdir()?;
        let sqlite = crate::SqliteConfig::new_for_testing(tmp_dir.path().abs());
        let runtime = StateRuntime::init(sqlite.clone(), "test-provider".to_string()).await?;
        let thread_id = ThreadId::new();
        let reference = issue_ticket_for_principal(
            runtime.as_ref(),
            thread_id,
            "trigger",
            "principal:account-1",
        )
        .await;
        runtime.close().await;

        let reopened = StateRuntime::init(sqlite, "test-provider".to_string()).await?;
        assert!(
            reopened
                .automatic_turn_continuation_epochs(thread_id, reference.as_str())
                .await
                .is_some(),
            "ticket and its epochs survive process-style State reopen"
        );
        assert!(
            !reopened
                .reserve_automatic_turn_continuation(
                    ThreadId::new(),
                    reference.as_str(),
                    "principal:account-1",
                    "wrong-thread-message",
                    "start",
                    None,
                )
                .await?,
            "a ticket cannot move to another thread"
        );
        assert!(
            !reopened
                .reserve_automatic_turn_continuation(
                    thread_id,
                    reference.as_str(),
                    "principal:account-2",
                    "wrong-principal-message",
                    "start",
                    None,
                )
                .await?,
            "a changed authenticated identity cannot redeem the ticket"
        );
        assert!(
            reopened
                .reserve_automatic_turn_continuation(
                    thread_id,
                    reference.as_str(),
                    "principal:account-1",
                    "reopened-message",
                    "start",
                    None,
                )
                .await?,
            "the same stable identity may redeem once after reopen"
        );
        assert!(
            !reopened
                .reserve_automatic_turn_continuation(
                    thread_id,
                    reference.as_str(),
                    "principal:account-1",
                    "replay-message",
                    "start",
                    None,
                )
                .await?,
            "redemption remains one-time across reopen"
        );
        reopened.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn automatic_turn_admission_is_single_use() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let thread_id = ThreadId::new();
        let reference = issue_ticket(runtime.as_ref(), thread_id, "trigger").await;
        let first = runtime.clone();
        let second = runtime.clone();
        let (first, second) = tokio::join!(
            first.reserve_automatic_turn_continuation(
                thread_id,
                reference.as_str(),
                "principal:stable-test",
                "client-1",
                "start",
                /*expected_turn_id*/ None,
            ),
            second.reserve_automatic_turn_continuation(
                thread_id,
                reference.as_str(),
                "principal:stable-test",
                "client-2",
                "start",
                /*expected_turn_id*/ None,
            ),
        );
        assert_eq!(u8::from(first?) + u8::from(second?), 1);
        Ok(())
    }

    #[tokio::test]
    async fn automatic_turn_contract_binds_operation_target_and_context() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let thread_id = ThreadId::new();
        let _reference = issue_ticket_for_principal(
            runtime.as_ref(),
            thread_id,
            "trigger",
            "principal:account-1",
        )
        .await;
        let capability = capability(runtime.as_ref(), thread_id, "trigger").await;
        assert!(
            runtime
                .bind_automatic_turn_capability_contract(
                    thread_id,
                    "trigger",
                    &capability,
                    "start",
                    /*expected_turn_id*/ None,
                    "context-v1",
                )
                .await?
        );
        assert_eq!(
            runtime
                .automatic_turn_capability_contract(thread_id, "trigger", &capability)
                .await,
            Some((
                Some("principal:account-1".to_string()),
                Some("start".to_string()),
                None,
                Some("context-v1".to_string()),
            ))
        );
        assert!(
            !runtime
                .bind_automatic_turn_capability_contract(
                    thread_id,
                    "trigger",
                    &capability,
                    "start",
                    /*expected_turn_id*/ None,
                    "different-context",
                )
                .await?
        );
        Ok(())
    }

    #[tokio::test]
    async fn automatic_turn_ticket_epochs_invalidate_settings_and_auth_transitions() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let thread_id = ThreadId::new();
        runtime
            .record_automatic_turn_event(thread_id, &policy_error("trigger"))
            .await;
        let capability = capability(runtime.as_ref(), thread_id, "trigger").await;
        assert_eq!(
            runtime
                .automatic_turn_capability_epochs(thread_id, "trigger", &capability)
                .await,
            Some((0, 0))
        );

        runtime
            .invalidate_automatic_turn_capabilities_for_thread(thread_id)
            .await?;
        assert!(
            runtime
                .automatic_turn_capability_epochs(thread_id, "trigger", &capability)
                .await
                .is_none()
        );
        assert_eq!(
            runtime.automatic_turn_current_epochs(thread_id).await,
            Some((1, 0))
        );

        runtime
            .record_automatic_turn_event(thread_id, &policy_error("trigger-2"))
            .await;
        let replacement = self::capability(runtime.as_ref(), thread_id, "trigger-2").await;
        assert_eq!(
            runtime
                .automatic_turn_capability_epochs(thread_id, "trigger-2", &replacement)
                .await,
            Some((1, 0))
        );

        runtime.invalidate_all_automatic_turn_capabilities().await?;
        assert!(
            runtime
                .automatic_turn_capability_epochs(thread_id, "trigger-2", &replacement)
                .await
                .is_none()
        );
        assert_eq!(
            runtime.automatic_turn_current_epochs(thread_id).await,
            Some((1, 1))
        );
        Ok(())
    }

    #[tokio::test]
    async fn auth_transition_gate_survives_restart_and_rejects_stale_completion() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let thread_id = ThreadId::new();
        let old_reference = issue_ticket(runtime.as_ref(), thread_id, "before-transition").await;

        let first = runtime.begin_automatic_turn_auth_transition().await?;
        assert_eq!(
            runtime.pending_automatic_turn_auth_transition().await?,
            Some(first)
        );
        assert!(
            !reserve_retry(
                runtime.as_ref(),
                thread_id,
                &old_reference,
                "transition-client",
                "principal:stable-test",
            )
            .await?
        );
        assert_eq!(runtime.automatic_turn_current_epochs(thread_id).await, None);
        assert!(
            runtime
                .record_automatic_turn_event_with_submission_context(
                    thread_id,
                    &policy_error("during-transition"),
                    Some("principal:stable-test"),
                    None,
                    None,
                    /*expected_epochs*/ None,
                )
                .await
                .is_none()
        );

        let sqlite = runtime.sqlite().clone();
        runtime.close().await;
        let reopened = StateRuntime::init(sqlite, "test-provider".to_string()).await?;
        assert_eq!(
            reopened.pending_automatic_turn_auth_transition().await?,
            Some(first),
            "startup must leave the durable gate closed until current auth has been established"
        );
        assert_eq!(
            reopened.automatic_turn_current_epochs(thread_id).await,
            None
        );

        let second = reopened.begin_automatic_turn_auth_transition().await?;
        assert_ne!(first, second);
        assert!(
            !reopened
                .complete_automatic_turn_auth_transition(first)
                .await?,
            "a stale completion must not clear a newer transition"
        );
        assert_eq!(
            reopened.pending_automatic_turn_auth_transition().await?,
            Some(second)
        );
        assert!(
            reopened
                .complete_automatic_turn_auth_transition(second)
                .await?
        );
        assert_eq!(
            reopened.pending_automatic_turn_auth_transition().await?,
            None
        );
        assert!(
            reopened
                .automatic_turn_current_epochs(thread_id)
                .await
                .is_some()
        );

        let replacement = issue_ticket(reopened.as_ref(), thread_id, "after-transition").await;
        assert!(!replacement.as_str().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn another_runtime_epoch_advance_rejects_stale_ticket_issue() -> Result<()> {
        let tmp_dir = tempdir()?;
        let sqlite = crate::SqliteConfig::new_for_testing(tmp_dir.path().abs());
        let first = StateRuntime::init(sqlite.clone(), "test-provider".to_string()).await?;
        let second = StateRuntime::init(sqlite, "test-provider".to_string()).await?;
        let thread_id = ThreadId::new();

        let first_epochs = first
            .automatic_turn_current_epochs(thread_id)
            .await
            .expect("State has an initial auth epoch");

        let transition = second.begin_automatic_turn_auth_transition().await?;
        assert!(
            second
                .complete_automatic_turn_auth_transition(transition)
                .await?
        );

        assert_eq!(
            first.automatic_turn_current_epochs(thread_id).await,
            Some((first_epochs.0, first_epochs.1 + 2))
        );
        assert!(
            first
                .record_automatic_turn_event_with_submission_context(
                    thread_id,
                    &policy_error("stale-cross-process-epoch"),
                    Some("principal:stable-test"),
                    None,
                    None,
                    Some(first_epochs),
                )
                .await
                .is_none(),
            "ticket issuance must reject the exact epoch captured before another process changed auth"
        );
        Ok(())
    }

    #[tokio::test]
    async fn aborted_automatic_turn_is_terminal_and_idempotent() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let thread_id = ThreadId::new();
        let reference = issue_ticket(runtime.as_ref(), thread_id, "trigger").await;
        let client_id = "aborted-client-id";
        let mut automatic = automatic_user_message(thread_id, "turn", client_id);
        assert!(
            reserve_retry(
                runtime.as_ref(),
                thread_id,
                &reference,
                client_id,
                "principal:stable-test",
            )
            .await?
        );
        automatic.id = "e1".to_string();
        record_retry(
            runtime.as_ref(),
            thread_id,
            &automatic,
            &reference,
            "principal:stable-test",
        )
        .await;

        let aborted = Event {
            id: "e1".to_string(),
            msg: EventMsg::TurnAborted(codex_protocol::protocol::TurnAbortedEvent {
                turn_id: Some("turn".to_string()),
                reason: codex_protocol::protocol::TurnAbortReason::Interrupted,
                provider_usage: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            }),
        };
        runtime
            .record_automatic_turn_event(thread_id, &aborted)
            .await;
        let generation: i64 = sqlx::query_scalar(
            "SELECT generation FROM usage_automatic_turn_chains WHERE thread_id = ?",
        )
        .bind(thread_id.to_string())
        .fetch_one(runtime.usage_ledger_pool().as_ref())
        .await?;
        let (outcome, completed_at): (String, Option<String>) = sqlx::query_as(
            "SELECT outcome, completed_at FROM usage_automatic_turns WHERE thread_id = ? AND turn_id = ?",
        )
        .bind(thread_id.to_string())
        .bind("turn")
        .fetch_one(runtime.usage_ledger_pool().as_ref())
        .await?;
        assert_eq!(outcome, OUTCOME_ABORTED);
        assert!(completed_at.is_some());
        assert!(
            runtime
                .automatic_turn_capability(thread_id, "trigger")
                .await
                .is_none()
        );

        runtime
            .record_automatic_turn_event(thread_id, &aborted)
            .await;
        let duplicate_generation: i64 = sqlx::query_scalar(
            "SELECT generation FROM usage_automatic_turn_chains WHERE thread_id = ?",
        )
        .bind(thread_id.to_string())
        .fetch_one(runtime.usage_ledger_pool().as_ref())
        .await?;
        assert_eq!(generation, duplicate_generation);
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM usage_automatic_turns WHERE thread_id = ? AND outcome = ?",
            )
            .bind(thread_id.to_string())
            .bind(OUTCOME_ABORTED)
            .fetch_one(runtime.usage_ledger_pool().as_ref())
            .await?,
            1
        );

        // A late duplicate for the exact aborted occurrence is tombstoned.
        runtime
            .record_automatic_turn_event(thread_id, &policy_error("e1"))
            .await;
        runtime
            .record_automatic_turn_event(
                thread_id,
                &Event {
                    id: "e1".to_string(),
                    msg: EventMsg::TurnComplete(TurnCompleteEvent {
                        turn_id: "turn".to_string(),
                        last_agent_message: None,
                        error: Some(ErrorEvent {
                            message: "blocked".to_string(),
                            codex_error_info: Some(CodexErrorInfo::CyberPolicy),
                        }),
                        started_at: None,
                        compaction_events_in_turn: 0,
                        final_model: None,
                        model_snapshot: None,
                        provider_usage: None,
                        completed_at: None,
                        duration_ms: None,
                        time_to_first_token_ms: None,
                    }),
                },
            )
            .await;
        // A newer occurrence may legitimately reuse regular turn id T and must remain eligible.
        runtime
            .record_automatic_turn_event(
                thread_id,
                &Event {
                    id: "e2".to_string(),
                    msg: EventMsg::TurnComplete(TurnCompleteEvent {
                        turn_id: "turn".to_string(),
                        last_agent_message: None,
                        error: Some(ErrorEvent {
                            message: "blocked".to_string(),
                            codex_error_info: Some(CodexErrorInfo::CyberPolicy),
                        }),
                        started_at: None,
                        compaction_events_in_turn: 0,
                        final_model: None,
                        model_snapshot: None,
                        provider_usage: None,
                        completed_at: None,
                        duration_ms: None,
                        time_to_first_token_ms: None,
                    }),
                },
            )
            .await;
        let after_late_policy_generation: i64 = sqlx::query_scalar(
            "SELECT generation FROM usage_automatic_turn_chains WHERE thread_id = ?",
        )
        .bind(thread_id.to_string())
        .fetch_one(runtime.usage_ledger_pool().as_ref())
        .await?;
        assert_eq!(generation, after_late_policy_generation);
        assert!(
            runtime
                .automatic_turn_capability(thread_id, "turn")
                .await
                .is_some()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM usage_automatic_turns WHERE thread_id = ? AND outcome = ?",
            )
            .bind(thread_id.to_string())
            .bind(OUTCOME_ABORTED)
            .fetch_one(runtime.usage_ledger_pool().as_ref())
            .await?,
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn admitted_abort_before_item_projection_is_persisted() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let thread_id = ThreadId::new();
        let reference = issue_ticket_for_principal(
            runtime.as_ref(),
            thread_id,
            "trigger",
            "principal:account-1",
        )
        .await;
        let client_id = "aborted-before-item-client-id";
        assert!(
            runtime
                .reserve_automatic_turn_continuation(
                    thread_id,
                    reference.as_str(),
                    "principal:account-1",
                    client_id,
                    "start",
                    /*expected_turn_id*/ None,
                )
                .await?
        );

        let aborted = Event {
            id: "turn-before-item".to_string(),
            msg: EventMsg::TurnAborted(codex_protocol::protocol::TurnAbortedEvent {
                turn_id: Some("turn-before-item".to_string()),
                reason: codex_protocol::protocol::TurnAbortReason::Interrupted,
                provider_usage: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            }),
        };
        runtime
            .record_automatic_turn_event_with_submission_context(
                thread_id,
                &aborted,
                Some("principal:account-1"),
                Some(&client_id),
                Some(reference.as_str()),
                runtime.automatic_turn_current_epochs(thread_id).await,
            )
            .await;

        let (outcome, completed_at): (String, Option<String>) = sqlx::query_as(
            "SELECT outcome, completed_at FROM usage_automatic_turns WHERE thread_id = ? AND turn_id = ?",
        )
        .bind(thread_id.to_string())
        .bind("turn-before-item")
        .fetch_one(runtime.usage_ledger_pool().as_ref())
        .await?;
        assert_eq!(outcome, OUTCOME_ABORTED);
        assert!(completed_at.is_some());
        assert!(
            runtime
                .automatic_turn_capability(thread_id, "trigger")
                .await
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn duplicate_trigger_and_replay_are_ignored() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let thread_id = ThreadId::new();
        let reference = issue_ticket(runtime.as_ref(), thread_id, "trigger").await;
        let client_id = "duplicate-client-id";
        let valid = automatic_user_message(thread_id, "turn", client_id);
        assert!(
            reserve_retry(
                runtime.as_ref(),
                thread_id,
                &reference,
                client_id,
                "principal:stable-test",
            )
            .await?
        );
        record_retry(
            runtime.as_ref(),
            thread_id,
            &valid,
            &reference,
            "principal:stable-test",
        )
        .await;
        record_retry(
            runtime.as_ref(),
            thread_id,
            &valid,
            &reference,
            "principal:stable-test",
        )
        .await;
        // A repeated policy event arrives after the ticket was consumed but before any new turn;
        // no second ticket is minted.
        runtime
            .record_automatic_turn_event(thread_id, &policy_error("trigger"))
            .await;
        assert!(
            sqlx::query_scalar::<_, i64>(
                "SELECT 1 FROM usage_automatic_turn_eligibility WHERE thread_id = ?",
            )
            .bind(thread_id.to_string())
            .fetch_optional(runtime.usage_ledger_pool().as_ref())
            .await?
            .is_none()
        );
        assert_eq!(rows(runtime.as_ref(), thread_id, "turn").await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn successful_manual_turn_resets_interrupted_pending_chain() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let thread_id = ThreadId::new();
        runtime
            .record_automatic_turn_event(thread_id, &policy_error("trigger"))
            .await;
        let old_capability = capability(runtime.as_ref(), thread_id, "trigger").await;

        runtime
            .record_automatic_turn_event(
                thread_id,
                &Event {
                    id: "interrupted".to_string(),
                    msg: EventMsg::TurnAborted(codex_protocol::protocol::TurnAbortedEvent {
                        turn_id: Some("interrupted".to_string()),
                        reason: codex_protocol::protocol::TurnAbortReason::Interrupted,
                        provider_usage: None,
                        started_at: None,
                        completed_at: None,
                        duration_ms: None,
                    }),
                },
            )
            .await;

        let manual_item = UserMessageItem::new(&[UserInput::Text {
            text: "manual".to_string(),
            text_elements: Vec::new(),
        }]);
        runtime
            .record_automatic_turn_event(
                thread_id,
                &Event {
                    id: "manual".to_string(),
                    msg: EventMsg::ItemCompleted(ItemCompletedEvent {
                        thread_id,
                        turn_id: "manual".to_string(),
                        item: TurnItem::UserMessage(manual_item),
                        completed_at_ms: 0,
                    }),
                },
            )
            .await;
        runtime
            .record_automatic_turn_event(
                thread_id,
                &Event {
                    id: "manual".to_string(),
                    msg: EventMsg::TurnComplete(TurnCompleteEvent {
                        turn_id: "manual".to_string(),
                        last_agent_message: Some("done".to_string()),
                        error: None,
                        started_at: None,
                        compaction_events_in_turn: 0,
                        final_model: None,
                        model_snapshot: None,
                        provider_usage: None,
                        completed_at: None,
                        duration_ms: None,
                        time_to_first_token_ms: None,
                    }),
                },
            )
            .await;

        assert!(
            runtime
                .automatic_turn_capability(thread_id, "trigger")
                .await
                .is_none()
        );
        runtime
            .record_automatic_turn_event(thread_id, &policy_error("trigger-new"))
            .await;
        let new_capability = capability(runtime.as_ref(), thread_id, "trigger-new").await;
        assert_ne!(old_capability, new_capability);
        Ok(())
    }

    #[tokio::test]
    async fn repeated_attempts_share_regular_turn_id_and_exhaust() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let thread_id = ThreadId::new();
        let turn_id = "same-turn";
        let mut reference = issue_ticket(runtime.as_ref(), thread_id, turn_id).await;
        for index in 0..3 {
            let client_id = format!("retry-client-{index}");
            let retry = automatic_user_message(thread_id, turn_id, &client_id);
            assert!(
                reserve_retry(
                    runtime.as_ref(),
                    thread_id,
                    &reference,
                    &client_id,
                    "principal:stable-test",
                )
                .await?
            );
            record_retry(
                runtime.as_ref(),
                thread_id,
                &retry,
                &reference,
                "principal:stable-test",
            )
            .await;
            if index < 2 {
                reference = issue_ticket_for_principal(
                    runtime.as_ref(),
                    thread_id,
                    turn_id,
                    "principal:stable-test",
                )
                .await;
            } else {
                runtime
                    .record_automatic_turn_event_with_principal(
                        thread_id,
                        &policy_error(turn_id),
                        Some("principal:stable-test"),
                    )
                    .await;
            }
        }
        let projected = rows(runtime.as_ref(), thread_id, turn_id).await?;
        assert_eq!(projected.len(), 3);
        assert_eq!(
            projected.iter().map(|row| row.attempt).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(
            projected
                .iter()
                .map(|row| row.outcome.as_str())
                .collect::<Vec<_>>(),
            [OUTCOME_REBLOCKED, OUTCOME_REBLOCKED, OUTCOME_EXHAUSTED]
        );
        assert!(
            sqlx::query(
                "SELECT 1 FROM usage_automatic_turn_eligibility WHERE thread_id = ? LIMIT 1",
            )
            .bind(thread_id.to_string())
            .fetch_optional(runtime.usage_ledger_pool().as_ref())
            .await?
            .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn late_completion_cannot_complete_a_later_same_turn_occurrence() -> Result<()> {
        let (runtime, _tmp_dir) = init_runtime().await?;
        let thread_id = ThreadId::new();
        let turn_id = "reused-turn";

        let first_reference = runtime
            .record_automatic_turn_event_with_submission_context(
                thread_id,
                &Event {
                    id: "occurrence-1".to_string(),
                    msg: EventMsg::Error(ErrorEvent {
                        message: "blocked".to_string(),
                        codex_error_info: Some(CodexErrorInfo::CyberPolicy),
                    }),
                },
                Some("principal:stable-test"),
                None,
                None,
                runtime.automatic_turn_current_epochs(thread_id).await,
            )
            .await
            .and_then(AutomaticTurnContinuationRef::new)
            .expect("first occurrence should issue a reference");
        let first_client_id = "occurrence-1-client";
        let mut first_attempt = automatic_user_message(thread_id, turn_id, first_client_id);
        assert!(
            reserve_retry(
                runtime.as_ref(),
                thread_id,
                &first_reference,
                first_client_id,
                "principal:stable-test",
            )
            .await?
        );
        first_attempt.id = "occurrence-1".to_string();
        record_retry(
            runtime.as_ref(),
            thread_id,
            &first_attempt,
            &first_reference,
            "principal:stable-test",
        )
        .await;
        let second_reference = runtime
            .record_automatic_turn_event_with_submission_context(
                thread_id,
                &Event {
                    id: "occurrence-1".to_string(),
                    msg: EventMsg::TurnComplete(TurnCompleteEvent {
                        turn_id: turn_id.to_string(),
                        last_agent_message: None,
                        error: Some(ErrorEvent {
                            message: "blocked".to_string(),
                            codex_error_info: Some(CodexErrorInfo::CyberPolicy),
                        }),
                        started_at: None,
                        compaction_events_in_turn: 0,
                        final_model: None,
                        model_snapshot: None,
                        provider_usage: None,
                        completed_at: None,
                        duration_ms: None,
                        time_to_first_token_ms: None,
                    }),
                },
                Some("principal:stable-test"),
                Some(first_client_id),
                Some(first_reference.as_str()),
                runtime.automatic_turn_current_epochs(thread_id).await,
            )
            .await
            .and_then(AutomaticTurnContinuationRef::new)
            .expect("policy completion should issue the next reference");

        // The policy-bearing completion already issues the next reference. The next retry is a
        // distinct server event occurrence even though its regular turn id is reused.
        let second_client_id = "occurrence-2-client";
        let mut second_attempt = automatic_user_message(thread_id, turn_id, second_client_id);
        assert!(
            reserve_retry(
                runtime.as_ref(),
                thread_id,
                &second_reference,
                second_client_id,
                "principal:stable-test",
            )
            .await?
        );
        second_attempt.id = "occurrence-2".to_string();
        record_retry(
            runtime.as_ref(),
            thread_id,
            &second_attempt,
            &second_reference,
            "principal:stable-test",
        )
        .await;

        // A delayed completion from attempt one carries its immutable occurrence id and must not
        // resolve the started attempt-two row or mint a third ticket.
        runtime
            .record_automatic_turn_event_with_submission_context(
                thread_id,
                &Event {
                    id: "occurrence-1".to_string(),
                    msg: EventMsg::TurnComplete(TurnCompleteEvent {
                        turn_id: turn_id.to_string(),
                        last_agent_message: None,
                        error: Some(ErrorEvent {
                            message: "blocked".to_string(),
                            codex_error_info: Some(CodexErrorInfo::CyberPolicy),
                        }),
                        started_at: None,
                        compaction_events_in_turn: 0,
                        duration_ms: None,
                        provider_usage: None,
                        time_to_first_token_ms: None,
                        final_model: None,
                        model_snapshot: None,
                        completed_at: None,
                    }),
                },
                Some("principal:stable-test"),
                Some(first_client_id),
                Some(first_reference.as_str()),
                runtime.automatic_turn_current_epochs(thread_id).await,
            )
            .await;

        let projected = rows(runtime.as_ref(), thread_id, turn_id).await?;
        assert_eq!(projected.len(), 2);
        assert_eq!(projected[0].outcome, OUTCOME_REBLOCKED);
        assert_eq!(projected[1].outcome, OUTCOME_STARTED);
        let second_trigger = runtime
            .automatic_turn_trigger_for_continuation_ref(thread_id, second_reference.as_str())
            .await
            .expect("later ticket remains pending");
        assert!(
            runtime
                .automatic_turn_continuation_reference(thread_id, &second_trigger)
                .await
                .as_ref()
                == Some(&second_reference),
            "a late completion must not replace the later pending ticket"
        );
        Ok(())
    }
}
