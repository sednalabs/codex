#!/usr/bin/env python3
"""Decide whether a scheduled workflow can reuse an equivalent green run."""

from __future__ import annotations

import argparse
import io
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
import zipfile
from collections.abc import Callable, Iterable
from pathlib import Path
from typing import Any

REDIRECT_STATUS_CODES = {301, 302, 303, 307, 308}
GITHUB_API_HOST = "api.github.com"


def parse_run_id(value: str | int | None) -> int | None:
    if value in {None, ""}:
        return None
    try:
        return int(value)
    except (TypeError, ValueError) as exc:
        raise ValueError(f"run ids must be integers: {value!r}") from exc


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


def workflow_run_artifacts_url(api_url: str, repo: str, run_id: int) -> str:
    base = api_url.rstrip("/")
    return f"{base}/repos/{repo}/actions/runs/{run_id}/artifacts?per_page=100"


def github_api_headers(token: str) -> dict[str, str]:
    headers = {
        "Accept": "application/vnd.github+json",
        "X-GitHub-Api-Version": "2022-11-28",
        "User-Agent": "sedna-codex-workflow-dedupe",
    }
    if token:
        headers["Authorization"] = f"Bearer {token}"
    return headers


def validated_github_api_url(url: str) -> str:
    parsed = urllib.parse.urlparse(url)
    if parsed.scheme != "https":
        raise ValueError("GitHub API requests must use https")
    if parsed.username or parsed.password:
        raise ValueError("GitHub API URLs must not include credentials")
    if (parsed.hostname or "").lower() != GITHUB_API_HOST:
        raise ValueError(f"GitHub API requests must target {GITHUB_API_HOST}")
    if not parsed.path.startswith("/repos/"):
        raise ValueError("GitHub API request path must stay under /repos/")
    return urllib.parse.urlunsplit(
        ("https", GITHUB_API_HOST, parsed.path, parsed.query, "")
    )


def api_get_json(url: str, token: str) -> dict[str, Any]:
    request = urllib.request.Request(
        validated_github_api_url(url),
        headers=github_api_headers(token),
    )
    with urllib.request.urlopen(request, timeout=20) as response:
        return json.loads(response.read().decode("utf-8"))


class NoRedirectHandler(urllib.request.HTTPRedirectHandler):
    def redirect_request(
        self,
        req: urllib.request.Request,
        fp: Any,
        code: int,
        msg: str,
        headers: Any,
        newurl: str,
    ) -> urllib.request.Request | None:
        return None


def artifact_download_headers(url: str, api_host: str, token: str) -> dict[str, str]:
    headers = github_api_headers(token)
    if urllib.parse.urlparse(url).netloc != api_host:
        headers.pop("Accept", None)
        headers.pop("Authorization", None)
        headers.pop("X-GitHub-Api-Version", None)
    return headers


def api_get_bytes(url: str, token: str) -> bytes:
    opener = urllib.request.build_opener(NoRedirectHandler)
    current_url = validated_github_api_url(url)
    api_host = urllib.parse.urlparse(current_url).netloc
    for _ in range(5):
        request = urllib.request.Request(
            current_url,
            headers=artifact_download_headers(current_url, api_host, token),
        )
        try:
            with opener.open(request, timeout=20) as response:
                return response.read()
        except urllib.error.HTTPError as exc:
            location = exc.headers.get("Location") if exc.headers else None
            if exc.code not in REDIRECT_STATUS_CODES or not location:
                raise
            current_url = urllib.parse.urljoin(current_url, location)
    raise RuntimeError("artifact archive download followed too many redirects")



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


def fetch_run_artifacts(
    *,
    api_url: str,
    repo: str,
    run_id: int,
    token: str,
) -> list[dict[str, Any]]:
    payload = api_get_json(workflow_run_artifacts_url(api_url, repo, run_id), token)
    artifacts = payload.get("artifacts")
    if not isinstance(artifacts, list):
        raise RuntimeError("GitHub API response did not include artifacts")
    return [artifact for artifact in artifacts if isinstance(artifact, dict)]


def artifact_by_name(artifacts: Iterable[dict[str, Any]], name: str) -> dict[str, Any] | None:
    for artifact in artifacts:
        if artifact.get("name") != name:
            continue
        if artifact.get("expired"):
            continue
        archive_url = artifact.get("archive_download_url")
        if not archive_url:
            continue
        return artifact
    return None


def read_json_from_artifact_zip(archive: bytes) -> dict[str, Any]:
    with zipfile.ZipFile(io.BytesIO(archive)) as archive_zip:
        for name in sorted(archive_zip.namelist()):
            if name.endswith("/") or not name.endswith(".json"):
                continue
            payload = json.loads(archive_zip.read(name).decode("utf-8"))
            if not isinstance(payload, dict):
                raise RuntimeError(f"artifact JSON was not an object: {name}")
            return payload
    raise RuntimeError("artifact archive did not contain a JSON file")


def fetch_summary_artifact_payload(
    *,
    api_url: str,
    repo: str,
    run_id: int,
    artifact_name: str,
    token: str,
) -> dict[str, Any] | None:
    artifacts = fetch_run_artifacts(api_url=api_url, repo=repo, run_id=run_id, token=token)
    artifact = artifact_by_name(artifacts, artifact_name)
    if artifact is None:
        return None
    archive = api_get_bytes(str(artifact["archive_download_url"]), token)
    return read_json_from_artifact_zip(archive)


def json_bool(value: Any) -> bool:
    if isinstance(value, bool):
        return value
    return str(value or "").strip().lower() in {"1", "true", "yes", "on"}


def validation_summary_matches(
    payload: dict[str, Any],
    *,
    planner_fingerprint: str,
) -> bool:
    selection = payload.get("selection")
    summary = payload.get("summary")
    dedupe = payload.get("dedupe")
    if not isinstance(selection, dict) or not isinstance(summary, dict):
        return False
    if str(selection.get("planner_fingerprint") or "") != planner_fingerprint:
        return False
    if str(summary.get("overall_conclusion") or "") != "success":
        return False
    if isinstance(dedupe, dict) and json_bool(dedupe.get("should_skip")):
        return False
    return True


def find_equivalent_success(
    runs: Iterable[dict[str, Any]],
    *,
    branch: str,
    head_sha: str,
    current_run_id: int | None,
    allowed_events: set[str],
    metadata_matcher: Callable[[dict[str, Any]], bool] | None = None,
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
        if metadata_matcher is not None and not metadata_matcher(run):
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
    parser.add_argument("--summary-artifact-name", default="")
    parser.add_argument("--required-planner-fingerprint", default="")
    parser.add_argument("--github-output", default=os.environ.get("GITHUB_OUTPUT", ""))
    parser.add_argument("--api-url", default=os.environ.get("GITHUB_API_URL", "https://api.github.com"))
    parser.add_argument("--token", default=os.environ.get("GITHUB_TOKEN", ""))
    args = parser.parse_args()

    allowed_events = {event for event in args.allowed_events.split(",") if event}

    try:
        current_run_id = parse_run_id(args.current_run_id)
        runs = fetch_successful_runs(
            api_url=args.api_url,
            repo=args.repo,
            workflow=args.workflow,
            branch=args.branch,
            token=args.token,
        )
        metadata_matcher = None
        if args.required_planner_fingerprint:
            artifact_name = args.summary_artifact_name or "validation-summary"

            def metadata_matcher(run: dict[str, Any]) -> bool:
                run_id = parse_run_id(run.get("id"))
                if run_id is None:
                    return False
                payload = fetch_summary_artifact_payload(
                    api_url=args.api_url,
                    repo=args.repo,
                    run_id=run_id,
                    artifact_name=artifact_name,
                    token=args.token,
                )
                if payload is None:
                    return False
                return validation_summary_matches(
                    payload,
                    planner_fingerprint=args.required_planner_fingerprint,
                )

        match = find_equivalent_success(
            runs,
            branch=args.branch,
            head_sha=args.head_sha,
            current_run_id=current_run_id,
            allowed_events=allowed_events,
            metadata_matcher=metadata_matcher,
        )
        outputs = result_from_match(match)
        if match is not None and args.required_planner_fingerprint:
            outputs["reason"] = "exact_plan_success_found"
    except Exception as exc:  # noqa: BLE001 - scheduled CI must fail open.
        outputs = fail_open_result(str(exc))

    write_outputs(args.github_output, outputs)
    print(json.dumps(outputs, sort_keys=True))


if __name__ == "__main__":
    main()
