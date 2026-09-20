use super::agent;
use codex_protocol::protocol::SandboxPolicy;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use tempfile::TempDir;

#[tokio::test]
async fn consolidation_uses_canonical_parent_enforcement() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let home = Arc::new(TempDir::new()?);
    let test = test_codex()
        .with_home(home)
        .build_with_auto_env(&server)
        .await?;
    let root = crate::memory_root(&test.config.codex_home);
    let managed_worker_policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![root.clone()],
        network_access: false,
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: true,
    };

    let agent_config = agent::get_config(&test.config).expect("agent config should be created");

    assert_eq!(
        agent_config.permissions.permission_profile(),
        &codex_protocol::models::PermissionProfile::from_legacy_sandbox_policy_for_cwd(
            &managed_worker_policy,
            root.as_path(),
        )
    );

    test.codex.shutdown_and_wait().await?;
    Ok(())
}
