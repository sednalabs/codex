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
    created_at: str
    author: str
    html_url: str


DEPENDABOT_TITLE = re.compile(r"^Bump (.+?) from .+? to .+?(?: in (/.+))?$", re.I)
DEPENDABOT_BODY = re.compile(r"Bumps? \[([^]]+)\]", re.I)
DEPENDABOT_VERSION = re.compile(r"dependency-version:\s*([0-9][0-9A-Za-z.+_-]*)", re.I)
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
    dependency = body_match.group(1).strip() if body_match else (title_match.group(1).strip() if title_match else None)
    if not dependency:
        return None
    directory = title_match.group(2).strip() if title_match and title_match.group(2) else ""
    return f"{dependency.lower()}::{directory.lower()}"


def dependabot_version(pr: PullRequest) -> tuple[int, ...]:
    match = DEPENDABOT_VERSION.search(pr.body)
    if not match:
        title_match = DEPENDABOT_TITLE.match(pr.title)
        if not title_match:
            return (0,)
        version = title_match.group(0).rsplit(" to ", 1)[-1].split(" in ", 1)[0]
    else:
        version = match.group(1)
    numbers = tuple(int(part) for part in re.findall(r"\d+", version))
    return numbers or (0,)


def models_key(pr: PullRequest) -> bool:
    return pr.head_ref.startswith("bot/sync-models-json-")


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
        ordered = sorted(candidates, key=lambda pr: (dependabot_version(pr), pr.created_at, pr.number), reverse=True)
        newest = ordered[0]
        for stale in ordered[1:]:
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
