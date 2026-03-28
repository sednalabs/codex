#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
lint_path="$repo_root/tools/argument-comment-lint"
manifest_path="$repo_root/codex-rs/Cargo.toml"
toolchain_channel="nightly-2025-09-18"
strict_lint="uncommented-anonymous-literal-argument"
noise_lint="unknown_lints"

has_manifest_path=false
has_package_selection=false
has_no_deps=false
has_library_selection=false
has_cargo_target_selection=false
has_fix=false
after_separator=false
expect_value=""
lint_args=()
cargo_args=()

ensure_local_prerequisites() {
    if ! command -v cargo-dylint >/dev/null 2>&1 || ! command -v dylint-link >/dev/null 2>&1; then
        cat >&2 <<EOF
argument-comment-lint source wrapper requires cargo-dylint and dylint-link.
Install them with:
  cargo install --locked cargo-dylint dylint-link
EOF
        exit 1
    fi

    if ! rustup toolchain list | grep -q "^${toolchain_channel}"; then
        cat >&2 <<EOF
argument-comment-lint source wrapper requires the ${toolchain_channel} toolchain with rustc-dev support.
Install it with:
  rustup toolchain install ${toolchain_channel} \\
    --component llvm-tools-preview \\
    --component rustc-dev \\
    --component rust-src
EOF
        exit 1
    fi
}

set_default_env() {
    if [[ "${DYLINT_RUSTFLAGS:-}" != *"$strict_lint"* ]]; then
        export DYLINT_RUSTFLAGS="${DYLINT_RUSTFLAGS:+${DYLINT_RUSTFLAGS} }-D $strict_lint"
    fi
    if [[ "${DYLINT_RUSTFLAGS:-}" != *"$noise_lint"* ]]; then
        export DYLINT_RUSTFLAGS="${DYLINT_RUSTFLAGS:+${DYLINT_RUSTFLAGS} }-A $noise_lint"
    fi

    if [[ -z "${CARGO_INCREMENTAL:-}" ]]; then
        export CARGO_INCREMENTAL=0
    fi
}

for arg in "$@"; do
    if [[ "$after_separator" == true ]]; then
        cargo_args+=("$arg")
        case "$arg" in
            --all-targets|--lib|--bins|--tests|--examples|--benches|--doc)
                has_cargo_target_selection=true
                ;;
            --bin|--test|--example|--bench)
                has_cargo_target_selection=true
                ;;
            --bin=*|--test=*|--example=*|--bench=*)
                has_cargo_target_selection=true
                ;;
        esac
        continue
    fi

    case "$arg" in
        --)
            after_separator=true
            continue
            ;;
    esac

    lint_args+=("$arg")

    if [[ -n "$expect_value" ]]; then
        case "$expect_value" in
            manifest_path)
                has_manifest_path=true
                ;;
            package_selection)
                has_package_selection=true
                ;;
            library_selection)
                has_library_selection=true
                ;;
        esac
        expect_value=""
        continue
    fi

    case "$arg" in
        --manifest-path)
            expect_value="manifest_path"
            ;;
        --manifest-path=*)
            has_manifest_path=true
            ;;
        -p|--package)
            expect_value="package_selection"
            ;;
        --package=*)
            has_package_selection=true
            ;;
        --fix)
            has_fix=true
            ;;
        --workspace)
            has_package_selection=true
            ;;
        --no-deps)
            has_no_deps=true
            ;;
        --lib|--lib-path)
            expect_value="library_selection"
            ;;
        --lib=*|--lib-path=*)
            has_library_selection=true
            ;;
    esac
done

final_args=()
if [[ "$has_manifest_path" == false ]]; then
    final_args+=(--manifest-path "$manifest_path")
fi
if [[ "$has_package_selection" == false ]]; then
    final_args+=(--workspace)
fi
if [[ "$has_no_deps" == false ]]; then
    final_args+=(--no-deps)
fi
if [[ "$has_fix" == false && "$has_cargo_target_selection" == false ]]; then
    cargo_args+=(--all-targets)
fi
if [[ ${#lint_args[@]} -gt 0 ]]; then
    final_args+=("${lint_args[@]}")
fi
if [[ ${#cargo_args[@]} -gt 0 ]]; then
    final_args+=(-- "${cargo_args[@]}")
fi

ensure_local_prerequisites
set_default_env

cmd=(cargo dylint --path "$lint_path")
if [[ "$has_library_selection" == false ]]; then
    cmd+=(--all)
fi
cmd+=("${final_args[@]}")

exec "${cmd[@]}"
