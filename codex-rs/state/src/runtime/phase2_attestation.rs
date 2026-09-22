use super::*;
use crate::Phase2AttestedBaseline;

impl StateRuntime {
    /// Records an attested phase-2 output baseline transactionally.
    pub async fn record_phase2_attested_baseline(
        &self,
        baseline: &Phase2AttestedBaseline,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        sqlx::query(
            r#"
INSERT INTO phase2_attested_baselines (
    memory_root_key,
    output_tree_sha256,
    schema_version,
    selection_sha256,
    prepared_inputs_sha256,
    consolidator_sha256,
    completion_watermark,
    selected_count,
    attested_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(memory_root_key, output_tree_sha256) DO UPDATE SET
    schema_version = excluded.schema_version,
    selection_sha256 = excluded.selection_sha256,
    prepared_inputs_sha256 = excluded.prepared_inputs_sha256,
    consolidator_sha256 = excluded.consolidator_sha256,
    completion_watermark = excluded.completion_watermark,
    selected_count = excluded.selected_count,
    attested_at = excluded.attested_at
            "#,
        )
        .bind(baseline.memory_root_key.as_str())
        .bind(baseline.output_tree_sha256.as_str())
        .bind(baseline.schema_version)
        .bind(baseline.selection_sha256.as_str())
        .bind(baseline.prepared_inputs_sha256.as_str())
        .bind(baseline.consolidator_sha256.as_str())
        .bind(baseline.completion_watermark)
        .bind(baseline.selected_count)
        .bind(baseline.attested_at)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(())
    }

    pub async fn get_phase2_attested_baseline(
        &self,
        memory_root_key: &str,
        output_tree_sha256: &str,
    ) -> anyhow::Result<Option<Phase2AttestedBaseline>> {
        let baseline = sqlx::query(
            r#"
SELECT
    memory_root_key,
    output_tree_sha256,
    schema_version,
    selection_sha256,
    prepared_inputs_sha256,
    consolidator_sha256,
    completion_watermark,
    selected_count,
    attested_at
FROM phase2_attested_baselines
WHERE memory_root_key = ? AND output_tree_sha256 = ?
            "#,
        )
        .bind(memory_root_key)
        .bind(output_tree_sha256)
        .fetch_optional(self.pool.as_ref())
        .await?
        .map(|row| {
            Ok::<_, sqlx::Error>(Phase2AttestedBaseline {
                memory_root_key: row.try_get("memory_root_key")?,
                output_tree_sha256: row.try_get("output_tree_sha256")?,
                schema_version: row.try_get("schema_version")?,
                selection_sha256: row.try_get("selection_sha256")?,
                prepared_inputs_sha256: row.try_get("prepared_inputs_sha256")?,
                consolidator_sha256: row.try_get("consolidator_sha256")?,
                completion_watermark: row.try_get("completion_watermark")?,
                selected_count: row.try_get("selected_count")?,
                attested_at: row.try_get("attested_at")?,
            })
        })
        .transpose()?;

        Ok(baseline)
    }

    pub async fn has_phase2_attested_baseline_for_root(
        &self,
        memory_root_key: &str,
    ) -> anyhow::Result<bool> {
        let exists = sqlx::query_scalar::<_, bool>(
            r#"SELECT EXISTS(SELECT 1 FROM phase2_attested_baselines WHERE memory_root_key = ?)"#,
        )
        .bind(memory_root_key)
        .fetch_one(self.pool.as_ref())
        .await?;

        Ok(exists)
    }
}

#[cfg(test)]
mod tests {
    use crate::Phase2AttestedBaseline;

    use super::super::test_support::unique_temp_dir;
    use super::StateRuntime;

    #[tokio::test]
    async fn phase2_attested_baseline_is_root_and_output_scoped() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let baseline = baseline("root-a", "tree-a");
        runtime
            .record_phase2_attested_baseline(&baseline)
            .await
            .expect("record baseline");

        assert_eq!(
            runtime
                .get_phase2_attested_baseline("root-a", "tree-a")
                .await
                .expect("load recorded baseline"),
            Some(baseline)
        );
        assert!(
            runtime
                .get_phase2_attested_baseline("root-a", "tree-b")
                .await
                .expect("load unmatched tree")
                .is_none(),
            "output tree hash must scope baseline lookups"
        );
        assert!(
            runtime
                .get_phase2_attested_baseline("root-b", "tree-a")
                .await
                .expect("load unmatched root")
                .is_none(),
            "memory root must scope baseline lookups"
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn record_phase2_attested_baseline_is_visible_for_root() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        runtime
            .record_phase2_attested_baseline(&baseline("root-a", "tree-a"))
            .await
            .expect("record baseline");

        assert!(
            runtime
                .has_phase2_attested_baseline_for_root("root-a")
                .await
                .expect("check recorded baselines")
        );
        assert!(
            !runtime
                .has_phase2_attested_baseline_for_root("root-b")
                .await
                .expect("check unrelated root")
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    fn baseline(memory_root_key: &str, output_tree_sha256: &str) -> Phase2AttestedBaseline {
        Phase2AttestedBaseline {
            memory_root_key: memory_root_key.to_string(),
            output_tree_sha256: output_tree_sha256.to_string(),
            schema_version: 1,
            selection_sha256: "selection".to_string(),
            prepared_inputs_sha256: "prepared".to_string(),
            consolidator_sha256: "consolidator".to_string(),
            completion_watermark: 42,
            selected_count: 3,
            attested_at: 123,
        }
    }
}
