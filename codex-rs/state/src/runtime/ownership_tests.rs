use super::StateRuntime;
use super::ownership::RuntimeOwnershipError;
use super::ownership::canonical_goals_db_identity;
use super::test_support::unique_temp_dir;
use crate::SqliteConfig;
use codex_protocol::ThreadId;
use codex_utils_absolute_path::test_support::PathExt;
use std::path::Path;
use std::process::Command;

fn sqlite_config(home: &Path) -> SqliteConfig {
    SqliteConfig::new_for_testing(home.abs())
}

fn test_thread_id() -> ThreadId {
    ThreadId::from_string("00000000-0000-0000-0000-000000000133").expect("valid thread id")
}

fn assert_busy(error: anyhow::Error) {
    let ownership_error = error
        .downcast_ref::<RuntimeOwnershipError>()
        .expect("initialization must return the typed ownership error");
    assert!(
        ownership_error.is_busy(),
        "expected busy ownership error, got {ownership_error}"
    );
}

#[tokio::test]
async fn same_process_competing_init_returns_busy() {
    let home = unique_temp_dir();
    tokio::fs::create_dir_all(&home)
        .await
        .expect("create test home");
    let sqlite = sqlite_config(&home);
    let owner = StateRuntime::init(sqlite.clone(), "test-provider".to_string())
        .await
        .expect("first runtime should initialize");

    let error = match StateRuntime::init(sqlite, "test-provider".to_string()).await {
        Ok(runtime) => {
            drop(runtime);
            panic!("same-process competing initialization must be rejected")
        }
        Err(error) => error,
    };
    assert_busy(error);

    drop(owner);
    tokio::fs::remove_dir_all(home)
        .await
        .expect("remove test home");
}

#[tokio::test]
async fn direct_goals_mutation_requires_the_owner_capability() {
    let home = unique_temp_dir();
    tokio::fs::create_dir_all(&home)
        .await
        .expect("create test home");
    let runtime = StateRuntime::init(sqlite_config(&home), "test-provider".to_string())
        .await
        .expect("runtime should initialize");
    assert!(runtime.thread_goals().owner_capability().is_some());

    let thread_id = test_thread_id();
    let goal = runtime
        .thread_goals()
        .replace_thread_goal(
            thread_id,
            "exercise the owner-only goals path",
            crate::ThreadGoalStatus::Active,
            None,
        )
        .await
        .expect("the owner capability should authorize direct mutation");
    assert_eq!(
        Some(goal),
        runtime
            .thread_goals()
            .get_thread_goal(thread_id)
            .await
            .expect("owner should read the goal")
    );

    drop(runtime);
    tokio::fs::remove_dir_all(home)
        .await
        .expect("remove test home");
}

#[tokio::test]
async fn failed_initialization_releases_ownership_for_reacquire() {
    let home = unique_temp_dir();
    tokio::fs::create_dir_all(&home)
        .await
        .expect("create test home");
    let sqlite = sqlite_config(&home);
    let state_path = sqlite.state_db_path();
    tokio::fs::create_dir(&state_path)
        .await
        .expect("create blocking state path");

    let error = match StateRuntime::init(sqlite.clone(), "test-provider".to_string()).await {
        Ok(runtime) => {
            drop(runtime);
            panic!("a directory at the state database path must fail initialization")
        }
        Err(error) => error,
    };
    assert!(!error.to_string().is_empty());

    tokio::fs::remove_dir(&state_path)
        .await
        .expect("remove blocking state path");
    let runtime = StateRuntime::init(sqlite, "test-provider".to_string())
        .await
        .expect("a failed initialization must release ownership for reacquire");
    drop(runtime);

    tokio::fs::remove_dir_all(home)
        .await
        .expect("remove test home");
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_parent_paths_converge_to_one_identity() {
    use std::os::unix::fs::symlink;

    let real_home = unique_temp_dir();
    let alias_home = unique_temp_dir();
    tokio::fs::create_dir_all(&real_home)
        .await
        .expect("create real test home");
    symlink(&real_home, &alias_home).expect("create symlinked parent");

    let real_sqlite = sqlite_config(&real_home);
    let alias_sqlite = sqlite_config(&alias_home);
    assert_eq!(
        canonical_goals_db_identity(&real_sqlite.goals_db_path()).expect("real identity"),
        canonical_goals_db_identity(&alias_sqlite.goals_db_path()).expect("alias identity")
    );

    let owner = StateRuntime::init(real_sqlite, "test-provider".to_string())
        .await
        .expect("real parent runtime should initialize");
    let error = match StateRuntime::init(alias_sqlite, "test-provider".to_string()).await {
        Ok(runtime) => {
            drop(runtime);
            panic!("a symlinked parent must not admit a competing runtime")
        }
        Err(error) => error,
    };
    assert_busy(error);

    drop(owner);
    std::fs::remove_file(&alias_home).expect("remove symlinked parent");
    tokio::fs::remove_dir_all(real_home)
        .await
        .expect("remove real test home");
}

#[cfg(unix)]
#[tokio::test]
async fn subprocess_competing_init_returns_busy() {
    let home = unique_temp_dir();
    tokio::fs::create_dir_all(&home)
        .await
        .expect("create test home");
    let owner = StateRuntime::init(sqlite_config(&home), "test-provider".to_string())
        .await
        .expect("owner should initialize");

    let result_path = home.join("subprocess-result");
    let executable = std::env::current_exe().expect("current test executable");
    let home_for_child = home.clone();
    let result_for_child = result_path.clone();
    let status = tokio::task::spawn_blocking(move || {
        Command::new(executable)
            .args([
                "--exact",
                "runtime::ownership_tests::subprocess_init_probe",
                "--nocapture",
            ])
            .env("CODEX_STATE_OWNERSHIP_PROBE_HOME", home_for_child)
            .env("CODEX_STATE_OWNERSHIP_PROBE_RESULT", result_for_child)
            .status()
    })
    .await
    .expect("join child process")
    .expect("spawn child process");
    assert!(status.success(), "subprocess probe should pass");
    assert_eq!(
        "busy",
        tokio::fs::read_to_string(result_path)
            .await
            .expect("read subprocess result")
    );

    drop(owner);
    tokio::fs::remove_dir_all(home)
        .await
        .expect("remove test home");
}

#[test]
fn subprocess_init_probe() {
    let (Some(home), Some(result_path)) = (
        std::env::var_os("CODEX_STATE_OWNERSHIP_PROBE_HOME"),
        std::env::var_os("CODEX_STATE_OWNERSHIP_PROBE_RESULT"),
    ) else {
        return;
    };

    let runtime = tokio::runtime::Runtime::new().expect("probe runtime");
    let outcome = runtime.block_on(async {
        match StateRuntime::init(sqlite_config(Path::new(&home)), "test-provider".to_string()).await
        {
            Err(error)
                if error
                    .downcast_ref::<RuntimeOwnershipError>()
                    .is_some_and(RuntimeOwnershipError::is_busy) =>
            {
                "busy"
            }
            Err(_) => "other-error",
            Ok(runtime) => {
                drop(runtime);
                "acquired"
            }
        }
    });
    std::fs::write(result_path, outcome).expect("write subprocess result");
}

#[tokio::test]
async fn close_closes_all_runtime_pools() {
    let home = unique_temp_dir();
    tokio::fs::create_dir_all(&home)
        .await
        .expect("create test home");
    let runtime = StateRuntime::init(sqlite_config(&home), "test-provider".to_string())
        .await
        .expect("runtime should initialize");

    runtime.close().await;

    assert!(runtime.pool.is_closed(), "state pool must be closed");
    assert!(runtime.logs_pool.is_closed(), "logs pool must be closed");
    assert!(runtime.usage_pool.is_closed(), "usage pool must be closed");
    assert!(
        runtime
            .thread_goals()
            .get_thread_goal(test_thread_id())
            .await
            .is_err(),
        "goals pool must reject work after close"
    );
    assert!(
        runtime.memories.clear_memory_data().await.is_err(),
        "memories pool must reject work after close"
    );

    drop(runtime);
    tokio::fs::remove_dir_all(home)
        .await
        .expect("remove test home");
}
