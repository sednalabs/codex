#!/usr/bin/env python3
"""Decide whether a scheduled workflow can reuse an equivalent green run."""

from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.parse
import urllib.request
from collections.abc import Iterable
from pathlib import Path
from typing import Any


def parse_run_id(value: str | int | None) -> int | None:
    if value in {None, ""}:
        return None
    try:
        return int(value)
    except (TypeError, ValueError) as exc:
        raise SystemExit(f"run ids must be integers: {value!r}") from exc


def write_outputs(path: str | None, outputs: dict[str, str]) -> None:
    if not path:
        return
    with Path(path).open("a", encoding="utf-8") as handle:
        for key, value in outputs.items():
            handle.write(f"{key}={value}\n")


def workflow_runs_url(api_url: str, repo: str, workflow: str, branch: str) -> str:
    base = api_url.rstrip("/")
    quoted_workflow = urllib.parse.quote(workflow, safe="")
    query = urllib.parse.urlencode(
        {
            "branch": branch,
            "status": "success",
            "per_page": "100",
        }
    )
    return f"{base}/repos/{repo}/actions/workflows/{quoted_workflow}/runs?{query}"


def api_get_json(url: str, token: str) -> dict[str, Any]:
    headers = {
        "Accept": "application/vnd.github+json",
        "X-GitHub-Api-Version": "2022-11-28",
        "User-Agent": "sedna-codex-workflow-dedupe",
    }
    if token:
        headers["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(url, headers=headers)
    with urllib.request.urlopen(request, timeout=20) as response:
        return json.loads(response.read().decode("utf-8"))


def fetch_successful_runs(
    *,
    api_url: str,
    repo: str,
    workflow: str,
    branch: str,
    token: str,
) -> list[dict[str, Any]]:
    payload = api_get_json(workflow_runs_url(api_url, repo, workflow, branch), token)
    runs = payload.get("workflow_runs")
    if not isinstance(runs, list):
        raise RuntimeError("GitHub API response did not include workflow_runs")
    return [run for run in runs if isinstance(run, dict)]


def find_equivalent_success(
    runs: Iterable[dict[str, Any]],
    *,
    branch: str,
    head_sha: str,
    current_run_id: int | None,
    allowed_events: set[str],
) -> dict[str, Any] | None:
    for run in runs:
        run_id = parse_run_id(run.get("id"))
        if current_run_id is not None and run_id == current_run_id:
            continue
        if run.get("head_branch") != branch:
            continue
        if run.get("head_sha") != head_sha:
            continue
        if run.get("status") != "completed":
            continue
        if run.get("conclusion") != "success":
            continue
        if allowed_events and str(run.get("event") or "") not in allowed_events:
            continue
        return run
    return None


def result_from_match(match: dict[str, Any] | None) -> dict[str, str]:
    if match is None:
        return {
            "should_skip": "false",
            "should_run": "true",
            "reason": "no_equivalent_success",
            "matched_run_id": "",
            "matched_run_url": "",
            "matched_run_event": "",
            "matched_run_created_at": "",
        }
    return {
        "should_skip": "true",
        "should_run": "false",
        "reason": "equivalent_success_found",
        "matched_run_id": str(match.get("id") or ""),
        "matched_run_url": str(match.get("html_url") or ""),
        "matched_run_event": str(match.get("event") or ""),
        "matched_run_created_at": str(match.get("created_at") or ""),
    }


def fail_open_result(message: str) -> dict[str, str]:
    print(f"::warning title=Workflow dedupe lookup failed::{message}", file=sys.stderr)
    return {
        "should_skip": "false",
        "should_run": "true",
        "reason": "lookup_failed_run_conservatively",
        "matched_run_id": "",
        "matched_run_url": "",
        "matched_run_event": "",
        "matched_run_created_at": "",
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", required=True)
    parser.add_argument("--workflow", required=True)
    parser.add_argument("--branch", required=True)
    parser.add_argument("--head-sha", required=True)
    parser.add_argument("--current-run-id", default="")
    parser.add_argument("--allowed-events", default="")
    parser.add_argument("--github-output", default=os.environ.get("GITHUB_OUTPUT", ""))
    parser.add_argument("--api-url", default=os.environ.get("GITHUB_API_URL", "https://api.github.com"))
    parser.add_argument("--token", default=os.environ.get("GITHUB_TOKEN", ""))
    args = parser.parse_args()

    allowed_events = {event for event in args.allowed_events.split(",") if event}
    current_run_id = parse_run_id(args.current_run_id)

    try:
        runs = fetch_successful_runs(
            api_url=args.api_url,
            repo=args.repo,
            workflow=args.workflow,
            branch=args.branch,
            token=args.token,
        )
        match = find_equivalent_success(
            runs,
            branch=args.branch,
            head_sha=args.head_sha,
            current_run_id=current_run_id,
            allowed_events=allowed_events,
        )
        outputs = result_from_match(match)
    except Exception as exc:  # noqa: BLE001 - scheduled CI must fail open.
        outputs = fail_open_result(str(exc))

    write_outputs(args.github_output, outputs)
    print(json.dumps(outputs, sort_keys=True))


if __name__ == "__main__":
    main()
