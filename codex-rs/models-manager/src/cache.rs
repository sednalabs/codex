use chrono::DateTime;
use chrono::Utc;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::deserialize_model_infos_with_legacy_base;
use serde::Deserialize;
use serde::Serialize;
use std::fmt;
use std::future::Future;
use std::io;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;
use tokio::fs;
use tracing::info;

/// Asynchronous storage for model catalog snapshots used by the models manager.
///
/// Implementations own cache freshness and lookup partitioning. [`ModelsCache::load`] must not
/// return stale entries or entries for a different client version. A shared backend must also keep
/// catalogs for different providers and tenants separate. The manager also validates the entry
/// identity against the current endpoint before using it.
///
/// Cache failures are non-fatal. The models manager logs them and falls back to the configured
/// models endpoint.
pub trait ModelsCache: fmt::Debug + Send + Sync {
    /// Loads a fresh entry for `client_version`.
    ///
    /// Returns `Ok(None)` for a normal miss, including an absent, stale, or version-mismatched
    /// entry. Returns `Err` when the backend could not complete the lookup; the models manager
    /// treats that error as a miss and fetches the catalog from the models endpoint.
    fn load<'a>(
        &'a self,
        client_version: &'a str,
    ) -> ModelsCacheFuture<'a, Result<Option<ModelsCacheEntry>, ModelsCacheError>>;

    /// Stores `entry`, replacing the snapshot for this cache implementation's lookup identity.
    ///
    /// The implementation derives that identity from its own configuration and the entry metadata.
    /// For example, a shared backend can capture provider and tenant identity when it is constructed.
    fn store<'a>(
        &'a self,
        entry: &'a ModelsCacheEntry,
    ) -> ModelsCacheFuture<'a, Result<(), ModelsCacheError>>;

    /// Extends freshness only if the stored version, identity, and ETag match.
    ///
    /// The models manager calls this after the endpoint confirms that the cached ETag is still
    /// current. Implementations must preserve the models, identity, ETag, and client version. This operation
    /// cannot be expressed in terms of [`ModelsCache::load`], because `load` intentionally omits
    /// expired entries. File-backed implementations can read and revalidate the stored entry
    /// directly, while backends with native TTL support can extend the entry in place.
    fn refresh_ttl<'a>(
        &'a self,
        client_version: &'a str,
        identity: &'a str,
        etag: &'a str,
    ) -> ModelsCacheFuture<'a, Result<(), ModelsCacheError>>;
}

/// Boxed future returned by [`ModelsCache`] implementations.
pub type ModelsCacheFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Serialized model catalog and validation metadata stored in a [`ModelsCache`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelsCacheEntry {
    /// Time when the catalog was fetched or last revalidated with the models endpoint.
    pub fetched_at: DateTime<Utc>,
    /// Endpoint ETag used to revalidate the catalog without downloading an unchanged payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// Client version for which this catalog was returned.
    ///
    /// The models manager rejects entries whose value is absent or differs from its current version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
    pub(crate) client_version: Option<String>,
    #[serde(deserialize_with = "deserialize_model_infos_with_legacy_base")]
    pub(crate) models: Vec<ModelInfo>,
=======
    pub client_version: Option<String>,
    /// Opaque provider and auth identity. Unscoped legacy entries are cache misses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// Models returned by the catalog endpoint.
    #[serde(
        deserialize_with = "codex_protocol::openai_models::deserialize_model_infos_with_legacy_base"
    )]
    pub models: Vec<ModelInfo>,
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
}

impl ModelsCacheEntry {
    /// Returns `true` when the cache entry has not exceeded the configured TTL.
    fn is_fresh(&self, ttl: Duration) -> bool {
        if ttl.is_zero() {
            return false;
        }
        let Ok(ttl_duration) = chrono::Duration::from_std(ttl) else {
            return false;
        };
        let age = Utc::now().signed_duration_since(self.fetched_at);
        age <= ttl_duration
    }
}

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
#[cfg(test)]
mod tests {
    use super::ModelsCache;
    use serde_json::json;

    #[test]
    fn cache_deserializer_accepts_catalog_without_legacy_base() {
        let mut payload = json!({
            "fetched_at": "2026-09-07T00:00:00Z",
            "models": [{
                "slug": "astra",
                "display_name": "Astra",
                "description": null,
                "supported_reasoning_levels": [],
                "shell_type": "shell_command",
                "visibility": "list",
                "supported_in_api": true,
                "priority": 1,
                "availability_nux": null,
                "upgrade": null,
                "model_messages": {
                    "instructions_template": "template",
                    "instructions_variables": null
                },
                "support_verbosity": false,
                "default_verbosity": null,
                "apply_patch_tool_type": null,
                "truncation_policy": {"mode": "bytes", "limit": 10000},
                "supports_parallel_tool_calls": false,
                "experimental_supported_tools": []
            }]
        });
        let cache: ModelsCache =
            serde_json::from_value(payload.clone()).expect("cache catalog should deserialize");

        assert_eq!(cache.models[0].base_instructions, "template");

        payload["models"][0]
            .as_object_mut()
            .unwrap()
            .remove("model_messages");
        let error = serde_json::from_value::<ModelsCache>(payload)
            .unwrap_err()
            .to_string();
        assert!(error.contains("missing both"));

        let mut whitespace = json!({
            "fetched_at": "2026-09-07T00:00:00Z",
            "models": [{
                "slug": "astra",
                "display_name": "Astra",
                "description": null,
                "supported_reasoning_levels": [],
                "shell_type": "shell_command",
                "visibility": "list",
                "supported_in_api": true,
                "priority": 1,
                "availability_nux": null,
                "upgrade": null,
                "model_messages": {
                    "instructions_template": " \n\t",
                    "instructions_variables": null
                },
                "support_verbosity": false,
                "default_verbosity": null,
                "apply_patch_tool_type": null,
                "truncation_policy": {"mode": "bytes", "limit": 10000},
                "supports_parallel_tool_calls": false,
                "experimental_supported_tools": []
            }]
        });
        let error = serde_json::from_value::<ModelsCache>(whitespace.take())
            .unwrap_err()
            .to_string();
        assert!(error.contains("missing both"));
    }
}
=======
/// Error returned by a [`ModelsCache`] implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelsCacheError {
    message: String,
}

impl ModelsCacheError {
    /// Create a cache error without exposing backend-specific error types.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ModelsCacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ModelsCacheError {}

/// Built-in file-backed model catalog cache.
#[derive(Debug)]
pub(crate) struct FileModelsCache {
    cache_path: PathBuf,
    cache_ttl: Duration,
}

impl FileModelsCache {
    /// Create a file-backed cache with the given path and TTL.
    pub(crate) fn new(cache_path: PathBuf, cache_ttl: Duration) -> Self {
        Self {
            cache_path,
            cache_ttl,
        }
    }
}

impl ModelsCache for FileModelsCache {
    fn load<'a>(
        &'a self,
        client_version: &'a str,
    ) -> ModelsCacheFuture<'a, Result<Option<ModelsCacheEntry>, ModelsCacheError>> {
        Box::pin(
            async move { load_fresh_file(&self.cache_path, self.cache_ttl, client_version).await },
        )
    }

    fn store<'a>(
        &'a self,
        entry: &'a ModelsCacheEntry,
    ) -> ModelsCacheFuture<'a, Result<(), ModelsCacheError>> {
        Box::pin(async move { save_file(&self.cache_path, entry).await })
    }

    fn refresh_ttl<'a>(
        &'a self,
        client_version: &'a str,
        identity: &'a str,
        etag: &'a str,
    ) -> ModelsCacheFuture<'a, Result<(), ModelsCacheError>> {
        Box::pin(async move {
            let mut entry = load_file(&self.cache_path)
                .await
                .map_err(cache_error)?
                .ok_or_else(|| ModelsCacheError::new("cache not found"))?;
            if entry.client_version.as_deref() != Some(client_version)
                || entry.identity.as_deref() != Some(identity)
                || entry.etag.as_deref() != Some(etag)
                || entry.is_fresh(self.cache_ttl / 2)
            {
                return Ok(());
            }
            entry.fetched_at = Utc::now();
            save_file(&self.cache_path, &entry).await
        })
    }
}

async fn load_fresh_file(
    cache_path: &PathBuf,
    cache_ttl: Duration,
    expected_version: &str,
) -> Result<Option<ModelsCacheEntry>, ModelsCacheError> {
    info!(
        cache_path = %cache_path.display(),
        expected_version,
        "models cache: attempting load_fresh"
    );
    let Some(cache) = load_file(cache_path).await.map_err(cache_error)? else {
        return Ok(None);
    };
    info!(
        cache_path = %cache_path.display(),
        cached_version = ?cache.client_version,
        fetched_at = %cache.fetched_at,
        "models cache: loaded cache file"
    );
    if cache.client_version.as_deref() != Some(expected_version) {
        info!(
            cache_path = %cache_path.display(),
            expected_version,
            cached_version = ?cache.client_version,
            "models cache: cache version mismatch"
        );
        return Ok(None);
    }
    if !cache.is_fresh(cache_ttl) {
        info!(
            cache_path = %cache_path.display(),
            cache_ttl_secs = cache_ttl.as_secs(),
            fetched_at = %cache.fetched_at,
            "models cache: cache is stale"
        );
        return Ok(None);
    }
    info!(
        cache_path = %cache_path.display(),
        cache_ttl_secs = cache_ttl.as_secs(),
        "models cache: cache hit"
    );
    Ok(Some(cache))
}

async fn load_file(cache_path: &PathBuf) -> io::Result<Option<ModelsCacheEntry>> {
    match fs::read(cache_path).await {
        Ok(contents) => {
            let cache = serde_json::from_slice(&contents)
                .map_err(|err| io::Error::new(ErrorKind::InvalidData, err.to_string()))?;
            Ok(Some(cache))
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

async fn save_file(cache_path: &PathBuf, cache: &ModelsCacheEntry) -> Result<(), ModelsCacheError> {
    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent).await.map_err(cache_error)?;
    }
    let json = serde_json::to_vec_pretty(cache).map_err(cache_error)?;
    fs::write(cache_path, json).await.map_err(cache_error)
}

fn cache_error(error: impl fmt::Display) -> ModelsCacheError {
    ModelsCacheError::new(error.to_string())
}
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
