#!/usr/bin/env bash
set -euo pipefail

python3 -m py_compile   .codex/skills/babysit-pr/scripts/gh_pr_watch.py   .codex/skills/babysit-gh-workflow-run/scripts/gh_workflow_run_watch.py   .codex/skills/babysit-gh-workflow-run/scripts/gh_dispatch_and_watch.py   .codex/skills/sedna/subagent-session-tail/scripts/inspect_subagent_tail.py
python3 .codex/skills/babysit-gh-workflow-run/tests/test_gh_workflow_run_watch.py
python3 .codex/skills/babysit-gh-workflow-run/tests/test_gh_dispatch_and_watch.py
python3 .codex/skills/sedna/subagent-session-tail/scripts/inspect_subagent_tail.py --help >/dev/null
