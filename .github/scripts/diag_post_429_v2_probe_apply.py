from pathlib import Path


def replace_one(path: str, old: str, new: str) -> None:
    file_path = Path(path)
    text = file_path.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, found {count}")
    file_path.write_text(text.replace(old, new, 1))


replace_one(
    "codex-rs/core/src/diagnostic_flags.rs",
    '''const GOAL_ERROR_CONTINUATION_ENV: &str = "CODEX_EXPERIMENTAL_GOAL_ERROR_CONTINUATION";
const GOAL_ERROR_RETRY_IN_PLACE_ENV: &str = "CODEX_EXPERIMENTAL_GOAL_ERROR_RETRY_IN_PLACE";
const GOAL_MULTI_AGENT_STRESS_ENV: &str = "CODEX_EXPERIMENTAL_GOAL_MULTI_AGENT_STRESS";
''',
    '''use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

const GOAL_ERROR_CONTINUATION_ENV: &str = "CODEX_EXPERIMENTAL_GOAL_ERROR_CONTINUATION";
const GOAL_ERROR_RETRY_IN_PLACE_ENV: &str = "CODEX_EXPERIMENTAL_GOAL_ERROR_RETRY_IN_PLACE";
const GOAL_MULTI_AGENT_STRESS_ENV: &str = "CODEX_EXPERIMENTAL_GOAL_MULTI_AGENT_STRESS";

static GOAL_MULTI_AGENT_PROBE_SEQUENCE: AtomicU64 = AtomicU64::new(1);
''',
)

replace_one(
    "codex-rs/core/src/diagnostic_flags.rs",
    '''pub fn goal_multi_agent_stress_enabled() -> bool {
    env_enabled(GOAL_MULTI_AGENT_STRESS_ENV)
}

pub fn suppress_usage_limit_state_updates() -> bool {
''',
    '''pub fn goal_multi_agent_stress_enabled() -> bool {
    env_enabled(GOAL_MULTI_AGENT_STRESS_ENV)
}

pub fn next_goal_multi_agent_probe_task_name(kind: &str) -> String {
    let epoch_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let sequence = GOAL_MULTI_AGENT_PROBE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("goal_{kind}_{epoch_millis}_{sequence}")
}

pub fn suppress_usage_limit_state_updates() -> bool {
''',
)

replace_one(
    "codex-rs/ext/goal/src/steering.rs",
    '''use std::sync::LazyLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
''',
    '''use std::sync::LazyLock;
''',
)

replace_one(
    "codex-rs/ext/goal/src/steering.rs",
    '''});
static DIAGNOSTIC_GOAL_PROBE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn parse_embedded_template''',
    '''});

fn parse_embedded_template''',
)

replace_one(
    "codex-rs/ext/goal/src/steering.rs",
    '''        let task_name = diagnostic_goal_probe_task_name();
''',
    '''        let task_name =
            codex_core::diagnostic_flags::next_goal_multi_agent_probe_task_name("continuation");
''',
)

replace_one(
    "codex-rs/ext/goal/src/steering.rs",
    '''             - Before other substantive work in this automatic continuation, call the plain `spawn_agent` tool exactly once.
             - Use task_name `{task_name}` and fork_turns `none`. Do not substitute a namespaced/legacy collaboration spawn tool.
''',
    '''             - Before other substantive work in this automatic continuation, call the Multi-Agent V2 `spawn_agent` tool exposed in this turn exactly once.
             - Use task_name `{task_name}` and fork_turns `none`. Use the configured V2 namespace if this turn exposes one; do not substitute the legacy V1 collaboration tool.
''',
)

replace_one(
    "codex-rs/ext/goal/src/steering.rs",
    '''
fn diagnostic_goal_probe_task_name() -> String {
    let epoch_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let sequence = DIAGNOSTIC_GOAL_PROBE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("goal_probe_{epoch_millis}_{sequence}")
}
''',
    '''
''',
)

replace_one(
    "codex-rs/core/src/session/turn.rs",
    '''const POST_SAMPLING_TOKEN_ESTIMATE_TARGET: &str = "codex_core::post_sampling_token_estimate";
''',
    '''const POST_SAMPLING_TOKEN_ESTIMATE_TARGET: &str = "codex_core::post_sampling_token_estimate";
const GOAL_MULTI_AGENT_STRESS_CONTINUATION_MARKER: &str = "Diagnostic continuation probe:";
''',
)

replace_one(
    "codex-rs/core/src/session/turn.rs",
    '''    let max_retries = turn_context.provider.info().stream_max_retries();
    let mut retries = 0;
    let mut usage_limit_retries = 0;
    let mut capacity_retries = 0;
    let mut initial_input = Some(input);
''',
    '''    let max_retries = turn_context.provider.info().stream_max_retries();
    let multi_agent_stress_goal_turn = crate::diagnostic_flags::goal_multi_agent_stress_enabled()
        && turn_context.multi_agent_version == codex_protocol::protocol::MultiAgentVersion::V2
        && !matches!(
            &turn_context.session_source,
            codex_protocol::protocol::SessionSource::SubAgent(_)
        )
        && goal_multi_agent_stress_continuation_input(&input);
    let mut retries = 0;
    let mut usage_limit_retries = 0;
    let mut post_usage_limit_v2_probe_dispatched = false;
    let mut capacity_retries = 0;
    let mut initial_input = Some(input);
''',
)

replace_one(
    "codex-rs/core/src/session/turn.rs",
    '''                    } else {
                        warn!(
                            turn_id = %turn_context.sub_id,
                            "goal error diagnostic mode skipped rate-limit snapshot update"
                        );
                    }
                    err
''',
    '''                    } else {
                        warn!(
                            turn_id = %turn_context.sub_id,
                            "goal error diagnostic mode skipped rate-limit snapshot update"
                        );
                    }
                    if multi_agent_stress_goal_turn && !post_usage_limit_v2_probe_dispatched {
                        post_usage_limit_v2_probe_dispatched = true;
                        run_goal_multi_agent_stress_post_usage_limit_probe(
                            tool_runtime.clone(),
                            Arc::clone(&turn_context),
                            cancellation_token.child_token(),
                        )
                        .await;
                    }
                    err
''',
)

insert_before = '''async fn run_sampling_request(
'''
helper = '''fn goal_multi_agent_stress_continuation_input(input: &[ResponseItem]) -> bool {
    input.iter().any(|item| {
        let ResponseItem::Message { content, .. } = item else {
            return false;
        };
        content.iter().any(|content_item| {
            matches!(
                content_item,
                ContentItem::InputText { text }
                    if text.contains(GOAL_MULTI_AGENT_STRESS_CONTINUATION_MARKER)
            )
        })
    })
}

async fn run_goal_multi_agent_stress_post_usage_limit_probe(
    tool_runtime: ToolCallRuntime,
    turn_context: Arc<TurnContext>,
    cancellation_token: CancellationToken,
) {
    let task_name =
        crate::diagnostic_flags::next_goal_multi_agent_probe_task_name("post_429");
    let call_id = format!("diag_{task_name}");
    let tool_name = if turn_context.provider.capabilities().namespace_tools {
        turn_context
            .config
            .multi_agent_v2
            .tool_namespace
            .as_deref()
            .map(|namespace| ToolName::namespaced(namespace, "spawn_agent"))
            .unwrap_or_else(|| ToolName::plain("spawn_agent"))
    } else {
        ToolName::plain("spawn_agent")
    };
    let arguments = serde_json::json!({
        "message": "Run one bounded diagnostic child step: use an available read-only tool to inspect the current environment or worktree, then report one concise evidence-backed fact to the parent.",
        "task_name": task_name,
        "fork_turns": "none"
    })
    .to_string();
    let call = crate::tools::router::ToolCall {
        tool_name: tool_name.clone(),
        call_id: call_id.clone(),
        payload: crate::tools::context::ToolPayload::Function { arguments },
    };

    turn_context.session_telemetry.counter(
        "codex.diagnostic.goal_multi_agent_stress",
        1,
        &[("stage", "post_usage_limit_dispatch_attempt")],
    );
    tracing::info!(
        turn_id = %turn_context.sub_id,
        %call_id,
        tool = %tool_name,
        "multi-agent stress diagnostic dispatching bounded post-usage-limit V2 spawn"
    );

    match tool_runtime
        .handle_tool_call_with_source(
            call,
            crate::tools::router::ToolCallSource::Direct,
            cancellation_token,
        )
        .await
    {
        Ok(_) => {
            turn_context.session_telemetry.counter(
                "codex.diagnostic.goal_multi_agent_stress",
                1,
                &[("stage", "post_usage_limit_dispatch_completed")],
            );
        }
        Err(error) => {
            turn_context.session_telemetry.counter(
                "codex.diagnostic.goal_multi_agent_stress",
                1,
                &[("stage", "post_usage_limit_dispatch_failed")],
            );
            warn!(
                turn_id = %turn_context.sub_id,
                %call_id,
                %error,
                "multi-agent stress diagnostic post-usage-limit V2 spawn failed"
            );
        }
    }
}

'''
path = Path("codex-rs/core/src/session/turn.rs")
text = path.read_text()
idx = text.find(insert_before)
if idx == -1:
    raise SystemExit("turn.rs: run_sampling_request marker not found")
text = text[:idx] + helper + text[idx:]
path.write_text(text)
