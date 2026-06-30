#!/usr/bin/env python3
"""Static policy checks for GitHub workflow files."""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path
from typing import Any

import yaml


REPO_ROOT = Path(__file__).resolve().parents[2]
WORKFLOW_PATTERNS = (
    ".github/workflows/*.yml",
    ".github/workflows/*.yaml",
    "codex-rs/.github/workflows/*.yml",
    "codex-rs/.github/workflows/*.yaml",
)
RELEASE_INSTALL_WORKFLOW = "sedna-release-install.yml"
RELEASE_INSTALL_SCRIPT = "scripts/install_sedna_release_asset"
DRY_RUN_FIELD_PATTERN = re.compile(
    r"(?:^|\s)(?:-f|--field|-F|--raw-field)\s+['\"]?dry_run=true['\"]?(?:\s|$)"
)
FORBIDDEN_RUNNER_SIZE_PATTERN = re.compile(
    r"(?<![A-Za-z0-9])(?:xlarge|large|xl)(?![A-Za-z0-9])", re.IGNORECASE
)
STANDARD_PUBLIC_RUNNER_PATTERNS = (
    re.compile(r"ubuntu-(?:latest|\d{2}\.\d{2})"),
    re.compile(r"windows-(?:latest|\d{4}(?:-vs\d{4})?)"),
    re.compile(r"macos-(?:latest|\d{2}(?:-intel)?)"),
)
RUNNER_FIELD_NAMES = {
    "runs-on",
    "runs_on",
    "runner",
    "archive_runner",
    "os",
}
RUNNER_GROUP_FIELD_NAMES = {
    "runner_group",
    "runner_labels",
    "archive_runner_group",
    "archive_runner_labels",
}


def workflow_paths(root: Path) -> list[Path]:
    paths: list[Path] = []
    for pattern in WORKFLOW_PATTERNS:
        paths.extend(root.glob(pattern))
    return sorted({path for path in paths if path.is_file()})


def load_workflow(path: Path) -> Any:
    return yaml.load(path.read_text(encoding="utf-8"), Loader=yaml.BaseLoader)


def walk_mappings(value: Any):
    if isinstance(value, dict):
        yield value
        for child in value.values():
            yield from walk_mappings(child)
    elif isinstance(value, list):
        for child in value:
            yield from walk_mappings(child)


def iter_runner_fields(value: Any, *, skip_runner_with_explicit_runs_on: bool = False):
    if isinstance(value, dict):
        has_explicit_runner_selector = "runs-on" in value or "runs_on" in value
        for child_key, child in value.items():
            key = str(child_key)
            if (
                key == "runner"
                and skip_runner_with_explicit_runs_on
                and has_explicit_runner_selector
            ):
                continue
            if key in RUNNER_FIELD_NAMES or key in RUNNER_GROUP_FIELD_NAMES:
                yield key, child
            else:
                yield from iter_runner_fields(
                    child,
                    skip_runner_with_explicit_runs_on=skip_runner_with_explicit_runs_on,
                )
    elif isinstance(value, list):
        for child in value:
            yield from iter_runner_fields(
                child,
                skip_runner_with_explicit_runs_on=skip_runner_with_explicit_runs_on,
            )


def iter_runner_labels(value: Any):
    if isinstance(value, str):
        yield value
    elif isinstance(value, list):
        for child in value:
            yield from iter_runner_labels(child)


def is_expression(value: str) -> bool:
    stripped = value.strip()
    return stripped.startswith("${{") and stripped.endswith("}}")


def is_standard_public_runner_label(value: str) -> bool:
    return any(pattern.fullmatch(value) for pattern in STANDARD_PUBLIC_RUNNER_PATTERNS)


def workflow_call_inputs(payload: Any) -> dict[str, Any]:
    if not isinstance(payload, dict):
        return {}
    triggers = payload.get("on")
    if not isinstance(triggers, dict):
        return {}
    workflow_call = triggers.get("workflow_call")
    if not isinstance(workflow_call, dict):
        return {}
    inputs = workflow_call.get("inputs")
    return inputs if isinstance(inputs, dict) else {}


def check_runner_value(key: str, value: Any, violations: set[str]) -> None:
    if key in RUNNER_GROUP_FIELD_NAMES:
        violations.add(
            "runner group inputs are not allowed; use standard public "
            "GitHub-hosted runner labels directly."
        )
        return

    if isinstance(value, list) and "self-hosted" in value:
        violations.add(
            "self-hosted runners are not allowed; use external deployment "
            "automation for host-local operations."
        )
        return

    if isinstance(value, dict):
        if "group" in value or "labels" in value:
            violations.add(
                "runner groups are not allowed; use standard public GitHub-hosted "
                "runner labels directly."
            )
        else:
            violations.add(
                "object-valued runner selectors are not allowed; use standard "
                "public GitHub-hosted runner labels directly."
            )
        return

    for label in iter_runner_labels(value):
        if label == "self-hosted":
            violations.add(
                "self-hosted runners are not allowed; use external deployment "
                "automation for host-local operations."
            )
            continue
        if "github.event.repository.name" in label:
            violations.add(
                f"repo-scoped runner label '{label}' is not allowed; use a "
                "standard public GitHub-hosted runner label."
            )
            continue
        if FORBIDDEN_RUNNER_SIZE_PATTERN.search(label):
            violations.add(
                f"runner label '{label}' uses a larger-runner size token; use "
                "standard public GitHub-hosted runner labels."
            )
            continue
        if is_expression(label):
            continue
        if not is_standard_public_runner_label(label):
            violations.add(
                f"runner label '{label}' is not a recognized standard public "
                "GitHub-hosted runner label."
            )


def check_runner_fields(
    value: Any,
    violations: set[str],
    *,
    skip_runner_with_explicit_runs_on: bool = False,
) -> None:
    for key, runner_value in iter_runner_fields(
        value,
        skip_runner_with_explicit_runs_on=skip_runner_with_explicit_runs_on,
    ):
        check_runner_value(key, runner_value, violations)


def runner_policy_violations(payload: Any) -> list[str]:
    violations: set[str] = set()
    for input_name in workflow_call_inputs(payload):
        if input_name in RUNNER_GROUP_FIELD_NAMES:
            violations.add(
                "runner group inputs are not allowed; use standard public "
                "GitHub-hosted runner labels directly."
            )

    for _job_id, job in iter_jobs(payload):
        runs_on = job.get("runs-on")
        if "runs-on" in job:
            check_runner_value("runs-on", runs_on, violations)
        strategy = job.get("strategy")
        if isinstance(strategy, dict):
            check_runner_fields(
                strategy.get("matrix"),
                violations,
                skip_runner_with_explicit_runs_on=isinstance(runs_on, str)
                and "matrix.runs_on" in runs_on,
            )
        workflow_inputs = job.get("with")
        if isinstance(workflow_inputs, dict):
            check_runner_fields(workflow_inputs, violations)

    return sorted(violations)


def is_action_ref(uses: Any, action: str) -> bool:
    return isinstance(uses, str) and uses.startswith(f"{action}@")


def workflow_has_trigger(payload: Any, trigger_name: str) -> bool:
    if not isinstance(payload, dict):
        return False
    triggers = payload.get("on")
    if isinstance(triggers, str):
        return triggers == trigger_name
    if isinstance(triggers, list):
        return trigger_name in triggers
    if isinstance(triggers, dict):
        return trigger_name in triggers
    return False


def permission_value(permissions: Any, permission: str) -> str | None:
    if isinstance(permissions, str):
        return permissions
    if isinstance(permissions, dict):
        value = permissions.get(permission)
        return value if isinstance(value, str) else None
    return None


def grants_write_all(permissions: Any) -> bool:
    return permissions == "write-all"


def grants_permission(permissions: Any, permission: str, value: str) -> bool:
    return permission_value(permissions, permission) == value


def job_permissions(job: dict[str, Any], payload: Any) -> Any:
    if "permissions" in job:
        return job["permissions"]
    if isinstance(payload, dict):
        return payload.get("permissions")
    return None


def iter_jobs(payload: Any):
    if not isinstance(payload, dict):
        return
    jobs = payload.get("jobs")
    if not isinstance(jobs, dict):
        return
    for job_id, job in jobs.items():
        if isinstance(job, dict):
            yield str(job_id), job


def job_steps(job: dict[str, Any]) -> list[Any]:
    steps = job.get("steps")
    return steps if isinstance(steps, list) else []


def job_uses_checkout(job: dict[str, Any]) -> bool:
    return any(
        is_action_ref(step.get("uses"), "actions/checkout")
        for step in job_steps(job)
        if isinstance(step, dict)
    )


def command_text(step: dict[str, Any]) -> str:
    run = step.get("run")
    return run if isinstance(run, str) else ""


def dispatches_release_install_without_dry_run(command: str) -> bool:
    return (
        "gh workflow run" in command
        and RELEASE_INSTALL_WORKFLOW in command
        and DRY_RUN_FIELD_PATTERN.search(command) is None
    )


def invokes_release_install_script_without_dry_run(command: str) -> bool:
    return RELEASE_INSTALL_SCRIPT in command and "--dry-run" not in command


def job_has_direct_release_create(job: dict[str, Any]) -> bool:
    return any(
        "gh release create" in command_text(step)
        for step in job_steps(job)
        if isinstance(step, dict)
    )


def job_uses_action(job: dict[str, Any], action: str) -> bool:
    return any(
        is_action_ref(step.get("uses"), action)
        for step in job_steps(job)
        if isinstance(step, dict)
    )


def job_release_create_uses_github_app_token(job: dict[str, Any]) -> bool:
    token_step_ids = {
        str(step.get("id"))
        for step in job_steps(job)
        if isinstance(step, dict)
        and is_action_ref(step.get("uses"), "actions/create-github-app-token")
        and isinstance(step.get("id"), str)
        and str(step.get("id"))
    }
    if not token_step_ids:
        return False

    for step in job_steps(job):
        if not isinstance(step, dict):
            continue
        if "gh release create" not in command_text(step):
            continue
        env = step.get("env")
        if not isinstance(env, dict):
            continue
        for value in env.values():
            if not isinstance(value, str):
                continue
            if any(
                f"steps.{token_step_id}.outputs.token" in value
                for token_step_id in token_step_ids
            ):
                return True
    return False


def job_environment_name(job: dict[str, Any]) -> str | None:
    environment = job.get("environment")
    if isinstance(environment, str):
        return environment
    if isinstance(environment, dict):
        name = environment.get("name")
        return name if isinstance(name, str) else None
    return None


def collect_violations(root: Path = REPO_ROOT) -> list[str]:
    violations: list[str] = []
    for path in workflow_paths(root):
        relative_path = path.relative_to(root)
        payload = load_workflow(path)
        for violation in runner_policy_violations(payload):
            violations.append(f"{relative_path}: {violation}")
        for node in walk_mappings(payload):
            if grants_write_all(node.get("permissions")):
                violations.append(
                    f"{relative_path}: permissions must not use write-all; "
                    "use job-scoped least privilege instead."
                )

            uses = node.get("uses")
            inputs = node.get("with") if isinstance(node.get("with"), dict) else {}

            if is_action_ref(uses, "actions/setup-node"):
                node_version_file = inputs.get("node-version-file")
                if isinstance(node_version_file, str):
                    version_path = root / node_version_file
                    if not version_path.exists():
                        violations.append(
                            f"{relative_path}: actions/setup-node references missing "
                            f"node-version-file '{node_version_file}'; use node-version "
                            "when the version is repository policy."
                        )

            if is_action_ref(uses, "taiki-e/install-action") and "version" in inputs:
                tool = inputs.get("tool", "<missing tool>")
                version = inputs["version"]
                violations.append(
                    f"{relative_path}: taiki-e/install-action does not support "
                    f"with.version; use tool: {tool}@{version} instead."
                )

            run_text = command_text(node)
            if dispatches_release_install_without_dry_run(run_text):
                violations.append(
                    f"{relative_path}: public workflows must dispatch "
                    f"{RELEASE_INSTALL_WORKFLOW} with dry_run=true; use external "
                    "deployment automation for host-local installs."
                )
            if invokes_release_install_script_without_dry_run(run_text):
                violations.append(
                    f"{relative_path}: public workflows must call "
                    f"{RELEASE_INSTALL_SCRIPT} with --dry-run; use external "
                    "deployment automation for host-local installs."
                )

        if workflow_has_trigger(payload, "pull_request_target"):
            for _job_id, job in iter_jobs(payload):
                if job_uses_checkout(job):
                    violations.append(
                        f"{relative_path}: pull_request_target jobs must not checkout "
                        "repository code; split trusted writes from untrusted PR context."
                    )

        for job_id, job in iter_jobs(payload):
            if not job_has_direct_release_create(job):
                continue

            permissions = job_permissions(job, payload)
            uses_app_token = job_release_create_uses_github_app_token(job)
            if job_environment_name(job) != "release":
                violations.append(
                    f"{relative_path}: job '{job_id}' creates a GitHub release without "
                    "the release environment."
                )
            if uses_app_token:
                if job_uses_action(job, "actions/download-artifact") and not grants_permission(
                    permissions, "actions", "read"
                ):
                    violations.append(
                        f"{relative_path}: job '{job_id}' creates a GitHub release with "
                        "a GitHub App token without actions: read for artifact download."
                    )
            else:
                if not grants_permission(permissions, "contents", "write"):
                    violations.append(
                        f"{relative_path}: job '{job_id}' creates a GitHub release without "
                        "contents: write scoped to the publishing job."
                    )
                if not grants_permission(permissions, "id-token", "write"):
                    violations.append(
                        f"{relative_path}: job '{job_id}' creates a GitHub release without "
                        "id-token: write for release signing or provenance."
                    )
    return violations


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=REPO_ROOT,
        help="Repository root to check.",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    violations = collect_violations(args.repo_root.resolve())
    if violations:
        for violation in violations:
            print(violation, file=sys.stderr)
        return 1
    print("workflow-policy-ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
