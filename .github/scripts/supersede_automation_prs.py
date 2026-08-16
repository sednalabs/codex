#!/usr/bin/env python3
"""Close stale automation PRs when a newer PR supersedes the same change."""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass


@dataclass(frozen=True)
class PullRequest:
    number: int
    title: str
    body: str
    head_ref: str
    head_repo: str
    created_at: str
    author: str
    html_url: str


@dataclass(frozen=True)
class DependabotVersion:
    release: tuple[int, ...]
    prerelease: bool
    constraint: str = ""


DEPENDABOT_TITLE = re.compile(
    r"^(?:(?:chore|build)\(deps(?:-dev)?\):\s*)?"
    r"(?:Bump|Update) (?P<dependency>.+?)(?: requirement)? from \S+ "
    r"to (?P<target>\S+)(?: in (?P<directory>/.+))?$",
    re.I,
)
DEPENDABOT_BODY = re.compile(r"Bumps? \[([^]]+)\]", re.I)
DEPENDABOT_VERSION = re.compile(r"dependency-version:\s*(\S+)", re.I)
STABLE_VERSION = re.compile(r"^(0|[1-9][0-9]*)(?:\.(0|[1-9][0-9]*))*$")
PRERELEASE_VERSION = re.compile(
    r"^(?P<release>(?:0|[1-9][0-9]*)(?:\.(?:0|[1-9][0-9]*))*)"
    r"-(?P<prerelease>[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)$"
)
GITHUB_API_BASE = "https://api.github.com/repos/sednalabs/codex"


class GitHub:
    def __init__(self, token: str) -> None:
        self.base = GITHUB_API_BASE
        self.request_headers = {
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {token}",
            "X-GitHub-Api-Version": "2022-11-28",
        }

    def request(self, method: str, path: str, payload: object | None = None) -> object:
        body = None if payload is None else json.dumps(payload).encode()
        request = urllib.request.Request(
            self.base + path,
            data=body,
            headers={**self.request_headers, "Content-Type": "application/json"},
            method=method,
        )
        try:
            with urllib.request.urlopen(request) as response:
                raw = response.read()
        except urllib.error.HTTPError as error:
            detail = error.read().decode("utf-8", errors="replace")
            raise RuntimeError(f"GitHub API {method} {path} failed ({error.code}): {detail}") from error
        return json.loads(raw) if raw else None

    def open_pull_requests(self) -> list[PullRequest]:
        result: list[PullRequest] = []
        for page in range(1, 11):
            query = urllib.parse.urlencode({"state": "open", "base": "main", "per_page": 100, "page": page})
            payload = self.request("GET", f"/pulls?{query}")
            if not isinstance(payload, list) or not payload:
                break
            result.extend(
                PullRequest(
                    number=item["number"],
                    title=item.get("title", ""),
                    body=item.get("body") or "",
                    head_ref=item.get("head", {}).get("ref", ""),
                    head_repo=(item.get("head", {}).get("repo") or {}).get("full_name", ""),
                    created_at=item.get("created_at", ""),
                    author=item.get("user", {}).get("login", ""),
                    html_url=item.get("html_url", ""),
                )
                for item in payload
            )
            if len(payload) < 100:
                break
        return result

def dependabot_key(pr: PullRequest) -> str | None:
    if pr.author.lower() not in {"dependabot[bot]", "dependabot"}:
        return None
    body_match = DEPENDABOT_BODY.search(pr.body)
    title_match = DEPENDABOT_TITLE.match(pr.title)
    dependency = (
        body_match.group(1).strip()
        if body_match
        else (title_match.group("dependency").strip() if title_match else None)
    )
    if not dependency:
        return None
    directory = (
        title_match.group("directory").strip()
        if title_match and title_match.group("directory")
        else ""
    )
    target = title_match.group("target") if title_match else ""
    constraint = ">=" if target.startswith(">=") else ""
    return f"{dependency.lower()}::{directory.lower()}::{constraint}"


def dependabot_version(pr: PullRequest) -> DependabotVersion | None:
    match = DEPENDABOT_VERSION.search(pr.body)
    if not match:
        title_match = DEPENDABOT_TITLE.match(pr.title)
        if not title_match:
            return None
        version = title_match.group("target")
    else:
        version = match.group(1)
    constraint = ">=" if version.startswith(">=") else ""
    if constraint:
        version = version[len(constraint) :]
    elif version[:1] in "<>=!~":
        return None
    if STABLE_VERSION.fullmatch(version):
        return DependabotVersion(normalize_release(version), prerelease=False, constraint=constraint)
    prerelease_match = PRERELEASE_VERSION.fullmatch(version)
    if prerelease_match:
        return DependabotVersion(
            normalize_release(prerelease_match.group("release")),
            prerelease=True,
            constraint=constraint,
        )
    return None


def normalize_release(version: str) -> tuple[int, ...]:
    release = [int(part) for part in version.split(".")]
    while len(release) > 1 and release[-1] == 0:
        release.pop()
    return tuple(release)


def strictly_supersedes(candidate: DependabotVersion, other: DependabotVersion) -> bool:
    if candidate.constraint != other.constraint:
        return False
    if candidate.prerelease:
        return False
    if other.prerelease:
        return candidate.release >= other.release
    return candidate.release > other.release


def models_key(pr: PullRequest) -> bool:
    return (
        pr.author.lower() == "github-actions[bot]"
        and pr.head_repo.lower() == "sednalabs/codex"
        and pr.head_ref.startswith("bot/sync-models-json-")
    )


def supersession_plan(prs: list[PullRequest], mode: str) -> list[dict[str, object]]:
    groups: dict[str, list[PullRequest]] = {}
    if mode == "dependabot":
        for pr in prs:
            key = dependabot_key(pr)
            if key:
                groups.setdefault(key, []).append(pr)
    elif mode == "models-json":
        candidates = [pr for pr in prs if models_key(pr)]
        if len(candidates) > 1:
            groups["models.json"] = candidates
    else:
        raise ValueError(f"unsupported mode: {mode}")

    plan: list[dict[str, object]] = []
    for key, candidates in groups.items():
        if mode == "models-json":
            ordered = sorted(candidates, key=lambda pr: (pr.created_at, pr.number), reverse=True)
            newest = ordered[0]
            stale_candidates = ordered[1:]
        else:
            versions = [(pr, dependabot_version(pr)) for pr in candidates]
            if any(version is None for _, version in versions):
                continue
            comparable_versions = [(pr, version) for pr, version in versions if version is not None]
            winners = [
                pr
                for pr, version in comparable_versions
                if all(
                    pr == other_pr or strictly_supersedes(version, other_version)
                    for other_pr, other_version in comparable_versions
                )
            ]
            if len(winners) != 1:
                continue
            newest = winners[0]
            stale_candidates = [pr for pr in candidates if pr != newest]
        for stale in stale_candidates:
            message = (
                f"Closing this superseded automation PR. The newer PR #{newest.number} "
                f"is the current {key.split('::', 1)[0]} update: {newest.html_url}."
            )
            plan.append({"number": stale.number, "message": message})
    return plan


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--mode", choices=("dependabot", "models-json"), required=True)
    parser.add_argument("--token", default=os.environ.get("GITHUB_TOKEN"))
    args = parser.parse_args()
    if not args.token:
        parser.error("GITHUB_TOKEN is required")
    github = GitHub(args.token)
    plan = supersession_plan(github.open_pull_requests(), args.mode)
    print(json.dumps({"mode": args.mode, "actions": plan}, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    sys.exit(main())
