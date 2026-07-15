use codex_protocol::ThreadId;
use sqlx::Row;

use super::StateRuntime;
use crate::ThreadInferenceIdentityAuthoritySnapshot;
use crate::ThreadInferenceIdentityAuthorityUpdate;
use crate::decode_thread_inference_identity_authority;

impl StateRuntime {
    /// Reads both persisted inference-identity authorities without collapsing a missing row into
    /// legacy-missing authority.
    pub async fn get_thread_inference_identity_authority(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<Option<ThreadInferenceIdentityAuthoritySnapshot>> {
        let row = sqlx::query(
            "SELECT configured_inference_identity_authority, latest_request_inference_identity_authority FROM threads WHERE id = ?",
        )
        .bind(thread_id.to_string())
        .fetch_optional(self.pool.as_ref())
        .await?;
        row.map(|row| {
            let configured: Option<String> =
                row.try_get("configured_inference_identity_authority")?;
            let latest_request: Option<String> =
                row.try_get("latest_request_inference_identity_authority")?;
            Ok(ThreadInferenceIdentityAuthoritySnapshot {
                configured: decode_thread_inference_identity_authority(configured.as_deref()),
                latest_request: decode_thread_inference_identity_authority(
                    latest_request.as_deref(),
                ),
            })
        })
        .transpose()
    }

    /// Canonically encodes and atomically updates only the supplied inference-identity columns.
    ///
    /// Returns whether a projection row was updated. `false` means either that both fields were
    /// omitted or that no row matched `thread_id`.
    pub async fn update_thread_inference_identity_authority(
        &self,
        thread_id: ThreadId,
        update: ThreadInferenceIdentityAuthorityUpdate,
    ) -> anyhow::Result<bool> {
        let ThreadInferenceIdentityAuthorityUpdate {
            configured,
            latest_request,
        } = update;
        let configured = configured.encode()?;
        let latest_request = latest_request.encode()?;
        if configured.is_none() && latest_request.is_none() {
            return Ok(false);
        }
        let thread_id = thread_id.to_string();
        let result = match (configured.as_deref(), latest_request.as_deref()) {
            (Some(configured), Some(latest_request)) => {
                sqlx::query(
                    "UPDATE threads SET configured_inference_identity_authority = ?, latest_request_inference_identity_authority = ? WHERE id = ?",
                )
                .bind(configured)
                .bind(latest_request)
                .bind(thread_id)
                .execute(self.pool.as_ref())
                .await?
            }
            (Some(configured), None) => {
                sqlx::query(
                    "UPDATE threads SET configured_inference_identity_authority = ? WHERE id = ?",
                )
                .bind(configured)
                .bind(thread_id)
                .execute(self.pool.as_ref())
                .await?
            }
            (None, Some(latest_request)) => {
                sqlx::query(
                    "UPDATE threads SET latest_request_inference_identity_authority = ? WHERE id = ?",
                )
                .bind(latest_request)
                .bind(thread_id)
                .execute(self.pool.as_ref())
                .await?
            }
            (None, None) => unreachable!("all-omitted update returned above"),
        };
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
#[path = "thread_inference_identity_tests.rs"]
mod tests;
