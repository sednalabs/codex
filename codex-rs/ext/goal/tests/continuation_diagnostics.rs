use std::sync::Arc;
use std::sync::Weak;

use codex_analytics::AnalyticsEventsClient;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::TurnErrorInput;
use codex_extension_api::TurnStartInput;
use codex_goal_extension::GoalObjectiveUpdate;
use codex_goal_extension::GoalService;
use codex_goal_extension::GoalSetRequest;
use codex_goal_extension::GoalTokenBudgetUpdate;
use codex_goal_extension::install_with_backend;
use codex_protocol::ThreadId;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::UsageLimitReachedError;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadGoalStatus;
use codex_protocol::protocol::TokenUsage;
use codex_utils_absolute_path::test_support::PathExt;
use serial_test::serial;
use std::ffi::OsString;
use tempfile::TempDir;

#[tokio::test]
#[serial]
async fn usage_limit_preservation_is_opt_in() -> anyhow::Result<()> {
    let (ordinary_status, ordinary_tokens, ordinary_marker) =
        exercise_usage_limit(/*preserve*/ false, temporary_usage_limit_details()).await?;
    assert_eq!(ThreadGoalStatus::UsageLimited, ordinary_status);
    assert_eq!(7, ordinary_tokens, "terminal usage must be accounted");
    assert!(!ordinary_marker, "ordinary stop must not leave a research marker");

    let (persisted_status, persisted_tokens, persisted_marker) =
        exercise_usage_limit(/*preserve*/ true, temporary_usage_limit_details()).await?;
    assert_eq!(ThreadGoalStatus::Active, persisted_status);
    assert_eq!(
        7, persisted_tokens,
        "preserved usage must be accounted once"
    );
    assert!(persisted_marker, "preserved goal must leave a restart marker");
    Ok(())
}

#[tokio::test]
#[serial]
async fn entitlement_denials_are_not_preserved_as_usage_limits() -> anyhow::Result<()> {
    for details in [
        CodexErrorDetails::QuotaExceeded,
        CodexErrorDetails::UsageNotIncluded,
    ] {
        let (status, tokens, marker) = exercise_usage_limit(/*preserve*/ true, details).await?;
        assert_eq!(ThreadGoalStatus::Blocked, status);
        assert_eq!(7, tokens, "entitlement denial usage must be accounted");
        assert!(!marker, "entitlement denial must not leave a research marker");
    }
    Ok(())
}

async fn exercise_usage_limit(
    preserve: bool,
    error_details: CodexErrorDetails,
) -> anyhow::Result<(ThreadGoalStatus, i64, bool)> {
    let _flag = EnvVarGuard::set(
        "CODEX_EXPERIMENTAL_CONTINUITY_PRESERVE_AFTER_USAGE_LIMIT",
        preserve,
    );
    let tempdir = TempDir::new()?;
    let runtime = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(tempdir.keep().as_path().abs()),
        "diagnostic-provider".to_string(),
    )
    .await?;
    let thread_id = ThreadId::from_string("11111111-1111-4111-8111-111111111111")
        .map_err(anyhow::Error::msg)?;
    let metadata = codex_state::ThreadMetadataBuilder::new(
        thread_id,
        runtime
            .sqlite()
            .home()
            .join(format!("rollout-{thread_id}.jsonl")),
        chrono::Utc::now(),
        SessionSource::Cli,
    )
    .build("diagnostic-provider");
    runtime.upsert_thread(&metadata).await?;

    let goal_service = Arc::new(GoalService::new());
    let mut builder = ExtensionRegistryBuilder::<()>::new();
    install_with_backend(
        &mut builder,
        Arc::clone(&runtime),
        AnalyticsEventsClient::disabled(),
        /*metrics_client*/ None,
        Weak::new(),
        Arc::clone(&goal_service),
        |_| true,
    );
    let registry = builder.build();
    let session_store = ExtensionData::new("session-1");
    let thread_store = ExtensionData::new(thread_id.to_string());
    let session_source = SessionSource::Cli;
    for contributor in registry.thread_lifecycle_contributors() {
        contributor
            .on_thread_start(ThreadStartInput {
                config: &(),
                session_source: &session_source,
                persistent_thread_state_available: true,
                environments: &[],
                mcp_resource_client: None,
                session_store: &session_store,
                thread_store: &thread_store,
            })
            .await;
    }

    let outcome = goal_service
        .set_thread_goal(
            runtime.as_ref(),
            GoalSetRequest {
                thread_id,
                objective: GoalObjectiveUpdate::Set("diagnostic continuation"),
                status: Some(ThreadGoalStatus::Active),
                token_budget: GoalTokenBudgetUpdate::Keep,
            },
        )
        .await?;
    outcome.apply_runtime_effects(&goal_service).await;

    let turn_store = ExtensionData::new("turn-1");
    let collaboration_mode = CollaborationMode {
        mode: ModeKind::Default,
        settings: Settings {
            model: "gpt-test".to_string(),
            reasoning_effort: None,
            developer_instructions: None,
        },
    };
    for contributor in registry.turn_lifecycle_contributors() {
        contributor
            .on_turn_start(TurnStartInput {
                turn_id: "turn-1",
                collaboration_mode: &collaboration_mode,
                token_usage_at_turn_start: &TokenUsage::default(),
                session_store: &session_store,
                thread_store: &thread_store,
                turn_store: &turn_store,
            })
            .await;
    }

    let usage = TokenUsage {
        input_tokens: 7,
        total_tokens: 7,
        ..TokenUsage::default()
    };
    for contributor in registry.token_usage_contributors() {
        contributor
            .on_token_usage(
                &session_store,
                &thread_store,
                &turn_store,
                &codex_protocol::protocol::TokenUsageInfo {
                    total_token_usage: usage.clone(),
                    last_token_usage: usage.clone(),
                    model_context_window: None,
                },
            )
            .await;
    }

    for contributor in registry.turn_lifecycle_contributors() {
        contributor
            .on_turn_error(TurnErrorInput {
                turn_id: "turn-1",
                error: CodexErrorInfo::UsageLimitExceeded,
                error_details: Some(&error_details),
                session_store: &session_store,
                thread_store: &thread_store,
                turn_store: &turn_store,
            })
            .await;
    }

    let goal = goal_service
        .get_thread_goal(runtime.as_ref(), thread_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("goal should exist"))?;
    let marker = runtime
        .thread_goals()
        .has_thread_goal_continuity_research(thread_id)
        .await?;
    Ok((goal.status, goal.tokens_used, marker))
}

fn temporary_usage_limit_details() -> CodexErrorDetails {
    CodexErrorDetails::UsageLimitReached(UsageLimitReachedError {
        plan_type: None,
        resets_at: None,
        rate_limits: None,
        promo_message: None,
        rate_limit_reached_type: None,
    })
}

struct EnvVarGuard {
    key: &'static str,
    original: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, enabled: bool) -> Self {
        let original = std::env::var_os(key);
        unsafe {
            if enabled {
                std::env::set_var(key, "1");
            } else {
                std::env::remove_var(key);
            }
        }
        Self { key, original }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.original {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}
