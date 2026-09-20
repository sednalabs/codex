use super::StateRuntime;
use codex_protocol::ThreadId;

/// Whether configured thread identity has been established from authoritative history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i64)]
pub enum ConfiguredIdentityProvenance {
    /// Authoritative history has not yet established presence or absence.
    Unknown = 0,
    /// Authoritative history was inspected and contained no configured identity.
    KnownAbsent = 1,
    /// Authoritative history contained configured identity.
    Present = 2,
}

impl ConfiguredIdentityProvenance {
    const fn as_i64(self) -> i64 {
        self as i64
    }
}

impl TryFrom<i64> for ConfiguredIdentityProvenance {
    type Error = anyhow::Error;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Unknown),
            1 => Ok(Self::KnownAbsent),
            2 => Ok(Self::Present),
            _ => Err(anyhow::anyhow!(
                "invalid configured identity provenance value: {value}"
            )),
        }
    }
}

impl StateRuntime {
    /// Read configured-identity provenance without hydrating generic thread metadata.
    ///
    /// Returns `None` when the thread row does not exist.
    pub async fn read_configured_identity_provenance(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<Option<ConfiguredIdentityProvenance>> {
        let value = sqlx::query_scalar::<_, i64>(
            "SELECT configured_identity_provenance FROM threads WHERE id = ?",
        )
        .bind(thread_id.to_string())
        .fetch_optional(self.pool.as_ref())
        .await?;
        value.map(ConfiguredIdentityProvenance::try_from).transpose()
    }

    /// Atomically advance configured-identity provenance to known absent.
    ///
    /// Returns `true` only when the stored state advanced.
    pub async fn mark_configured_identity_known_absent(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<bool> {
        self.advance_configured_identity_provenance(
            thread_id,
            ConfiguredIdentityProvenance::KnownAbsent,
        )
        .await
    }

    /// Atomically advance configured-identity provenance to present.
    ///
    /// Returns `true` only when the stored state advanced.
    pub async fn mark_configured_identity_present(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<bool> {
        self.advance_configured_identity_provenance(
            thread_id,
            ConfiguredIdentityProvenance::Present,
        )
        .await
    }

    async fn advance_configured_identity_provenance(
        &self,
        thread_id: ThreadId,
        target: ConfiguredIdentityProvenance,
    ) -> anyhow::Result<bool> {
        let target = target.as_i64();
        let result = sqlx::query(
            r#"
UPDATE threads
SET configured_identity_provenance = ?
WHERE id = ? AND configured_identity_provenance < ?
            "#,
        )
        .bind(target)
        .bind(thread_id.to_string())
        .bind(target)
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected() == 1)
    }
}

#[cfg(test)]
#[path = "configured_identity_provenance_tests.rs"]
mod tests;
