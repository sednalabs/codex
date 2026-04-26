#!/usr/bin/env python3
"""Install a published Sedna Codex release from GitHub release assets."""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import json
import os
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


REPOSITORY = "sednalabs/codex"
API_ROOT = f"https://api.github.com/repos/{REPOSITORY}"
INSTALL_ROOT = Path.home() / ".codex/packages/standalone"
BIN_DIR = Path.home() / ".local/bin"
TARGET = "x86_64-unknown-linux-gnu"
REQUIRED_ASSET_NAMES = ("SHA256SUMS.txt", "RELEASE-METADATA.json")
ARCHIVE_EXECUTABLES = frozenset(("codex", "codex-responses-api-proxy"))


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
    if repository.lower() != REPOSITORY:
        raise InstallError(f"repository must be sednalabs/codex, got {repository!r}")

    paths = InstallPaths(
        install_root=INSTALL_ROOT,
        bin_dir=BIN_DIR,
    )
    assets = download_release_assets(repository, tag, allow_prerelease=args.allow_prerelease)
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
    parser.add_argument("--allow-prerelease", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    return parser.parse_args(argv)


def validate_tag(tag: str) -> str:
    tag = tag.strip()
    if len(tag) > 80 or not tag.startswith("v") or "-sedna." not in tag:
        raise InstallError(f"release tag must look like v0.124.0-sedna.2, got {tag!r}")
    version, sedna_suffix = tag[1:].rsplit("-sedna.", 1)
    if not sedna_suffix.isdecimal():
        raise InstallError(f"release tag must look like v0.124.0-sedna.2, got {tag!r}")
    parts = version.split(".")
    if len(parts) < 3 or not all(part for part in parts[:3]):
        raise InstallError(f"release tag must look like v0.124.0-sedna.2, got {tag!r}")
    if not all(part.isdecimal() for part in parts[:3]):
        raise InstallError(f"release tag must look like v0.124.0-sedna.2, got {tag!r}")
    allowed = set("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789.-")
    if any(char not in allowed for char in version):
        raise InstallError(f"release tag must look like v0.124.0-sedna.2, got {tag!r}")
    if ".." in version or version.endswith((".", "-")):
        raise InstallError(f"release tag must look like v0.124.0-sedna.2, got {tag!r}")
    return tag


def release_version(tag: str) -> str:
    return tag.removeprefix("v")


def expected_archive_name(tag: str) -> str:
    return f"codex-sedna-{release_version(tag)}-{TARGET}.tar.gz"


def download_release_assets(repository: str, tag: str, *, allow_prerelease: bool) -> ReleaseAssets:
    token = os.environ.get("GITHUB_TOKEN", "").strip()
    if repository.lower() != REPOSITORY:
        raise InstallError(f"repository must be sednalabs/codex, got {repository!r}")
    release = json.loads(fetch_github_api(f"/releases/tags/{urllib.parse.quote(tag, safe='')}", token=token).decode("utf-8"))
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
        archive_bytes=download_release_asset(assets_by_name[archive_name], token=token),
        checksums=download_release_asset(assets_by_name["SHA256SUMS.txt"], token=token),
        metadata=download_release_asset(assets_by_name["RELEASE-METADATA.json"], token=token),
    )


def download_release_asset(asset: dict[str, object], *, token: str) -> bytes:
    asset_id = asset.get("id")
    if not isinstance(asset_id, int):
        raise InstallError(f"release asset {asset.get('name')!r} has no numeric id")
    return fetch_github_api(f"/releases/assets/{asset_id}", token=token, accept="application/octet-stream")


def fetch_github_api(path: str, *, token: str, accept: str = "application/vnd.github+json") -> bytes:
    if not path.startswith("/"):
        raise InstallError(f"internal GitHub API path must start with /: {path!r}")
    url = f"{API_ROOT}{path}"
    headers = {
        "Accept": accept,
        "User-Agent": "sedna-release-install",
    }
    if token:
        headers["Authorization"] = f"Bearer {token}"

    # Callers build GitHub API paths from a fixed repository plus validated tag or numeric asset id.
    # codeql[py/partial-ssrf]
    request = urllib.request.Request(url, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            return response.read()
    except urllib.error.HTTPError as exc:
        body = exc.read().decode("utf-8", errors="replace")
        raise InstallError(f"GET {url} returned HTTP {exc.code}: {body[:500]}") from exc
    except urllib.error.URLError as exc:
        raise InstallError(f"GET {url} failed: {exc}") from exc


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
        if len(digest) != 64 or any(char not in "0123456789abcdefABCDEF" for char in digest):
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

        # The release tag is validated to exclude path separators before this path is built.
        # codeql[py/path-injection]
        staged.mkdir()
        extract_archive_safely(assets.archive_bytes, staged)
        require_executable(staged / "codex")
        require_executable(staged / "codex-responses-api-proxy")

        # The destination directory is derived from a validated release tag.
        # codeql[py/path-injection]
        (staged / "RELEASE-METADATA.json").write_bytes(assets.metadata)

        # The destination directory is derived from a validated release tag.
        # codeql[py/path-injection]
        (staged / "SHA256SUMS.txt").write_bytes(assets.checksums)

        if dry_run:
            return release_dir

        paths.releases_dir.mkdir(parents=True, exist_ok=True)
        tmp_release = paths.releases_dir / f".{tag}.tmp.{os.getpid()}"
        if tmp_release.exists():
            shutil.rmtree(tmp_release)

        # Both source and target live under fixed install roots using a validated release tag.
        # codeql[py/path-injection]
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
                relative = safe_member_name(member.name)
                target = destination / relative
                if member.isdir():
                    raise InstallError(f"refusing unexpected archive directory {member.name!r}")
                if not member.isfile():
                    raise InstallError(f"refusing unsafe archive member {member.name!r}")
                source = archive.extractfile(member)
                if source is None:
                    raise InstallError(f"failed to read archive member {member.name!r}")

                # Archive members are allowlisted to exact root executable names.
                # codeql[py/path-injection]
                with source, target.open("wb") as output:
                    shutil.copyfileobj(source, output)

                # Archive members are allowlisted to exact root executable names.
                # codeql[py/path-injection]
                os.chmod(target, member.mode & 0o777)


def safe_member_name(name: str) -> str:
    if name.startswith("/"):
        raise InstallError(f"refusing absolute archive path {name!r}")
    parts: list[str] = []
    for part in name.split("/"):
        if part in ("", "."):
            continue
        if part == "..":
            raise InstallError(f"refusing parent traversal archive path {name!r}")
        parts.append(part)
    if len(parts) != 1 or parts[0] not in ARCHIVE_EXECUTABLES:
        raise InstallError(f"refusing unexpected archive member {name!r}")
    return parts[0]


def require_executable(path: Path) -> None:
    # Callers pass fixed executable names under validated release directories.
    # codeql[py/path-injection]
    if not path.is_file():
        raise InstallError(f"required executable is missing: {path}")

    # Callers pass fixed executable names under validated release directories.
    # codeql[py/path-injection]
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
