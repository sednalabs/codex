//! Test-only helpers shared across the TUI crate.

use std::sync::LazyLock;

use codex_models_manager::bundled_models_response;
use codex_protocol::openai_models::ModelPreset;
pub(crate) use codex_utils_absolute_path::test_support::PathBufExt;
pub(crate) use codex_utils_absolute_path::test_support::test_path_buf;
use color_eyre::eyre::Result;
use serde::Serialize;
use serde::de::DeserializeOwned;

const TEST_WORKER_THREADS: usize = 1;
const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

pub(crate) static TEST_MODEL_PRESETS: LazyLock<Vec<ModelPreset>> = LazyLock::new(|| {
    let mut response = bundled_models_response()
        .unwrap_or_else(|err| panic!("bundled models.json should parse: {err}"));
    response.models.sort_by_key(|model| model.priority);
    let mut presets: Vec<ModelPreset> = response.models.into_iter().map(Into::into).collect();
    ModelPreset::mark_default_by_picker_visibility(&mut presets);
    presets
});

pub(crate) fn run_large_stack_test<F, Fut>(thread_name: &str, test_body: F) -> Result<()>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<()>> + 'static,
{
    std::thread::Builder::new()
        .name(thread_name.to_string())
        .stack_size(TEST_STACK_SIZE_BYTES)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(TEST_WORKER_THREADS)
                .thread_stack_size(TEST_STACK_SIZE_BYTES)
                .enable_all()
                .build()?;

            runtime.block_on(test_body())
        })?
        .join()
        .map_err(|panic| {
            let panic_message = if let Some(message) = panic.downcast_ref::<&str>() {
                (*message).to_string()
            } else if let Some(message) = panic.downcast_ref::<String>() {
                message.clone()
            } else {
                "large-stack test thread panicked".to_string()
            };
            color_eyre::eyre::eyre!("{panic_message}")
        })?
}

pub(crate) fn test_path_display(path: &str) -> String {
    test_path_buf(path).display().to_string()
}

pub(crate) fn session_source_cli<T>() -> T
where
    T: DeserializeOwned,
{
    from_app_server_wire(codex_app_server_protocol::SessionSource::Cli)
}

pub(crate) fn skill_scope_user<T>() -> T
where
    T: DeserializeOwned,
{
    from_app_server_wire(codex_app_server_protocol::SkillScope::User)
}

pub(crate) fn skill_scope_repo<T>() -> T
where
    T: DeserializeOwned,
{
    from_app_server_wire(codex_app_server_protocol::SkillScope::Repo)
}

fn from_app_server_wire<T>(value: impl Serialize) -> T
where
    T: DeserializeOwned,
{
    serde_json::to_value(value)
        .and_then(serde_json::from_value)
        .unwrap_or_else(|err| {
            panic!("app-server wire value should map to legacy helper type: {err}")
        })
}
