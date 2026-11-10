#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: $0 <recipe> [args...]" >&2
  exit 64
fi

recipe="$1"
shift
exec just "${recipe}" "$@"
