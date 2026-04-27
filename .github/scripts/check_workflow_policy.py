#!/usr/bin/env python3
"""Static policy checks for GitHub workflow files."""

from __future__ import annotations

import argparse
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


def is_action_ref(uses: Any, action: str) -> bool:
    return isinstance(uses, str) and uses.startswith(f"{action}@")


def uses_self_hosted_runner(runs_on: Any) -> bool:
    if isinstance(runs_on, str):
        return runs_on == "self-hosted"
    if isinstance(runs_on, list):
        return "self-hosted" in runs_on
    return False


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


def job_has_direct_release_create(job: dict[str, Any]) -> bool:
    return any(
        "gh release create" in command_text(step)
        for step in job_steps(job)
        if isinstance(step, dict)
    )


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
        for node in walk_mappings(payload):
            if uses_self_hosted_runner(node.get("runs-on")):
                violations.append(
                    f"{relative_path}: public workflows must not use self-hosted runners; "
                    "use private deployment infrastructure for host-local operations."
                )

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
            if job_environment_name(job) != "release":
                violations.append(
                    f"{relative_path}: job '{job_id}' creates a GitHub release without "
                    "the release environment."
                )
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
