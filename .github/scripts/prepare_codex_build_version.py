#!/usr/bin/env python3
"""Check or stamp the Rust workspace version from an exact upstream lineage."""

import argparse
import json
import re
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path


import resolve_sedna_release_version as VERSION_RESOLVER

COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
VERSION_ASSIGNMENT_RE = re.compile(r'^\s*version\s*=\s*"([^"]+)"\s*(?:#.*)?(?:\r?\n)?$')
PLACEHOLDER_VERSION = "0.0.0"


class BuildVersionError(RuntimeError):
    """The requested source lineage or workspace version state is unsafe."""


@dataclass(frozen=True)
class VersionProvenance:
    source_commit: str
    upstream_ref: str
    upstream_ref_commit: str
    upstream_base: str
    upstream_tag: str
    upstream_tag_commit: str
    upstream_track: str
    tag_distance: int
    exact_tag_base: bool


@dataclass(frozen=True)
class WorkspacePackage:
    name: str
    version: str
    inherits_workspace_version: bool


@dataclass(frozen=True)
class WorkspaceState:
    cargo_dir: Path
    manifest_text: str
    lock_text: str
    root_version: str
    packages: tuple[WorkspacePackage, ...]
    lock_blocks: tuple[tuple[int, int, str], ...]


def resolve_provenance(
    repo: Path, source_commit: str, upstream_ref: str, expected_track: str
) -> VersionProvenance:
    if not COMMIT_RE.fullmatch(source_commit):
        raise BuildVersionError("--source-commit must be a full 40-character commit SHA")
    try:
        expected = VERSION_RESOLVER.SemVer.parse(expected_track)
    except VERSION_RESOLVER.ReleaseVersionError as exc:
        raise BuildVersionError(f"invalid --expected-track: {exc}") from exc
    if str(expected) != expected_track or expected_track == PLACEHOLDER_VERSION:
        raise BuildVersionError("--expected-track must be a canonical non-placeholder version")

    try:
        resolved_source = VERSION_RESOLVER.resolve_commit(repo, source_commit)
        if resolved_source != source_commit:
            raise BuildVersionError("--source-commit did not resolve to the supplied exact SHA")
        checkout_head = VERSION_RESOLVER.resolve_commit(repo, "HEAD")
        if checkout_head != source_commit:
            raise BuildVersionError(
                f"checkout HEAD is {checkout_head}, not --source-commit {source_commit}"
            )
        dirty_state = VERSION_RESOLVER.git(
            repo, "status", "--porcelain=v1", "--untracked-files=all"
        )
        if dirty_state:
            raise BuildVersionError("source checkout must be clean before version preparation")
        checkout_root = Path(
            VERSION_RESOLVER.git(repo, "rev-parse", "--show-toplevel")
        ).resolve()
        if checkout_root != repo.resolve():
            raise BuildVersionError(
                f"--repo-root is {repo.resolve()}, not checkout root {checkout_root}"
            )
        upstream_ref_commit = VERSION_RESOLVER.resolve_commit(repo, upstream_ref)
        upstream_base = VERSION_RESOLVER.git(
            repo, "merge-base", resolved_source, upstream_ref_commit
        )
        track, tag, distance, exact = VERSION_RESOLVER.select_upstream_tag(
            repo, upstream_base, upstream_base
        )
        tag_commit = VERSION_RESOLVER.resolve_commit(repo, tag)
    except VERSION_RESOLVER.ReleaseVersionError as exc:
        raise BuildVersionError(str(exc)) from exc

    actual_track = str(track)
    if actual_track != expected_track:
        raise BuildVersionError(
            f"source resolves to upstream track {actual_track} via {tag}, "
            f"not expected {expected_track}"
        )
    return VersionProvenance(
        source_commit=resolved_source,
        upstream_ref=upstream_ref,
        upstream_ref_commit=upstream_ref_commit,
        upstream_base=upstream_base,
        upstream_tag=tag,
        upstream_tag_commit=tag_commit,
        upstream_track=actual_track,
        tag_distance=distance,
        exact_tag_base=exact,
    )


def _load_toml(path: Path) -> dict:
    try:
        return tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as exc:
        raise BuildVersionError(f"cannot read valid TOML from {path}: {exc}") from exc


def _member_manifests(cargo_dir: Path, member_patterns: list[str]) -> list[Path]:
    manifests: set[Path] = set()
    for pattern in member_patterns:
        matches = sorted(cargo_dir.glob(pattern))
        if not matches:
            raise BuildVersionError(f"workspace member does not resolve: {pattern}")
        found_for_pattern = False
        for match in matches:
            path = Path(match)
            manifest = path / "Cargo.toml" if path.is_dir() else path
            if manifest.name == "Cargo.toml" and manifest.is_file():
                manifests.add(manifest)
                found_for_pattern = True
            else:
                raise BuildVersionError(f"workspace member has no Cargo.toml: {path}")
        if not found_for_pattern:
            raise BuildVersionError(f"workspace member pattern has no package manifests: {pattern}")
    return sorted(manifests)


def _dependency_specs(manifest_doc: dict) -> list[tuple[str, dict]]:
    specs: list[tuple[str, dict]] = []

    def collect(table: object) -> None:
        if isinstance(table, dict):
            for alias, spec in table.items():
                if isinstance(spec, dict):
                    specs.append((alias, spec))

    for section in ("dependencies", "dev-dependencies", "build-dependencies"):
        collect(manifest_doc.get(section))
    targets = manifest_doc.get("target", {})
    if isinstance(targets, dict):
        for target in targets.values():
            if isinstance(target, dict):
                for section in ("dependencies", "dev-dependencies", "build-dependencies"):
                    collect(target.get(section))
    return specs


def _workspace_path(cargo_dir: Path, dependency_dir: Path, dependency: dict) -> Path | None:
    path = dependency.get("path")
    if not isinstance(path, str) and dependency.get("workspace") is True:
        return None
    if not isinstance(path, str):
        return None
    resolved = (dependency_dir / path).resolve()
    try:
        resolved.relative_to(cargo_dir.resolve())
    except ValueError:
        return None
    return resolved / "Cargo.toml"


def _belongs_to_root_workspace(manifest: Path, package_doc: dict, cargo_dir: Path) -> bool:
    root_manifest = (cargo_dir / "Cargo.toml").resolve()
    explicit_workspace = package_doc.get("workspace")
    if isinstance(explicit_workspace, str):
        return (manifest.parent / explicit_workspace / "Cargo.toml").resolve() == root_manifest
    directory = manifest.parent.resolve()
    root = cargo_dir.resolve()
    while directory == root or root in directory.parents:
        ancestor_manifest = directory / "Cargo.toml"
        if ancestor_manifest.is_file() and "workspace" in _load_toml(ancestor_manifest):
            return ancestor_manifest.resolve() == root_manifest
        if directory == root:
            break
        directory = directory.parent
    return False


def _workspace_member_manifests(cargo_dir: Path, workspace_doc: dict) -> list[Path]:
    explicit = _member_manifests(cargo_dir, workspace_doc.get("members", []))
    manifests = set(explicit)
    queue = list(explicit)
    shared_dependencies = workspace_doc.get("dependencies", {})
    if not isinstance(shared_dependencies, dict):
        raise BuildVersionError("[workspace.dependencies] must be a table")

    while queue:
        manifest = queue.pop()
        manifest_doc = _load_toml(manifest)
        for alias, spec in _dependency_specs(manifest_doc):
            dependency = spec
            dependency_dir = manifest.parent
            if spec.get("workspace") is True:
                inherited = shared_dependencies.get(alias)
                if not isinstance(inherited, dict):
                    continue
                dependency = inherited
                dependency_dir = cargo_dir
            package_manifest = _workspace_path(cargo_dir, dependency_dir, dependency)
            if package_manifest is None or package_manifest in manifests:
                continue
            package_doc = _load_toml(package_manifest).get("package")
            if not isinstance(package_doc, dict):
                raise BuildVersionError(f"local path dependency is not a package: {package_manifest}")
            if not _belongs_to_root_workspace(package_manifest, package_doc, cargo_dir):
                continue
            manifests.add(package_manifest)
            queue.append(package_manifest)
    return sorted(manifests)


def _package_version(package: dict, root_version: str, manifest: Path) -> WorkspacePackage:
    value = package.get("version")
    if isinstance(value, dict) and value.get("workspace") is True:
        version = root_version
        inherited = True
    elif isinstance(value, str):
        version = value
        inherited = False
    else:
        raise BuildVersionError(f"workspace package has an unsupported version field: {manifest}")
    return WorkspacePackage(package["name"], version, inherited)


def _workspace_version_line(lines: list[str]) -> int:
    section_starts = [i for i, line in enumerate(lines) if line.strip() == "[workspace.package]"]
    if len(section_starts) != 1:
        raise BuildVersionError("expected exactly one [workspace.package] section")
    start = section_starts[0]
    end = next(
        (i for i in range(start + 1, len(lines)) if re.match(r"^\s*\[", lines[i])),
        len(lines),
    )
    versions = [i for i in range(start + 1, end) if re.match(r"^\s*version\s*=", lines[i])]
    if len(versions) != 1 or not VERSION_ASSIGNMENT_RE.fullmatch(lines[versions[0]]):
        raise BuildVersionError("expected one literal version assignment in [workspace.package]")
    return versions[0]


def _package_blocks(lock_text: str) -> list[tuple[int, int, dict]]:
    lines = lock_text.splitlines(keepends=True)
    starts = [i for i, line in enumerate(lines) if line.strip() == "[[package]]"]
    blocks: list[tuple[int, int, dict]] = []
    for start in starts:
        end = next(
            (i for i in range(start + 1, len(lines)) if re.match(r"^\s*\[", lines[i])),
            len(lines),
        )
        try:
            entry = tomllib.loads("".join(lines[start:end]))["package"][0]
        except (tomllib.TOMLDecodeError, KeyError, IndexError) as exc:
            raise BuildVersionError(f"cannot parse Cargo.lock package block at line {start + 1}") from exc
        blocks.append((start, end, entry))
    if not blocks:
        raise BuildVersionError("Cargo.lock has no [[package]] entries")
    return blocks


def load_workspace_state(repo: Path) -> WorkspaceState:
    cargo_dir = repo / "codex-rs"
    manifest_path = cargo_dir / "Cargo.toml"
    lock_path = cargo_dir / "Cargo.lock"
    manifest_text = manifest_path.read_text(encoding="utf-8")
    lock_text = lock_path.read_text(encoding="utf-8")
    workspace_doc = _load_toml(manifest_path).get("workspace")
    if not isinstance(workspace_doc, dict):
        raise BuildVersionError(f"missing [workspace] table in {manifest_path}")
    root_version = workspace_doc.get("package", {}).get("version")
    member_patterns = workspace_doc.get("members")
    if not isinstance(root_version, str) or not isinstance(member_patterns, list):
        raise BuildVersionError("workspace must define a literal package version and member list")

    packages: list[WorkspacePackage] = []
    names: set[str] = set()
    for path in _workspace_member_manifests(cargo_dir, workspace_doc):
        package_doc = _load_toml(path).get("package")
        if not isinstance(package_doc, dict) or not isinstance(package_doc.get("name"), str):
            raise BuildVersionError(f"workspace member is not a named package: {path}")
        package = _package_version(package_doc, root_version, path)
        if package.name in names:
            raise BuildVersionError(f"duplicate workspace package name: {package.name}")
        names.add(package.name)
        packages.append(package)

    lock_doc = _load_toml(lock_path)
    lock_entries = lock_doc.get("package")
    if not isinstance(lock_entries, list):
        raise BuildVersionError("Cargo.lock has no package list")
    blocks = _package_blocks(lock_text)
    block_by_name: dict[str, list[tuple[int, int, str]]] = {}
    for start, end, entry in blocks:
        if entry.get("name") in names and "source" not in entry:
            block_by_name.setdefault(entry["name"], []).append((start, end, entry["version"]))

    matching_blocks: list[tuple[int, int, str]] = []
    for package in packages:
        matches = block_by_name.get(package.name, [])
        if len(matches) != 1:
            raise BuildVersionError(
                f"expected one source-free Cargo.lock entry for {package.name}, found {len(matches)}"
            )
        start, end, version = matches[0]
        if version != package.version:
            raise BuildVersionError(
                f"Cargo.lock version for {package.name} is {version}, "
                f"but workspace manifest resolves to {package.version}"
            )
        matching_blocks.append((start, end, package.name))

    if len(lock_entries) != len(_package_blocks(lock_text)):
        raise BuildVersionError("Cargo.lock package parse is inconsistent")
    return WorkspaceState(
        cargo_dir=cargo_dir,
        manifest_text=manifest_text,
        lock_text=lock_text,
        root_version=root_version,
        packages=tuple(packages),
        lock_blocks=tuple(matching_blocks),
    )


def prepare_workspace(state: WorkspaceState, target_version: str, mode: str) -> tuple[str, str, int]:
    allowed_root_versions = {PLACEHOLDER_VERSION, target_version}
    if state.root_version not in allowed_root_versions:
        raise BuildVersionError(
            f"unexpected prior workspace version {state.root_version}; "
            f"expected {PLACEHOLDER_VERSION} or {target_version}"
        )
    if mode == "check" and state.root_version != target_version:
        raise BuildVersionError(
            f"workspace version is {state.root_version}, expected source track {target_version}"
        )

    manifest_lines = state.manifest_text.splitlines(keepends=True)
    root_version_index = _workspace_version_line(manifest_lines)
    observed_root_version = VERSION_ASSIGNMENT_RE.fullmatch(manifest_lines[root_version_index])
    if observed_root_version is None or observed_root_version.group(1) != state.root_version:
        raise BuildVersionError("workspace package version changed during inspection")

    lock_lines = state.lock_text.splitlines(keepends=True)
    inherited_names = {
        package.name for package in state.packages if package.inherits_workspace_version
    }
    updated_entries = 0
    for start, end, name in state.lock_blocks:
        if name not in inherited_names:
            continue
        version_lines = [i for i in range(start, end) if re.match(r"^\s*version\s*=", lock_lines[i])]
        if len(version_lines) != 1:
            raise BuildVersionError(f"expected one version line for local package {name}")
        match = VERSION_ASSIGNMENT_RE.fullmatch(lock_lines[version_lines[0]])
        if match is None or match.group(1) != state.root_version:
            raise BuildVersionError(f"unexpected prior Cargo.lock version for {name}")
        if state.root_version != target_version and mode == "write":
            lock_lines[version_lines[0]] = re.sub(
                r'("[^"]+")', f'"{target_version}"', lock_lines[version_lines[0]], count=1
            )
            updated_entries += 1

    if state.root_version != target_version and mode == "write":
        manifest_lines[root_version_index] = re.sub(
            r'("[^"]+")', f'"{target_version}"', manifest_lines[root_version_index], count=1
        )
    return "".join(manifest_lines), "".join(lock_lines), updated_entries


def run(repo: Path, source_commit: str, upstream_ref: str, expected_track: str, mode: str) -> dict:
    provenance = resolve_provenance(repo, source_commit, upstream_ref, expected_track)
    state = load_workspace_state(repo)
    manifest_text, lock_text, updated_entries = prepare_workspace(state, expected_track, mode)
    if mode == "write":
        manifest_path = state.cargo_dir / "Cargo.toml"
        lock_path = state.cargo_dir / "Cargo.lock"
        if manifest_text != state.manifest_text:
            manifest_path.write_text(manifest_text, encoding="utf-8")
        if lock_text != state.lock_text:
            lock_path.write_text(lock_text, encoding="utf-8")
    return {
        "mode": mode,
        "source_commit": provenance.source_commit,
        "upstream_ref": provenance.upstream_ref,
        "upstream_ref_commit": provenance.upstream_ref_commit,
        "upstream_base": provenance.upstream_base,
        "upstream_tag": provenance.upstream_tag,
        "upstream_tag_commit": provenance.upstream_tag_commit,
        "upstream_track": provenance.upstream_track,
        "tag_distance": provenance.tag_distance,
        "exact_tag_base": provenance.exact_tag_base,
        "workspace_version_before": state.root_version,
        "workspace_version_after": expected_track,
        "local_lock_entries_updated": updated_entries,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-commit", required=True)
    parser.add_argument("--upstream-ref", required=True)
    parser.add_argument("--expected-track", required=True)
    parser.add_argument("--repo-root", type=Path, default=Path(__file__).resolve().parents[2])
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--check", action="store_true")
    mode.add_argument("--write", action="store_true")
    args = parser.parse_args(argv)
    try:
        result = run(
            args.repo_root.resolve(),
            args.source_commit,
            args.upstream_ref,
            args.expected_track,
            "write" if args.write else "check",
        )
    except (BuildVersionError, VERSION_RESOLVER.ReleaseVersionError, OSError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
