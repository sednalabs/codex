"""Prepare review-pending TUI snapshots from one exact hosted source input."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import stat
import subprocess
import sys


PROFILE = "tui-snapshots"
MODE = "prepare-only"
COMMAND = ["just", "test", "-p", "codex-tui", "--locked"]
TARGET_DIRECTORY_NAME = "w16072-tui-snapshot-target"
ARTIFACT_DIRECTORY_NAME = "w16072-tui-snapshot-artifact"
ARTIFACT_STAGING_DIRECTORY_NAME = "w16072-tui-snapshot-artifact.staging"
TUI_SOURCE_ROOT = "codex-rs/tui/src"
LOCK_PATHS = ("codex-rs/Cargo.lock", "MODULE.bazel.lock")
MAX_OUTPUTS = 69
MAX_OUTPUT_BYTES = 32 * 1024 * 1024
MAX_FILE_BYTES = 1024 * 1024
MAX_ARTIFACT_BYTES = 40 * 1024 * 1024
MAX_METADATA_BYTES = 128 * 1024
FIXED_TARGET_SUFFIX = TARGET_DIRECTORY_NAME
FIXED_ARTIFACT_SUFFIX = ARTIFACT_DIRECTORY_NAME
SNAPSHOT_PATHS = (
    "codex-rs/tui/src/app/snapshots/codex_tui__app__composer_hints__tests__composer_usage_notice.snap",
    "codex-rs/tui/src/app/snapshots/codex_tui__app__owned_transcript__follow_tests__running_copy_feedback_spacing.snap",
    "codex-rs/tui/src/app/snapshots/codex_tui__app__owned_transcript__follow_tests__running_follow_control.snap",
    "codex-rs/tui/src/app/snapshots/codex_tui__app__owned_transcript__follow_tests__running_follow_control_short_terminal.snap",
    "codex-rs/tui/src/app/snapshots/codex_tui__app__owned_transcript__follow_tests__running_without_composer_hint.snap",
    "codex-rs/tui/src/app/snapshots/codex_tui__app__turn_tips__tests__turn_tip_placements.snap",
    "codex-rs/tui/src/app/snapshots/codex_tui__app__turn_tips__tests__working_tip_mouse_down.snap",
    "codex-rs/tui/src/bottom_pane/request_user_input/snapshots/codex_tui__bottom_pane__request_user_input__tests__request_user_input_auto_resolution_countdown.snap",
    "codex-rs/tui/src/bottom_pane/request_user_input/snapshots/codex_tui__bottom_pane__request_user_input__tests__request_user_input_footer_wrap.snap",
    "codex-rs/tui/src/bottom_pane/request_user_input/snapshots/codex_tui__bottom_pane__request_user_input__tests__request_user_input_freeform.snap",
    "codex-rs/tui/src/bottom_pane/request_user_input/snapshots/codex_tui__bottom_pane__request_user_input__tests__request_user_input_freeform_remapped_submit.snap",
    "codex-rs/tui/src/bottom_pane/request_user_input/snapshots/codex_tui__bottom_pane__request_user_input__tests__request_user_input_hidden_options_footer.snap",
    "codex-rs/tui/src/bottom_pane/request_user_input/snapshots/codex_tui__bottom_pane__request_user_input__tests__request_user_input_long_option_text.snap",
    "codex-rs/tui/src/bottom_pane/request_user_input/snapshots/codex_tui__bottom_pane__request_user_input__tests__request_user_input_multi_question_first.snap",
    "codex-rs/tui/src/bottom_pane/request_user_input/snapshots/codex_tui__bottom_pane__request_user_input__tests__request_user_input_multi_question_last.snap",
    "codex-rs/tui/src/bottom_pane/request_user_input/snapshots/codex_tui__bottom_pane__request_user_input__tests__request_user_input_options.snap",
    "codex-rs/tui/src/bottom_pane/request_user_input/snapshots/codex_tui__bottom_pane__request_user_input__tests__request_user_input_scrolling_options.snap",
    "codex-rs/tui/src/bottom_pane/request_user_input/snapshots/codex_tui__bottom_pane__request_user_input__tests__request_user_input_tight_height.snap",
    "codex-rs/tui/src/bottom_pane/request_user_input/snapshots/codex_tui__bottom_pane__request_user_input__tests__request_user_input_wrapped_options.snap",
    "codex-rs/tui/src/bottom_pane/snapshots/codex_tui__bottom_pane__pending_input_preview__tests__render_multiline_pending_steer_uses_single_prefix_and_truncates.snap",
    "codex-rs/tui/src/bottom_pane/snapshots/codex_tui__bottom_pane__pending_input_preview__tests__render_one_pending_steer.snap",
    "codex-rs/tui/src/bottom_pane/snapshots/codex_tui__bottom_pane__pending_input_preview__tests__render_pending_steers_above_queued_messages.snap",
    "codex-rs/tui/src/bottom_pane/snapshots/codex_tui__bottom_pane__tests__slash_command_popup_dismissed.snap",
    "codex-rs/tui/src/bottom_pane/snapshots/codex_tui__bottom_pane__tests__status_and_queued_messages_snapshot.snap",
    "codex-rs/tui/src/bottom_pane/snapshots/codex_tui__bottom_pane__tests__status_only_snapshot.snap",
    "codex-rs/tui/src/bottom_pane/snapshots/codex_tui__bottom_pane__tests__status_timer_survives_hidden_row.snap",
    "codex-rs/tui/src/bottom_pane/snapshots/codex_tui__bottom_pane__tests__status_with_details_and_queued_messages_snapshot.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__app_server_guardian_review_denied_renders_denied_request.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__app_server_guardian_review_timed_out_renders_timed_out_request.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__chatwidget_exec_and_status_layout_vt100_snapshot.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__chatwidget_tall.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__compact_queues_user_messages_snapshot.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__compaction_running.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__completed_turn_clears_visible_running_hook.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__esc_interrupt_goal_paused_footer.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__esc_interrupt_goal_paused_footer@windows.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__guardian_approved_request_permissions_clears_status_without_history.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__guardian_denied_exec_renders_warning_and_denied_request.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__guardian_goal_continuation_drops_stale_reviews.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__guardian_parallel_reviews_render_aggregate_status.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__guardian_timed_out_exec_renders_warning_and_timed_out_request.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__guardian_write_stdin_review_status.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__hidden_shell_paste_queued_preview.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__hook_runs_while_exec_active_snapshot.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__image_generation_begin_restores_working_status.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__interrupted_turn_clears_visible_running_hook.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__long_running_hook_below_background_activity_row.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__manual_compaction_pending.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__mixed_running_hooks_share_background_activity_row.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__overlapping_hook_live_cell_snapshot.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__preamble_keeps_working_status.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__reasoning_activity_row_80.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__reasoning_delta_restores_recreated_status_indicator.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__replayed_in_progress_turn.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__review_queues_user_messages_snapshot.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__running_hooks_share_background_activity_row.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__slash_side_requests_forked_side_question_while_task_running.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__status_widget_active.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__unified_exec_begin_restores_working_status.snap",
    "codex-rs/tui/src/chatwidget/tests/snapshots/codex_tui__chatwidget__tests__luna_reserve_usage_tests__luna_reserve_usage_running.snap",
    "codex-rs/tui/src/chatwidget/tests/snapshots/codex_tui__chatwidget__tests__questions_tests__questions_with_status_and_queue.snap",
    "codex-rs/tui/src/chatwidget/tests/snapshots/codex_tui__chatwidget__tests__questions_tests__single_question_working_spacing.snap",
    "codex-rs/tui/src/snapshots/codex_tui__screen_reader__tests__persisted_default_loads_and_renders_without_animation.snap",
    "codex-rs/tui/src/snapshots/codex_tui__status_indicator_widget__tests__hook_status_reflows_with_background_activity.snap",
    "codex-rs/tui/src/snapshots/codex_tui__status_indicator_widget__tests__hook_status_reflows_without_background_activity.snap",
    "codex-rs/tui/src/snapshots/codex_tui__status_indicator_widget__tests__renders_with_queued_messages.snap",
    "codex-rs/tui/src/snapshots/codex_tui__status_indicator_widget__tests__renders_with_queued_messages@macos.snap",
    "codex-rs/tui/src/snapshots/codex_tui__status_indicator_widget__tests__renders_with_working_header.snap",
    "codex-rs/tui/src/status_indicator_widget/snapshots/codex_tui__status_indicator_widget__effects_tests__shimmer_and_progress_are_independent_and_obey_master_switch.snap",
)


def validate_profile(mode: str, profile: str | None) -> str:
    if mode != MODE:
        raise ValueError("tui-snapshots preparation is restricted to prepare-only mode")
    if profile != PROFILE:
        raise ValueError("unsupported or empty preparation profile")
    return PROFILE


def _git(root: Path, *args: str) -> bytes:
    return subprocess.check_output(
        ["git", "-C", str(root), *args], stderr=subprocess.DEVNULL
    )


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _relative_path(root: Path, relative: str) -> Path:
    path = root
    components = Path(relative).parts
    if not components or Path(relative).is_absolute() or ".." in components:
        raise ValueError("snapshot path is not a canonical relative path")
    for index, component in enumerate(components):
        path = path / component
        try:
            mode = path.lstat().st_mode
        except FileNotFoundError as error:
            raise ValueError(f"expected tracked path is missing: {relative}") from error
        if stat.S_ISLNK(mode):
            raise ValueError(f"snapshot path contains a symlink: {relative}")
        if index < len(components) - 1 and not stat.S_ISDIR(mode):
            raise ValueError(f"snapshot path parent is not a directory: {relative}")
    return path


def _tracked_file(root: Path, relative: str) -> tuple[bytes, int]:
    path = _relative_path(root, relative)
    path_stat = path.lstat()
    if not stat.S_ISREG(path_stat.st_mode) or path_stat.st_nlink != 1:
        raise ValueError(f"expected a regular single-link tracked file: {relative}")
    _git(root, "ls-files", "--error-unmatch", "--", relative)
    return path.read_bytes(), stat.S_IMODE(path_stat.st_mode)


def _check_clean_checkout(root: Path, label: str) -> None:
    if _git(root, "status", "--porcelain=v1", "-z", "--untracked-files=all"):
        raise ValueError(f"{label} checkout is not clean")


def _tracked_path_set(root: Path) -> set[str]:
    listed = _git(root, "ls-files", "-z").decode("utf-8", errors="strict").split("\0")
    return {path for path in listed if path}


def _snapshot_inventory(root: Path) -> list[str]:
    source_root = root / TUI_SOURCE_ROOT
    if source_root.is_symlink() or not source_root.is_dir():
        raise ValueError("TUI source root is unavailable or a symlink")
    found: list[str] = []
    for directory, subdirectories, filenames in os.walk(source_root, followlinks=False):
        directory_path = Path(directory)
        for subdirectory in subdirectories:
            child = directory_path / subdirectory
            if child.is_symlink():
                raise ValueError("symlink directory found under the bounded TUI source root")
        for filename in filenames:
            if not filename.endswith(".snap.new"):
                continue
            path = directory_path / filename
            relative = path.relative_to(root).as_posix()
            mode = path.lstat().st_mode
            if not stat.S_ISREG(mode) or stat.S_ISLNK(mode):
                raise ValueError(f"pending snapshot is not a regular file: {relative}")
            found.append(relative)
    return sorted(found)


def validate_no_preexisting_pending(root: Path) -> None:
    if _snapshot_inventory(root):
        raise ValueError("pre-existing .snap.new files were found under the TUI source root")


def _capture_baseline(root: Path) -> dict[str, dict[str, object]]:
    if len(SNAPSHOT_PATHS) != MAX_OUTPUTS or len(set(SNAPSHOT_PATHS)) != MAX_OUTPUTS:
        raise ValueError("fixed snapshot allowlist count or uniqueness is invalid")
    tracked = _tracked_path_set(root)
    if not set(SNAPSHOT_PATHS).issubset(tracked):
        raise ValueError("snapshot allowlist contains an untracked baseline")
    state: dict[str, dict[str, object]] = {}
    for relative in SNAPSHOT_PATHS:
        contents, mode = _tracked_file(root, relative)
        if not contents:
            raise ValueError(f"tracked baseline snapshot is empty: {relative}")
        state[relative] = {"sha256": _sha256(contents), "mode": mode}
    return state


def _capture_locks(root: Path) -> dict[str, dict[str, object]]:
    return {
        relative: {
            "sha256": _sha256(contents),
            "mode": mode,
        }
        for relative in LOCK_PATHS
        for contents, mode in [_tracked_file(root, relative)]
    }


def _parse_status(root: Path) -> list[tuple[str, str]]:
    status = _git(root, "status", "--porcelain=v1", "-z", "--untracked-files=all")
    result = []
    for entry in status.split(b"\0"):
        if not entry:
            continue
        if len(entry) < 4 or entry[2:3] != b" ":
            raise ValueError("Git returned an unrecognized status record")
        state = entry[:2].decode("ascii")
        path = entry[3:].decode("utf-8", errors="strict")
        if "R" in state or "C" in state:
            raise ValueError("snapshot preparation produced a rename or copy")
        result.append((state, path))
    return result


def _validate_public_output(contents: bytes, relative: str, environment: dict[str, str]) -> None:
    text = contents.decode("utf-8", errors="strict")
    if "\x00" in text:
        raise ValueError(f"pending snapshot contains a NUL byte: {relative}")
    forbidden = [
        environment.get("GITHUB_WORKSPACE", ""),
        environment.get("RUNNER_TEMP", ""),
        "/home/runner/",
        "/Users/",
        "C:\\Users\\",
        "-----BEGIN PRIVATE KEY-----",
        "-----BEGIN OPENSSH PRIVATE KEY-----",
    ]
    if any(value and value in text for value in forbidden):
        raise ValueError(f"pending snapshot contains a runner path or private-key marker: {relative}")
    if re.search(r"\b(?:gh[pousr]_[A-Za-z0-9_]{20,}|sk-[A-Za-z0-9]{20,})\b", text):
        raise ValueError(f"pending snapshot contains a token-like value: {relative}")


def validate_outputs(
    product_root: Path,
    baseline: dict[str, dict[str, object]],
    locks_before: dict[str, dict[str, object]],
    environment: dict[str, str],
) -> dict[str, bytes]:
    tracked = _tracked_path_set(product_root)
    if not set(SNAPSHOT_PATHS).issubset(tracked):
        raise ValueError("snapshot allowlist baseline tracking changed during the run")
    baseline_after = _capture_baseline(product_root)
    if baseline_after != baseline:
        raise ValueError("accepted .snap baseline content or mode changed")
    locks_after = _capture_locks(product_root)
    if locks_after != locks_before:
        raise ValueError("Cargo.lock or MODULE.bazel.lock changed during snapshot preparation")

    generated = _snapshot_inventory(product_root)
    if len(generated) > MAX_OUTPUTS:
        raise ValueError("pending snapshot output count exceeds its fixed limit")
    expected_paths = {f"{path}.new" for path in SNAPSHOT_PATHS}
    if not set(generated).issubset(expected_paths):
        raise ValueError("pending snapshot output is outside the exact allowlist")

    outputs: dict[str, bytes] = {}
    total_bytes = 0
    for relative in generated:
        path = _relative_path(product_root, relative)
        path_stat = path.lstat()
        if not stat.S_ISREG(path_stat.st_mode) or stat.S_ISLNK(path_stat.st_mode):
            raise ValueError(f"pending snapshot is not a regular nonsymlink file: {relative}")
        if path_stat.st_nlink != 1:
            raise ValueError(f"pending snapshot has unexpected hard links: {relative}")
        contents = path.read_bytes()
        if not contents or len(contents) > MAX_FILE_BYTES:
            raise ValueError(f"pending snapshot is empty or exceeds the per-file limit: {relative}")
        _validate_public_output(contents, relative, environment)
        total_bytes += len(contents)
        if total_bytes > MAX_OUTPUT_BYTES:
            raise ValueError("pending snapshot output exceeds the aggregate byte limit")
        outputs[relative] = contents

    expected_untracked = set(generated)
    for state, relative in _parse_status(product_root):
        if state == "??" and relative in expected_untracked:
            continue
        raise ValueError(f"product checkout contains an unexpected change: {relative}")
    nonignored_untracked = _git(
        product_root, "ls-files", "--others", "--exclude-standard", "-z"
    ).decode("utf-8", errors="strict").split("\0")
    if not {path for path in nonignored_untracked if path}.issubset(expected_untracked):
        raise ValueError("product checkout has an unexpected nonignored untracked file")
    return outputs


def _fixed_runner_paths(
    environment: dict[str, str], workspace: Path
) -> tuple[Path, Path, Path]:
    runner_temp = Path(environment.get("RUNNER_TEMP", ""))
    if not runner_temp.is_absolute() or runner_temp.is_symlink() or not runner_temp.is_dir():
        raise ValueError("runner temporary directory is unavailable")
    product_root = workspace / "product"
    workflow_root = workspace / ".workflow-src"
    runner_real = runner_temp.resolve()
    for checkout in (product_root, workflow_root):
        checkout_real = checkout.resolve()
        if (
            runner_real == checkout_real
            or checkout_real in runner_real.parents
            or runner_real in checkout_real.parents
        ):
            raise ValueError("runner temporary directory must be outside both checkouts")
    target = runner_temp / FIXED_TARGET_SUFFIX
    artifact = runner_temp / FIXED_ARTIFACT_SUFFIX
    staging = runner_temp / ARTIFACT_STAGING_DIRECTORY_NAME
    if environment.get("CARGO_TARGET_DIR") != str(target):
        raise ValueError("CARGO_TARGET_DIR does not match the fixed external target path")
    for directory in (target, artifact, staging):
        if directory.is_symlink():
            raise ValueError("fixed runner output path must not be a symlink")
        if directory.exists():
            raise ValueError("fixed runner output directory already exists")
    return target, artifact, staging


def validate_input_identity(
    environment: dict[str, str], workspace: Path
) -> dict[str, str]:
    validate_profile(environment.get("MODE", ""), environment.get("PREPARATION_PROFILE"))
    if environment.get("GITHUB_REPOSITORY") != "sednalabs/codex":
        raise ValueError("unexpected repository identity")
    if environment.get("GITHUB_WORKFLOW") != "sedna-branch-build":
        raise ValueError("unexpected workflow identity")
    if environment.get("GITHUB_SHA") != environment.get("EXPECTED_H"):
        raise ValueError("GitHub event SHA does not match workflow host H")
    expected_h = environment.get("EXPECTED_H", "")
    target_sha = environment.get("TARGET_SHA", "")
    base_sha = environment.get("BASE_SHA", "")
    for label, value in (("H", expected_h), ("T", target_sha), ("B", base_sha)):
        if re.fullmatch(r"[0-9a-f]{40}", value) is None:
            raise ValueError(f"{label} must be a full lowercase commit SHA")
    if _git(workspace / ".workflow-src", "rev-parse", "HEAD").decode().strip() != expected_h:
        raise ValueError("workflow host H does not match the dispatched identity")
    product_root = workspace / "product"
    if _git(product_root, "rev-parse", "HEAD").decode().strip() != target_sha:
        raise ValueError("product source T does not match the dispatched identity")
    if _git(product_root, "rev-parse", "FETCH_HEAD^{commit}").decode().strip() != base_sha:
        raise ValueError("comparison base B does not match the fetched identity")
    _git(product_root, "cat-file", "-e", f"{base_sha}^{{commit}}")

    run_id = environment.get("GITHUB_RUN_ID", "")
    attempt = environment.get("GITHUB_RUN_ATTEMPT", "")
    if re.fullmatch(r"[1-9][0-9]*", run_id) is None:
        raise ValueError("workflow run ID is malformed")
    if re.fullmatch(r"[1-9][0-9]*", attempt) is None:
        raise ValueError("workflow run attempt is malformed")
    if not environment.get("GITHUB_SERVER_URL"):
        raise ValueError("workflow identity is incomplete")
    if environment.get("INSTA_UPDATE") != "new":
        raise ValueError("INSTA_UPDATE must be the fixed pending-only value")
    if environment.get("RUNNER_OS") != "Linux" or environment.get("RUNNER_ARCH") != "X64":
        raise ValueError("snapshot preparation requires standard ubuntu-24.04 x86_64")
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        raise ValueError("snapshot preparation requires standard ubuntu-24.04 x86_64")

    return {
        "workflow_host_sha": expected_h,
        "workflow_host_tree": _git(
            workspace / ".workflow-src", "rev-parse", "HEAD^{tree}"
        ).decode().strip(),
        "product_sha": target_sha,
        "product_tree": _git(product_root, "rev-parse", "HEAD^{tree}").decode().strip(),
        "comparison_base_sha": base_sha,
        "comparison_base_tree": _git(
            product_root, "rev-parse", f"{base_sha}^{{tree}}"
        ).decode().strip(),
        "workflow_run_id": run_id,
        "workflow_run_attempt": attempt,
    }


def _json_bytes(value: object, name: str) -> bytes:
    encoded = (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")
    if len(encoded) > MAX_METADATA_BYTES:
        raise ValueError(f"artifact metadata exceeds its fixed size limit: {name}")
    return encoded


def test_result_metadata(return_code: int) -> dict[str, object]:
    return {
        "test_exit_code": return_code,
        "test_status": "passed" if return_code == 0 else "failed",
    }


def _write_artifact(
    artifact_root: Path,
    identity: dict[str, object],
    baseline: dict[str, dict[str, object]],
    locks: dict[str, dict[str, object]],
    outputs: dict[str, bytes],
) -> None:
    allowed_outputs = {f"{path}.new" for path in SNAPSHOT_PATHS}
    if len(outputs) > MAX_OUTPUTS or not set(outputs).issubset(allowed_outputs):
        raise ValueError("artifact output paths do not match the fixed snapshot allowlist")
    if any(not contents or len(contents) > MAX_FILE_BYTES for contents in outputs.values()):
        raise ValueError("artifact output is empty or exceeds the per-file limit")
    if sum(len(contents) for contents in outputs.values()) > MAX_OUTPUT_BYTES:
        raise ValueError("artifact outputs exceed the aggregate byte limit")
    hashes: dict[str, str] = {}
    for relative, contents in sorted(outputs.items()):
        hashes[relative] = _sha256(contents)

    identity["baseline_snapshot_sha256"] = {
        path: state["sha256"] for path, state in sorted(baseline.items())
    }
    identity["baseline_snapshot_modes"] = {
        path: state["mode"] for path, state in sorted(baseline.items())
    }
    identity["lock_files"] = locks
    identity["generated_snapshot_sha256"] = hashes
    candidate_data = _json_bytes(
        {"count": len(SNAPSHOT_PATHS), "paths": list(SNAPSHOT_PATHS)}, "candidate-paths.json"
    )
    generated_data = _json_bytes(
        {"count": len(outputs), "paths": sorted(outputs)}, "generated-paths.json"
    )
    metadata = _json_bytes(identity, "identity.json")
    total = len(metadata) + len(candidate_data) + len(generated_data)
    total += sum(len(data) for data in outputs.values())
    if total > MAX_ARTIFACT_BYTES:
        raise ValueError("snapshot artifact exceeds its fixed total byte limit")

    artifact_root.mkdir(mode=0o700)
    (artifact_root / "candidate-paths.json").write_bytes(candidate_data)
    (artifact_root / "generated-paths.json").write_bytes(generated_data)
    output_root = artifact_root / "snapshots"
    for relative, contents in sorted(outputs.items()):
        destination = output_root / Path(relative.removesuffix(".new") + ".new")
        destination.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        destination.write_bytes(contents)
    (artifact_root / "identity.json").write_bytes(metadata)


def prepare(environment: dict[str, str]) -> int:
    workspace = Path(environment["GITHUB_WORKSPACE"])
    product_root = workspace / "product"
    runner_temp = Path(environment["RUNNER_TEMP"])
    identity = validate_input_identity(environment, workspace)
    target_dir, artifact_dir, staging_dir = _fixed_runner_paths(environment, workspace)
    _check_clean_checkout(workspace / ".workflow-src", "workflow host")
    _check_clean_checkout(product_root, "product")

    baseline = _capture_baseline(product_root)
    locks_before = _capture_locks(product_root)
    validate_no_preexisting_pending(product_root)

    target_dir.mkdir(mode=0o700)
    output_env = dict(environment)
    output_env["INSTA_UPDATE"] = "new"
    output_env["CARGO_TARGET_DIR"] = str(target_dir)
    result = subprocess.run(
        COMMAND,
        cwd=product_root,
        env=output_env,
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )

    outputs = validate_outputs(product_root, baseline, locks_before, environment)
    _check_clean_checkout(workspace / ".workflow-src", "workflow host")
    identity.update(
        {
            "schema_version": "sedna-tui-snapshot-prep-v1",
            "mode": MODE,
            "preparation_profile": PROFILE,
            "repository": "sednalabs/codex",
            "workflow": "sedna-branch-build",
            "workflow_run": (
                f'{environment["GITHUB_SERVER_URL"]}/sednalabs/codex/actions/runs/'
                f'{identity["workflow_run_id"]}'
            ),
            "runner_label": "ubuntu-24.04",
            "architecture": "x86_64",
            "command": COMMAND,
            "working_directory": "product",
            "command_environment": {
                "INSTA_UPDATE": "new",
                "RUST_MIN_STACK": "8388608",
                "NEXTEST_PROFILE": "local",
                "CARGO_TARGET_DIR": "$RUNNER_TEMP/" + TARGET_DIRECTORY_NAME,
            },
            **test_result_metadata(result.returncode),
            "snapshot_candidate_count": len(SNAPSHOT_PATHS),
            "snapshot_output_count": len(outputs),
            "cargo_lock_sha256": locks_before[LOCK_PATHS[0]]["sha256"],
            "module_bazel_lock_sha256": locks_before[LOCK_PATHS[1]]["sha256"],
            "artifact_name": (
                "sedna-tui-snapshots-prep-"
                f'{identity["product_sha"]}-{identity["workflow_run_id"]}-'
                f'{identity["workflow_run_attempt"]}'
            ),
        }
    )
    _write_artifact(staging_dir, identity, baseline, locks_before, outputs)
    staging_dir.rename(artifact_dir)
    if result.returncode < 0:
        return 128 + abs(result.returncode)
    return result.returncode


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--validate-profile", action="store_true")
    args = parser.parse_args()
    environment = dict(os.environ)
    try:
        validate_profile(environment.get("MODE", ""), environment.get("PREPARATION_PROFILE"))
        if args.validate_profile:
            return 0
        return prepare(environment)
    except (KeyError, OSError, subprocess.CalledProcessError, ValueError) as error:
        print(f"TUI snapshot preparation failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
