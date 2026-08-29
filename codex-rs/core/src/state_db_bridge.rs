use codex_rollout::state_db as rollout_state_db;
pub use codex_rollout::state_db::StateDbHandle;

use crate::config::Config;

pub async fn init_state_db(config: &Config) -> Option<StateDbHandle> {
    rollout_state_db::init(config).await
}

/// Initialize local state for a root that will install the Goal extension.
/// The returned bootstrap binds the diagnostic state handle to its one-time
/// admission authority; callers must pass it intact to their composition host.
pub async fn init_state_db_with_goal_runtime_bootstrap(
    config: &Config,
) -> Option<rollout_state_db::StateDbBootstrap> {
    match rollout_state_db::try_init_with_goal_runtime_bootstrap(config).await {
        Ok(bootstrap) => Some(bootstrap),
        Err(error) => {
            tracing::warn!(
                error = %error,
                "local state and the goal runtime were not initialized; continuing without Goal support"
            );
            None
        }
    }
}
