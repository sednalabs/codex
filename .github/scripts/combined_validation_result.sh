#!/usr/bin/env bash

# Reduce a set of GitHub job results without silently dropping later inputs.
# The order is intentional: a failure is stronger than cancellation, which is
# stronger than a successful member of the same validation family.
combined_validation_result() {
  local result max_weight=0 weight
  for result in "$@"; do
    case "${result}" in
      failure) weight=3 ;;
      cancelled) weight=2 ;;
      success) weight=1 ;;
      *) weight=0 ;;
    esac
    if (( weight > max_weight )); then
      max_weight="${weight}"
    fi
  done
  case "${max_weight}" in
    3) printf '%s\n' failure ;;
    2) printf '%s\n' cancelled ;;
    1) printf '%s\n' success ;;
    *) printf '%s\n' skipped ;;
  esac
}
