#!/usr/bin/env python3
"""Resolve the authoritative Sedna release version for a commit."""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from functools import total_ordering
from pathlib import Path
from typing import Iterable


UPSTREAM_TAG_RE = re.compile(
    r"^rust-v(?P<version>[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z]+(?:\.[0-9A-Za-z]+)*)?)$"
)
SEDNA_TAG_RE = re.compile(
    r"^v(?P<track>[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z]+(?:\.[0-9A-Za-z]+)*)?)-sedna\.(?P<ordinal>[0-9]+)$"
)
RELEASE_MARKER_RE = re.compile(r"^Sedna-Release:\s*(?P<channel>[A-Za-z0-9_.-]+)\s*$")
VERSION_POLICY = "sedna-upstream-track-v1"


class ReleaseVersionError(RuntimeError):
    pass


@total_ordering
@dataclass(frozen=True)
class SemVer:
    major: int
    minor: int
    patch: int
    prerelease: tuple[str | int, ...] = ()

    @classmethod
    def parse(cls, value: str) -> "SemVer":
        main, separator, prerelease = value.partition("-")
        try:
            major, minor, patch = (int(part) for part in main.split("."))
        except ValueError as exc:
            raise ReleaseVersionError(f"invalid semantic version: {value}") from exc
        identifiers: list[str | int] = []
        if separator:
            for item in prerelease.split("."):
                identifiers.append(int(item) if item.isdigit() else item)
        return cls(major=major, minor=minor, patch=patch, prerelease=tuple(identifiers))

    @property
    def is_prerelease(self) -> bool:
        return bool(self.prerelease)

    def __str__(self) -> str:
        base = f"{self.major}.{self.minor}.{self.patch}"
        if not self.prerelease:
            return base
        suffix = ".".join(str(part) for part in self.prerelease)
        return f"{base}-{suffix}"

    def __lt__(self, other: object) -> bool:
        if not isinstance(other, SemVer):
            return NotImplemented
        base = (self.major, self.minor, self.patch)
        other_base = (other.major, other.minor, other.patch)
        if base != other_base:
            return base < other_base
        if not self.prerelease and other.prerelease:
            return False
        if self.prerelease and not other.prerelease:
            return True
        return self._prerelease_key() < other._prerelease_key()

    def _prerelease_key(self) -> tuple[tuple[int, int | str], ...]:
        key: list[tuple[int, int | str]] = []
        for item in self.prerelease:
            key.append((0, item) if isinstance(item, int) else (1, item))
        return tuple(key)


@dataclass(frozen=True)
class SednaTag:
    tag: str
    track: str
    ordinal: int


def git(repo: Path, *args: str, check: bool = True) -> str:
    proc = subprocess.run(
        ["git", "-C", str(repo), *args],
        check=False,
        capture_output=True,
        text=True,
    )
    if check and proc.returncode != 0:
        stderr = proc.stderr.strip()
        command = " ".join(args)
        raise ReleaseVersionError(f"git {command} failed: {stderr}")
    return proc.stdout.strip()


def resolve_commit(repo: Path, ref: str) -> str:
    return git(repo, "rev-parse", f"{ref}^{{commit}}")


def require_ancestor(repo: Path, ancestor: str, descendant_ref: str, description: str) -> None:
    proc = subprocess.run(
        ["git", "-C", str(repo), "merge-base", "--is-ancestor", ancestor, descendant_ref],
        check=False,
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        raise ReleaseVersionError(f"target commit is not on {description}: {ancestor}")


def commit_message(repo: Path, commit: str) -> str:
    return git(repo, "log", "-1", "--format=%B", commit)


def release_marker_channel(message: str) -> str | None:
    found: list[str] = []
    for line in message.splitlines():
        match = RELEASE_MARKER_RE.match(line)
        if match:
            found.append(match.group("channel").lower())
    if not found:
        return None
    if len(found) > 1:
        raise ReleaseVersionError("commit has more than one Sedna-Release marker")
    channel = found[0]
    if channel not in {"stable", "prerelease"}:
        raise ReleaseVersionError(
            "Sedna-Release marker must be either 'stable' or 'prerelease'"
        )
    return channel


def well_formed_upstream_tags(repo: Path, upstream_base_commit: str) -> list[tuple[SemVer, str]]:
    tags: list[tuple[SemVer, str]] = []
    for tag in git(repo, "tag", "--merged", upstream_base_commit, "--list", "rust-v*").splitlines():
        match = UPSTREAM_TAG_RE.match(tag)
        if not match:
            continue
        tags.append((SemVer.parse(match.group("version")), tag))
    return tags


def select_upstream_tag(repo: Path, upstream_base_commit: str) -> tuple[SemVer, str, int, bool]:
    candidates = well_formed_upstream_tags(repo, upstream_base_commit)
    if not candidates:
        raise ReleaseVersionError(
            f"no well-formed rust-v<semver> tag is reachable from {upstream_base_commit}"
        )
    version, tag = max(candidates, key=lambda item: item[0])
    distance = int(git(repo, "rev-list", "--count", f"{tag}..{upstream_base_commit}"))
    exact = resolve_commit(repo, tag) == upstream_base_commit
    return version, tag, distance, exact


def parse_sedna_tag(tag: str) -> SednaTag | None:
    match = SEDNA_TAG_RE.match(tag)
    if not match:
        return None
    return SednaTag(
        tag=tag,
        track=match.group("track"),
        ordinal=int(match.group("ordinal")),
    )


def local_sedna_tags(repo: Path) -> set[str]:
    return {
        tag
        for tag in git(repo, "tag", "--list", "v*-sedna.*").splitlines()
        if parse_sedna_tag(tag) is not None
    }


def github_release_tags(repository: str, mode: str) -> set[str]:
    if mode == "off" or not repository:
        return set()
    gh = shutil.which("gh")
    if gh is None:
        if mode == "required":
            raise ReleaseVersionError("gh is required to check existing GitHub releases")
        return set()
    proc = subprocess.run(
        [
            gh,
            "api",
            "--paginate",
            f"repos/{repository}/releases",
            "--jq",
            ".[].tag_name",
        ],
        check=False,
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        if mode == "required":
            raise ReleaseVersionError(
                f"failed to list GitHub releases for {repository}: {proc.stderr.strip()}"
            )
        print(f"warning: failed to list GitHub releases for {repository}: {proc.stderr.strip()}", file=sys.stderr)
        return set()
    return {line.strip() for line in proc.stdout.splitlines() if line.strip()}


def next_sedna_ordinal(existing_tags: Iterable[str], upstream_track: str) -> int:
    ordinals = [
        parsed.ordinal
        for tag in existing_tags
        if (parsed := parse_sedna_tag(tag)) is not None and parsed.track == upstream_track
    ]
    return max(ordinals, default=0) + 1


def resolve_release(
    *,
    repo: Path,
    target_sha: str,
    main_ref: str,
    upstream_ref: str,
    repository: str,
    channel: str,
    release_tag: str | None,
    current_release_tag: str | None,
    require_marker: bool,
    github_releases: str,
) -> dict[str, object]:
    target_commit = resolve_commit(repo, target_sha)
    require_ancestor(repo, target_commit, main_ref, main_ref)

    marker = release_marker_channel(commit_message(repo, target_commit))
    if require_marker and marker is None:
        return {
            "release_requested": False,
            "skip_reason": "missing_sedna_release_marker",
            "target_commit": target_commit,
            "version_policy": VERSION_POLICY,
        }

    effective_channel = channel
    if effective_channel == "auto":
        effective_channel = marker or "auto"
    if effective_channel == "auto" and release_tag:
        parsed_release_tag = parse_sedna_tag(release_tag)
        if parsed_release_tag is None:
            raise ReleaseVersionError(f"invalid Sedna release tag: {release_tag}")
        effective_channel = (
            "prerelease"
            if SemVer.parse(parsed_release_tag.track).is_prerelease
            else "stable"
        )
    if effective_channel == "auto":
        raise ReleaseVersionError("release channel could not be inferred")
    if effective_channel not in {"stable", "prerelease"}:
        raise ReleaseVersionError("release channel must be stable, prerelease, or auto")

    upstream_base_commit = git(repo, "merge-base", target_commit, upstream_ref)
    upstream_version, upstream_tag, upstream_distance, upstream_exact = select_upstream_tag(
        repo, upstream_base_commit
    )
    upstream_track = str(upstream_version)

    if effective_channel == "stable" and upstream_version.is_prerelease:
        raise ReleaseVersionError(
            f"stable Sedna releases cannot use prerelease upstream track {upstream_track}"
        )

    existing_tags = local_sedna_tags(repo) | github_release_tags(repository, github_releases)
    if current_release_tag:
        existing_tags.discard(current_release_tag)
    ordinal = next_sedna_ordinal(existing_tags, upstream_track)
    computed_release_tag = f"v{upstream_track}-sedna.{ordinal}"

    if release_tag:
        parsed = parse_sedna_tag(release_tag)
        if parsed is None:
            raise ReleaseVersionError(f"invalid Sedna release tag: {release_tag}")
        if release_tag != computed_release_tag:
            raise ReleaseVersionError(
                f"supplied release tag {release_tag} does not match computed tag {computed_release_tag}"
            )
    elif computed_release_tag in existing_tags:
        raise ReleaseVersionError(f"computed release tag already exists: {computed_release_tag}")

    release_version = computed_release_tag.removeprefix("v")
    downstream_short = git(repo, "rev-parse", "--short=8", target_commit)
    upstream_short = git(repo, "rev-parse", "--short=8", upstream_base_commit)
    build_provenance = f"up:{upstream_short} down:{downstream_short}"

    return {
        "release_requested": True,
        "skip_reason": "",
        "version_policy": VERSION_POLICY,
        "release_marker": marker or "",
        "release_channel": effective_channel,
        "release_tag": computed_release_tag,
        "release_version": release_version,
        "github_prerelease": effective_channel == "prerelease",
        "upstream_track": upstream_track,
        "upstream_base_commit": upstream_base_commit,
        "upstream_base_commit_short": upstream_short,
        "upstream_base_tag": upstream_tag,
        "upstream_base_tag_exact": upstream_exact,
        "upstream_distance_from_tag": upstream_distance,
        "downstream_commit": target_commit,
        "downstream_commit_short": downstream_short,
        "target_commit": target_commit,
        "build_provenance": build_provenance,
        "version_display": f"{release_version} ({build_provenance})",
    }


def resolve_safe_output_path(path: str, workspace: Path) -> Path:
    workspace_resolved = workspace.resolve()
    candidate = Path(path).expanduser().resolve()
    try:
        candidate.relative_to(workspace_resolved)
    except ValueError as exc:
        raise ReleaseVersionError(
            f"github output path must be within workspace: {workspace_resolved}"
        ) from exc
    return candidate


def write_github_output(payload: dict[str, object], path: str | None) -> None:
    if not path:
        return
    safe_path = resolve_safe_output_path(path, Path.cwd())
    with safe_path.open("a", encoding="utf-8") as handle:
        for key, value in payload.items():
            if isinstance(value, bool):
                rendered = "true" if value else "false"
            else:
                rendered = str(value)
            handle.write(f"{key}={rendered}\n")


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    parser.add_argument("--target-sha", default="HEAD")
    parser.add_argument("--main-ref", default="refs/remotes/origin/main")
    parser.add_argument("--upstream-ref", default="refs/remotes/origin/upstream-main")
    parser.add_argument("--repository", default="")
    parser.add_argument(
        "--channel",
        choices=("stable", "prerelease", "auto"),
        default="auto",
    )
    parser.add_argument("--release-tag", default=None)
    parser.add_argument("--current-release-tag", default=None)
    parser.add_argument("--require-marker", action="store_true")
    parser.add_argument(
        "--github-releases",
        choices=("required", "best-effort", "off"),
        default="best-effort",
    )
    parser.add_argument("--format", choices=("json",), default="json")
    parser.add_argument("--github-output", default=os.environ.get("GITHUB_OUTPUT"))
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    try:
        payload = resolve_release(
            repo=args.repo,
            target_sha=args.target_sha,
            main_ref=args.main_ref,
            upstream_ref=args.upstream_ref,
            repository=args.repository,
            channel=args.channel,
            release_tag=args.release_tag,
            current_release_tag=args.current_release_tag,
            require_marker=args.require_marker,
            github_releases=args.github_releases,
        )
    except ReleaseVersionError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1

    write_github_output(payload, args.github_output)
    print(json.dumps(payload, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
