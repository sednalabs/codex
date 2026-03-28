use super::*;

impl StateRuntime {
    /// Returns whether this memory root has already completed the attestation
    /// bootstrap and should now fail closed when the attestation file is
    /// missing.
    pub async fn global_phase2_attestation_required_for_root(
        &self,
        memory_root_key: &str,
    ) -> anyhow::Result<bool> {
        let required = sqlx::query_scalar::<_, bool>(
            r#"SELECT EXISTS(SELECT 1 FROM phase2_attestation_roots WHERE memory_root_key = ?)"#,
        )
        .bind(memory_root_key)
        .fetch_one(self.pool.as_ref())
        .await?;

        Ok(required)
    }

    /// Marks this memory root as having consumed the one-time bootstrap path,
    /// so future unchanged-selection reuse must present a valid attestation.
    pub async fn mark_global_phase2_attestation_required_for_root(
        &self,
        memory_root_key: &str,
    ) -> anyhow::Result<()> {
        let now = Utc::now().timestamp();
        sqlx::query(
            r#"
INSERT INTO phase2_attestation_roots (
    memory_root_key,
    required_since,
    updated_at
) VALUES (?, ?, ?)
ON CONFLICT(memory_root_key) DO UPDATE SET
    updated_at = excluded.updated_at
            "#,
        )
        .bind(memory_root_key)
        .bind(now)
        .bind(now)
        .execute(self.pool.as_ref())
        .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::unique_temp_dir;
    use super::StateRuntime;

    #[tokio::test]
    async fn global_phase2_attestation_requirement_is_root_scoped() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        assert!(
            !runtime
                .global_phase2_attestation_required_for_root("root-a")
                .await
                .expect("load initial root-a requirement state"),
            "new roots should not require attestation before the first successful attested run"
        );
        assert!(
            !runtime
                .global_phase2_attestation_required_for_root("root-b")
                .await
                .expect("load initial root-b requirement state"),
            "other roots should also start without the attestation-required flag"
        );

        runtime
            .mark_global_phase2_attestation_required_for_root("root-a")
            .await
            .expect("mark root-a attestation requirement");

        assert!(
            runtime
                .global_phase2_attestation_required_for_root("root-a")
                .await
                .expect("load updated root-a requirement state"),
            "marked roots should require attestation on future reuse"
        );
        assert!(
            !runtime
                .global_phase2_attestation_required_for_root("root-b")
                .await
                .expect("load untouched root-b requirement state"),
            "marking one root must not leak attestation state into another root"
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }
}
