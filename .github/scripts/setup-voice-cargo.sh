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

./.github/scripts/run-bazel-ci.sh \
  --remote-download-toplevel \
  --print-failed-action-summary \
  -- build -c opt --output_groups=default,sdk -- \
  //third_party/voice:native_sdk \
  //third_party/voice:native_link

bazel_bin="$(bazel info bazel-bin)"
sdk="$bazel_bin/third_party/voice/native_runtime_${os}_${cpu}_sdk"
native_lib="$bazel_bin/third_party/voice/native_link_${os}_${cpu}/lib"
pkg_config="$(bazel cquery --noimplicit_deps --output=files //third_party/voice:pkg_config | head -n 1)"

for path in "$sdk/lib/pkgconfig" "$native_lib" "$pkg_config"; do
  [[ -e "$path" ]] || { echo "missing voice Cargo input: $path" >&2; exit 1; }
done

{
  echo "PKG_CONFIG=$pkg_config"
  echo "PKG_CONFIG_LIBDIR=$sdk/lib/pkgconfig"
  echo "PKG_CONFIG_PATH="
  for key in \
    GLIB_2_0 GOBJECT_2_0 GIO_2_0 \
    GSTREAMER_1_0 GSTREAMER_BASE_1_0 GSTREAMER_APP_1_0 GSTREAMER_AUDIO_1_0; do
    echo "SYSTEM_DEPS_${key}_SEARCH_NATIVE=$native_lib"
  done
  for key in GSTREAMER_1_0 GSTREAMER_BASE_1_0 GSTREAMER_APP_1_0 GSTREAMER_AUDIO_1_0; do
    echo "SYSTEM_DEPS_${key}_LDFLAGS="
  done
} >> "$GITHUB_ENV"
