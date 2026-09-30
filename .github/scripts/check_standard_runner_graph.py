#!/usr/bin/env python3
"""Fail closed unless a bounded reusable-workflow graph uses standard runners."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from pathlib import Path
import re
import sys


ALLOWED_RUNNERS = frozenset({"ubuntu-24.04", "ubuntu-24.04-arm"})
LOCAL_WORKFLOW_PREFIX = "./.github/workflows/"
WORKFLOW_NAME = re.compile(r"[A-Za-z0-9_.-]+\.ya?ml\Z")
JOB_NAME = re.compile(r"([A-Za-z0-9_-]+):(?:\s*(?:#.*)?)\Z")
FIELD = re.compile(r"(runs-on|uses):\s*(.*?)\s*(?:#.*)?\Z")


class RunnerGraphError(RuntimeError):
    """The workflow graph cannot be proven to use only allowed runners."""


@dataclass(frozen=True)
class Job:
    name: str
    runner: str | None
    reusable_workflow: str | None


def parse_jobs(path: Path) -> list[Job]:
    """Parse only job-level runner and reusable-workflow fields, failing closed."""
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError as exc:
        raise RunnerGraphError(f"cannot read workflow {path}: {exc}") from exc

    jobs_started = False
    current_job: str | None = None
    fields: dict[str, dict[str, str]] = {}
    for line_number, line in enumerate(lines, start=1):
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        indent = len(line) - len(line.lstrip(" "))
        if indent == 0:
            if stripped == "jobs:":
                jobs_started = True
                continue
            if jobs_started:
                break
        if not jobs_started:
            continue
        if indent == 2:
            match = JOB_NAME.fullmatch(stripped)
            if match is None:
                raise RunnerGraphError(
                    f"{path}:{line_number}: unsupported job mapping syntax"
                )
            current_job = match.group(1)
            if current_job in fields:
                raise RunnerGraphError(f"{path}:{line_number}: duplicate job {current_job}")
            fields[current_job] = {}
            continue
        if indent == 4 and current_job is not None:
            match = FIELD.fullmatch(stripped)
            if match is not None:
                key, value = match.groups()
                if key in fields[current_job]:
                    raise RunnerGraphError(
                        f"{path}:{line_number}: duplicate {key} in job {current_job}"
                    )
                fields[current_job][key] = value

    if not jobs_started or not fields:
        raise RunnerGraphError(f"{path}: missing or unsupported jobs mapping")

    jobs: list[Job] = []
    for name, values in fields.items():
        runner = values.get("runs-on")
        reusable = values.get("uses")
        if runner is not None and reusable is not None:
            raise RunnerGraphError(
                f"{path}#{name}: job cannot declare both runs-on and uses"
            )
        if runner is None and reusable is None:
            raise RunnerGraphError(
                f"{path}#{name}: job has neither a literal runner nor a local reusable workflow"
            )
        jobs.append(Job(name=name, runner=runner, reusable_workflow=reusable))
    return jobs


def check_workflow_graph(repo_root: Path, workflow_path: Path) -> list[tuple[Path, str, str]]:
    """Return literal standard runner assignments or raise on any unsafe edge."""
    root = repo_root.resolve()
    workflow = workflow_path.resolve()
    try:
        workflow.relative_to(root)
    except ValueError as exc:
        raise RunnerGraphError(f"workflow escapes repository root: {workflow}") from exc

    visited: set[Path] = set()
    assignments: list[tuple[Path, str, str]] = []

    def walk(path: Path, stack: tuple[Path, ...]) -> None:
        resolved = path.resolve()
        try:
            resolved.relative_to(root)
        except ValueError as exc:
            raise RunnerGraphError(f"workflow escapes repository root: {resolved}") from exc
        if resolved in stack:
            cycle = " -> ".join(str(item.relative_to(root)) for item in (*stack, resolved))
            raise RunnerGraphError(f"reusable workflow cycle: {cycle}")
        if resolved in visited:
            return
        visited.add(resolved)
        for job in parse_jobs(resolved):
            if job.runner is not None:
                if job.runner not in ALLOWED_RUNNERS:
                    raise RunnerGraphError(
                        f"{resolved.relative_to(root)}#{job.name}: "
                        f"runner must be one of {sorted(ALLOWED_RUNNERS)}, "
                        f"got {job.runner!r}"
                    )
                assignments.append((resolved.relative_to(root), job.name, job.runner))
                continue

            assert job.reusable_workflow is not None
            use = job.reusable_workflow
            if not use.startswith(LOCAL_WORKFLOW_PREFIX):
                raise RunnerGraphError(
                    f"{resolved.relative_to(root)}#{job.name}: "
                    f"external or dynamic reusable workflow is not allowed: {use!r}"
                )
            child_name = use[len(LOCAL_WORKFLOW_PREFIX) :]
            if WORKFLOW_NAME.fullmatch(child_name) is None:
                raise RunnerGraphError(
                    f"{resolved.relative_to(root)}#{job.name}: "
                    f"unsafe local reusable workflow path: {use!r}"
                )
            child = root / ".github" / "workflows" / child_name
            walk(child, (*stack, resolved))

    walk(workflow, ())
    return assignments


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("workflows", nargs="+", help="workflow paths relative to repository root")
    args = parser.parse_args(argv)
    repo_root = Path.cwd()
    try:
        assignments: list[tuple[Path, str, str]] = []
        for relative_path in args.workflows:
            assignments.extend(
                check_workflow_graph(repo_root, repo_root / relative_path)
            )
    except RunnerGraphError as exc:
        print(f"runner graph rejected: {exc}", file=sys.stderr)
        return 1

    for path, job, runner in assignments:
        print(f"{path}#{job}: {runner}")
    print(f"verified {len(assignments)} literal standard runner assignments")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
