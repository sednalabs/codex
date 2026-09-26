#!/usr/bin/env bash
set -euo pipefail

target="${1:?target triple is required}"
case "$target" in
  aarch64-apple-darwin) os=macos; cpu=aarch64 ;;
  x86_64-apple-darwin) os=macos; cpu=x86_64 ;;
  aarch64-unknown-linux-gnu) os=linux; cpu=aarch64 ;;
  x86_64-unknown-linux-gnu) os=linux; cpu=x86_64 ;;
  *) echo "voice Cargo setup does not support target $target" >&2; exit 2 ;;
esac

bazel_config=ci-linux
if [[ "${RUNNER_OS:-}" == macOS ]]; then
  bazel_config=ci-macos
fi
bazel_config_args=()
if [[ -n "${BUILDBUDDY_API_KEY:-}" ]]; then
  bazel_config_args+=(--config="$bazel_config")
fi

bazel_cquery() {
  if (( ${#bazel_config_args[@]} > 0 )); then
    bazel cquery "${bazel_config_args[@]}" "$@"
  else
    bazel cquery "$@"
  fi
}

./.github/scripts/run-bazel-ci.sh \
  --remote-download-all \
  --print-failed-action-summary \
  -- build -c opt --output_groups=default,sdk -- \
  //third_party/voice:native_sdk \
  //third_party/voice:native_link \
  //third_party/voice:pkg_config

bazel_cquery_args=(-c opt --noimplicit_deps --output=files)
sdk="$(bazel_cquery "${bazel_cquery_args[@]}" --output_groups=default,sdk //third_party/voice:native_sdk | grep '/native_runtime_' | head -n 1)"
native_link="$(bazel_cquery "${bazel_cquery_args[@]}" //third_party/voice:native_link | grep '/native_link_' | head -n 1)"
pkg_config="$(bazel_cquery "${bazel_cquery_args[@]}" //third_party/voice:pkg_config | head -n 1)"

[[ -n "$sdk" && -n "$native_link" && -n "$pkg_config" ]] || {
  echo "Bazel did not return all voice Cargo outputs" >&2
  exit 1
}

for path_name in sdk native_link pkg_config; do
  path_value="${!path_name}"
  if [[ "$path_value" != /* ]]; then
    printf -v "$path_name" '%s/%s' "$PWD" "$path_value"
  fi
done
native_lib="$(dirname "$native_link")"

for path in "$sdk/lib/pkgconfig" "$native_lib" "$pkg_config"; do
  [[ -e "$path" ]] || { echo "missing voice Cargo input: $path" >&2; exit 1; }
done

{
  echo "PKG_CONFIG=$pkg_config"
  echo "PKG_CONFIG_LIBDIR=$sdk/lib/pkgconfig"
  echo "PKG_CONFIG_PATH="
  case "$target" in
    aarch64-unknown-linux-gnu)
      echo "OPENSSL_DIR=/usr"
      echo "OPENSSL_LIB_DIR=/usr/lib/aarch64-linux-gnu"
      ;;
    x86_64-unknown-linux-gnu)
      echo "OPENSSL_DIR=/usr"
      echo "OPENSSL_LIB_DIR=/usr/lib/x86_64-linux-gnu"
      ;;
  esac
  for key in \
    GLIB_2_0 GOBJECT_2_0 GIO_2_0 \
    GSTREAMER_1_0 GSTREAMER_BASE_1_0 GSTREAMER_APP_1_0 GSTREAMER_AUDIO_1_0; do
    echo "SYSTEM_DEPS_${key}_SEARCH_NATIVE=$native_lib"
  done
  for key in GSTREAMER_1_0 GSTREAMER_BASE_1_0 GSTREAMER_APP_1_0 GSTREAMER_AUDIO_1_0; do
    echo "SYSTEM_DEPS_${key}_LDFLAGS="
  done
} >> "$GITHUB_ENV"
