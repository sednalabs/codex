from pathlib import Path

FILES = [
    "codex-rs/core/src/agent/control/spawn.rs",
    "codex-rs/core/src/session/turn.rs",
    "codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs",
    "codex-rs/ext/goal/src/runtime.rs",
]

needle = "GOAL_MULTI_AGENT_STRESS_METRIC,\n"
replacements = 0
for path in FILES:
    file_path = Path(path)
    lines = file_path.read_text().splitlines(keepends=True)
    for index, line in enumerate(lines[:-1]):
        if needle.strip() not in line:
            continue
        next_line = lines[index + 1]
        if next_line.strip() != "1,":
            continue
        indent = next_line[: len(next_line) - len(next_line.lstrip())]
        lines[index + 1] = f"{indent}/*inc*/ 1,\n"
        replacements += 1
    file_path.write_text("".join(lines))

if replacements != 9:
    raise SystemExit(
        f"expected exactly 9 diagnostic counter increment annotations, found {replacements}"
    )
