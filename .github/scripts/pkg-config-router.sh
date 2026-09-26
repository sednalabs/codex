#!/usr/bin/env bash
set -euo pipefail

: "${CODEX_PINNED_PKG_CONFIG:?pinned pkg-config path is required}"
: "${CODEX_VOICE_PKG_CONFIG_LIBDIR:?voice pkg-config directory is required}"
: "${CODEX_DISTRO_PKG_CONFIG:?distro pkg-config path is required}"

voice_modules=(
  glib-2.0 gobject-2.0 gio-2.0 gmodule-no-export-2.0
  gstreamer-1.0 gstreamer-base-1.0 gstreamer-app-1.0
  gstreamer-audio-1.0 gstreamer-tag-1.0
)
is_voice_query() {
  local query="$1"
  local candidate
  for candidate in "${voice_modules[@]}"; do
    # pkg-config-rust passes version requirements as one module argument,
    # for example `glib-2.0 >= 2.56`.
    [[ "$query" == "$candidate" || "$query" == "$candidate "* ]] && return 0
  done
  return 1
}

modules=()
has_define_prefix=0
for arg in "$@"; do
  case "$arg" in
    --define-prefix) has_define_prefix=1 ;;
    --cflags|--libs|--static|--exists|--modversion|--print-errors|--silence-errors|--debug|--path|--print-provides|--print-requires) ;;
    --atleast-version=*|--exact-version=*|--max-version=*|--variable=*|--requires*) ;;
    -*)
      echo "unsupported pkg-config option: $arg" >&2
      exit 2
      ;;
    *) modules+=("$arg") ;;
  esac
done

[[ ${#modules[@]} -gt 0 ]] || {
  echo "pkg-config query must name at least one module" >&2
  exit 2
}

voice_count=0
for module in "${modules[@]}"; do
  if is_voice_query "$module"; then
    ((voice_count += 1))
  fi
done

if (( voice_count > 0 && voice_count != ${#modules[@]} )); then
  echo "mixed pinned-voice and system pkg-config query is not supported" >&2
  exit 2
fi

if (( voice_count == ${#modules[@]} )); then
  export PKG_CONFIG_LIBDIR="$CODEX_VOICE_PKG_CONFIG_LIBDIR"
  export PKG_CONFIG_PATH=
  unset PKG_CONFIG_SYSROOT_DIR
  if (( has_define_prefix == 0 )); then
    set -- --define-prefix "$@"
  fi
  exec "$CODEX_PINNED_PKG_CONFIG" "$@"
fi

export PKG_CONFIG_PATH=
unset PKG_CONFIG_LIBDIR PKG_CONFIG_SYSROOT_DIR
exec "$CODEX_DISTRO_PKG_CONFIG" "$@"
