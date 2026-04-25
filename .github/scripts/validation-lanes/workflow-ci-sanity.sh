#!/usr/bin/env bash
set -euo pipefail

python3 -m py_compile   .github/scripts/aggregate_validation_summary.py   .github/scripts/check_markdown_links.py   .github/scripts/report_actions_cache_occupancy.py   .github/scripts/resolve_rust_ci_mode.py   .github/scripts/resolve_validation_plan.py   .github/scripts/run_validation_lane.py   .github/scripts/sync_upstream_mirror.py   .github/scripts/test_ci_planners.py
python3 -m unittest discover -s .github/scripts -p 'test_ci_planners.py'
ruby -e 'require "yaml"; %w[.github/workflows/_validation-lane-workflow.yml .github/workflows/_validation-lane-node.yml .github/workflows/_validation-lane-rust-minimal.yml .github/workflows/_validation-lane-rust-integration.yml .github/workflows/_validation-lane-release.yml .github/workflows/docs-sanity.yml .github/workflows/rust-ci-full.yml .github/workflows/rust-ci.yml .github/workflows/sedna-heavy-tests.yml .github/workflows/sedna-sync-upstream.yml .github/workflows/validation-lab.yml].each { |path| YAML.load_file(path) }; puts "yaml-ok"'
