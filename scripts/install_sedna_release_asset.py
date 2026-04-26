#!/usr/bin/env python3
"""Install a published Sedna Codex release from GitHub release assets."""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable


TARGET = "x86_64-unknown-linux-gnu"
TAG_RE = re.compile(r"^v[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.]+)*-sedna\.[0-9]+$")
REQUIRED_ASSET_NAMES = ("SHA256SUMS.txt", "RELEASE-METADATA.json")


class InstallError(RuntimeError):
    """Raised when a release cannot be safely installed."""


@dataclass(frozen=True)
class ReleaseAssets:
    archive_name: str
    archive_bytes: bytes
    checksums: bytes
    metadata: bytes


@dataclass(frozen=True)
class InstallPaths:
    install_root: Path
    bin_dir: Path

    @property
    def releases_dir(self) -> Path:
        return self.install_root / "releases"

    @property
    def current_link(self) -> Path:
        return self.install_root / "current"

    @property
    def visible_codex(self) -> Path:
        return self.bin_dir / "codex"

    @property
    def backups_dir(self) -> Path:
        return self.install_root / "backups"


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    tag = validate_tag(args.release_tag)
    repository = args.repository.strip()
    if repository.lower() != "sednalabs/codex":
        raise InstallError(f"repository must be sednalabs/codex, got {repository!r}")

    paths = InstallPaths(
        install_root=Path(args.install_root).expanduser(),
        bin_dir=Path(args.bin_dir).expanduser(),
    )
    assets = (
        load_assets_from_dir(Path(args.asset_dir), tag)
        if args.asset_dir
        else download_release_assets(repository, tag, allow_prerelease=args.allow_prerelease)
    )
    verify_assets(repository, tag, assets)
    release_dir = stage_or_reuse_release(paths, tag, assets, dry_run=args.dry_run)
    if args.dry_run:
        print(f"dry-run: verified {repository}@{tag}; would install to {release_dir}")
        return 0

    promote_release(paths, release_dir)
    version_output = verify_visible_codex(paths.visible_codex, tag)
    print(f"installed {repository}@{tag} into {release_dir}")
    print(version_output)
    return 0


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", default=os.environ.get("GITHUB_REPOSITORY", ""))
    parser.add_argument("--release-tag", required=True)
    parser.add_argument("--install-root", default="~/.codex/packages/standalone")
    parser.add_argument("--bin-dir", default="~/.local/bin")
    parser.add_argument("--asset-dir", help="Use already downloaded assets instead of GitHub API")
    parser.add_argument("--allow-prerelease", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    return parser.parse_args(argv)


def validate_tag(tag: str) -> str:
    tag = tag.strip()
    if not TAG_RE.fullmatch(tag):
        raise InstallError(f"release tag must look like v0.124.0-sedna.2, got {tag!r}")
    return tag


def release_version(tag: str) -> str:
    return tag.removeprefix("v")


def expected_archive_name(tag: str) -> str:
    return f"codex-sedna-{release_version(tag)}-{TARGET}.tar.gz"


def download_release_assets(repository: str, tag: str, *, allow_prerelease: bool) -> ReleaseAssets:
    token = os.environ.get("GITHUB_TOKEN", "").strip()
    api_url = f"https://api.github.com/repos/{repository}/releases/tags/{urllib.parse.quote(tag)}"
    release = json.loads(fetch_url(api_url, token=token).decode("utf-8"))
    if release.get("draft"):
        raise InstallError(f"refusing draft release {tag}")
    if release.get("prerelease") and not allow_prerelease:
        raise InstallError(f"refusing prerelease {tag}; pass --allow-prerelease to override")
    if release.get("tag_name") != tag:
        raise InstallError(f"release API returned tag {release.get('tag_name')!r}, expected {tag!r}")

    assets_by_name = {asset["name"]: asset for asset in release.get("assets", [])}
    archive_name = expected_archive_name(tag)
    required = (archive_name, *REQUIRED_ASSET_NAMES)
    missing = [name for name in required if name not in assets_by_name]
    if missing:
        raise InstallError(f"release {tag} is missing required assets: {', '.join(missing)}")

    return ReleaseAssets(
        archive_name=archive_name,
        archive_bytes=fetch_url(assets_by_name[archive_name]["browser_download_url"], token=token),
        checksums=fetch_url(assets_by_name["SHA256SUMS.txt"]["browser_download_url"], token=token),
        metadata=fetch_url(assets_by_name["RELEASE-METADATA.json"]["browser_download_url"], token=token),
    )


def fetch_url(url: str, *, token: str) -> bytes:
    headers = {
        "Accept": "application/vnd.github+json",
        "User-Agent": "sedna-release-install",
    }
    if token:
        headers["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(url, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            return response.read()
    except urllib.error.HTTPError as exc:
        body = exc.read().decode("utf-8", errors="replace")
        raise InstallError(f"GET {url} returned HTTP {exc.code}: {body[:500]}") from exc
    except urllib.error.URLError as exc:
        raise InstallError(f"GET {url} failed: {exc}") from exc


def load_assets_from_dir(asset_dir: Path, tag: str) -> ReleaseAssets:
    archive_name = expected_archive_name(tag)
    required = (archive_name, *REQUIRED_ASSET_NAMES)
    missing = [name for name in required if not (asset_dir / name).is_file()]
    if missing:
        raise InstallError(f"{asset_dir} is missing required assets: {', '.join(missing)}")
    return ReleaseAssets(
        archive_name=archive_name,
        archive_bytes=(asset_dir / archive_name).read_bytes(),
        checksums=(asset_dir / "SHA256SUMS.txt").read_bytes(),
        metadata=(asset_dir / "RELEASE-METADATA.json").read_bytes(),
    )


def verify_assets(repository: str, tag: str, assets: ReleaseAssets) -> None:
    metadata = json.loads(assets.metadata.decode("utf-8"))
    expected = {
        "release_tag": tag,
        "release_version": release_version(tag),
        "target": TARGET,
    }
    for key, value in expected.items():
        if metadata.get(key) != value:
            raise InstallError(f"metadata {key}={metadata.get(key)!r}, expected {value!r}")
    if str(metadata.get("repository", "")).lower() != repository.lower():
        raise InstallError(
            f"metadata repository={metadata.get('repository')!r}, expected {repository!r}"
        )

    checksums = parse_sha256sums(assets.checksums)
    digest = checksums.get(assets.archive_name)
    if digest is None:
        raise InstallError(f"SHA256SUMS.txt does not include {assets.archive_name}")
    actual = hashlib.sha256(assets.archive_bytes).hexdigest()
    if actual != digest:
        raise InstallError(f"checksum mismatch for {assets.archive_name}: expected {digest}, got {actual}")


def parse_sha256sums(contents: bytes) -> dict[str, str]:
    checksums: dict[str, str] = {}
    for line in contents.decode("utf-8").splitlines():
        line = line.strip()
        if not line:
            continue
        parts = line.split()
        if len(parts) < 2:
            raise InstallError(f"invalid SHA256SUMS line: {line!r}")
        digest, name = parts[0], parts[1]
        if not re.fullmatch(r"[0-9a-fA-F]{64}", digest):
            raise InstallError(f"invalid SHA256 digest in SHA256SUMS: {digest!r}")
        checksums[normalize_checksum_name(name)] = digest.lower()
    if not checksums:
        raise InstallError("SHA256SUMS.txt did not contain any checksums")
    return checksums


def normalize_checksum_name(name: str) -> str:
    return name.lstrip("*").removeprefix("./")


def stage_or_reuse_release(paths: InstallPaths, tag: str, assets: ReleaseAssets, *, dry_run: bool) -> Path:
    release_dir = paths.releases_dir / tag
    if release_dir.exists():
        verify_existing_release(release_dir, tag)
        return release_dir

    with tempfile.TemporaryDirectory(prefix="sedna-release-install-") as temp_root:
        staged = Path(temp_root) / tag
        staged.mkdir()
        extract_archive_safely(assets.archive_bytes, staged)
        require_executable(staged / "codex")
        require_executable(staged / "codex-responses-api-proxy")
        (staged / "RELEASE-METADATA.json").write_bytes(assets.metadata)
        (staged / "SHA256SUMS.txt").write_bytes(assets.checksums)

        if dry_run:
            return release_dir

        paths.releases_dir.mkdir(parents=True, exist_ok=True)
        tmp_release = paths.releases_dir / f".{tag}.tmp.{os.getpid()}"
        if tmp_release.exists():
            shutil.rmtree(tmp_release)
        shutil.copytree(staged, tmp_release, symlinks=False)
        os.replace(tmp_release, release_dir)
        return release_dir


def verify_existing_release(release_dir: Path, tag: str) -> None:
    metadata_path = release_dir / "RELEASE-METADATA.json"
    if not metadata_path.is_file():
        raise InstallError(f"existing release is missing metadata: {metadata_path}")
    metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
    if metadata.get("release_tag") != tag:
        raise InstallError(f"existing release metadata tag does not match {tag}: {release_dir}")
    require_executable(release_dir / "codex")
    require_executable(release_dir / "codex-responses-api-proxy")


def extract_archive_safely(archive_bytes: bytes, destination: Path) -> None:
    with tempfile.NamedTemporaryFile() as archive_file:
        archive_file.write(archive_bytes)
        archive_file.flush()
        with tarfile.open(archive_file.name, mode="r:gz") as archive:
            for member in archive.getmembers():
                if member.name in ("", ".", "./"):
                    continue
                relative = safe_member_path(member.name)
                target = destination / relative
                if member.isdir():
                    target.mkdir(parents=True, exist_ok=True)
                    continue
                if not member.isfile():
                    raise InstallError(f"refusing unsafe archive member {member.name!r}")
                target.parent.mkdir(parents=True, exist_ok=True)
                source = archive.extractfile(member)
                if source is None:
                    raise InstallError(f"failed to read archive member {member.name!r}")
                with source, target.open("wb") as output:
                    shutil.copyfileobj(source, output)
                os.chmod(target, member.mode & 0o777)


def safe_member_path(name: str) -> Path:
    path = Path(name)
    if path.is_absolute():
        raise InstallError(f"refusing absolute archive path {name!r}")
    parts = []
    for part in path.parts:
        if part in ("", "."):
            continue
        if part == "..":
            raise InstallError(f"refusing parent traversal archive path {name!r}")
        parts.append(part)
    if not parts:
        raise InstallError("refusing empty archive path")
    return Path(*parts)


def require_executable(path: Path) -> None:
    if not path.is_file():
        raise InstallError(f"required executable is missing: {path}")
    mode = path.stat().st_mode
    if not mode & (stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH):
        raise InstallError(f"required executable is not executable: {path}")


def promote_release(paths: InstallPaths, release_dir: Path) -> None:
    paths.install_root.mkdir(parents=True, exist_ok=True)
    paths.bin_dir.mkdir(parents=True, exist_ok=True)
    promote_symlink(release_dir, paths.current_link)
    if paths.visible_codex.exists() and not paths.visible_codex.is_symlink():
        paths.backups_dir.mkdir(parents=True, exist_ok=True)
        backup = paths.backups_dir / f"codex.{int(time.time())}"
        os.replace(paths.visible_codex, backup)
        print(f"backed up previous codex binary to {backup}")
    promote_symlink(release_dir / "codex", paths.visible_codex)


def promote_symlink(target: Path, link: Path) -> None:
    tmp = link.parent / f".{link.name}.tmp.{os.getpid()}"
    with contextlib.suppress(FileNotFoundError):
        tmp.unlink()
    tmp.symlink_to(target)
    os.replace(tmp, link)


def verify_visible_codex(binary: Path, tag: str) -> str:
    result = subprocess.run(
        [str(binary), "--version"],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        timeout=15,
    )
    output = result.stdout.strip()
    if result.returncode != 0:
        raise InstallError(f"{binary} --version failed with {result.returncode}: {output}")
    if release_version(tag) not in output:
        raise InstallError(f"{binary} --version did not report {release_version(tag)!r}: {output}")
    return output


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv[1:]))
    except InstallError as exc:
        print(f"error: {exc}", file=sys.stderr)
        raise SystemExit(1)
