#!/usr/bin/env bash
set -euo pipefail

timeout_seconds="${LINUX_BUILD_DEPS_TIMEOUT_SECONDS:-300}"
retry_count="${LINUX_BUILD_DEPS_RETRIES:-3}"
retry_sleep_seconds="${LINUX_BUILD_DEPS_RETRY_SLEEP_SECONDS:-15}"

if [[ "$#" -gt 0 ]]; then
  packages=("$@")
else
  # shellcheck disable=SC2206
  packages=(${LINUX_BUILD_DEPS_PACKAGES:-pkg-config libcap-dev})
fi

if [[ "${#packages[@]}" -eq 0 ]]; then
  echo "No Linux build dependencies requested."
  exit 0
fi

run_apt_with_retries() {
  local attempt=1
  local status=0
  while true; do
    if sudo env DEBIAN_FRONTEND=noninteractive \
      timeout --kill-after=30s "${timeout_seconds}s" \
      apt-get -o Acquire::Retries=3 -o DPkg::Lock::Timeout=60 "$@"; then
      return 0
    fi

    status="$?"
    if [[ "${attempt}" -ge "${retry_count}" ]]; then
      echo "::error title=Linux dependency setup failed::apt-get $* failed after ${attempt} attempt(s) with status ${status}."
      return "${status}"
    fi

    echo "::warning title=Linux dependency setup retry::apt-get $* failed with status ${status}; retrying in ${retry_sleep_seconds}s (${attempt}/${retry_count})."
    sleep "${retry_sleep_seconds}"
    attempt="$((attempt + 1))"
  done
}

run_apt_with_retries update -y
run_apt_with_retries install -y --no-install-recommends "${packages[@]}"
