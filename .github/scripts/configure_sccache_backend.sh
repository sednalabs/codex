#!/usr/bin/env bash
set -euo pipefail

policy="${1:-restore-only}"

case "${policy}" in
  off|restore-only|write-fallback) ;;
  *)
    echo "::warning title=Unknown cache policy::${policy}; using restore-only"
    policy="restore-only"
    ;;
esac

echo "policy=${policy}" >> "${GITHUB_OUTPUT}"

# The sccache GHA backend writes cache entries when enabled; keep the validation
# policy enforceable by using actions/cache restore and an explicit save step.
{
  echo "SCCACHE_GHA_ENABLED=false"
  echo "SCCACHE_DIR=${GITHUB_WORKSPACE}/.sccache"
} >> "${GITHUB_ENV}"

case "${policy}" in
  off)
    echo "backend=off" >> "${GITHUB_OUTPUT}"
    echo "sccache disabled by cache policy"
    ;;
  restore-only)
    echo "backend=fallback" >> "${GITHUB_OUTPUT}"
    echo "Using sccache local disk with restore-only actions/cache"
    ;;
  write-fallback)
    echo "backend=fallback" >> "${GITHUB_OUTPUT}"
    echo "Using sccache local disk with explicit actions/cache save"
    ;;
esac
