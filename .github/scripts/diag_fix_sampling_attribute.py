from pathlib import Path

path = Path("codex-rs/core/src/session/turn.rs")
text = path.read_text()
attrs = '''#[allow(clippy::too_many_arguments)]
#[allow(deprecated)]
#[instrument(level = "trace",
    skip_all,
    fields(
        turn_id = %step_context.turn.sub_id,
        model = %step_context.turn.model_info.slug,
        cwd = %step_context.turn.cwd.display()
    )
)]
'''
helper_start = attrs + '''fn goal_multi_agent_stress_continuation_input(input: &[ResponseItem]) -> bool {
'''
if text.count(helper_start) != 1:
    raise SystemExit("expected exactly one misbound sampling attribute block")
text = text.replace(
    helper_start,
    '''fn goal_multi_agent_stress_continuation_input(input: &[ResponseItem]) -> bool {
''',
    1,
)
marker = '''async fn run_sampling_request(
'''
if text.count(marker) != 1:
    raise SystemExit("expected exactly one run_sampling_request marker")
text = text.replace(marker, attrs + marker, 1)
path.write_text(text)
