use codex_protocol::ThreadId;

use super::StateRuntime;

impl StateRuntime {
    /// Missing means the thread predates this control and requires explicit opt-in.
    pub async fn external_notice_stopped(&self, id: ThreadId) -> anyhow::Result<Option<bool>> {
        Ok(sqlx::query_scalar::<_, bool>(
            "SELECT stopped FROM thread_external_notice_gate WHERE thread_id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(self.pool.as_ref())
        .await?)
    }

    /// Persist only control state, independently of source messages and cursors.
    pub async fn set_external_notice_stopped(&self, id: ThreadId, stopped: bool) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO thread_external_notice_gate(thread_id, stopped) VALUES (?, ?) \
             ON CONFLICT(thread_id) DO UPDATE SET stopped = excluded.stopped",
        )
        .bind(id.to_string())
        .bind(stopped)
        .execute(self.pool.as_ref())
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_utils_absolute_path::test_support::PathExt;
    use crate::SqliteConfig;
    use crate::runtime::test_support::unique_temp_dir;

    #[tokio::test]
    async fn explicit_gate_survives_reopen_and_is_independent_per_thread() -> anyhow::Result<()> {
        let home = unique_temp_dir();
        let sqlite = SqliteConfig::new_for_testing(home.as_path().abs());
        let first = StateRuntime::init(sqlite.clone(), "test".to_string()).await?;
        let thread = ThreadId::new();
        let other = ThreadId::new();
        assert_eq!(first.external_notice_stopped(thread).await?, None);
        first.set_external_notice_stopped(thread, true).await?;
        let reopened = StateRuntime::init(sqlite, "test".to_string()).await?;
        assert_eq!(reopened.external_notice_stopped(thread).await?, Some(true));
        assert_eq!(reopened.external_notice_stopped(other).await?, None);
        reopened.set_external_notice_stopped(thread, false).await?;
        assert_eq!(first.external_notice_stopped(thread).await?, Some(false));
        Ok(())
    }
}
