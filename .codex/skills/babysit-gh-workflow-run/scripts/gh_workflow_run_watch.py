#!/usr/bin/env python3
"""Watch GitHub Actions workflow runs for remote validation babysitting."""

import io
import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import zipfile
from pathlib import Path
from urllib.parse import urlparse

GEMINI_API_BASE_URL = "https://generativelanguage.googleapis.com/v1beta"
GEMINI_DEFAULT_MODEL = "gemini-3.1-flash-lite-preview"
GEMINI_DEFAULT_TIMEOUT_SECONDS = 40.0
GEMINI_LOG_CHAR_BUDGET = 120_000
GEMINI_CODE_CONTEXT_CHAR_BUDGET = 20_000
GEMINI_CODE_CONTEXT_MAX_FILES = 6
GEMINI_CODE_EXCERPT_CONTEXT = 8
GEMINI_MAX_OUTPUT_TOKENS = 1024
GEMINI_MAX_REQUEST_RETRIES = 2
GEMINI_REDACTED_VALUE = "<redacted>"
GEMINI_PRIMARY_JOB_CHAR_BUDGET = 8_000
GEMINI_SUPPORTING_JOB_CHAR_BUDGET = 3_500
GEMINI_META_JOB_CHAR_BUDGET = 2_500
GEMINI_FAILURE_OVERVIEW_MAX_JOBS = 8
VALIDATION_CHECKPOINT_PROFILES = {"checkpoint", "broad", "full", "artifact", "smoke"}

GEMINI_DIAGNOSIS_SCHEMA = {
    "type": "object",
    "additionalProperties": False,
    "required": [
        "summary",
        "likely_root_cause",
        "confidence",
        "next_steps",
        "suspect_paths",
        "evidence_notes",
    ],
    "properties": {
        "summary": {"type": "string"},
        "likely_root_cause": {"type": "string"},
        "confidence": {"type": "string", "enum": ["high", "medium", "low"]},
        "next_steps": {"type": "array", "items": {"type": "string"}},
        "suspect_paths": {"type": "array", "items": {"type": "string"}},
        "evidence_notes": {"type": "array", "items": {"type": "string"}},
        "primary_failed_job": {"type": "string"},
        "primary_failed_step": {"type": "string"},
        "failing_test": {"type": "string"},
        "failing_location": {"type": "string"},
        "failure_structure": {"type": "string"},
        "recommended_follow_up": {"type": "string"},
    },
}

GEMINI_REDACT_PATTERNS = (
    (re.compile(r"(?i)(authorization:\s*bearer\s+)([^\s]+)"), r"\1<redacted>"),
    (re.compile(r"(?i)(x-github-token:\s*)([^\s]+)"), r"\1<redacted>"),
    (re.compile(r"(?i)(github_pat_[A-Za-z0-9_]+)"), "<redacted>"),
    (re.compile(r"(?i)\bgh[pousr]_[A-Za-z0-9_]{36,}\b"), "<redacted>"),
    (
        re.compile(
            r"(?i)\b(password|token|secret|api[_-]?key|client_secret|subject_token|access_token|refresh_token)\b"
            r"(\s*[:=]\s*)([^\s'\"`]+)"
        ),
        r"\1\2<redacted>",
    ),
    (re.compile(r"(?i)\b(postgresql|postgres)(\+\w+)?://[^\s'\"`]+"), "postgresql://<redacted>"),
    (re.compile(r"(?i)(set-cookie:\s*)([^\r\n]+)"), r"\1<redacted>"),
    (re.compile(r"(?i)(cookie:\s*)([^\r\n]+)"), r"\1<redacted>"),
)

LOG_FAILURE_MARKERS = (
    "error",
    "failed",
    "failure",
    "traceback",
    "exception",
    "assertion failed",
    "panic",
    "panicked",
    "fatal",
    "timeout",
    "segmentation fault",
)

ANSI_ESCAPE_RE = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")
GH_LOG_TIMESTAMP_RE = re.compile(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d")
NOISY_FAILURE_PATTERNS = (
    re.compile(r"--fail(?:\s|$)", re.IGNORECASE),
    re.compile(r"\bpipefail\b", re.IGNORECASE),
    re.compile(r"\bshow-error\b", re.IGNORECASE),
)
HIGH_SIGNAL_FAILURE_PATTERNS = (
    re.compile(r"\btest\s+([A-Za-z0-9_:-]+)\s+\.\.\.\s+FAILED\b", re.IGNORECASE),
    re.compile(r"\bFAIL\b.*?\)\s+(.+)$"),
    re.compile(r"thread '([^']+)'.*?\bpanicked at\b\s+(.+)", re.IGNORECASE),
    re.compile(r"assertion failed:\s*(.+)", re.IGNORECASE),
    re.compile(r"test result:\s*FAILED\b", re.IGNORECASE),
    re.compile(r"error:\s*test (?:run )?failed\b", re.IGNORECASE),
    re.compile(r"Traceback \(most recent call last\):"),
)
EXACT_TEST_FAILURE_RE = re.compile(r"\btest\s+([A-Za-z0-9_:-]+)\s+\.\.\.\s+FAILED\b", re.IGNORECASE)
NEXTEST_FAILURE_RE = re.compile(r"\bFAIL\b.*?\)\s+(.+)$")
PANIC_LINE_RE = re.compile(r"thread '([^']+)'.*?\bpanicked at\b\s+(.+)", re.IGNORECASE)
ASSERTION_LINE_RE = re.compile(r"assertion failed:\s*(.+)", re.IGNORECASE)

META_JOB_NAME_MARKERS = (
    "ci results",
    "required checks",
    "workflow summary",
    "job summary",
    "rollup",
    "aggregate",
)

CODE_FILE_EXTENSIONS = (".py", ".rs", ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".toml", ".yaml", ".yml")

CODE_PATH_PATTERNS = (
    re.compile(r'File "([^"]+)", line (\d+)'),
    re.compile(
        r"(?i)-->\s*([^\s:]+(?:\.(?:py|rs|ts|tsx|js|jsx|mjs|cjs|toml|yaml|yml))):(\d+)(?::(\d+))?"
    ),
    re.compile(
        r"(?<!\w)((?:[A-Za-z]:)?[^\s:\"\']+?\.(?:py|rs|ts|tsx|js|jsx|mjs|cjs|toml|yaml|yml)):(\d+)(?::(\d+))?"
    ),
    re.compile(
        r"(?<!\w)((?:[A-Za-z]:)?[^\s:\"\']+?\.(?:py|rs|ts|tsx|js|jsx|mjs|cjs|toml|yaml|yml))::[A-Za-z0-9_:-]+"
    ),
)

PENDING_STATUSES = {
    "queued",
    "in_progress",
    "pending",
    "requested",
    "waiting",
}
FAILED_CONCLUSIONS = {
    "failure",
    "timed_out",
    "cancelled",
    "action_required",
    "startup_failure",
    "stale",
}
SUCCESS_CONCLUSIONS = {
    "success",
    "neutral",
    "skipped",
}

TARGET_KIND_RUN_ID = "run_id"
TARGET_KIND_WORKFLOW = "workflow"
FOLLOWED_RUN_RELIST_MULTIPLIER = 5
FOLLOWED_RUN_RELIST_MIN_SECONDS = 60
HOST_MISMATCH_RECHECK_MIN_SECONDS = 60

_GH_ENV = None


class GhCommandError(RuntimeError):
    pass


class GeminiDiagnosisError(RuntimeError):
    def __init__(self, message, *, evidence=None, telemetry=None):
        super().__init__(message)
        self.evidence = evidence
        self.telemetry = telemetry


def parse_args():
    parser = argparse.ArgumentParser(
        description=(
            "Normalize GitHub Actions workflow-run state for lab/heavy/build babysitting "
            "and optionally block until an actionable or terminal result appears."
        )
    )
    parser.add_argument(
        "--run-id",
        type=int,
        help=(
            "Deprecated legacy arg for a single exact GitHub Actions run id. Prefer "
            "--target run-id=<id> instead."
        ),
    )
    parser.add_argument(
        "--workflow",
        default=None,
        help="Workflow name or workflow file when watching the newest run on a ref",
    )
    parser.add_argument(
        "--ref",
        default="auto",
        help="Branch or SHA to watch when not using --run-id (default: current branch)",
    )
    parser.add_argument(
        "--head-sha",
        default=None,
        help=(
            "Optional exact or prefix head SHA to pin a workflow/ref watch to a specific run "
            "generation."
        ),
    )
    parser.add_argument(
        "--host-ref",
        default=None,
        help=(
            "Optional run host branch for workflow/ref targets when workflow_dispatch runs are "
            "created on a different branch than the logical --ref."
        ),
    )
    parser.add_argument(
        "--target",
        action="append",
        default=[],
        help=(
            "Target spec to watch. Repeatable. Supported forms: "
            "'run-id=<id>' or "
            "'workflow=<name>,ref=<ref>[,host-ref=<branch>][,head-sha=<sha>][,min-run-id=<id>]'."
        ),
    )
    parser.add_argument("--repo", help="Optional OWNER/REPO override")
    parser.add_argument("--poll-seconds", type=int, default=60, help="Watch poll interval")
    parser.add_argument(
        "--appearance-timeout-seconds",
        type=int,
        default=None,
        help=(
            "When watching workflow/ref targets with --watch-until-action, keep waiting for a "
            "matching run to appear for up to this many seconds before returning actionable "
            "timeout state. Defaults to 300 in --watch-until-action mode and 0 otherwise."
        ),
    )
    parser.add_argument(
        "--min-run-id",
        type=int,
        default=None,
        help="Optional lower bound for workflow/ref watch targets to ignore older matching runs.",
    )
    parser.add_argument("--once", action="store_true", help="Emit one snapshot and exit")
    parser.add_argument("--watch", action="store_true", help="Continuously emit JSONL snapshots")
    parser.add_argument(
        "--watch-until-action",
        action="store_true",
        help="Poll until the run succeeds, fails, or otherwise needs parent action",
    )
    parser.add_argument(
        "--wait-for",
        choices=("first_action", "all_done"),
        default="first_action",
        help=(
            "When using --watch-until-action: return on first actionable target, "
            "or only when all targets become actionable."
        ),
    )
    parser.add_argument(
        "--watch-until-terminal",
        "--wait-until-terminal",
        dest="watch_until_terminal",
        action="store_true",
        help=(
            "Poll until watched targets reach terminal states (implies "
            "--watch-until-action and --require-terminal-run)."
        ),
    )
    parser.add_argument(
        "--require-terminal-run",
        action="store_true",
        help=(
            "(Only relevant with --watch-until-action) keep waiting until watched runs "
            "reach status completed before returning failure actions triggered by in-progress failed jobs."
        ),
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Emit machine-readable output (default behavior for --once and --watch modes)",
    )
    parser.add_argument(
        "--verbose-details",
        action="store_true",
        help="Emit full per-target detail payloads instead of compact default output.",
    )
    parser.add_argument(
        "--gemini-model",
        default=GEMINI_DEFAULT_MODEL,
        help=f"Gemini model to use for failure diagnosis (default: {GEMINI_DEFAULT_MODEL})",
    )
    parser.add_argument(
        "--gemini-timeout-seconds",
        type=float,
        default=GEMINI_DEFAULT_TIMEOUT_SECONDS,
        help=(
            "Timeout for a single Gemini diagnosis request. Keep this short so the watcher stays "
            "snappy."
        ),
    )
    parser.add_argument(
        "--no-gemini-diagnosis",
        dest="no_gemini_diagnosis",
        action="store_true",
        help="Skip the Gemini diagnosis step and only return raw workflow state.",
    )
    parser.add_argument(
        "--gemini-diagnosis",
        dest="no_gemini_diagnosis",
        action="store_false",
        help="Force-enable Gemini diagnosis even when disable env is set.",
    )
    parser.add_argument(
        "--ack-action",
        action="append",
        default=[],
        help=(
            "Suppress previously handled actionable blocker fingerprints and keep waiting for the "
            "next actionable state."
        ),
    )
    parser.set_defaults(no_gemini_diagnosis=None)
    args = parser.parse_args()

    if args.poll_seconds <= 0:
        parser.error("--poll-seconds must be > 0")
    if args.appearance_timeout_seconds is not None and args.appearance_timeout_seconds < 0:
        parser.error("--appearance-timeout-seconds must be >= 0")
    if args.gemini_timeout_seconds is not None and args.gemini_timeout_seconds <= 0:
        parser.error("--gemini-timeout-seconds must be > 0")
    if args.min_run_id is not None and args.min_run_id <= 0:
        parser.error("--min-run-id must be > 0")
    watch_mode_enabled = args.watch_until_action or args.watch_until_terminal
    selected_modes = sum(1 for enabled in (args.once, args.watch, watch_mode_enabled) if enabled)
    if selected_modes > 1:
        parser.error(
            "choose only one of --once, --watch, --watch-until-action, or --watch-until-terminal"
        )
    if selected_modes == 0:
        args.once = True
    if watch_mode_enabled:
        args.watch_until_action = True
        if args.watch_until_terminal:
            args.require_terminal_run = True
    if args.appearance_timeout_seconds is None:
        args.appearance_timeout_seconds = 300 if args.watch_until_action else 0
    if args.no_gemini_diagnosis is None:
        args.no_gemini_diagnosis = _env_flag_is_true("GH_WORKFLOW_RUN_WATCH_DISABLE_GEMINI")
    return args


def _env_flag_is_true(name):
    value = str(os.environ.get(name, "")).strip().lower()
    if not value:
        return False
    return value in {"1", "true", "yes", "on"}


def parse_target_arg(spec):
    if not spec:
        raise GhCommandError("Target spec cannot be empty")
    raw = str(spec).strip()
    if not raw:
        raise GhCommandError("Target spec cannot be empty")

    fields = {}
    for chunk in raw.split(","):
        chunk = chunk.strip()
        if not chunk:
            continue
        if "=" not in chunk:
            raise GhCommandError(f"Invalid target chunk '{chunk}' in '{raw}'. Use key=value format.")
        key, value = chunk.split("=", 1)
        key = key.strip().lower()
        value = value.strip()
        if not key or value == "":
            raise GhCommandError(f"Invalid target chunk '{chunk}' in '{raw}'.")
        if key in fields:
            raise GhCommandError(f"Duplicate target key '{key}' in '{raw}'.")
        fields[key] = value

    if not fields:
        raise GhCommandError(f"Target spec '{raw}' is missing key=value pairs.")

    if "run-id" in fields or "run_id" in fields:
        if len(fields) > 1:
            raise GhCommandError(f"'run-id' target cannot include other fields: '{raw}'.")
        key = "run-id" if "run-id" in fields else "run_id"
        run_id = fields[key]
        try:
            value = int(run_id)
        except ValueError as err:
            raise GhCommandError(f"Invalid run-id '{run_id}' in target '{raw}'.") from err
        return {
            "kind": TARGET_KIND_RUN_ID,
            "run_id": value,
            "spec": raw,
        }

    workflow = fields.get("workflow")
    if not workflow:
        raise GhCommandError(
            f"Target spec '{raw}' must include 'run-id' or 'workflow'."
        )
    if "name" in fields and "workflow" not in fields:
        workflow = fields["name"]
    ref = fields.get("ref", "auto")
    if not ref:
        raise GhCommandError(f"Target spec '{raw}' has empty ref.")
    head_sha = fields.get("head-sha") or fields.get("head_sha") or fields.get("commit")
    host_ref = (
        fields.get("host-ref")
        or fields.get("host_ref")
        or fields.get("host-branch")
        or fields.get("host_branch")
    )
    min_run_id = None
    min_run_id_raw = fields.get("min-run-id") or fields.get("min_run_id")
    if min_run_id_raw is not None:
        try:
            min_run_id = int(min_run_id_raw)
        except ValueError as err:
            raise GhCommandError(f"Invalid min-run-id '{min_run_id_raw}' in target '{raw}'.") from err
        if min_run_id <= 0:
            raise GhCommandError(f"Invalid min-run-id '{min_run_id_raw}' in target '{raw}'.")
    return {
        "kind": TARGET_KIND_WORKFLOW,
        "workflow": workflow,
        "ref": ref,
        "host_ref": host_ref,
        "head_sha": head_sha,
        "min_run_id": min_run_id,
        "spec": raw,
    }


def build_targets(args):
    targets = []
    for spec in args.target:
        targets.append(parse_target_arg(spec))

    if not targets:
        if args.run_id is not None:
            targets.append(
                {
                    "kind": TARGET_KIND_RUN_ID,
                    "run_id": args.run_id,
                    "spec": f"run-id={args.run_id}",
                }
            )
        else:
            workflow = args.workflow or "validation-lab"
            targets.append(
                {
                    "kind": TARGET_KIND_WORKFLOW,
                    "workflow": workflow,
                    "ref": args.ref,
                    "host_ref": args.host_ref,
                    "head_sha": args.head_sha,
                    "min_run_id": args.min_run_id,
                    "spec": ",".join(
                        part
                        for part in (
                            f"workflow={workflow}",
                            f"ref={args.ref}",
                            f"host-ref={args.host_ref}" if args.host_ref else "",
                            f"head-sha={args.head_sha}" if args.head_sha else "",
                            f"min-run-id={args.min_run_id}" if args.min_run_id else "",
                        )
                        if part
                    ),
                }
            )
    return targets


def _default_gh_dir(kind):
    home = Path.home()
    if kind == "config":
        base = Path(os.environ.get("XDG_CONFIG_HOME", home / ".config"))
    elif kind == "cache":
        base = Path(os.environ.get("XDG_CACHE_HOME", home / ".cache"))
    else:
        return None
    return base / "gh"


def _ensure_writable_dir(path_like):
    path = Path(path_like)
    try:
        path.mkdir(parents=True, exist_ok=True)
    except OSError:
        return False
    return os.access(path, os.W_OK)


def _is_readable_dir(path_like):
    path = Path(path_like)
    return path.is_dir() and os.access(path, os.R_OK | os.X_OK)


def _ensure_config_dir(env, var):
    current_value = env.get(var)
    if current_value:
        candidate = Path(current_value)
        if _is_readable_dir(candidate) or _ensure_writable_dir(candidate):
            env[var] = str(candidate)
            return

    default_path = _default_gh_dir("config")
    if default_path and (_is_readable_dir(default_path) or _ensure_writable_dir(default_path)):
        env[var] = str(default_path)
        return

    env[var] = tempfile.mkdtemp(prefix=f"gh-{var.lower()}-")


def _ensure_env_dir(env, var, kind):
    current_value = env.get(var)
    if current_value:
        candidate = Path(current_value)
        if _ensure_writable_dir(candidate):
            env[var] = str(candidate)
            return

    default_path = _default_gh_dir(kind)
    if default_path and _ensure_writable_dir(default_path):
        env[var] = str(default_path)
        return

    env[var] = tempfile.mkdtemp(prefix=f"gh-{var.lower()}-")


def _prepare_gh_env():
    global _GH_ENV
    if _GH_ENV is not None:
        return _GH_ENV
    env = os.environ.copy()
    _ensure_config_dir(env, "GH_CONFIG_DIR")
    _ensure_env_dir(env, "GH_CACHE_DIR", "cache")
    _GH_ENV = env
    return env


def _format_gh_error(cmd, err):
    stdout = (err.stdout or "").strip()
    stderr = (err.stderr or "").strip()
    parts = [f"GitHub CLI command failed: {' '.join(cmd)}"]
    if stdout:
        parts.append(f"stdout: {stdout}")
    if stderr:
        parts.append(f"stderr: {stderr}")
    return "\n".join(parts)


def gh_text(args, repo=None):
    cmd = ["gh"]
    if repo and (not args or args[0] != "api"):
        cmd.extend(["-R", repo])
    cmd.extend(args)
    try:
        proc = subprocess.run(
            cmd,
            check=True,
            capture_output=True,
            text=True,
            env=_prepare_gh_env(),
        )
    except FileNotFoundError as err:
        raise GhCommandError("`gh` command not found") from err
    except subprocess.CalledProcessError as err:
        raise GhCommandError(_format_gh_error(cmd, err)) from err
    return proc.stdout


def gh_json(args, repo=None):
    raw = gh_text(args, repo=repo).strip()
    if not raw:
        return None
    try:
        return json.loads(raw)
    except json.JSONDecodeError as err:
        raise GhCommandError(f"Failed to parse JSON from gh output for {' '.join(args)}") from err


def gh_download(args, repo=None):
    cmd = ["gh"]
    if repo and (not args or args[0] != "api"):
        cmd.extend(["-R", repo])
    cmd.extend(args)
    try:
        subprocess.run(
            cmd,
            check=True,
            capture_output=True,
            text=True,
            env=_prepare_gh_env(),
        )
    except FileNotFoundError as err:
        raise GhCommandError("`gh` command not found") from err
    except subprocess.CalledProcessError as err:
        raise GhCommandError(_format_gh_error(cmd, err)) from err


def gh_bytes(args, repo=None):
    cmd = ["gh"]
    if repo and (not args or args[0] != "api"):
        cmd.extend(["-R", repo])
    cmd.extend(args)
    try:
        proc = subprocess.run(
            cmd,
            check=True,
            capture_output=True,
            env=_prepare_gh_env(),
        )
    except FileNotFoundError as err:
        raise GhCommandError("`gh` command not found") from err
    except subprocess.CalledProcessError as err:
        raise GhCommandError(_format_gh_error(cmd, err)) from err
    return proc.stdout


def _strip_export_prefix(value):
    value = value.strip()
    if value.startswith("export "):
        return value.split(" ", 1)[1].strip()
    return value


def _candidate_env_paths():
    cwd = Path.cwd()
    script_dir = Path(__file__).resolve().parent
    seen = set()
    for base in (cwd, *cwd.parents, script_dir, *script_dir.parents):
        candidate = base / ".env.local"
        if candidate in seen:
            continue
        seen.add(candidate)
        yield candidate


def _parse_env_assignment(line, name):
    stripped = line.strip()
    if not stripped or stripped.startswith("#"):
        return None
    if stripped.startswith("export "):
        stripped = stripped[len("export ") :].strip()
    prefix = f"{name}="
    if not stripped.startswith(prefix):
        return None
    raw = stripped[len(prefix) :].strip()
    if raw and raw[0] in {'"', "'"} and raw[-1:] == raw[0]:
        raw = raw[1:-1]
    return raw.strip()


def _split_gemini_key_list(raw_value):
    return [token.strip() for token in re.split(r"[,\s]+", raw_value) if token.strip()]


def _load_gemini_api_keys():
    env_keys = os.getenv("GEMINI_API_KEYS")
    keys = []
    if env_keys:
        keys.extend(_split_gemini_key_list(env_keys))
    else:
        single = os.getenv("GEMINI_API_KEY")
        if single and single.strip():
            keys.append(_strip_export_prefix(single))

    if not keys:
        for candidate in _candidate_env_paths():
            if not candidate.exists():
                continue
            try:
                lines = candidate.read_text(encoding="utf-8").splitlines()
            except OSError:
                continue
            keys_line = next((line for line in lines if _parse_env_assignment(line, "GEMINI_API_KEYS")), None)
            single_line = next((line for line in lines if _parse_env_assignment(line, "GEMINI_API_KEY")), None)
            if keys_line:
                parsed = _parse_env_assignment(keys_line, "GEMINI_API_KEYS")
                if parsed:
                    keys.extend(_split_gemini_key_list(parsed))
            elif single_line:
                parsed = _parse_env_assignment(single_line, "GEMINI_API_KEY")
                if parsed:
                    keys.append(parsed)
            if keys:
                break

    deduped = []
    seen = set()
    for key in keys:
        token = key.strip()
        if not token or token in seen:
            continue
        seen.add(token)
        deduped.append(token)

    if deduped:
        os.environ.setdefault("GEMINI_API_KEY", deduped[0])
    return deduped


def _redact_text(text):
    if not text:
        return ""
    redacted = str(text)
    for pattern, replacement in GEMINI_REDACT_PATTERNS:
        redacted = pattern.sub(replacement, redacted)
    return redacted


def _truncate_middle(text, limit):
    if limit <= 0:
        return ""
    if len(text) <= limit:
        return text
    if limit <= 12:
        return text[:limit]
    head = max(1, limit // 3)
    tail = max(1, limit - head - 12)
    return f"{text[:head]}\n... [truncated {len(text) - head - tail} chars] ...\n{text[-tail:]}"


def _validation_summary_excerpt(summary, limit=8000):
    if summary is None:
        return None
    try:
        rendered = json.dumps(summary, indent=2, sort_keys=True, default=str)
    except TypeError:
        rendered = str(summary)
    redacted = _redact_text(rendered)
    return _truncate_middle(redacted, limit)


def _focused_validation_summary_text(summary, limit=5000):
    summary_root = _dict_or_empty(summary)
    if not summary_root:
        return None

    context = _derive_validation_mode_context(summary_root)
    lanes = _list_or_empty(summary_root.get("lanes"))
    rendered = {
        "selection": {
            "profile": context.get("profile_raw") or context.get("profile"),
            "lane_set": context.get("lane_set"),
            "explicit_lanes": context.get("explicit_lanes"),
            "baseline_required": context.get("baseline_required"),
        },
        "summary": {
            "failed_lane_count": context.get("failed_lane_count"),
            "failure_structure": context.get("failure_structure"),
            "recommended_follow_up": context.get("recommended_follow_up"),
            "first_blocker": context.get("first_blocker"),
            "candidate_next_slices": context.get("candidate_next_slices"),
        },
        "jobs": context.get("job_results"),
        "lanes": [],
    }
    for lane in lanes[:5]:
        if not isinstance(lane, dict):
            continue
        rendered["lanes"].append(
            {
                "lane_id": lane.get("lane_id"),
                "outcome": lane.get("outcome"),
                "primary_signal": lane.get("primary_signal"),
                "error_lines": _list_or_empty(lane.get("error_lines"))[:4],
                "tail_excerpt": _list_or_empty(lane.get("tail_excerpt"))[-6:],
            }
        )

    try:
        text = json.dumps(rendered, indent=2, sort_keys=True, default=str)
    except TypeError:
        text = str(rendered)
    return _truncate_middle(_redact_text(text), limit)


def _normalize_validation_profile(raw_profile):
    profile = str(raw_profile or "").strip().lower()
    if not profile:
        return None
    if profile in {"targeted", "frontier", "checkpoint"}:
        return profile
    if profile in VALIDATION_CHECKPOINT_PROFILES:
        return "checkpoint"
    return None


def _dict_or_empty(value):
    return value if isinstance(value, dict) else {}


def _list_or_empty(value):
    return value if isinstance(value, list) else []


def _derive_validation_mode_context(validation_summary, *, run_view=None, failed_jobs=None):
    summary_root = _dict_or_empty(validation_summary)
    selection = _dict_or_empty(summary_root.get("selection"))
    summary_branch = _dict_or_empty(summary_root.get("summary"))
    first_failure = _dict_or_empty(summary_branch.get("first_failure"))
    candidate_next_slices = _list_or_empty(summary_branch.get("candidate_next_slices"))
    jobs = _dict_or_empty(summary_root.get("jobs"))

    raw_profile = selection.get("profile")
    normalized_profile = _normalize_validation_profile(raw_profile)
    failed_lane_count = int(summary_branch.get("failed_lane_count") or 0)

    non_cancelled_jobs = [
        job
        for job in (failed_jobs or [])
        if str(job.get("conclusion") or "") != "cancelled"
    ]
    direct_non_meta_jobs = [
        job for job in non_cancelled_jobs if not _is_meta_job_name(job.get("name"))
    ]
    cancelled_job_count = sum(
        1 for job in (failed_jobs or []) if str(job.get("conclusion") or "") == "cancelled"
    )

    if failed_lane_count > 1:
        if normalized_profile == "frontier" or len(candidate_next_slices) > 1:
            failure_structure = "independent"
        else:
            failure_structure = "cascading"
    elif failed_lane_count == 1:
        failure_structure = "single_blocker"
    elif len(direct_non_meta_jobs) > 1:
        failure_structure = "independent" if normalized_profile == "frontier" else "cascading"
    elif len(direct_non_meta_jobs) == 1:
        failure_structure = "cascading" if cancelled_job_count or len(non_cancelled_jobs) > 1 else "single_blocker"
    elif len(non_cancelled_jobs) > 1:
        failure_structure = "cascading"
    elif len(non_cancelled_jobs) == 1:
        failure_structure = "single_blocker"
    elif cancelled_job_count:
        failure_structure = "cascading"
    else:
        failure_structure = "unknown"

    if normalized_profile == "targeted":
        recommended_follow_up = "targeted_repair"
    elif normalized_profile == "frontier":
        recommended_follow_up = "frontier_harvest"
    elif normalized_profile == "checkpoint":
        recommended_follow_up = "checkpoint_review"
    elif len(direct_non_meta_jobs) == 1:
        recommended_follow_up = "targeted_repair"
    elif failure_structure == "single_blocker":
        recommended_follow_up = "targeted_repair"
    elif failure_structure in {"independent", "cascading"}:
        recommended_follow_up = "checkpoint_review"
    else:
        recommended_follow_up = "manual_diagnosis"

    first_blocker = None
    lane_id = str(first_failure.get("lane_id") or "").strip()
    signal = str(first_failure.get("signal") or "").strip()
    if lane_id or signal:
        first_blocker = {"lane_id": lane_id or None, "signal": signal or None}
    elif direct_non_meta_jobs:
        first_job = direct_non_meta_jobs[0]
        first_blocker = {
            "job_id": first_job.get("id"),
            "job_name": first_job.get("name"),
            "signal": None,
        }
    elif non_cancelled_jobs:
        first_job = non_cancelled_jobs[0]
        first_blocker = {
            "job_id": first_job.get("id"),
            "job_name": first_job.get("name"),
            "signal": None,
        }

    summarized_candidates = []
    for candidate in candidate_next_slices[:5]:
        if not isinstance(candidate, dict):
            continue
        candidate_lane = str(candidate.get("lane_id") or "").strip()
        candidate_signal = str(candidate.get("signal") or "").strip()
        if not candidate_lane and not candidate_signal:
            continue
        summarized_candidates.append(
            {
                "lane_id": candidate_lane or None,
                "signal": candidate_signal or None,
            }
        )

    return {
        "profile": normalized_profile,
        "profile_raw": str(raw_profile or "").strip() or None,
        "lane_set": str(selection.get("lane_set") or "").strip() or None,
        "explicit_lanes": [
            str(item).strip()
            for item in _list_or_empty(selection.get("explicit_lanes"))
            if str(item).strip()
        ],
        "baseline_required": bool(selection.get("baseline_required")),
        "failed_lane_count": failed_lane_count,
        "candidate_next_slice_count": len(candidate_next_slices),
        "failure_structure": failure_structure,
        "recommended_follow_up": recommended_follow_up,
        "preferred_signal_source": "validation_summary" if summary_root else "logs",
        "first_blocker": first_blocker,
        "candidate_next_slices": summarized_candidates,
        "job_results": {
            "smoke_gate": str(_dict_or_empty(jobs.get("smoke_gate")).get("result") or "").strip() or None,
            "downstream_lanes": str(_dict_or_empty(jobs.get("downstream_lanes")).get("result") or "").strip() or None,
            "artifact": str(_dict_or_empty(jobs.get("artifact")).get("result") or "").strip() or None,
        },
    }


def _validation_summary_has_actionable_detail(validation_summary):
    context = _derive_validation_mode_context(validation_summary)
    first_blocker = _dict_or_empty(context.get("first_blocker"))
    if first_blocker.get("lane_id") or first_blocker.get("job_name") or first_blocker.get("signal"):
        return True
    return bool(context.get("candidate_next_slices"))


def _build_diagnosis_status(*, actions, gemini_diagnosis, gemini_error, gemini_disabled):
    action_list = actions or []
    if "diagnose_run_failure" not in action_list:
        return {
            "state": "not_needed",
            "summary": "Diagnosis is not needed because the run is not in terminal failure.",
        }
    if gemini_disabled:
        return {
            "state": "disabled",
            "summary": "Gemini diagnosis was disabled by the caller.",
        }
    if gemini_diagnosis:
        return {
            "state": "available",
            "summary": "Gemini diagnosis is available.",
        }
    if _is_skipped_gemini_error(gemini_error):
        return {
            "state": "skipped",
            "summary": _truncate_middle(str(gemini_error), 240),
        }
    if gemini_error:
        return {
            "state": "unavailable",
            "summary": f"Gemini diagnosis unavailable: {_truncate_middle(str(gemini_error), 240)}",
        }
    return {
        "state": "pending",
        "summary": "Gemini diagnosis has not completed yet.",
    }


def _prepare_gemini_log_sections(log_sources):
    if not log_sources:
        return [], 0, False

    sections = []
    total_chars = 0
    truncated = False
    per_source_budget = max(8000, GEMINI_LOG_CHAR_BUDGET // max(1, len(log_sources)))
    for source in log_sources:
        if total_chars >= GEMINI_LOG_CHAR_BUDGET:
            truncated = True
            break
        source_text = _redact_text(source.get("text") or "")
        source_truncated = False
        if len(source_text) > per_source_budget:
            source_text = _excerpt_around_failure(source_text, max_lines=220, context_lines=30)
            source_truncated = True
        source_text = _truncate_middle(source_text, per_source_budget)
        if len(source_text) > per_source_budget:
            source_truncated = True
        if total_chars + len(source_text) > GEMINI_LOG_CHAR_BUDGET:
            remaining = max(1, GEMINI_LOG_CHAR_BUDGET - total_chars)
            source_text = _truncate_middle(source_text, remaining)
            source_truncated = True
        total_chars += len(source_text)
        truncated = truncated or source_truncated
        sections.append(
            {
                "kind": source["kind"],
                "label": source["label"],
                "job_id": source.get("job_id"),
                "job_name": source.get("job_name"),
                "retrieved_via": source.get("retrieved_via"),
                "text": source_text,
                "chars": len(source_text),
            }
        )
    return sections, total_chars, truncated


def command_text(cmd):
    try:
        proc = subprocess.run(cmd, check=True, capture_output=True, text=True)
    except (FileNotFoundError, subprocess.CalledProcessError):
        return None
    output = (proc.stdout or "").strip()
    return output or None


def parse_repo_from_remote_url(remote_url):
    if not remote_url:
        return None
    if remote_url.startswith("git@"):
        _, _, path = remote_url.partition(":")
    else:
        path = urlparse(remote_url).path
    parts = [part for part in path.split("/") if part]
    if len(parts) < 2:
        return None
    owner = parts[-2]
    repo = parts[-1]
    if repo.endswith(".git"):
        repo = repo[:-4]
    return f"{owner}/{repo}" if owner and repo else None


def detect_repo():
    env_repo = (os.environ.get("GH_WORKFLOW_RUN_WATCH_REPO") or os.environ.get("GH_REPO") or "").strip()
    if env_repo:
        return env_repo

    for remote_name in ("origin", "upstream"):
        remote_url = command_text(["git", "config", "--get", f"remote.{remote_name}.url"])
        repo = parse_repo_from_remote_url(remote_url)
        if repo:
            return repo

    try:
        gh_repo = gh_json(["repo", "view", "--json", "nameWithOwner"])
    except GhCommandError as err:
        raise GhCommandError(
            "Unable to determine OWNER/REPO. Set GH_WORKFLOW_RUN_WATCH_REPO, run inside a checkout, or pass --repo."
        ) from err

    if isinstance(gh_repo, dict):
        name_with_owner = str(gh_repo.get("nameWithOwner") or "").strip()
        if name_with_owner:
            return name_with_owner

    return None


def detect_ref(ref_value):
    if ref_value != "auto":
        return ref_value
    branch = command_text(["git", "branch", "--show-current"])
    if branch:
        return branch
    sha = command_text(["git", "rev-parse", "HEAD"])
    if sha:
        return sha
    raise GhCommandError("Unable to infer current branch or HEAD; pass --ref explicitly")


def is_sha_like(value):
    if not value:
        return False
    return all(ch in "0123456789abcdefABCDEF" for ch in value) and len(value) >= 7


def list_workflow_runs(repo, workflow, ref, expected_head_sha=None, minimum_run_id=None, host_ref=None):
    fields = "databaseId,displayTitle,event,headBranch,headSha,name,number,status,conclusion,url,workflowName,createdAt,updatedAt"
    cmd = ["run", "list", "--workflow", workflow, "--limit", "30", "--json", fields]
    branch_filter = host_ref if host_ref is not None else ref
    if branch_filter and not is_sha_like(branch_filter):
        cmd.extend(["--branch", branch_filter])
    data = gh_json(cmd, repo=repo)
    if data is None:
        return []
    if not isinstance(data, list):
        raise GhCommandError("Unexpected payload from `gh run list`")
    matches = []
    for run in data:
        if not isinstance(run, dict):
            continue
        head_branch = str(run.get("headBranch") or "")
        run_head_sha = str(run.get("headSha") or "")
        if ref:
            if is_sha_like(ref):
                if not run_head_sha.startswith(ref):
                    continue
            elif branch_filter and head_branch != branch_filter:
                continue
        if expected_head_sha and not run_head_sha.startswith(str(expected_head_sha)):
            continue
        run_id = int(run.get("databaseId") or 0)
        if minimum_run_id is not None and run_id < int(minimum_run_id):
            continue
        matches.append(run)
    matches.sort(key=lambda run: int(run.get("databaseId") or 0), reverse=True)
    return matches


def _followed_run_relist_interval_seconds(poll_seconds):
    return max(FOLLOWED_RUN_RELIST_MIN_SECONDS, int(max(1, poll_seconds)) * FOLLOWED_RUN_RELIST_MULTIPLIER)


def _host_mismatch_recheck_interval_seconds(poll_seconds):
    return max(HOST_MISMATCH_RECHECK_MIN_SECONDS, int(max(1, poll_seconds)) * FOLLOWED_RUN_RELIST_MULTIPLIER)


def view_run(repo, run_id):
    fields = "databaseId,displayTitle,event,headBranch,headSha,name,number,status,conclusion,url,workflowName,createdAt,updatedAt,jobs"
    data = gh_json(["run", "view", str(run_id), "--json", fields], repo=repo)
    if not isinstance(data, dict):
        raise GhCommandError("Unexpected payload from `gh run view`")
    return data


def load_validation_summary(repo, run_id):
    with tempfile.TemporaryDirectory(prefix="gh-run-download-") as tmpdir:
        try:
            gh_download(
                ["run", "download", str(run_id), "--name", "validation-summary", "--dir", tmpdir],
                repo=repo,
            )
        except GhCommandError:
            return None

        candidates = sorted(Path(tmpdir).rglob("validation-summary.json"))
        if not candidates:
            return None
        try:
            payload = json.loads(candidates[0].read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return None
        return payload if isinstance(payload, dict) else None


def _strip_ansi(text):
    return ANSI_ESCAPE_RE.sub("", str(text or ""))


def _parse_gh_log_entries(text):
    entries = []
    for raw_line in str(text or "").splitlines():
        cleaned = _strip_ansi(raw_line.rstrip("\n"))
        parts = cleaned.split("\t", 3)
        if len(parts) == 4 and GH_LOG_TIMESTAMP_RE.match(parts[2].strip()):
            entries.append(
                {
                    "job": parts[0].strip(),
                    "step": parts[1].strip(),
                    "timestamp": parts[2].strip(),
                    "message": parts[3].strip(),
                    "raw": cleaned,
                }
            )
            continue
        entries.append(
            {
                "job": "",
                "step": "",
                "timestamp": "",
                "message": cleaned.strip(),
                "raw": cleaned,
            }
        )
    return entries


def _line_is_noise(cleaned_line):
    lowered = str(cleaned_line or "").strip().lower()
    if not lowered:
        return True
    return any(pattern.search(lowered) for pattern in NOISY_FAILURE_PATTERNS)


def _line_has_high_signal(cleaned_line):
    if _line_is_noise(cleaned_line):
        return False
    text = str(cleaned_line or "").strip()
    return any(pattern.search(text) for pattern in HIGH_SIGNAL_FAILURE_PATTERNS)


def _line_is_failure_like(cleaned_line):
    if _line_is_noise(cleaned_line):
        return False
    lowered = str(cleaned_line or "").strip().lower()
    if not lowered:
        return False
    return any(marker in lowered for marker in LOG_FAILURE_MARKERS)


def _render_entry_excerpt(entries, start, end):
    return "\n".join(entry["raw"] for entry in entries[start:end] if entry.get("raw"))


def _failure_line_score(cleaned_line):
    text = str(cleaned_line or "").strip()
    if not text or _line_is_noise(text):
        return 0
    if ASSERTION_LINE_RE.search(text):
        return 100
    if PANIC_LINE_RE.search(text):
        return 95
    if EXACT_TEST_FAILURE_RE.search(text):
        return 90
    if NEXTEST_FAILURE_RE.search(text):
        return 85
    lowered = text.lower()
    if "test result: failed" in lowered:
        return 70
    if re.search(r"error:\s*test (?:run )?failed\b", text, re.IGNORECASE):
        return 60
    if "traceback" in lowered:
        return 50
    if _line_is_failure_like(text):
        return 10
    return 0


def _find_failure_index(lines):
    best_index = None
    best_score = 0
    for idx in range(len(lines) - 1, -1, -1):
        cleaned = _strip_ansi(lines[idx]).strip()
        score = _failure_line_score(cleaned)
        if score > best_score:
            best_index = idx
            best_score = score
    return best_index


def _excerpt_around_failure(text, *, max_lines=180, context_lines=30):
    entries = _parse_gh_log_entries(text)
    if not entries:
        return ""

    failure_index = _find_failure_index([entry["raw"] for entry in entries])
    if failure_index is None:
        return _truncate_middle(text, max_lines * 120)

    before_context = 1 if _line_has_high_signal(entries[failure_index]["raw"]) else context_lines
    start = max(0, failure_index - before_context)
    end = min(len(entries), failure_index + context_lines + 1)
    excerpt = _render_entry_excerpt(entries, start, end)
    return _truncate_middle(excerpt, max_lines * 120)


def _excerpt_around_terms(text, terms, *, max_lines=180, context_lines=24):
    entries = _parse_gh_log_entries(text)
    if not entries:
        return ""
    lowered_terms = [str(term).strip().lower() for term in terms if str(term).strip()]
    for term in lowered_terms:
        for idx, entry in enumerate(entries):
            message = entry.get("message") or ""
            if term and term in message.lower():
                start = max(0, idx - context_lines)
                end = min(len(entries), idx + context_lines + 1)
                excerpt = _render_entry_excerpt(entries, start, end)
                return _truncate_middle(excerpt, max_lines * 120)
    return _excerpt_around_failure(text, max_lines=max_lines, context_lines=context_lines)


def _collect_failure_highlight_lines(text, *, limit=10):
    highlights = []
    fallback = []
    for entry in _parse_gh_log_entries(text):
        message = (entry.get("message") or "").strip()
        if not message:
            continue
        if _line_has_high_signal(message):
            highlights.append(message)
            if len(highlights) >= limit:
                return highlights
            continue
        if _line_is_failure_like(message):
            fallback.append(message)
    return highlights or fallback[:limit]


def _excerpt_for_failed_step(text, step_name, *, max_lines=120, context_lines=18):
    entries = _parse_gh_log_entries(text)
    normalized_step = str(step_name or "").strip().casefold()
    if not entries or not normalized_step:
        return ""

    best_index = None
    best_score = 0
    for idx in range(len(entries) - 1, -1, -1):
        entry = entries[idx]
        if entry.get("step", "").casefold() != normalized_step:
            continue
        message = entry.get("message") or ""
        score = _failure_line_score(message)
        if score > best_score:
            best_index = idx
            best_score = score
    target_index = best_index
    if target_index is None:
        return ""
    before_context = 1 if best_score >= 60 else context_lines
    start = max(0, target_index - before_context)
    end = min(len(entries), target_index + context_lines + 1)
    excerpt = _render_entry_excerpt(entries, start, end)
    return _truncate_middle(excerpt, max_lines * 120)


def _extract_structured_failure_signals(text, *, limit=4):
    failing_tests = []
    assertions = []
    failure_locations = []
    evidence_lines = []
    seen_tests = set()
    seen_assertions = set()
    seen_locations = set()
    seen_lines = set()

    for entry in _parse_gh_log_entries(text):
        message = (entry.get("message") or "").strip()
        if not message or _line_is_noise(message):
            continue

        if _line_has_high_signal(message) and message not in seen_lines:
            seen_lines.add(message)
            evidence_lines.append(message)

        match = EXACT_TEST_FAILURE_RE.search(message)
        if match:
            value = match.group(1).strip()
            if value and value not in seen_tests:
                seen_tests.add(value)
                failing_tests.append(value)

        match = NEXTEST_FAILURE_RE.search(message)
        if match:
            value = match.group(1).strip()
            if value and value not in seen_tests:
                seen_tests.add(value)
                failing_tests.append(value)

        match = PANIC_LINE_RE.search(message)
        if match:
            thread_name = match.group(1).strip()
            panic_detail = match.group(2).strip()
            if thread_name and thread_name not in seen_tests:
                seen_tests.add(thread_name)
                failing_tests.append(thread_name)
            for path, line in _extract_code_candidates_from_text(panic_detail):
                location = f"{path}:{line}" if line else path
                if location and location not in seen_locations:
                    seen_locations.add(location)
                    failure_locations.append(location)

        match = ASSERTION_LINE_RE.search(message)
        if match:
            value = match.group(1).strip()
            if value and value not in seen_assertions:
                seen_assertions.add(value)
                assertions.append(value)

        if len(failing_tests) >= limit and len(assertions) >= limit and len(failure_locations) >= limit:
            break

    return {
        "failing_tests": failing_tests[:limit],
        "assertions": assertions[:limit],
        "failure_locations": failure_locations[:limit],
        "evidence_lines": evidence_lines[: max(limit * 2, 6)],
    }


def _signals_have_actionable_detail(signals):
    if not isinstance(signals, dict):
        return False
    return bool(
        signals.get("failing_tests")
        or signals.get("assertions")
        or signals.get("failure_locations")
        or signals.get("evidence_lines")
    )


def _collect_structured_failure_signals(log_sources):
    combined = {
        "failing_tests": [],
        "assertions": [],
        "failure_locations": [],
        "evidence_lines": [],
    }
    seen = {key: set() for key in combined}
    for source in log_sources:
        if source.get("kind") != "failed_job_log":
            continue
        signals = _extract_structured_failure_signals(source.get("text") or "")
        for key in combined:
            for value in signals.get(key) or []:
                if value in seen[key]:
                    continue
                seen[key].add(value)
                combined[key].append(value)
    return combined


def _extract_referenced_paths(text, *, limit=5):
    paths = []
    seen = set()
    for raw_line in str(text or "").splitlines():
        for pattern in CODE_PATH_PATTERNS:
            for match in pattern.finditer(raw_line):
                groups = match.groups()
                if not groups:
                    continue
                path = str(groups[0] or "").strip()
                if not path:
                    continue
                if pattern is CODE_PATH_PATTERNS[-1]:
                    path = path.split("::", 1)[0]
                if path in seen:
                    continue
                seen.add(path)
                paths.append(path)
                if len(paths) >= limit:
                    return paths
    return paths


def _decode_log_zip(payload):
    chunks = []
    try:
        with zipfile.ZipFile(io.BytesIO(payload)) as archive:
            for name in sorted(archive.namelist()):
                if name.endswith("/"):
                    continue
                try:
                    raw = archive.read(name)
                except KeyError:
                    continue
                chunks.append((name, raw.decode("utf-8", errors="replace")))
    except zipfile.BadZipFile:
        return []
    return chunks


def _load_job_log_text(repo, job_id):
    try:
        text = gh_text(["run", "view", "--job", str(job_id), "--log"], repo=repo)
        return text.strip(), "gh run view --job --log"
    except GhCommandError:
        pass

    try:
        payload = gh_bytes(
            ["api", f"/repos/{repo}/actions/jobs/{job_id}/logs"],
            repo=repo,
        )
    except GhCommandError:
        return "", ""

    chunks = _decode_log_zip(payload)
    if not chunks:
        return "", ""
    if len(chunks) == 1:
        return chunks[0][1].strip(), f"gh api /repos/{repo}/actions/jobs/{job_id}/logs"
    combined = []
    for name, text in chunks:
        combined.append(f"=== {name} ===")
        combined.append(text.strip())
    return "\n".join(combined).strip(), f"gh api /repos/{repo}/actions/jobs/{job_id}/logs"


def _failed_step_names(job):
    names = []
    for step in job.get("steps") or []:
        if not isinstance(step, dict):
            continue
        conclusion = str(step.get("conclusion") or "")
        if conclusion not in FAILED_CONCLUSIONS:
            continue
        name = str(step.get("name") or "").strip()
        if not name:
            continue
        if name not in names:
            names.append(name)
    return names


def _failed_jobs_from_run_view(run_view):
    failed_jobs = []
    jobs = run_view.get("jobs") or []
    if not isinstance(jobs, list):
        return failed_jobs
    for job in jobs:
        if not isinstance(job, dict):
            continue
        conclusion = str(job.get("conclusion") or "")
        if conclusion not in FAILED_CONCLUSIONS:
            continue
        failed_jobs.append(
            {
                "id": int(job.get("databaseId") or 0),
                "name": str(job.get("name") or ""),
                "status": str(job.get("status") or ""),
                "conclusion": conclusion,
                "url": str(job.get("url") or ""),
                "failed_steps": _failed_step_names(job),
            }
        )
    return failed_jobs


def _is_meta_job_name(name):
    lowered = str(name or "").strip().lower()
    if not lowered:
        return False
    return any(marker in lowered for marker in META_JOB_NAME_MARKERS)


def _job_priority(job):
    conclusion = str(job.get("conclusion") or "")
    if conclusion == "cancelled":
        conclusion_rank = 2
    elif conclusion in {"failure", "timed_out", "action_required", "startup_failure"}:
        conclusion_rank = 0
    else:
        conclusion_rank = 1
    meta_rank = 1 if _is_meta_job_name(job.get("name")) else 0
    no_steps_rank = 1 if not job.get("failed_steps") else 0
    job_id = int(job.get("id") or 0)
    return (conclusion_rank, meta_rank, no_steps_rank, job_id)


def _select_jobs_for_gemini_logs(failed_jobs):
    ordered = sorted(failed_jobs, key=_job_priority)
    direct = [job for job in ordered if job.get("conclusion") != "cancelled" and not _is_meta_job_name(job.get("name"))]
    meta = [job for job in ordered if job.get("conclusion") != "cancelled" and _is_meta_job_name(job.get("name"))]
    cancelled = [job for job in ordered if job.get("conclusion") == "cancelled" and not _is_meta_job_name(job.get("name"))]

    selected = []
    selected.extend(direct[:2])
    if meta:
        selected.append(meta[0])
    if not selected and cancelled:
        selected.append(cancelled[0])

    deduped = []
    seen = set()
    for job in selected:
        job_id = int(job.get("id") or 0)
        if job_id <= 0 or job_id in seen:
            continue
        seen.add(job_id)
        deduped.append(job)
    return deduped


def _render_failed_jobs_overview(failed_jobs, selected_job_ids):
    if not failed_jobs:
        return ""
    ordered = sorted(failed_jobs, key=_job_priority)
    lines = [
        "Observed non-green jobs, ranked by likely causality.",
        json.dumps(
            {
                "failure_count": sum(1 for job in ordered if str(job.get("conclusion") or "") != "cancelled"),
                "cancelled_count": sum(1 for job in ordered if str(job.get("conclusion") or "") == "cancelled"),
                "selected_job_ids": sorted(int(job_id) for job_id in selected_job_ids if int(job_id) > 0),
            },
            sort_keys=True,
        ),
        "Detailed logs were included only for jobs marked selected_for_detailed_logs=true.",
    ]
    for job in ordered[:GEMINI_FAILURE_OVERVIEW_MAX_JOBS]:
        steps = job.get("failed_steps") or []
        step_suffix = f" failed_steps={steps[:2]}" if steps else ""
        lines.append(
            "- "
            + json.dumps(
                {
                    "job_id": job.get("id"),
                    "job_name": job.get("name"),
                    "conclusion": job.get("conclusion"),
                    "selected_for_detailed_logs": int(job.get("id") or 0) in selected_job_ids,
                },
                sort_keys=True,
            )
            + step_suffix
        )
    omitted = max(0, len(ordered) - GEMINI_FAILURE_OVERVIEW_MAX_JOBS)
    if omitted:
        lines.append(f"- omitted_jobs={omitted}")
    return "\n".join(lines)


def _focus_job_log_text(job, text):
    failed_steps = job.get("failed_steps") or []
    is_meta_job = _is_meta_job_name(job.get("name"))
    char_budget = GEMINI_META_JOB_CHAR_BUDGET if is_meta_job else GEMINI_PRIMARY_JOB_CHAR_BUDGET
    if str(job.get("conclusion") or "") == "cancelled":
        char_budget = min(char_budget, GEMINI_SUPPORTING_JOB_CHAR_BUDGET)
    sections = []
    if failed_steps:
        seen = set()
        for step_name in failed_steps[:2]:
            excerpt = _excerpt_for_failed_step(
                text,
                step_name,
                max_lines=80 if is_meta_job else 120,
                context_lines=12 if is_meta_job else 18,
            )
            if not excerpt:
                excerpt = _excerpt_around_terms(
                    text,
                    [step_name],
                    max_lines=80 if is_meta_job else 120,
                    context_lines=12 if is_meta_job else 18,
                )
            if not excerpt:
                continue
            if not _signals_have_actionable_detail(_extract_structured_failure_signals(excerpt)):
                continue
            key = excerpt.strip()
            if not key or key in seen:
                continue
            seen.add(key)
            sections.append(f"== Failed step: {step_name} ==\n{excerpt}")

    failure_excerpt = _excerpt_around_failure(
        text,
        max_lines=90 if is_meta_job else 140,
        context_lines=12 if is_meta_job else 22,
    )
    if failure_excerpt:
        sections.append(f"== Failure excerpt ==\n{failure_excerpt}")

    signals = _extract_structured_failure_signals(text)
    signal_lines = []
    if signals.get("failing_tests"):
        signal_lines.append("failing_tests=" + json.dumps(signals["failing_tests"], ensure_ascii=True))
    if signals.get("assertions"):
        signal_lines.append("assertions=" + json.dumps(signals["assertions"], ensure_ascii=True))
    if signals.get("failure_locations"):
        signal_lines.append("failure_locations=" + json.dumps(signals["failure_locations"], ensure_ascii=True))
    if signal_lines:
        sections.append("== Extracted failure signals ==\n" + "\n".join(signal_lines))

    highlights = _collect_failure_highlight_lines(text, limit=5 if is_meta_job else 8)
    if highlights:
        sections.append("== Failure highlights ==\n" + "\n".join(f"- {line}" for line in highlights))

    if not sections:
        return _excerpt_around_failure(text, max_lines=160, context_lines=26)

    deduped_sections = []
    seen_section_bodies = set()
    for section in sections:
        body = section.split("\n", 1)[1] if "\n" in section else section
        key = body.strip()
        if not key or key in seen_section_bodies:
            continue
        seen_section_bodies.add(key)
        deduped_sections.append(section)
    return _truncate_middle("\n\n".join(deduped_sections), char_budget)


def _collect_triage_hints(run_view, log_sources):
    failed_jobs = _failed_jobs_from_run_view(run_view)
    selected_job_ids = {
        int(source.get("job_id") or 0)
        for source in log_sources
        if source.get("kind") == "failed_job_log" and int(source.get("job_id") or 0) > 0
    }
    ordered_jobs = sorted(failed_jobs, key=_job_priority)
    primary_job = None
    for job in ordered_jobs:
        if int(job.get("id") or 0) in selected_job_ids and not _is_meta_job_name(job.get("name")):
            primary_job = job
            break
    if primary_job is None:
        for job in ordered_jobs:
            if int(job.get("id") or 0) in selected_job_ids:
                primary_job = job
                break

    cancelled_jobs = [job for job in ordered_jobs if str(job.get("conclusion") or "") == "cancelled"]
    supporting_jobs = [
        job
        for job in ordered_jobs
        if int(job.get("id") or 0) in selected_job_ids and primary_job is not None and int(job.get("id") or 0) != int(primary_job.get("id") or 0)
    ]

    highlights = []
    referenced_paths = []
    seen_highlights = set()
    seen_paths = set()
    for source in log_sources:
        if source.get("kind") != "failed_job_log":
            continue
        for line in _collect_failure_highlight_lines(source.get("text") or "", limit=4):
            key = line.strip()
            if not key or key in seen_highlights:
                continue
            seen_highlights.add(key)
            highlights.append(line)
            if len(highlights) >= 8:
                break
        for path in _extract_referenced_paths(source.get("text") or "", limit=4):
            if path in seen_paths:
                continue
            seen_paths.add(path)
            referenced_paths.append(path)
            if len(referenced_paths) >= 6:
                break
        if len(highlights) >= 8 and len(referenced_paths) >= 6:
            break

    structured_signals = _collect_structured_failure_signals(log_sources)

    return {
        "primary_job": {
            "job_id": primary_job.get("id") if primary_job else None,
            "job_name": primary_job.get("name") if primary_job else None,
            "conclusion": primary_job.get("conclusion") if primary_job else None,
            "failed_steps": (primary_job or {}).get("failed_steps") or [],
        },
        "supporting_jobs": [
            {
                "job_id": job.get("id"),
                "job_name": job.get("name"),
                "conclusion": job.get("conclusion"),
                "failed_steps": job.get("failed_steps") or [],
            }
            for job in supporting_jobs[:3]
        ],
        "cancelled_job_count": len(cancelled_jobs),
        "cancelled_job_examples": [job.get("name") for job in cancelled_jobs[:3] if job.get("name")],
        "selected_job_ids": sorted(selected_job_ids),
        "failure_highlights": highlights[:8],
        "referenced_paths": referenced_paths[:6],
        "structured_failure_signals": structured_signals,
    }


def _collect_log_sources(repo, run_view, *, validation_summary=None):
    run_id = int(run_view.get("databaseId") or 0)
    sources = []
    if run_id <= 0:
        return sources

    failed_jobs = _failed_jobs_from_run_view(run_view)
    validation_context = _derive_validation_mode_context(
        validation_summary,
        run_view=run_view,
        failed_jobs=failed_jobs,
    )
    selected_jobs = _select_jobs_for_gemini_logs(failed_jobs)
    if validation_summary and validation_context.get("profile") in {"frontier", "checkpoint"}:
        direct_jobs = [job for job in selected_jobs if not _is_meta_job_name(job.get("name"))]
        selected_jobs = direct_jobs[:1] or selected_jobs[:1]
    selected_job_ids = {int(job.get("id") or 0) for job in selected_jobs}

    overview_text = _render_failed_jobs_overview(failed_jobs, selected_job_ids)
    if overview_text:
        sources.append(
            {
                "kind": "failed_jobs_overview",
                "label": f"run {run_id} failed jobs overview",
                "job_id": None,
                "job_name": None,
                "retrieved_via": "run metadata",
                "text": overview_text,
            }
        )

    if validation_summary:
        sources.append(
            {
                "kind": "validation_summary",
                "label": "validation-summary artifact",
                "job_id": None,
                "job_name": None,
                "retrieved_via": "validation-summary artifact",
                "text": _focused_validation_summary_text(validation_summary) or "",
            }
        )

    try:
        run_failed_log = gh_text(["run", "view", str(run_id), "--log-failed"], repo=repo).strip()
    except GhCommandError:
        run_failed_log = ""
    if run_failed_log and not selected_jobs:
        sources.append(
            {
                "kind": "run_log_failed",
                "label": f"run {run_id} --log-failed",
                "job_id": None,
                "job_name": None,
                "retrieved_via": "gh run view --log-failed",
                "text": run_failed_log,
            }
        )

    for job in selected_jobs:
        text, retrieved_via = _load_job_log_text(repo, job["id"])
        if not text:
            continue
        sources.append(
            {
                "kind": "failed_job_log",
                "label": job["name"] or f"job {job['id']}",
                "job_id": job["id"],
                "job_name": job["name"],
                "retrieved_via": retrieved_via,
                "text": _focus_job_log_text(job, text),
            }
        )

    if run_failed_log and len(sources) <= 1:
        sources.append(
            {
                "kind": "run_log_failed",
                "label": f"run {run_id} --log-failed",
                "job_id": None,
                "job_name": None,
                "retrieved_via": "gh run view --log-failed",
                "text": _excerpt_around_failure(run_failed_log, max_lines=160, context_lines=24),
            }
        )

    return sources


def _resolve_repo_root():
    text = command_text(["git", "rev-parse", "--show-toplevel"])
    if not text:
        return None
    try:
        return Path(text).resolve()
    except OSError:
        return Path(text)


def _sanitize_candidate_path(raw_path):
    cleaned = raw_path.strip().strip("`'\"").rstrip(").,;:")
    if not cleaned or "://" in cleaned:
        return None
    return cleaned


def _normalize_repo_path(repo_root, candidate):
    raw = _sanitize_candidate_path(str(candidate))
    if not raw:
        return None

    path = Path(raw)
    if path.is_absolute():
        try:
            resolved = path.resolve(strict=False)
        except OSError:
            return None
        try:
            if repo_root and resolved.is_relative_to(repo_root):
                return resolved
        except AttributeError:
            if repo_root and str(resolved).startswith(str(repo_root)):
                return resolved
        if repo_root:
            parts = list(resolved.parts)
            repo_name = repo_root.name
            for idx in range(len(parts) - 1):
                if parts[idx] == repo_name and idx + 1 < len(parts) and parts[idx + 1] == repo_name:
                    suffix = parts[idx + 2 :]
                    if suffix:
                        candidate_path = (repo_root.joinpath(*suffix)).resolve(strict=False)
                        if candidate_path.exists():
                            return candidate_path
        return resolved if resolved.exists() else None

    if not repo_root:
        return None
    resolved = (repo_root / path).resolve(strict=False)
    try:
        if resolved.exists() and resolved.is_relative_to(repo_root):
            return resolved
    except AttributeError:
        if resolved.exists() and str(resolved).startswith(str(repo_root)):
            return resolved
    return None


def _read_code_excerpt(repo_root, candidate_path, *, line=None, context_lines=GEMINI_CODE_EXCERPT_CONTEXT):
    resolved = _normalize_repo_path(repo_root, candidate_path)
    if resolved is None:
        return None
    try:
        lines = resolved.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        return None

    if line is None or line <= 0:
        start = 0
        end = min(len(lines), max(1, context_lines * 4))
        anchor = None
    else:
        start = max(0, line - 1 - context_lines)
        end = min(len(lines), line + context_lines)
        anchor = line

    rendered = []
    for idx in range(start, end):
        rendered.append(f"{idx + 1:>5} {lines[idx]}")
    snippet = _truncate_middle("\n".join(rendered), 4000)
    return {
        "path": str(resolved),
        "line": anchor,
        "snippet": snippet,
        "chars": len(snippet),
    }


def _extract_code_candidates_from_text(log_text):
    candidates = []
    for raw_line in log_text.splitlines():
        for pattern in CODE_PATH_PATTERNS:
            for match in pattern.finditer(raw_line):
                groups = match.groups()
                if not groups:
                    continue
                path = groups[0]
                line = None
                if len(groups) > 1 and groups[1] and groups[1].isdigit():
                    line = int(groups[1])
                if pattern is CODE_PATH_PATTERNS[-1]:
                    # The path-only pattern ends in a test or node identifier.
                    path = path.split("::", 1)[0]
                candidates.append((path, line))
    return candidates


def _collect_code_context(repo_root, log_texts):
    if repo_root is None:
        return []

    seen = set()
    contexts = []
    total_chars = 0
    for log_text in log_texts:
        for path, line in _extract_code_candidates_from_text(log_text):
            key = (path, line)
            if key in seen:
                continue
            seen.add(key)
            excerpt = _read_code_excerpt(repo_root, path, line=line)
            if excerpt is None:
                continue
            contexts.append(excerpt)
            total_chars += len(excerpt["snippet"])
            if len(contexts) >= GEMINI_CODE_CONTEXT_MAX_FILES or total_chars >= GEMINI_CODE_CONTEXT_CHAR_BUDGET:
                return contexts
    return contexts


def _build_gemini_prompt(*, repo, run_view, validation_summary, log_sources, code_context):
    run_id = run_view.get("databaseId")
    triage_hints = _collect_triage_hints(run_view, log_sources)
    validation_context = _derive_validation_mode_context(
        validation_summary,
        run_view=run_view,
        failed_jobs=_failed_jobs_from_run_view(run_view),
    )
    metadata = {
        "repo": repo,
        "run_id": run_id,
        "run_number": run_view.get("number"),
        "workflow_name": run_view.get("workflowName") or run_view.get("name") or "",
        "head_branch": run_view.get("headBranch") or "",
        "head_sha": run_view.get("headSha") or "",
        "status": run_view.get("status") or "",
        "conclusion": run_view.get("conclusion") or "",
        "url": run_view.get("url") or "",
    }
    lines = [
        "You are diagnosing a GitHub Actions failure for the agent that invoked the watcher.",
        "Use only the evidence in this prompt. Do not invent facts or recommend broad refactors.",
        "Find the earliest causal failure, not the noisiest downstream fallout.",
        "Treat cancelled sibling jobs and meta summary jobs as secondary unless they contain the only direct evidence.",
        "When a required-results or summary job reports failures from other jobs, use it as confirmation rather than as the root cause unless no more direct failing job is present.",
        "Prefer concrete failing job or failing step evidence over workflow-level summaries.",
        "When validation mode context is present, adapt the diagnosis to that mode instead of forcing every failure into the same shape.",
        "When the evidence names an exact failing test, assertion, or source location, include those exact details instead of speaking generically.",
        "Do not claim the exact failing test is unknown if it appears in the logs or extracted failure signals.",
        "Call out uncertainty plainly when the logs only show cancellation fallout.",
        "The logs below are focused excerpts chosen to maximize causal signal rather than complete raw logs.",
        "Return JSON only, matching the response schema.",
        "",
        "## Analysis priorities",
        "1. Identify the primary failing job and step if the evidence supports it.",
        "2. Name the exact failing test, assertion, and source location when the evidence contains them.",
        "3. Separate root cause from downstream cancellations or required-results fallout.",
        "4. Decide whether the failure shape looks like a single blocker, independent blockers, or cascading fallout.",
        "5. Recommend targeted_repair, frontier_harvest, checkpoint_review, or manual_diagnosis based on the evidence.",
        "",
        "## Run metadata",
        json.dumps(metadata, indent=2, sort_keys=True),
        "",
    ]

    if validation_context.get("profile") or validation_context.get("first_blocker"):
        lines.extend(
            [
                "## Validation mode context",
                "Use this to decide whether the parent should treat the failure as targeted repair, frontier harvest, or checkpoint review.",
                "```json",
                json.dumps(validation_context, indent=2, sort_keys=True),
                "```",
                "",
                "Mode-specific guidance:",
                "- targeted: surface the first blocking seam clearly and avoid speculative extra blockers.",
                "- frontier: summarize the first blocker plus independent candidate next slices from the summary artifact when present.",
                "- checkpoint: summarize broad state without pretending all failures are independent.",
                "",
            ]
        )

    if triage_hints:
        lines.extend(
            [
                "## Heuristic triage hints",
                "These are watcher-side heuristics to help you focus. Treat them as strong hints, but override them if the logs contradict them.",
                "```json",
                json.dumps(triage_hints, indent=2, sort_keys=True),
                "```",
                "",
            ]
        )

    summary_excerpt = _focused_validation_summary_text(validation_summary)
    if summary_excerpt:
        lines.extend(
            [
                "## Validation summary",
                "```json",
                summary_excerpt,
                "```",
                "",
            ]
        )

    if log_sources:
        lines.append("## Logs")
        for source in log_sources:
            lines.extend(
                [
                    f"### {source['label']}",
                    json.dumps(
                        {
                            "kind": source["kind"],
                            "job_id": source.get("job_id"),
                            "job_name": source.get("job_name"),
                            "retrieved_via": source.get("retrieved_via"),
                            "chars": source.get("chars"),
                        },
                        sort_keys=True,
                    ),
                    "```text",
                    source["text"],
                    "```",
                    "",
                ]
            )

    if code_context:
        lines.append("## Likely code areas")
        for item in code_context:
            lines.extend(
                [
                    f"### {item['path']}",
                    json.dumps({"line": item.get("line"), "chars": item.get("chars")}, sort_keys=True),
                    "```text",
                    item["snippet"],
                    "```",
                    "",
                ]
            )

    lines.extend(
        [
            "## Response contract",
            "Return a JSON object with the following fields:",
            json.dumps(GEMINI_DIAGNOSIS_SCHEMA, indent=2, sort_keys=True),
            "",
            "Required output keys:",
            "- summary",
            "- likely_root_cause",
            "- confidence",
            "- next_steps",
            "- suspect_paths",
            "- evidence_notes",
            "",
            "Optional output keys when supported by the evidence:",
            "- primary_failed_job",
            "- primary_failed_step",
            "- failing_test",
            "- failing_location",
            "- failure_structure",
            "- recommended_follow_up",
        ]
    )
    return "\n".join(lines)


def _extract_gemini_text(payload):
    candidates = payload.get("candidates") or []
    if not candidates:
        prompt_feedback = payload.get("promptFeedback") or {}
        block_reason = prompt_feedback.get("blockReason") or ""
        if block_reason:
            raise RuntimeError(f"Gemini blocked the request: {block_reason}")
        raise RuntimeError("Gemini response did not include any candidates")

    content = candidates[0].get("content") or {}
    parts = content.get("parts") or []
    pieces = []
    for part in parts:
        if not isinstance(part, dict):
            continue
        text = part.get("text")
        if text:
            pieces.append(str(text))
    text = "".join(pieces).strip()
    if not text:
        raise RuntimeError("Gemini response did not include text content")
    return text


def _parse_gemini_json(text):
    stripped = text.strip()
    if stripped.startswith("```"):
        stripped = stripped.strip("`")
        if stripped.lower().startswith("json"):
            stripped = stripped[4:].strip()
    try:
        return json.loads(stripped)
    except json.JSONDecodeError:
        start = stripped.find("{")
        end = stripped.rfind("}")
        if start >= 0 and end > start:
            return json.loads(stripped[start : end + 1])
        raise


def _normalize_diagnosis_payload(model, parsed):
    if not isinstance(parsed, dict):
        raise RuntimeError("Gemini diagnosis payload must be a JSON object")

    next_steps = parsed.get("next_steps") or []
    if not isinstance(next_steps, list):
        next_steps = [str(next_steps)]
    suspect_paths = parsed.get("suspect_paths") or []
    if not isinstance(suspect_paths, list):
        suspect_paths = [str(suspect_paths)]
    evidence_notes = parsed.get("evidence_notes") or []
    if not isinstance(evidence_notes, list):
        evidence_notes = [str(evidence_notes)]

    confidence = str(parsed.get("confidence") or "low").strip().lower()
    if confidence not in {"high", "medium", "low"}:
        confidence = "low"

    normalized = {
        "model": model,
        "summary": str(parsed.get("summary") or "").strip(),
        "likely_root_cause": str(parsed.get("likely_root_cause") or "").strip(),
        "confidence": confidence,
        "next_steps": [str(item).strip() for item in next_steps if str(item).strip()],
        "suspect_paths": [str(item).strip() for item in suspect_paths if str(item).strip()],
        "evidence_notes": [str(item).strip() for item in evidence_notes if str(item).strip()],
    }
    for key in (
        "primary_failed_job",
        "primary_failed_step",
        "failing_test",
        "failing_location",
        "failure_structure",
        "recommended_follow_up",
    ):
        value = str(parsed.get(key) or "").strip()
        if value:
            normalized[key] = value
    return normalized


def _normalize_usage_metadata(usage_metadata):
    if not isinstance(usage_metadata, dict):
        return None

    key_map = {
        "promptTokenCount": "prompt_token_count",
        "cachedContentTokenCount": "cached_content_token_count",
        "candidatesTokenCount": "candidates_token_count",
        "toolUsePromptTokenCount": "tool_use_prompt_token_count",
        "thoughtsTokenCount": "thoughts_token_count",
        "totalTokenCount": "total_token_count",
        "promptTokensDetails": "prompt_tokens_details",
        "cachedContentTokensDetails": "cached_content_tokens_details",
        "candidatesTokensDetails": "candidates_tokens_details",
        "toolUsePromptTokensDetails": "tool_use_prompt_tokens_details",
    }
    normalized = {}
    for source_key, target_key in key_map.items():
        value = usage_metadata.get(source_key)
        if value is None:
            continue
        if isinstance(value, list):
            normalized[target_key] = [dict(item) if isinstance(item, dict) else item for item in value]
        else:
            normalized[target_key] = value
    return normalized or None


def _build_gemini_telemetry(
    *,
    model,
    started_at,
    attempts,
    usage_metadata=None,
    response_id=None,
    model_version=None,
):
    telemetry = {
        "model": model,
        "attempts": int(attempts),
        "latency_ms": max(0, int(round((time.perf_counter() - started_at) * 1000))),
        "usage_metadata": _normalize_usage_metadata(usage_metadata),
    }
    if response_id is not None:
        response_id = str(response_id).strip()
        if response_id:
            telemetry["response_id"] = response_id
    if model_version is not None:
        model_version = str(model_version).strip()
        if model_version:
            telemetry["model_version"] = model_version
    return telemetry


def _call_gemini_diagnosis(*, model, prompt, timeout_seconds):
    keys = _load_gemini_api_keys()
    if not keys:
        raise RuntimeError("GEMINI_API_KEY or GEMINI_API_KEYS is not set")

    body = {
        "contents": [{"role": "user", "parts": [{"text": prompt}]}],
        "generationConfig": {
            "temperature": 0.2,
            "topP": 0.95,
            "maxOutputTokens": GEMINI_MAX_OUTPUT_TOKENS,
            "responseMimeType": "application/json",
            "responseJsonSchema": GEMINI_DIAGNOSIS_SCHEMA,
        },
    }
    endpoint = f"{GEMINI_API_BASE_URL}/models/{urllib.parse.quote(model, safe='-._~')}:generateContent"
    started_at = time.perf_counter()
    last_error = None
    response_usage_metadata = None
    response_id = None
    response_model_version = None
    for attempt in range(1, GEMINI_MAX_REQUEST_RETRIES + 1):
        api_key = keys[(attempt - 1) % len(keys)]
        request = urllib.request.Request(
            endpoint,
            data=json.dumps(body).encode("utf-8"),
            headers={
                "Content-Type": "application/json",
                "Accept": "application/json",
                "x-goog-api-key": api_key,
            },
            method="POST",
        )
        try:
            with urllib.request.urlopen(request, timeout=timeout_seconds) as response:
                payload = json.loads(response.read().decode("utf-8"))
            if not isinstance(payload, dict):
                raise RuntimeError("Gemini response was not a JSON object")
            response_usage_metadata = payload.get("usageMetadata")
            response_id = payload.get("responseId")
            response_model_version = payload.get("modelVersion")
            text = _extract_gemini_text(payload)
            parsed = _parse_gemini_json(text)
            telemetry = _build_gemini_telemetry(
                model=model,
                started_at=started_at,
                attempts=attempt,
                usage_metadata=response_usage_metadata,
                response_id=response_id,
                model_version=response_model_version,
            )
            return _normalize_diagnosis_payload(model, parsed), telemetry
        except urllib.error.HTTPError as err:
            message = err.read().decode("utf-8", errors="replace")
            last_error = f"HTTP {err.code}: {message.strip() or err.reason}"
            telemetry = _build_gemini_telemetry(
                model=model,
                started_at=started_at,
                attempts=attempt,
                usage_metadata=response_usage_metadata,
                response_id=response_id,
                model_version=response_model_version,
            )
            if err.code in {401, 403, 429, 500, 502, 503, 504} and attempt < GEMINI_MAX_REQUEST_RETRIES:
                time.sleep(min(2 * attempt, 4))
                continue
            error = RuntimeError(last_error)
            error.telemetry = telemetry
            raise error from err
        except (urllib.error.URLError, TimeoutError, ValueError, RuntimeError) as err:
            last_error = str(err)
            telemetry = _build_gemini_telemetry(
                model=model,
                started_at=started_at,
                attempts=attempt,
                usage_metadata=response_usage_metadata,
                response_id=response_id,
                model_version=response_model_version,
            )
            if attempt < GEMINI_MAX_REQUEST_RETRIES:
                time.sleep(min(2 * attempt, 4))
                continue
            error = RuntimeError(last_error)
            error.telemetry = telemetry
            raise error from err

    error = RuntimeError(last_error or "Gemini diagnosis failed")
    error.telemetry = _build_gemini_telemetry(
        model=model,
        started_at=started_at,
        attempts=GEMINI_MAX_REQUEST_RETRIES,
        usage_metadata=response_usage_metadata,
        response_id=response_id,
        model_version=response_model_version,
    )
    raise error


def _build_diagnostic_evidence(
    *,
    log_sources,
    code_context,
    truncated,
    log_chars_sent,
    structured_failure_signals,
    validation_context,
):
    return {
        "redaction_applied": True,
        "truncated": bool(truncated),
        "failed_job_count": sum(1 for source in log_sources if source.get("kind") == "failed_job_log"),
        "log_chars_sent": int(log_chars_sent),
        "log_sources": [
            {
                "kind": source["kind"],
                "label": source["label"],
                "job_id": source.get("job_id"),
                "job_name": source.get("job_name"),
                "retrieved_via": source.get("retrieved_via"),
                "chars": len(_redact_text(source.get("text") or "")),
            }
            for source in log_sources
        ],
        "code_context_paths": [item["path"] for item in code_context],
        "structured_failure_signals": structured_failure_signals,
        "validation_context": validation_context,
    }


def _gemini_failure_alert(gemini_error):
    if not gemini_error:
        return []
    if _is_skipped_gemini_error(gemini_error):
        return []
    return [
        {
            "kind": "gemini_diagnosis_failed",
            "severity": "warning",
            "message": "Gemini diagnosis failed; see gemini_error and diagnostic_evidence.",
            "details": _truncate_middle(str(gemini_error), 600),
        }
    ]


def _is_skipped_gemini_error(gemini_error):
    text = str(gemini_error or "").strip().lower()
    if not text:
        return False
    return text.startswith("skipped gemini diagnosis")


def _diagnose_failure(*, repo, run_view, validation_summary, model, timeout_seconds):
    repo_root = _resolve_repo_root()
    validation_context = _derive_validation_mode_context(
        validation_summary,
        run_view=run_view,
        failed_jobs=_failed_jobs_from_run_view(run_view),
    )
    log_sources = _collect_log_sources(repo, run_view, validation_summary=validation_summary)
    redacted_log_texts = [_redact_text(source.get("text") or "") for source in log_sources if source.get("text")]
    code_context = _collect_code_context(repo_root, redacted_log_texts)
    prompt_log_sections, log_chars_sent, truncated = _prepare_gemini_log_sections(log_sources)
    structured_failure_signals = _collect_structured_failure_signals(prompt_log_sections)
    evidence = _build_diagnostic_evidence(
        log_sources=log_sources,
        code_context=code_context,
        truncated=truncated,
        log_chars_sent=log_chars_sent,
        structured_failure_signals=structured_failure_signals,
        validation_context=validation_context,
    )
    if not _signals_have_actionable_detail(structured_failure_signals) and not _validation_summary_has_actionable_detail(
        validation_summary
    ):
        raise GeminiDiagnosisError(
            "Skipped Gemini diagnosis to avoid low-value token spend: focused failure evidence did not yield an exact test, assertion, source location, or structured validation blocker signal.",
            evidence=evidence,
            telemetry=None,
        )
    prompt = _build_gemini_prompt(
        repo=repo,
        run_view=run_view,
        validation_summary=validation_summary,
        log_sources=prompt_log_sections,
        code_context=code_context,
    )
    try:
        diagnosis, telemetry = _call_gemini_diagnosis(model=model, prompt=prompt, timeout_seconds=timeout_seconds)
    except Exception as err:
        raise GeminiDiagnosisError(
            str(err),
            evidence=evidence,
            telemetry=getattr(err, "telemetry", None),
        ) from err
    return diagnosis, evidence, telemetry


def _collect_failure_evidence(*, repo, run_view, validation_summary):
    repo_root = _resolve_repo_root()
    validation_context = _derive_validation_mode_context(
        validation_summary,
        run_view=run_view,
        failed_jobs=_failed_jobs_from_run_view(run_view),
    )
    log_sources = _collect_log_sources(repo, run_view, validation_summary=validation_summary)
    redacted_log_texts = [_redact_text(source.get("text") or "") for source in log_sources if source.get("text")]
    code_context = _collect_code_context(repo_root, redacted_log_texts)
    prompt_log_sections, log_chars_sent, truncated = _prepare_gemini_log_sections(log_sources)
    structured_failure_signals = _collect_structured_failure_signals(prompt_log_sections)
    evidence = _build_diagnostic_evidence(
        log_sources=log_sources,
        code_context=code_context,
        truncated=truncated,
        log_chars_sent=log_chars_sent,
        structured_failure_signals=structured_failure_signals,
        validation_context=validation_context,
    )
    return {
        "evidence": evidence,
        "validation_context": validation_context,
    }


def summarize_jobs(run_view):
    failed_jobs = []
    jobs = run_view.get("jobs") or []
    if not isinstance(jobs, list):
        return failed_jobs
    for job in jobs:
        if not isinstance(job, dict):
            continue
        conclusion = str(job.get("conclusion") or "")
        if conclusion not in FAILED_CONCLUSIONS:
            continue
        failed_jobs.append(
            {
                "id": int(job.get("databaseId") or 0),
                "name": str(job.get("name") or ""),
                "status": str(job.get("status") or ""),
                "conclusion": conclusion,
                "url": str(job.get("url") or ""),
            }
        )
    return failed_jobs


def target_to_display_key(target):
    if target["kind"] == TARGET_KIND_RUN_ID:
        return f"run-id:{target['run_id']}"
    parts = [f"workflow:{target['workflow']}", f"ref:{target['ref']}"]
    host_ref = str(target.get("host_ref") or "").strip()
    if host_ref:
        parts.append(f"host-ref:{host_ref}")
    head_sha = str(target.get("head_sha") or "").strip()
    if head_sha:
        parts.append(f"head-sha:{head_sha}")
    min_run_id = target.get("min_run_id")
    if min_run_id is not None:
        parts.append(f"min-run-id:{int(min_run_id)}")
    return "|".join(parts)


def _detect_dispatch_host_branch_mismatch(repo, target, resolved_ref):
    expected_head_sha = str(target.get("head_sha") or "").strip()
    if not expected_head_sha:
        return None
    logical_ref = str(resolved_ref or "")
    if not logical_ref or is_sha_like(logical_ref):
        return None
    if target.get("host_ref"):
        return None

    candidates = list_workflow_runs(
        repo,
        target["workflow"],
        ref=None,
        expected_head_sha=expected_head_sha,
        minimum_run_id=target.get("min_run_id"),
    )
    for run in candidates:
        head_branch = str(run.get("headBranch") or "")
        event = str(run.get("event") or "")
        if not head_branch or head_branch == logical_ref:
            continue
        if event != "workflow_dispatch":
            continue
        run_id = int(run.get("databaseId") or 0)
        return {
            "run_id": run_id if run_id > 0 else None,
            "run_url": str(run.get("url") or ""),
            "host_branch": head_branch,
            "event": event,
            "head_sha": str(run.get("headSha") or ""),
            "message": (
                f"Found workflow_dispatch run on host branch '{head_branch}' with matching "
                f"head_sha '{expected_head_sha}'. This watch target is filtering on logical "
                f"ref '{logical_ref}', so that run is invisible."
            ),
            "suggested_target": (
                f"workflow={target['workflow']},ref={logical_ref},host-ref={head_branch},"
                f"head-sha={expected_head_sha}"
            ),
        }
    return None


def _maybe_detect_dispatch_host_branch_mismatch(repo, target, resolved_ref, state, poll_seconds):
    now = int(time.time())
    last_checked_at = int(state.get("dispatch_host_mismatch_last_checked_at") or 0)
    recheck_after = _host_mismatch_recheck_interval_seconds(poll_seconds)
    if last_checked_at and now - last_checked_at < recheck_after:
        return state.get("dispatch_host_mismatch_last_result")

    dispatch_host_mismatch = _detect_dispatch_host_branch_mismatch(repo, target, resolved_ref)
    state["dispatch_host_mismatch_last_checked_at"] = now
    state["dispatch_host_mismatch_last_result"] = dispatch_host_mismatch
    return dispatch_host_mismatch


def normalize_snapshot(
    run_view,
    *,
    target,
    repo,
    followed_newer_run,
    resolved_ref,
    appearance_wait=None,
    gemini_diagnosis=None,
    gemini_error=None,
    diagnostic_evidence=None,
    gemini_telemetry=None,
    alerts=None,
    gemini_disabled=False,
):
    status = str(run_view.get("status") or "")
    conclusion = str(run_view.get("conclusion") or "")
    failed_jobs = summarize_jobs(run_view)
    validation_summary = None
    if status == "completed":
        run_id = int(run_view.get("databaseId") or 0)
        if run_id > 0:
            validation_summary = load_validation_summary(repo, run_id)

    if not run_view:
        actions = ["idle"]
    elif status != "completed" or status.lower() in PENDING_STATUSES:
        actions = ["diagnose_run_failure"] if failed_jobs else ["idle"]
    elif conclusion in SUCCESS_CONCLUSIONS:
        actions = ["stop_run_succeeded"]
    elif conclusion in FAILED_CONCLUSIONS:
        actions = ["diagnose_run_failure"]
    else:
        actions = ["stop_run_terminal"]

    validation_context = _derive_validation_mode_context(
        validation_summary,
        run_view=run_view,
        failed_jobs=failed_jobs,
    )
    diagnosis_status = _build_diagnosis_status(
        actions=actions,
        gemini_diagnosis=gemini_diagnosis,
        gemini_error=gemini_error,
        gemini_disabled=gemini_disabled,
    )

    return {
        "target": target,
        "repo": repo,
        "resolved_ref": resolved_ref,
        "run": {
            "id": run_view.get("databaseId"),
            "number": run_view.get("number"),
            "name": str(run_view.get("displayTitle") or run_view.get("name") or ""),
            "workflow_name": str(run_view.get("workflowName") or target.get("workflow", "")),
            "url": str(run_view.get("url") or ""),
            "head_branch": str(run_view.get("headBranch") or ""),
            "head_sha": str(run_view.get("headSha") or ""),
            "event": str(run_view.get("event") or ""),
            "status": status,
            "conclusion": conclusion,
            "created_at": str(run_view.get("createdAt") or ""),
            "updated_at": str(run_view.get("updatedAt") or ""),
        },
        "failed_jobs": failed_jobs,
        "validation_summary": validation_summary,
        "appearance_wait": appearance_wait,
        "followed_newer_run": followed_newer_run,
        "gemini_diagnosis": gemini_diagnosis,
        "gemini_error": gemini_error,
        "diagnostic_evidence": diagnostic_evidence,
        "gemini_telemetry": gemini_telemetry,
        "validation_context": validation_context,
        "diagnosis_status": diagnosis_status,
        "alerts": alerts or [],
        "actions": actions,
        "ts": int(time.time()),
    }


def _action_descriptors_for_snapshot(snapshot):
    descriptors = []
    action_list = list(snapshot.get("actions") or [])
    run = _dict_or_empty(snapshot.get("run"))
    target = _dict_or_empty(snapshot.get("target"))
    failed_jobs = _list_or_empty(snapshot.get("failed_jobs"))
    diagnostic_evidence = _dict_or_empty(snapshot.get("diagnostic_evidence"))
    log_sources = _list_or_empty(diagnostic_evidence.get("log_sources"))
    run_id = run.get("id") or target.get("run_id")

    for action in action_list:
        if action == "diagnose_run_failure":
            primary_job = _dict_or_empty(failed_jobs[0] if failed_jobs else {})
            job_id = primary_job.get("id")
            run_status = str(run.get("status") or "")
            failure_phase = "terminal_failure" if run_status == "completed" else "in_progress_failed_job"
            logs_available = any(
                source.get("kind") == "failed_job_log" and (job_id is None or source.get("job_id") == job_id)
                for source in log_sources
                if isinstance(source, dict)
            )
            fingerprint_parts = [action, f"run:{run_id or 'unknown'}", f"phase:{failure_phase}"]
            if job_id:
                fingerprint_parts.append(f"job:{job_id}")
            descriptors.append(
                {
                    "action": action,
                    "fingerprint": ":".join(fingerprint_parts),
                    "run_id": run_id,
                    "run_url": run.get("url"),
                    "job_id": job_id,
                    "job_name": primary_job.get("name"),
                    "failure_phase": failure_phase,
                    "logs_available": bool(logs_available),
                }
            )
            continue
        if action == "stop_run_appearance_timeout":
            descriptors.append(
                {
                    "action": action,
                    "fingerprint": f"{action}:{target_to_display_key(target) if target else 'unknown'}",
                }
            )
            continue
        descriptors.append(
            {
                "action": action,
                "fingerprint": f"{action}:run:{run_id or 'unknown'}",
            }
        )
    return descriptors


def _apply_acknowledged_actions(snapshot, acknowledged):
    acknowledged_set = {str(item).strip() for item in _list_or_empty(acknowledged) if str(item).strip()}
    descriptors = _action_descriptors_for_snapshot(snapshot)
    remaining_descriptors = []
    suppressed = []
    for descriptor in descriptors:
        fingerprint = str(descriptor.get("fingerprint") or "").strip()
        if fingerprint and fingerprint in acknowledged_set:
            suppressed.append(fingerprint)
            continue
        remaining_descriptors.append(descriptor)

    if remaining_descriptors:
        actions = merge_ordered_unique([item.get("action") for item in remaining_descriptors if item.get("action")])
    else:
        actions = ["idle"]

    snapshot["actions"] = actions
    snapshot["action_triggers"] = remaining_descriptors
    snapshot["action_fingerprints"] = [item.get("fingerprint") for item in remaining_descriptors if item.get("fingerprint")]
    snapshot["suppressed_action_fingerprints"] = suppressed
    return snapshot


def merge_ordered_unique(items):
    seen = set()
    output = []
    for item in items:
        if item not in seen:
            seen.add(item)
            output.append(item)
    return output


def _compact_run_payload(run_payload):
    run = _dict_or_empty(run_payload)
    if not run:
        return run_payload
    compact = {
        "id": run.get("id"),
        "number": run.get("number"),
        "workflow_name": run.get("workflow_name"),
        "url": run.get("url"),
        "head_branch": run.get("head_branch"),
        "head_sha": run.get("head_sha"),
        "status": run.get("status"),
        "conclusion": run.get("conclusion"),
    }
    return {key: value for key, value in compact.items() if value not in (None, "", [])}


def _compact_failed_jobs(failed_jobs):
    compact_jobs = []
    for job in _list_or_empty(failed_jobs):
        if not isinstance(job, dict):
            continue
        compact = {
            "id": job.get("id"),
            "name": job.get("name"),
            "conclusion": job.get("conclusion"),
        }
        compact_jobs.append({key: value for key, value in compact.items() if value not in (None, "", [])})
    return compact_jobs


def _compact_appearance_wait(appearance_wait):
    wait_state = _dict_or_empty(appearance_wait)
    if not wait_state:
        return appearance_wait
    compact = {
        "waiting_for_match": bool(wait_state.get("waiting_for_match")),
        "timed_out": bool(wait_state.get("timed_out")),
        "elapsed_seconds": int(wait_state.get("elapsed_seconds") or 0),
        "timeout_seconds": int(wait_state.get("timeout_seconds") or 0),
    }
    dispatch_host_mismatch = _dict_or_empty(wait_state.get("dispatch_host_mismatch"))
    if dispatch_host_mismatch:
        compact["dispatch_host_mismatch"] = {
            key: dispatch_host_mismatch.get(key)
            for key in ("run_id", "run_url", "host_branch", "event", "head_sha", "message", "suggested_target")
            if dispatch_host_mismatch.get(key) not in (None, "", [])
        }
    return compact


def _compact_validation_context(validation_context):
    context = _dict_or_empty(validation_context)
    if not context:
        return None
    compact = {
        "profile": context.get("profile"),
        "failure_structure": context.get("failure_structure"),
        "recommended_follow_up": context.get("recommended_follow_up"),
        "first_blocker": context.get("first_blocker"),
        "candidate_next_slices": context.get("candidate_next_slices"),
        "failed_lane_count": context.get("failed_lane_count"),
    }
    return {key: value for key, value in compact.items() if value not in (None, [], {}, "")} or None


def _compact_snapshot(snapshot, *, verbose_details):
    if verbose_details:
        return snapshot
    compact = dict(snapshot)
    compact.pop("repo", None)
    target = _dict_or_empty(compact.get("target"))
    if target:
        compact["target"] = {
            key: value
            for key, value in target.items()
            if key in {"kind", "run_id", "workflow", "ref", "host_ref", "head_sha", "min_run_id"}
            and value not in (None, "", [])
        }
    compact["run"] = _compact_run_payload(compact.get("run"))
    compact["failed_jobs"] = _compact_failed_jobs(compact.get("failed_jobs"))
    compact["appearance_wait"] = _compact_appearance_wait(compact.get("appearance_wait"))
    compact["validation_context"] = _compact_validation_context(compact.get("validation_context"))
    diagnosis_status = _dict_or_empty(compact.get("diagnosis_status"))
    if diagnosis_status:
        compact["diagnosis_status"] = {"state": diagnosis_status.get("state")}
    compact.pop("validation_summary", None)
    return compact


def target_state_from_target(args, target, repo, remembered):
    key = target_to_display_key(target)
    state = remembered.setdefault(key, {})
    gemini_cache = remembered.setdefault("__gemini_cache__", {})
    if target["kind"] == TARGET_KIND_RUN_ID:
        run_view = view_run(repo, target["run_id"])
        snapshot = normalize_snapshot(
            run_view,
            target=target,
            repo=repo,
            followed_newer_run=False,
            resolved_ref=str(run_view.get("headBranch") or str(target.get("ref") or "")),
            gemini_disabled=args.no_gemini_diagnosis,
        )
        state["last_run_id"] = int(run_view.get("databaseId") or target["run_id"])
        if snapshot.get("actions") == ["diagnose_run_failure"]:
            run_id = int(run_view.get("databaseId") or target["run_id"])
            cached = gemini_cache.get(run_id)
            if cached is None:
                if args.no_gemini_diagnosis:
                    evidence_bundle = _collect_failure_evidence(
                        repo=repo,
                        run_view=run_view,
                        validation_summary=snapshot.get("validation_summary"),
                    )
                    cached = {
                        "gemini_diagnosis": None,
                        "gemini_error": None,
                        "diagnostic_evidence": _dict_or_empty(evidence_bundle).get("evidence") or {},
                        "gemini_telemetry": None,
                    }
                else:
                    try:
                        diagnosis, evidence, telemetry = _diagnose_failure(
                            repo=repo,
                            run_view=run_view,
                            validation_summary=snapshot.get("validation_summary"),
                            model=args.gemini_model,
                            timeout_seconds=args.gemini_timeout_seconds,
                        )
                        cached = {
                            "gemini_diagnosis": diagnosis,
                            "gemini_error": None,
                            "diagnostic_evidence": evidence,
                            "gemini_telemetry": telemetry,
                        }
                    except GeminiDiagnosisError as err:
                        cached = {
                            "gemini_diagnosis": None,
                            "gemini_error": str(err),
                            "diagnostic_evidence": getattr(err, "evidence", None),
                            "gemini_telemetry": getattr(err, "telemetry", None),
                        }
                    except Exception as err:
                        cached = {
                            "gemini_diagnosis": None,
                            "gemini_error": str(err),
                            "diagnostic_evidence": None,
                            "gemini_telemetry": getattr(err, "telemetry", None),
                        }
                gemini_cache[run_id] = cached
            snapshot.update(cached)
            snapshot["alerts"] = _gemini_failure_alert(snapshot.get("gemini_error"))
            snapshot["diagnosis_status"] = _build_diagnosis_status(
                actions=snapshot.get("actions"),
                gemini_diagnosis=snapshot.get("gemini_diagnosis"),
                gemini_error=snapshot.get("gemini_error"),
                gemini_disabled=args.no_gemini_diagnosis,
            )
        return _apply_acknowledged_actions(snapshot, getattr(args, "ack_action", []))

    ref = detect_ref(target["ref"])
    expected_head_sha = target.get("head_sha")
    host_ref = str(target.get("host_ref") or "").strip() or None
    now = int(time.time())
    follow_relist_after = _followed_run_relist_interval_seconds(args.poll_seconds)
    cached_run_id = state.get("last_run_id")
    next_run_list_at = int(state.get("next_run_list_at") or 0)
    run_view = None
    followed_newer_run = False
    if cached_run_id is not None and now < next_run_list_at:
        state.pop("appearance_wait_started_at", None)
        run_view = view_run(repo, int(cached_run_id))
        latest_run_id = int(cached_run_id)
    else:
        matching_runs = list_workflow_runs(
            repo,
            target["workflow"],
            ref,
            expected_head_sha,
            minimum_run_id=target.get("min_run_id"),
            host_ref=host_ref,
        )
        state["last_run_list_at"] = now
        if not matching_runs:
            wait_started_at = int(state.get("appearance_wait_started_at") or now)
            state["appearance_wait_started_at"] = wait_started_at
            elapsed_seconds = max(0, now - wait_started_at)
            timeout_seconds = int(args.appearance_timeout_seconds or 0)
            timed_out = timeout_seconds > 0 and elapsed_seconds >= timeout_seconds
            dispatch_host_mismatch = _maybe_detect_dispatch_host_branch_mismatch(
                repo,
                target,
                ref,
                state,
                args.poll_seconds,
            )
            if dispatch_host_mismatch:
                timed_out = False
            actions = (
                ["stop_dispatch_host_branch_mismatch"]
                if dispatch_host_mismatch
                else (["stop_run_appearance_timeout"] if timed_out else ["idle"])
            )
            snapshot = {
                "target": target,
                "repo": repo,
                "resolved_ref": ref,
                "run": None,
                "failed_jobs": [],
                "validation_summary": None,
                "appearance_wait": {
                    "waiting_for_match": True,
                    "wait_started_at": wait_started_at,
                    "elapsed_seconds": elapsed_seconds,
                    "timeout_seconds": timeout_seconds,
                    "timed_out": timed_out,
                    "dispatch_host_mismatch": dispatch_host_mismatch,
                },
                "followed_newer_run": False,
                "gemini_diagnosis": None,
                "gemini_error": None,
                "diagnostic_evidence": None,
                "gemini_telemetry": None,
                "validation_context": None,
                "diagnosis_status": _build_diagnosis_status(
                    actions=actions,
                    gemini_diagnosis=None,
                    gemini_error=None,
                    gemini_disabled=args.no_gemini_diagnosis,
                ),
                "alerts": [],
                "actions": actions,
                "ts": now,
            }
            return _apply_acknowledged_actions(snapshot, getattr(args, "ack_action", []))

        latest = matching_runs[0]
        latest_run_id = int(latest["databaseId"])
        state.pop("appearance_wait_started_at", None)
        last_run_id = state.get("last_run_id")
        if last_run_id is not None and latest_run_id != last_run_id:
            followed_newer_run = True
        run_view = view_run(repo, latest_run_id)
        state["last_run_id"] = latest_run_id
        state["next_run_list_at"] = now + follow_relist_after

    resolved = normalize_snapshot(
        run_view,
        target=target,
        repo=repo,
        followed_newer_run=followed_newer_run,
        resolved_ref=ref,
        gemini_disabled=args.no_gemini_diagnosis,
    )
    state["last_run_id"] = latest_run_id
    if resolved.get("actions") == ["diagnose_run_failure"]:
        cached = gemini_cache.get(latest_run_id)
        if cached is None:
            if args.no_gemini_diagnosis:
                evidence_bundle = _collect_failure_evidence(
                    repo=repo,
                    run_view=run_view,
                    validation_summary=resolved.get("validation_summary"),
                )
                cached = {
                    "gemini_diagnosis": None,
                    "gemini_error": None,
                    "diagnostic_evidence": _dict_or_empty(evidence_bundle).get("evidence") or {},
                    "gemini_telemetry": None,
                }
            else:
                try:
                    diagnosis, evidence, telemetry = _diagnose_failure(
                        repo=repo,
                        run_view=run_view,
                        validation_summary=resolved.get("validation_summary"),
                        model=args.gemini_model,
                        timeout_seconds=args.gemini_timeout_seconds,
                    )
                    cached = {
                        "gemini_diagnosis": diagnosis,
                        "gemini_error": None,
                        "diagnostic_evidence": evidence,
                        "gemini_telemetry": telemetry,
                    }
                except GeminiDiagnosisError as err:
                    cached = {
                        "gemini_diagnosis": None,
                        "gemini_error": str(err),
                        "diagnostic_evidence": getattr(err, "evidence", None),
                        "gemini_telemetry": getattr(err, "telemetry", None),
                    }
                except Exception as err:
                    cached = {
                        "gemini_diagnosis": None,
                        "gemini_error": str(err),
                        "diagnostic_evidence": None,
                        "gemini_telemetry": getattr(err, "telemetry", None),
                    }
            gemini_cache[latest_run_id] = cached
        resolved.update(cached)
        resolved["alerts"] = _gemini_failure_alert(resolved.get("gemini_error"))
        resolved["diagnosis_status"] = _build_diagnosis_status(
            actions=resolved.get("actions"),
            gemini_diagnosis=resolved.get("gemini_diagnosis"),
            gemini_error=resolved.get("gemini_error"),
            gemini_disabled=args.no_gemini_diagnosis,
        )
    return _apply_acknowledged_actions(resolved, getattr(args, "ack_action", []))


def evaluate_targets(args, repo, targets, remembered):
    snapshots = []
    aggregate_actions = []
    summary = {
        "targets_total": len(targets),
        "targets_idle": 0,
        "targets_actionable": 0,
        "targets_terminal_success": 0,
        "targets_terminal_failure": 0,
        "targets_terminal_other": 0,
        "targets_no_match": 0,
        "targets_waiting_for_match": 0,
        "targets_appearance_timeout": 0,
    }

    for target in targets:
        snapshot = target_state_from_target(args, target, repo, remembered)
        snapshot_actions = snapshot.get("actions") or []
        if snapshot_actions == ["idle"]:
            summary["targets_idle"] += 1
        else:
            summary["targets_actionable"] += 1
            aggregate_actions.extend(snapshot_actions)
            if "stop_run_succeeded" in snapshot_actions:
                summary["targets_terminal_success"] += 1
            elif "diagnose_run_failure" in snapshot_actions:
                summary["targets_terminal_failure"] += 1
            elif "stop_run_appearance_timeout" in snapshot_actions:
                summary["targets_appearance_timeout"] += 1
            else:
                summary["targets_terminal_other"] += 1
        if snapshot.get("run") is None:
            summary["targets_no_match"] += 1
        appearance_wait = snapshot.get("appearance_wait") or {}
        if appearance_wait.get("waiting_for_match"):
            summary["targets_waiting_for_match"] += 1
        snapshots.append(_compact_snapshot(snapshot, verbose_details=args.verbose_details))

    if not aggregate_actions:
        aggregate_actions = ["idle"]
    return {
        "repo": repo,
        "targets": snapshots,
        "summary": summary,
        "actions": merge_ordered_unique(aggregate_actions),
        "wait_for": args.wait_for,
        "ts": int(time.time()),
    }


def resolve_snapshot(args, repo, targets, remembered):
    return evaluate_targets(args, repo, targets, remembered)


def _payload_has_in_progress_failure(payload):
    for target in payload.get("targets") or []:
        target_actions = target.get("actions") or []
        if "diagnose_run_failure" not in target_actions:
            continue
        run = _dict_or_empty(target.get("run"))
        status = str(run.get("status") or "").lower()
        if status != "completed":
            return True
    return False


def _payload_has_unready_failure_logs(payload):
    for target in payload.get("targets") or []:
        for trigger in target.get("action_triggers") or []:
            if not isinstance(trigger, dict):
                continue
            if trigger.get("action") != "diagnose_run_failure":
                continue
            if trigger.get("logs_available"):
                continue
            return True
    return False


def emit(payload):
    sys.stdout.write(json.dumps(payload, sort_keys=True) + "\n")
    sys.stdout.flush()


def watch_until_action(args, repo):
    targets = build_targets(args)
    remembered = {}
    while True:
        payload = resolve_snapshot(args, repo, targets, remembered)
        if args.require_terminal_run and _payload_has_in_progress_failure(payload):
            time.sleep(args.poll_seconds)
            continue
        actions = payload.get("actions") or []
        if args.wait_for == "all_done":
            if actions != ["idle"]:
                pending_count = payload.get("summary", {}).get("targets_idle", 0)
                if pending_count == 0 and (
                    not _payload_has_unready_failure_logs(payload)
                    or not _payload_has_in_progress_failure(payload)
                ):
                    emit(payload)
                    return
        else:
            if actions != ["idle"]:
                emit(payload)
                return
        time.sleep(args.poll_seconds)


def watch_stream(args, repo):
    targets = build_targets(args)
    remembered = {}
    while True:
        payload = resolve_snapshot(args, repo, targets, remembered)
        emit(payload)
        time.sleep(args.poll_seconds)


def main():
    args = parse_args()
    repo = args.repo or detect_repo()
    if not repo:
        raise GhCommandError(
            "Unable to determine OWNER/REPO from GH_REPO, git remotes, or `gh repo view`; "
            f"cwd={Path.cwd()}. Pass --repo explicitly."
        )

    if args.watch_until_action:
        watch_until_action(args, repo)
        return
    if args.watch:
        watch_stream(args, repo)
        return

    payload = resolve_snapshot(args, repo, build_targets(args), {})
    emit(payload)


if __name__ == "__main__":
    try:
        main()
    except GhCommandError as err:
        emit({"error": str(err), "actions": ["stop_operator_help_required"], "ts": int(time.time())})
        sys.exit(1)
