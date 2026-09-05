#!/usr/bin/env bash
set -euo pipefail

python3 -m py_compile   .codex/skills/babysit-pr/scripts/gh_pr_watch.py   .codex/skills/babysit-pr/scripts/github_app_installation_broker.py   .codex/skills/babysit-gh-workflow-run/scripts/gh_workflow_run_watch.py   .codex/skills/babysit-gh-workflow-run/scripts/gh_dispatch_and_watch.py   .codex/skills/sedna/subagent-session-tail/scripts/inspect_subagent_tail.py
python3 .codex/skills/babysit-pr/scripts/test_github_app_installation_broker.py
watcher_test_venv="${RUNNER_TEMP}/codex-watcher-tests-venv"
python3 -m venv "${watcher_test_venv}"
"${watcher_test_venv}/bin/python" -m pip install --disable-pip-version-check --quiet "pytest==9.0.3"
"${watcher_test_venv}/bin/python" -m pytest -q .codex/skills/babysit-pr/scripts/test_gh_pr_watch.py
python3 .codex/skills/babysit-gh-workflow-run/tests/test_gh_workflow_run_watch.py
python3 .codex/skills/babysit-gh-workflow-run/tests/test_gh_dispatch_and_watch.py
python3 .codex/skills/sedna/subagent-session-tail/tests/test_inspect_subagent_tail.py
python3 .codex/skills/sedna/subagent-session-tail/scripts/inspect_subagent_tail.py --help >/dev/null
