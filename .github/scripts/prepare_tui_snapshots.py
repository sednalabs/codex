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
DIAGNOSTIC_SCHEMA = "sedna-tui-snapshot-diagnostic-v1"
SAFE_PENDING_PATH = re.compile(
    r"codex-rs/tui/src/(?:[A-Za-z0-9_-]{1,64}/)*"
    r"codex_tui__[A-Za-z0-9_]{1,240}(?:@(windows|macos|linux))?\.snap\.new"
)
SAFE_SOURCE_PATH = re.compile(r"(?:[A-Za-z0-9_.-]+/)*[A-Za-z0-9_.-]+")
CREDENTIAL_PATH = re.compile(r"(?:gh[pousr]_|github_pat_|sk-)[A-Za-z0-9_]{20,}")
OMISSION_REASONS = (
    "credential_shaped", "path_limit", "unsafe_syntax", "attribution_unavailable",
    "not_prelaunch_tracked", "prelaunch_ineligible", "path_validation_failed",
    "not_regular", "hardlinked", "observation_failed", "metadata_budget",
)
FAILURE_CODES = {
    "input_identity_invalid", "prelaunch_failed", "generator_launch_failed",
    "generator_result_invalid", "outputs_validation_failed", "output_count_exceeded",
    "output_outside_allowlist", "baseline_changed", "locks_changed",
    "output_size_limit", "other_files_changed", "workflow_host_changed",
    "accepted_artifact_persistence_failed", "diagnostic_persistence_failed",
    "diagnostic_metadata_overflow", "preparation_failed",
    "diagnostic_body_inventory_mismatch", "diagnostic_body_file_invalid",
    "diagnostic_body_inline_invalid", "diagnostic_body_privacy_failed",
    "diagnostic_body_size_limit", "diagnostic_body_persistence_failed",
}
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


class SnapshotPreparationError(ValueError):
    """A closed public failure code with an internal validation explanation."""

    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code if code in FAILURE_CODES else "preparation_failed"


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
    if re.search(r"\b(?:(?:gh[pousr]_|github_pat_)[A-Za-z0-9_]{20,}|sk-[A-Za-z0-9]{20,})\b", text):
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
        raise SnapshotPreparationError("baseline_changed", "accepted .snap baseline content or mode changed")
    locks_after = _capture_locks(product_root)
    if locks_after != locks_before:
        raise SnapshotPreparationError("locks_changed", "Cargo.lock or MODULE.bazel.lock changed during snapshot preparation")

    generated = _snapshot_inventory(product_root)
    if len(generated) > MAX_OUTPUTS:
        raise SnapshotPreparationError("output_count_exceeded", "pending snapshot output count exceeds its fixed limit")
    expected_paths = {f"{path}.new" for path in SNAPSHOT_PATHS}
    if not set(generated).issubset(expected_paths):
        raise SnapshotPreparationError("output_outside_allowlist", "pending snapshot output is outside the exact allowlist")

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
            raise SnapshotPreparationError("output_size_limit", "pending snapshot is empty or exceeds the per-file limit")
        _validate_public_output(contents, relative, environment)
        total_bytes += len(contents)
        if total_bytes > MAX_OUTPUT_BYTES:
            raise SnapshotPreparationError("output_size_limit", "pending snapshot output exceeds the aggregate byte limit")
        outputs[relative] = contents

    expected_untracked = set(generated)
    for state, relative in _parse_status(product_root):
        if state == "??" and relative in expected_untracked:
            continue
        raise SnapshotPreparationError("other_files_changed", "product checkout contains an unexpected change")
    nonignored_untracked = _git(
        product_root, "ls-files", "--others", "--exclude-standard", "-z"
    ).decode("utf-8", errors="strict").split("\0")
    if not {path for path in nonignored_untracked if path}.issubset(expected_untracked):
        raise SnapshotPreparationError("other_files_changed", "product checkout has an unexpected nonignored untracked file")
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

    identity = {
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
    if any(re.fullmatch(r"[0-9a-f]{40}", identity[key]) is None for key in (
        "workflow_host_tree", "product_tree", "comparison_base_tree"
    )) or len(run_id) > 20 or len(attempt) > 20:
        raise SnapshotPreparationError("input_identity_invalid", "tree or run identity is malformed")
    return identity


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


def _diagnostic_attribution(root: Path, catalogue: set[str]) -> set[str]:
    """Bind public pending names to regular tracked snapshots before generation."""
    attributed = set()
    for relative in catalogue:
        if not SAFE_PENDING_PATH.fullmatch(relative + ".new"):
            continue
        path = _relative_path(root, relative)
        info = path.lstat()
        if stat.S_ISREG(info.st_mode) and info.st_nlink == 1:
            attributed.add(relative + ".new")
    return attributed


DIAGNOSTIC_SNAPSHOT_CANDIDATES = (
    "codex-rs/tui/src/app/snapshots/codex_tui__app__tests__app_server_thread_replacement_clears_previous_transcript_before_replay-2.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__approvals_selection_popup@windows_degraded.snap",
)
DIAGNOSTIC_INLINE_SOURCE = "codex-rs/tui/src/analytics/activity_chart_tests.rs"
DIAGNOSTIC_INLINE_PENDING = "codex-rs/tui/src/analytics/.activity_chart_tests.rs.pending-snap"
# A diagnostic-only inventory from one immutable source, never an acceptance expansion.
DIAGNOSTIC_BODY_INPUT = {
    "product_sha": "60171beef0932ba64af67fc6cff33237e21da32d",
    "product_tree": "a926cf929f046242a9ccd531903a75c74c37d77e",
    "comparison_base_sha": "8389b61d82cb6fb936e4e500b977f31682441ffe",
    "comparison_base_tree": "699cd8b075e3aeafff573557612cdb944eb75c0a",
}
DIAGNOSTIC_BODY_SOURCES = (
    "codex-rs/tui/src/app/snapshots/codex_tui__app__agents_overview__tests__agents_overview_markdown.snap",
    "codex-rs/tui/src/app/snapshots/codex_tui__app__agents_overview__tests__agents_overview_model_grouping.snap",
    "codex-rs/tui/src/app/snapshots/codex_tui__app__agents_overview__tests__agents_overview_recent_sessions.snap",
    "codex-rs/tui/src/app/snapshots/codex_tui__app__agents_overview__tests__command_center__live_center_columns.snap",
    "codex-rs/tui/src/app/snapshots/codex_tui__app__agents_overview__tests__usage__agents_overview_usage.snap",
    "codex-rs/tui/src/app/snapshots/codex_tui__app__composer_hints__tests__composer_usage_notice.snap",
    "codex-rs/tui/src/app/snapshots/codex_tui__app__daemon_menu__tests__daemon_cli_confirmation.snap",
    "codex-rs/tui/src/app/snapshots/codex_tui__app__owned_transcript__empty_state_animation_tests__fresh_thread_header.snap",
    "codex-rs/tui/src/app/snapshots/codex_tui__app__owned_transcript__follow_tests__running_without_composer_hint.snap",
    "codex-rs/tui/src/app/snapshots/codex_tui__app__tests__app_server_thread_replacement_clears_previous_transcript_before_replay-2.snap",
    "codex-rs/tui/src/app/snapshots/codex_tui__app__turn_tips__tests__turn_tip_placements.snap",
    "codex-rs/tui/src/app/snapshots/codex_tui__app__turn_tips__tests__working_tip_mouse_down.snap",
    "codex-rs/tui/src/app/tests/snapshots/codex_tui__app__tests__active_reconnect__unavailable_conversation.snap",
    "codex-rs/tui/src/app/tests/snapshots/codex_tui__app__tests__navigation_reconnect__daemon_command_center_reconnecting.snap",
    "codex-rs/tui/src/app/tests/snapshots/codex_tui__app__tests__realtime_requests__voice_buffered_replay_reconciles_captions.snap",
    "codex-rs/tui/src/app/tests/snapshots/codex_tui__app__tests__realtime_requests__voice_completed_after_thread_switch.snap",
    "codex-rs/tui/src/app/tests/snapshots/codex_tui__app__tests__realtime_requests__voice_partial_completed_after_thread_switch.snap",
    "codex-rs/tui/src/app/tests/snapshots/codex_tui__app__tests__realtime_requests__voice_replay_reconciles_matching_captions.snap",
    "codex-rs/tui/src/app/tests/snapshots/codex_tui__app__tests__startup_frame_tests__startup_tip_in_transcript.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__app_server_collab_wait_items_render_history.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__catalog_model_session_header_startup.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__chatwidget_exec_and_status_layout_vt100_snapshot.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__chatwidget_tall.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__compact_queues_user_messages_snapshot.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__compaction_running.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__completed_turn_clears_visible_running_hook.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__guardian_goal_continuation_drops_stale_reviews.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__guardian_parallel_reviews_render_aggregate_status.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__guardian_write_stdin_review_status.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__hidden_shell_paste_queued_preview.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__image_generation_begin_restores_working_status.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__interrupted_turn_clears_visible_running_hook.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__overlapping_hook_live_cell_snapshot.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__preamble_keeps_working_status.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__reasoning_activity_row_80.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__reasoning_delta_restores_recreated_status_indicator.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__replayed_in_progress_turn.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__review_queues_user_messages_snapshot.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__running_hooks_share_background_activity_row.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__slash_copy_picker_status_fields.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__slash_side_requests_forked_side_question_while_task_running.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__status_widget_active.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__unified_exec_begin_restores_working_status.snap",
    "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__unified_exec_wait_status_renders_command_in_single_details_row.snap",
    "codex-rs/tui/src/history_cell/snapshots/codex_tui__history_cell__tests__pnpm_update_available_history_cell_snapshot.snap",
    "codex-rs/tui/src/history_cell/snapshots/codex_tui__history_cell__tests__session_info_availability_nux_tooltip_snapshot.snap",
    "codex-rs/tui/src/history_cell/snapshots/codex_tui__history_cell__tests__standalone_unix_update_available_history_cell_snapshot.snap",
    "codex-rs/tui/src/history_cell/snapshots/codex_tui__history_cell__tests__standalone_windows_update_available_history_cell_snapshot.snap",
    "codex-rs/tui/src/history_cell/snapshots/codex_tui__history_cell__tests__vite_plus_update_available_history_cell_snapshot.snap",
    "codex-rs/tui/src/snapshots/codex_tui__app__agents_overview__tests__agents_overview_live_activity.snap",
    "codex-rs/tui/src/snapshots/codex_tui__app__tests__clear_ui_after_long_transcript_fresh_header_only.snap",
    "codex-rs/tui/src/snapshots/codex_tui__multi_agents__tests__started_sub_agent_activity_configured_identity.snap",
    "codex-rs/tui/src/snapshots/codex_tui__multi_agents__tests__started_sub_agent_activity_missing_identity.snap",
    "codex-rs/tui/src/snapshots/codex_tui__startup_draft__layout__tests__owned_startup_layout.snap",
    "codex-rs/tui/src/snapshots/codex_tui__startup_draft__tests__startup_draft_fresh_session_transitions.snap",
    "codex-rs/tui/src/snapshots/codex_tui__startup_draft__tests__startup_draft_full_frames.snap",
    "codex-rs/tui/src/snapshots/codex_tui__startup_draft__tests__startup_draft_onboarding_transition.snap",
    "codex-rs/tui/src/snapshots/codex_tui__startup_draft__tests__terminal_app_ssh_startup.snap",
    "codex-rs/tui/src/snapshots/codex_tui__update_prompt__tests__update_picker_selected_28x12.snap",
    "codex-rs/tui/src/snapshots/codex_tui__update_prompt__tests__update_prompt_modal.snap",
    "codex-rs/tui/src/status/snapshots/codex_tui__status__tests__status_server_auth_not_required.snap",
    "codex-rs/tui/src/status/snapshots/codex_tui__status__tests__status_snapshot_cached_limits_hide_credits_without_flag.snap",
    "codex-rs/tui/src/status/snapshots/codex_tui__status__tests__status_snapshot_includes_credits_and_limits.snap",
    "codex-rs/tui/src/status/snapshots/codex_tui__status__tests__status_snapshot_includes_enterprise_monthly_credit_limit.snap",
    "codex-rs/tui/src/status/snapshots/codex_tui__status__tests__status_snapshot_includes_forked_from.snap",
    "codex-rs/tui/src/status/snapshots/codex_tui__status__tests__status_snapshot_includes_monthly_limit.snap",
    "codex-rs/tui/src/status/snapshots/codex_tui__status__tests__status_snapshot_includes_reasoning_details.snap",
    "codex-rs/tui/src/status/snapshots/codex_tui__status__tests__status_snapshot_shows_active_user_defined_profile.snap",
    "codex-rs/tui/src/status/snapshots/codex_tui__status__tests__status_snapshot_shows_auto_review_permissions.snap",
    "codex-rs/tui/src/status/snapshots/codex_tui__status__tests__status_snapshot_shows_chatgpt_plan_without_email.snap",
    "codex-rs/tui/src/status/snapshots/codex_tui__status__tests__status_snapshot_shows_missing_limits_message.snap",
    "codex-rs/tui/src/status/snapshots/codex_tui__status__tests__status_snapshot_shows_refreshing_limits_notice.snap",
    "codex-rs/tui/src/status/snapshots/codex_tui__status__tests__status_snapshot_shows_stale_limits_message.snap",
    "codex-rs/tui/src/status/snapshots/codex_tui__status__tests__status_snapshot_shows_unavailable_limits_message.snap",
    "codex-rs/tui/src/status/snapshots/codex_tui__status__tests__status_snapshot_treats_refreshing_empty_limits_as_unavailable.snap",
    "codex-rs/tui/src/status/snapshots/codex_tui__status__tests__status_snapshot_truncates_halfwidth_kana_in_narrow_terminal.snap",
    "codex-rs/tui/src/status/snapshots/codex_tui__status__tests__status_snapshot_truncates_in_narrow_terminal.snap",
    "codex-rs/tui/src/status/snapshots/codex_tui__status__tests__status_snapshot_uses_default_reasoning_when_config_empty.snap",
    "codex-rs/tui/src/status/snapshots/codex_tui__status__tests__status_snapshot_uses_generic_limit_labels_for_unsupported_windows.snap",
    "codex-rs/tui/src/status/snapshots/codex_tui__status__tests__transcript_overlay_status_rate_limit_refresh.snap",
)
DIAGNOSTIC_BODY_INLINE_SOURCE = "codex-rs/tui/src/chatwidget/rendering_tests.rs"
DIAGNOSTIC_BODY_INLINE_WORKSPACE_SOURCE = "tui/src/chatwidget/rendering_tests.rs"
DIAGNOSTIC_BODY_INLINE_MODULE = "codex_tui__chatwidget__rendering__tests"
DIAGNOSTIC_BODY_INLINE_LINES = (152, 350, 403)



def _inline_pending_locators(catalogue: set[str] | None) -> dict[str, str]:
    """Derive exact convention associations, not evidence of an Insta producer."""
    locators = {}
    for source in catalogue or ():
        if (not source.startswith(TUI_SOURCE_ROOT + "/") or not source.endswith(".rs")
                or _path_omission(source, source, catalogue, SAFE_SOURCE_PATH)):
            continue
        path = Path(source)
        pending = path.with_name("." + path.name + ".pending-snap").as_posix()
        if not _path_omission(pending, source, catalogue, SAFE_SOURCE_PATH):
            locators[pending] = source
    return locators


def _closed_diagnostic_attribution(root: Path, catalogue: set[str]) -> set[str]:
    """Capture regular one-link eligibility of public locators before generation."""
    eligible = set()
    for source in (*DIAGNOSTIC_SNAPSHOT_CANDIDATES, *_inline_pending_locators(catalogue).values()):
        if source not in catalogue:
            continue
        try:
            info = _relative_path(root, source).lstat()
            if stat.S_ISREG(info.st_mode) and info.st_nlink == 1:
                eligible.add(source)
        except (OSError, ValueError):
            continue
    return eligible


def _observed_file_omission(root: Path, relative: str) -> str:
    try:
        info = _relative_path(root, relative).lstat()
        if not stat.S_ISREG(info.st_mode):
            return "not_regular"
        if info.st_nlink != 1:
            return "hardlinked"
    except ValueError:
        return "path_validation_failed"
    except OSError:
        return "observation_failed"
    return ""


def _closed_locator_omission(root: Path, relative: str, source: str,
                            catalogue: set[str] | None, eligible: set[str] | None) -> str:
    if catalogue is None or eligible is None:
        return "attribution_unavailable"
    if source not in catalogue:
        return "not_prelaunch_tracked"
    if source not in eligible:
        return "prelaunch_ineligible"
    return _observed_file_omission(root, relative)


def _omission_counts(catalogue: set[str] | None) -> dict[str, dict[str, int | None]]:
    return {reason: {"count": 0, "prelaunch_tracked_count": 0 if catalogue is not None else None}
            for reason in OMISSION_REASONS}


def _path_omission(relative: str, source: str, catalogue: set[str] | None, syntax) -> str:
    if CREDENTIAL_PATH.search(relative):
        return "credential_shaped"
    if len(relative) > 1024:
        return "path_limit"
    if not syntax.fullmatch(relative) or any(part in (".", "..") for part in relative.split("/")):
        return "unsafe_syntax"
    if catalogue is None:
        return "attribution_unavailable"
    return "" if source in catalogue else "not_prelaunch_tracked"


def _record_omission(counts, reason: str, source: str, catalogue: set[str] | None) -> None:
    counts[reason]["count"] += 1
    if catalogue is not None:
        counts[reason]["prelaunch_tracked_count"] += int(source in catalogue)


def _diagnostic_inventory(root: Path, attributed: set[str] | None,
                          catalogue: set[str] | None, closed_eligible: set[str] | None) -> tuple[dict[str, object], set[str]]:
    """Count entries and expose attributable names, never pending file bodies."""
    observed: set[str] = set()
    paths = []
    omissions = _omission_counts(catalogue)
    traversal_complete = True

    def walk_error(_error):
        nonlocal traversal_complete
        traversal_complete = False

    source = root / TUI_SOURCE_ROOT
    if source.is_symlink() or not source.is_dir():
        traversal_complete = False
    else:
        for directory, subdirectories, filenames in os.walk(source, followlinks=False, onerror=walk_error):
            directory_path = Path(directory)
            safe_directories = []
            for name in subdirectories:
                if (directory_path / name).is_symlink():
                    traversal_complete = False
                else:
                    safe_directories.append(name)
            subdirectories[:] = safe_directories
            for name in filenames:
                if not name.endswith(".snap.new"):
                    continue
                relative = (directory_path / name).relative_to(root).as_posix()
                if relative in observed:
                    continue
                observed.add(relative)
                original = relative.removesuffix(".new")
                if original in DIAGNOSTIC_SNAPSHOT_CANDIDATES and relative == original + ".new":
                    reason = _closed_locator_omission(root, relative, original, catalogue, closed_eligible)
                    if reason:
                        _record_omission(omissions, reason, original, catalogue)
                    else:
                        paths.append({"source_locator": original, "kind": "tracked_snapshot_candidate",
                                      "classification": "pending_source_owner"})
                    continue
                reason = _path_omission(relative, original, catalogue if attributed is not None else None,
                                        SAFE_PENDING_PATH)
                if not reason and relative not in attributed:
                    reason = "prelaunch_ineligible"
                if not reason:
                    reason = _observed_file_omission(root, relative)
                if reason:
                    _record_omission(omissions, reason, original, catalogue)
                    continue
                paths.append({"path": relative, "origin": "historical69" if relative.removesuffix(".new") in SNAPSHOT_PATHS else "additional_tracked_snapshot",
                              "classification": "historical_allowed" if relative.removesuffix(".new") in SNAPSHOT_PATHS else "pending_source_owner"})
    complete = traversal_complete and attributed is not None and len(paths) == len(observed)
    return {"status": "complete" if complete else "partial" if observed else "unknown",
            "observed_count": len(observed) if traversal_complete else None,
            "observed_entry_count": len(observed), "paths": sorted(paths, key=lambda item: item.get("path", item.get("source_locator", ""))),
            "emitted_count": len(paths), "omitted_count": len(observed) - len(paths),
            "unobserved_count": 0 if traversal_complete else None,
            "complete": complete, "traversal_complete": traversal_complete,
            "attribution_available": attributed is not None,
            "omission_reasons": omissions, "omission_reasons_complete": True}, observed


def _observe_check(operation) -> dict[str, object]:
    try:
        matches = operation()
        return {"status": "verified" if matches else "failed", "actual_matches": bool(matches),
                "failure_code": "" if matches else "conservation_mismatch"}
    except ValueError:
        return {"status": "failed", "actual_matches": None, "failure_code": "conservation_invalid"}
    except (OSError, subprocess.CalledProcessError):
        return {"status": "unknown", "actual_matches": None, "failure_code": "observation_failed"}


def _other_files_unchanged(root: Path, pending: set[str], pending_complete: bool,
                           catalogue: set[str] | None, closed_eligible: set[str] | None) -> dict[str, object]:
    """Observe both existing queries; name only immutable, public source paths."""
    independently_checked = set(SNAPSHOT_PATHS) | set(LOCK_PATHS)
    records = {}
    uncertain_pending = set()
    queries = {}
    for query in ("status", "nonignored_untracked"):
        seen = 0
        try:
            if query == "status":
                entries = _parse_status(root)
            else:
                listed = _git(root, "ls-files", "--others", "--exclude-standard", "-z").decode("utf-8", errors="strict")
                entries = [(None, relative) for relative in listed.split("\0") if relative]
            for state, relative in entries:
                if state is not None and (state != "??" and
                        (len(state) != 2 or state == "  " or any(char not in " MTADU" for char in state))):
                    raise ValueError("unrecognized status state")
                seen += 1
                if relative in independently_checked or ((state is None or state == "??") and relative in pending):
                    continue
                if (not pending_complete and (state is None or state == "??")
                        and relative.startswith(TUI_SOURCE_ROOT + "/") and relative.endswith(".snap.new")):
                    uncertain_pending.add(relative)
                    continue
                record = records.setdefault(relative, {"git_status": None, "observed_via": set()})
                if state is not None:
                    if record["git_status"] not in (None, state):
                        raise ValueError("conflicting status states")
                    record["git_status"] = state
                record["observed_via"].add(query)
            queries[query] = {"status": "complete", "failure_code": "", "observed_count": seen,
                              "observed_entry_count": seen}
        except (OSError, ValueError, subprocess.CalledProcessError):
            queries[query] = {"status": "unknown", "failure_code": "observation_failed",
                              "observed_count": None, "observed_entry_count": seen}
    paths = []
    omissions = _omission_counts(catalogue)
    inline_locators = _inline_pending_locators(catalogue)
    for relative, record in sorted(records.items()):
        source = inline_locators.get(relative)
        if source is not None and record["git_status"] in (None, "??"):
            reason = _closed_locator_omission(root, relative, source, catalogue, closed_eligible)
            if reason:
                _record_omission(omissions, reason, source, catalogue)
            else:
                paths.append({"source_locator": source, "kind": "insta_inline_pending",
                              "git_status": record["git_status"], "observed_via": sorted(record["observed_via"])})
            continue
        reason = _path_omission(relative, relative, catalogue, SAFE_SOURCE_PATH)
        if reason:
            _record_omission(omissions, reason, relative, catalogue)
        else:
            paths.append({"path": relative, "git_status": record["git_status"],
                          "observed_via": sorted(record["observed_via"])})
    queries_complete = all(item["status"] == "complete" for item in queries.values())
    observations_complete = queries_complete and pending_complete and not uncertain_pending
    complete = observations_complete and len(paths) == len(records)
    matches = False if records else True if observations_complete else None
    return {"status": "failed" if matches is False else "verified" if matches is True else "unknown",
            "actual_matches": matches,
            "failure_code": "conservation_mismatch" if matches is False else "" if matches is True else "observation_failed",
            "evidence": {"status": "complete" if complete else "partial" if records else "unknown",
                "observed_count": len(records) if observations_complete else None,
                "observed_entry_count": len(records), "emitted_count": len(paths),
                "omitted_count": len(records) - len(paths), "paths": paths, "complete": complete,
                "queries": queries, "pending_inventory_complete": pending_complete,
                "unclassified_pending_count": len(uncertain_pending), "attribution_available": catalogue is not None,
                "omission_reasons": omissions}}


def _rejection_diagnostic(
    workspace: Path, identity: dict[str, str], baseline, locks, attributed, catalogue, closed_eligible,
    phase: str, code: str, exit_code: int | None, generation_attempted: bool,
) -> dict[str, object]:
    product = workspace / "product"
    try:
        inventory, observed = _diagnostic_inventory(product, attributed, catalogue, closed_eligible)
    except (OSError, ValueError, subprocess.CalledProcessError):
        inventory = {"status": "unknown", "observed_count": None, "observed_entry_count": 0, "paths": [],
                     "emitted_count": 0, "omitted_count": None, "unobserved_count": None,
                     "complete": False, "traversal_complete": False, "attribution_available": attributed is not None,
                     "omission_reasons": _omission_counts(catalogue), "omission_reasons_complete": False}
        observed = set()
    not_run = {"status": "not-run", "actual_matches": None, "failure_code": "expected_identity_unavailable"}
    conservation = {
        "baselines": {**(_observe_check(lambda: _capture_baseline(product) == baseline) if baseline is not None else not_run), "expected": baseline},
        "locks": {**(_observe_check(lambda: _capture_locks(product) == locks) if locks is not None else not_run), "expected": locks},
        "other_files": _other_files_unchanged(product, observed, inventory["traversal_complete"], catalogue, closed_eligible),
        "workflow_host": _observe_check(lambda: not _git(workspace / ".workflow-src", "status", "--porcelain=v1", "-z", "--untracked-files=all")),
    }
    return {"schema_version": DIAGNOSTIC_SCHEMA, "status": "failure", "artifact_kind": "diagnostic-only",
            "generated_output_acceptance": False, "phase": phase, "failure_code": code,
            "generator_exit_code": exit_code, "generation_attempted": generation_attempted,
            "identity": {**identity, "repository": "sednalabs/codex", "workflow": "sedna-branch-build",
                         "runner_label": "ubuntu-24.04", "architecture": "x86_64"},
            "historical_candidate_count": len(SNAPSHOT_PATHS), "accepted_output_limit": MAX_OUTPUTS,
            "inventory": inventory, "conservation": conservation,
            "metadata_status": "complete" if inventory["complete"] and conservation["other_files"]["evidence"]["complete"]
                and all(item["status"] != "unknown" for item in conservation.values()) else "incomplete"}


def _diagnostic_body_input(identity: dict[str, str]) -> bool:
    return all(identity.get(key) == value for key, value in DIAGNOSTIC_BODY_INPUT.items())


def _read_diagnostic_body(root: Path, relative: str) -> bytes:
    path = _relative_path(root, relative)
    before = path.lstat()
    if not stat.S_ISREG(before.st_mode) or before.st_nlink != 1:
        raise SnapshotPreparationError("diagnostic_body_file_invalid", "diagnostic body is not a regular single-link file")
    if not 0 < before.st_size <= MAX_FILE_BYTES:
        raise SnapshotPreparationError("diagnostic_body_size_limit", "diagnostic body exceeds its file limit")
    with path.open("rb") as stream:
        opened = os.fstat(stream.fileno())
        if (opened.st_dev, opened.st_ino, opened.st_mode, opened.st_nlink, opened.st_size) != (
                before.st_dev, before.st_ino, before.st_mode, 1, before.st_size):
            raise SnapshotPreparationError("diagnostic_body_file_invalid", "diagnostic body changed before reading")
        data = stream.read(MAX_FILE_BYTES + 1)
        after = os.fstat(stream.fileno())
    if (len(data) != before.st_size or (after.st_mode, after.st_nlink, after.st_size, after.st_mtime_ns, after.st_ctime_ns) !=
            (before.st_mode, 1, before.st_size, before.st_mtime_ns, before.st_ctime_ns)):
        raise SnapshotPreparationError("diagnostic_body_file_invalid", "diagnostic body changed during reading")
    return data


def _project_inline_pending(data: bytes, environment: dict[str, str]) -> bytes:
    """Read pinned Insta JSON-lines, never publish the pending record or its metadata."""
    def unique_object(pairs):
        value = {}
        for key, item in pairs:
            if key in value:
                raise ValueError("duplicate inline JSON key")
            value[key] = item
        return value

    def invalid_constant(_value):
        raise ValueError("nonfinite inline JSON value")

    records, seen, run_id = [], set(), None
    lines = data.decode("utf-8", errors="strict").split("\n")
    if lines[-1] != "":
        raise ValueError("inline pending batch is not a complete JSON-lines write")
    for line in lines[:-1]:
        value = json.loads(line, object_pairs_hook=unique_object, parse_constant=invalid_constant)
        if not isinstance(value, dict) or set(value) != {"run_id", "line", "new", "old"}:
            raise ValueError("inline pending record fields differ")
        identity, location = value["run_id"], value["line"]
        if (not isinstance(identity, str) or not re.fullmatch(r"[A-Za-z0-9_-]{1,128}", identity)
                or type(location) is not int or location not in DIAGNOSTIC_BODY_INLINE_LINES
                or location in seen or run_id not in (None, identity)):
            raise ValueError("inline pending identity or assertion location differs")
        seen.add(location)
        run_id = identity
        projected = {"source_locator": DIAGNOSTIC_BODY_INLINE_SOURCE, "assertion_line": location}
        for side in ("old", "new"):
            snapshot = value[side]
            if snapshot is None:
                projected[side] = None
                continue
            if (not isinstance(snapshot, dict) or not {"module_name", "metadata", "snapshot"}.issubset(snapshot)
                    or set(snapshot) - {"module_name", "snapshot_name", "metadata", "snapshot"}
                    or snapshot["module_name"] != DIAGNOSTIC_BODY_INLINE_MODULE
                    or ("snapshot_name" in snapshot and (side == "old" or not isinstance(snapshot["snapshot_name"], str)
                        or not re.fullmatch(r"[A-Za-z0-9_]{1,240}", snapshot["snapshot_name"])
                        or CREDENTIAL_PATH.search(snapshot["snapshot_name"])))
                    or not isinstance(snapshot["metadata"], dict) or not isinstance(snapshot["snapshot"], str)):
                raise ValueError("inline snapshot identity or fields differ")
            metadata = snapshot["metadata"]
            if (side == "old" and metadata) or (side == "new" and (
                    set(metadata) - {"source", "assertion_line", "description", "expression", "info", "input_file"}
                    or metadata.get("source") != DIAGNOSTIC_BODY_INLINE_WORKSPACE_SOURCE
                    or type(metadata.get("assertion_line")) is not int or metadata["assertion_line"] != location
                    or any(key in metadata and not isinstance(metadata[key], str)
                           for key in ("description", "expression", "input_file")))):
                raise ValueError("inline snapshot source or assertion location differs")
            body = snapshot["snapshot"].encode("utf-8", errors="strict")
            try:
                _validate_public_output(body, DIAGNOSTIC_BODY_INLINE_SOURCE, environment)
            except ValueError as error:
                raise SnapshotPreparationError("diagnostic_body_privacy_failed", "inline text failed its public guard") from error
            projected[side] = snapshot["snapshot"]
        if (projected["old"] is None) != (projected["new"] is None):
            raise ValueError("inline pending pair is incomplete")
        records.append(projected)
    if not records:
        raise ValueError("inline pending batch is empty")
    encoded = (json.dumps(records, indent=2, sort_keys=True) + "\n").encode("utf-8")
    if len(encoded) > MAX_FILE_BYTES:
        raise SnapshotPreparationError("diagnostic_body_size_limit", "inline projection exceeds its file limit")
    return encoded


def _capture_diagnostic_bodies(root: Path, diagnostic, attributed, catalogue, closed_eligible,
                              inline_absent: bool, environment: dict[str, str]) -> tuple[dict[str, bytes], str | None]:
    expected = {source + ".new" for source in DIAGNOSTIC_BODY_SOURCES}
    inventory = diagnostic["inventory"]
    actual = {item.get("path", item.get("source_locator", "") + ".new") for item in inventory["paths"]}
    conservation = diagnostic["conservation"]
    other = conservation["other_files"]
    evidence = other["evidence"]
    inline = {"source_locator": DIAGNOSTIC_BODY_INLINE_SOURCE, "kind": "insta_inline_pending",
              "git_status": "??", "observed_via": ["nonignored_untracked", "status"]}
    if (diagnostic["failure_code"] != "output_count_exceeded" or diagnostic["phase"] != "outputs-validation"
            or not diagnostic["generation_attempted"] or type(diagnostic["generator_exit_code"]) is not int
            or not inline_absent or not inventory["complete"] or actual != expected
            or len(inventory["paths"]) != len(expected) or inventory["observed_count"] != len(expected)
            or inventory["observed_entry_count"] != len(expected) or inventory["emitted_count"] != len(expected)
            or inventory["omitted_count"] != 0 or other["status"] != "failed" or other["actual_matches"] is not False
            or not evidence["complete"] or evidence["paths"] != [inline]
            or any(evidence[key] != value for key, value in (("observed_count", 1), ("observed_entry_count", 1), ("emitted_count", 1), ("omitted_count", 0)))
            or set(evidence["queries"]) != {"status", "nonignored_untracked"}
            or any(item["status"] != "complete" or item["observed_count"] != len(expected) + 1
                   for item in evidence["queries"].values())
            or any(conservation[key]["status"] != "verified" for key in ("baselines", "locks", "workflow_host"))
            or catalogue is None or attributed is None or closed_eligible is None
            or DIAGNOSTIC_BODY_INLINE_SOURCE not in closed_eligible):
        raise SnapshotPreparationError("diagnostic_body_inventory_mismatch", "diagnostic body input set is incomplete or differs")
    bodies, total = {}, 0
    for source in DIAGNOSTIC_BODY_SOURCES:
        if source not in catalogue or (source + ".new" not in attributed and source not in closed_eligible):
            raise SnapshotPreparationError("diagnostic_body_inventory_mismatch", "diagnostic body source was not eligible before generation")
        if _observed_file_omission(root, source):
            raise SnapshotPreparationError("diagnostic_body_file_invalid", "diagnostic body source is no longer eligible")
        data = _read_diagnostic_body(root, source + ".new")
        try:
            _validate_public_output(data, source, environment)
        except ValueError as error:
            raise SnapshotPreparationError("diagnostic_body_privacy_failed", "diagnostic snapshot body failed its public guard") from error
        total += len(data)
        if total > MAX_OUTPUT_BYTES:
            raise SnapshotPreparationError("diagnostic_body_size_limit", "diagnostic bodies exceed the aggregate limit")
        bodies["snapshots/" + source + ".new"] = data
    # The tracked body set is independently guarded above. An unavailable inline
    # projection must not suppress those diagnostic bodies or weaken its guards.
    try:
        source = Path(DIAGNOSTIC_BODY_INLINE_SOURCE)
        if _observed_file_omission(root, source.as_posix()):
            raise SnapshotPreparationError("diagnostic_body_file_invalid", "inline source is no longer eligible")
        pending = source.with_name("." + source.name + ".pending-snap").as_posix()
        raw_inline = _read_diagnostic_body(root, pending)
        projected = _project_inline_pending(raw_inline, environment)
        if total + len(projected) > MAX_OUTPUT_BYTES:
            raise SnapshotPreparationError("diagnostic_body_size_limit", "diagnostic bodies exceed the aggregate limit")
    except (OSError, ValueError, TypeError, RecursionError) as error:
        code = error.code if isinstance(error, SnapshotPreparationError) else "diagnostic_body_inline_invalid"
        return bodies, code
    bodies["inline-review.json"] = projected
    return bodies, None


def _write_diagnostic_artifact(staging: Path, artifact: Path, diagnostic: dict[str, object],
                               bodies: dict[str, bytes] | None = None) -> None:
    tracked = {"snapshots/" + source + ".new" for source in DIAGNOSTIC_BODY_SOURCES}
    inline_unavailable = (diagnostic.get("metadata_status") == "incomplete"
        and diagnostic.get("diagnostic_body_status") == "partial"
        and diagnostic.get("diagnostic_inline_status") == "unavailable"
        and diagnostic.get("metadata_failure_code") in {
            "diagnostic_body_file_invalid", "diagnostic_body_inline_invalid",
            "diagnostic_body_privacy_failed", "diagnostic_body_size_limit"})
    expected = tracked if inline_unavailable else tracked | {"inline-review.json"}
    if bodies is not None and (set(bodies) != expected or any(type(data) is not bytes or not 0 < len(data) <= MAX_FILE_BYTES
                               for data in bodies.values()) or sum(len(data) for data in bodies.values()) > MAX_OUTPUT_BYTES):
        diagnostic = {**diagnostic, "metadata_status": "incomplete", "metadata_failure_code": "diagnostic_body_size_limit",
                      "diagnostic_body_status": "unavailable"}
        bodies = None
    encoded = (json.dumps(diagnostic, indent=2, sort_keys=True) + "\n").encode("utf-8")
    if bodies is not None and len(encoded) + sum(len(data) for data in bodies.values()) > MAX_ARTIFACT_BYTES:
        diagnostic = {**diagnostic, "metadata_status": "incomplete", "metadata_failure_code": "diagnostic_body_size_limit",
                      "diagnostic_body_status": "unavailable"}
        bodies = None
        encoded = (json.dumps(diagnostic, indent=2, sort_keys=True) + "\n").encode("utf-8")
    if len(encoded) > MAX_METADATA_BYTES:
        bodies = None
        inventory = dict(diagnostic["inventory"])
        other = dict(diagnostic["conservation"]["other_files"])
        evidence = dict(other["evidence"])
        for section in (inventory, evidence):
            omissions = {reason: dict(counts) for reason, counts in section["omission_reasons"].items()}
            dropped = section["emitted_count"]
            omissions["metadata_budget"]["count"] += dropped
            if omissions["metadata_budget"]["prelaunch_tracked_count"] is not None:
                omissions["metadata_budget"]["prelaunch_tracked_count"] += dropped
            section.update(status="partial", paths=[], emitted_count=0,
                           omitted_count=None if section["omitted_count"] is None else section["omitted_count"] + dropped,
                           complete=False, omission_reasons=omissions)
        other["evidence"] = evidence
        diagnostic = {**diagnostic, "metadata_status": "incomplete", "metadata_failure_code": "diagnostic_metadata_overflow",
                      "diagnostic_body_status": "unavailable", "original_metadata_bytes": len(encoded), "inventory": inventory,
                      "conservation": {**diagnostic["conservation"], "other_files": other}}
        encoded = (json.dumps(diagnostic, indent=2, sort_keys=True) + "\n").encode("utf-8")
    if len(encoded) > MAX_METADATA_BYTES or len(encoded) > MAX_ARTIFACT_BYTES:
        raise SnapshotPreparationError("diagnostic_metadata_overflow", "diagnostic metadata exceeds its fixed limit")
    # Never reuse accepted-output staging or expose a partially written body set.
    if any(path.exists() or path.is_symlink() for path in (staging, artifact)):
        raise SnapshotPreparationError("diagnostic_persistence_failed", "diagnostic output path is already occupied")
    staging.mkdir(mode=0o700)
    (staging / "diagnostic.json").write_bytes(encoded)
    attempted, directories = [], set()
    try:
        for relative, data in sorted((bodies or {}).items()):
            destination = staging / relative
            for parent in destination.parents:
                if parent == staging:
                    break
                directories.add(parent)
            destination.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            attempted.append(destination)
            destination.write_bytes(data)
    except OSError:
        # Remove only this fresh writer's finite body paths, never recurse or retry
        # a failed/uncertain metadata write. Cleanup failure leaves no visible artifact.
        for destination in reversed(attempted):
            destination.unlink(missing_ok=True)
        for directory in sorted(directories, key=lambda path: len(path.parts), reverse=True):
            if directory.exists():
                directory.rmdir()
        diagnostic = {**diagnostic, "metadata_status": "incomplete", "metadata_failure_code": "diagnostic_body_persistence_failed",
                      "diagnostic_body_status": "unavailable"}
        encoded = (json.dumps(diagnostic, indent=2, sort_keys=True) + "\n").encode("utf-8")
        if len(encoded) > MAX_METADATA_BYTES or len(encoded) > MAX_ARTIFACT_BYTES:
            raise SnapshotPreparationError("diagnostic_metadata_overflow", "body failure metadata exceeds its fixed limit")
        (staging / "diagnostic.json").write_bytes(encoded)
    staging.rename(artifact)


def prepare(environment: dict[str, str]) -> int:
    workspace = Path(environment["GITHUB_WORKSPACE"])
    product_root = workspace / "product"
    runner_temp = Path(environment["RUNNER_TEMP"])
    try:
        identity = validate_input_identity(environment, workspace)
    except (KeyError, OSError, subprocess.CalledProcessError, ValueError) as error:
        raise SnapshotPreparationError("input_identity_invalid", "input identity validation failed") from error
    target_dir, artifact_dir, staging_dir = _fixed_runner_paths(environment, workspace)
    _check_clean_checkout(workspace / ".workflow-src", "workflow host")
    _check_clean_checkout(product_root, "product")

    baseline = locks_before = attributed = catalogue = closed_eligible = None
    phase = "prelaunch"
    fallback_code = "prelaunch_failed"
    actual_exit = None
    generation_attempted = False
    inline_absent = False
    try:
        baseline = _capture_baseline(product_root)
        locks_before = _capture_locks(product_root)
        validate_no_preexisting_pending(product_root)
        catalogue = _tracked_path_set(product_root)
        attributed = _diagnostic_attribution(product_root, catalogue)
        closed_eligible = _closed_diagnostic_attribution(product_root, catalogue)
        if _diagnostic_body_input(identity) and DIAGNOSTIC_BODY_INLINE_SOURCE in closed_eligible:
            source = _relative_path(product_root, DIAGNOSTIC_BODY_INLINE_SOURCE)
            pending = source.with_name("." + source.name + ".pending-snap")
            inline_absent = not (pending.exists() or pending.is_symlink())
        target_dir.mkdir(mode=0o700)
        output_env = dict(environment)
        output_env["INSTA_UPDATE"] = "new"
        output_env["CARGO_TARGET_DIR"] = str(target_dir)
        phase, fallback_code = "generator", "generator_launch_failed"
        generation_attempted = True
        result = subprocess.run(
            COMMAND, cwd=product_root, env=output_env, check=False,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        if type(result.returncode) is not int or not -99999 <= result.returncode <= 99999:
            raise SnapshotPreparationError("generator_result_invalid", "generator result is not a valid exit code")
        actual_exit = result.returncode
        phase, fallback_code = "outputs-validation", "outputs_validation_failed"
        outputs = validate_outputs(product_root, baseline, locks_before, environment)
        phase, fallback_code = "workflow-conservation", "workflow_host_changed"
        _check_clean_checkout(workspace / ".workflow-src", "workflow host")
    except (KeyError, OSError, subprocess.CalledProcessError, ValueError) as error:
        code = error.code if isinstance(error, SnapshotPreparationError) else fallback_code
        diagnostic = _rejection_diagnostic(workspace, identity, baseline, locks_before, attributed, catalogue, closed_eligible,
                                            phase, code, actual_exit, generation_attempted)
        bodies = None
        if _diagnostic_body_input(identity):
            try:
                bodies, inline_code = _capture_diagnostic_bodies(product_root, diagnostic, attributed, catalogue, closed_eligible,
                                                                 inline_absent, environment)
                if inline_code is not None:
                    diagnostic = {**diagnostic, "metadata_status": "incomplete", "metadata_failure_code": inline_code,
                                  "diagnostic_body_status": "partial", "diagnostic_inline_status": "unavailable"}
            except (OSError, ValueError, TypeError, RecursionError) as capture_error:
                capture_code = capture_error.code if isinstance(capture_error, SnapshotPreparationError) else "diagnostic_body_file_invalid"
                diagnostic = {**diagnostic, "metadata_status": "incomplete", "metadata_failure_code": capture_code}
        try:
            _write_diagnostic_artifact(staging_dir, artifact_dir, diagnostic, bodies)
        except (OSError, ValueError) as persistence_error:
            persistence_code = persistence_error.code if isinstance(persistence_error, SnapshotPreparationError) else "diagnostic_persistence_failed"
            print(f"TUI snapshot preparation failed: {persistence_code}; artifact_state=unknown", file=sys.stderr)
            return 1
        print(f"TUI snapshot preparation failed: {code}; artifact_state=diagnostic-only", file=sys.stderr)
        return 1
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
    try:
        _write_artifact(staging_dir, identity, baseline, locks_before, outputs)
        staging_dir.rename(artifact_dir)
    except (OSError, ValueError):
        print("TUI snapshot preparation failed: accepted_artifact_persistence_failed; artifact_state=unknown", file=sys.stderr)
        return 1
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
        code = error.code if isinstance(error, SnapshotPreparationError) else "preparation_failed"
        print(f"TUI snapshot preparation failed: {code}; artifact_state=unknown", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
