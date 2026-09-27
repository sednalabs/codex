#!/usr/bin/env bash

# Reduce a set of GitHub job results without silently dropping later inputs.
# The order is intentional: a failure is stronger than cancellation, which is
# stronger than a successful member of the same validation family.
combined_validation_result() {
  local result
  for result in "$@"; do
    if [[ "${result}" == "failure" ]]; then
      printf '%s\n' failure
      return 0
    fi
  done
  for result in "$@"; do
    if [[ "${result}" == "cancelled" ]]; then
      printf '%s\n' cancelled
      return 0
    fi
  done
  for result in "$@"; do
    if [[ "${result}" == "success" ]]; then
      printf '%s\n' success
      return 0
    fi
  done
  printf '%s\n' skipped
}
