#!/usr/bin/env bash
set -euo pipefail

python3 -m py_compile \
  .github/scripts/aggregate_validation_summary.py \
  .github/scripts/check_markdown_links.py \
  .github/scripts/check_workflow_policy.py \
  .github/scripts/report_actions_cache_occupancy.py \
  .github/scripts/resolve_rust_ci_mode.py \
  .github/scripts/resolve_sedna_release_version.py \
  .github/scripts/resolve_validation_plan.py \
  .github/scripts/run_validation_lane.py \
  .github/scripts/skip_duplicate_workflow_run.py \
  .github/scripts/summarize_rust_ci_full.py \
  .github/scripts/sync_upstream_mirror.py \
  .github/scripts/test_ci_planners.py \
  scripts/downstream-divergence-audit.py
python3 -m unittest discover -s .github/scripts -p 'test_ci_planners.py'
python3 .github/scripts/check_workflow_policy.py
ruby -e 'require "yaml"; paths = Dir.glob([".github/workflows/*.{yml,yaml}", "codex-rs/.github/workflows/*.{yml,yaml}"]).sort; paths.each { |path| YAML.load_file(path) }; puts "yaml-ok #{paths.length}"'
