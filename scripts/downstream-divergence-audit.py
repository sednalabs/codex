#!/usr/bin/env python3
from __future__ import annotations

import argparse
import fnmatch
import json
import os
import subprocess
import sys
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

EXIT_OK = 0
EXIT_UNSTABLE_SNAPSHOT = 2
EXIT_INVALID_MIRROR = 3
EXIT_UNCOVERED_LIVE_CODE = 4
EXIT_STALE_REGISTRY = 5

NON_CODE_PREFIXES = ("docs/",)
NON_CODE_SUFFIXES = (".md",)
NON_CODE_EXACT = {
    "AGENTS.md",
    "README",
    "README.md",
    "LICENSE",
    "LICENSE.md",
}


@dataclass(frozen=True)
class RemoteTip:
    remote: str
    branch: str
    sha: str
    local_ref: str | None = None

    @property
    def local_tracking_ref(self) -> str:
        if self.local_ref is not None:
            return self.local_ref
        return f"refs/remotes/{self.remote}/{self.branch}"


@dataclass(frozen=True)
class DiffItem:
    status: str
    paths: tuple[str, ...]
    old_mode: str
    new_mode: str
    rename_score: str | None
    is_code: bool
    registry_ids: tuple[str, ...] = field(default_factory=tuple)

    @property
    def display_path(self) -> str:
        if len(self.paths) == 1:
            return self.paths[0]
        return f"{self.paths[0]} -> {self.paths[-1]}"

    @property
    def mode_change(self) -> str:
        if self.old_mode == self.new_mode:
            return self.old_mode
        return f"{self.old_mode} -> {self.new_mode}"


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()

    repo = resolve_repo_root(Path(args.repo))
    output_dir = Path(args.output_dir)
    output_dir.mkdir(parents=True, exist_ok=True)

    registry = load_registry(Path(args.registry_path), args.enforce_registry)

    snapshot_1 = resolve_snapshot(
        repo,
        args.downstream_remote,
        args.downstream_branch,
        args.mirror_remote,
        args.mirror_branch,
        args.upstream_remote,
        args.upstream_branch,
        args.downstream_ref,
    )
    mirror_mismatch = bool(
        args.expected_mirror_sha and snapshot_1["mirror"].sha != args.expected_mirror_sha
    )

    fetch_live_refs(repo, snapshot_1)
    verify_local_tracking_refs(repo, snapshot_1)

    downstream_tree = tree_sha(repo, snapshot_1["downstream"].sha)
    mirror_tree = tree_sha(repo, snapshot_1["mirror"].sha)
    upstream_tree = tree_sha(repo, snapshot_1["upstream"].sha)

    diff_items = diff_items_between(repo, snapshot_1["upstream"].sha, snapshot_1["downstream"].sha)
    code_items = [item for item in diff_items if item.is_code]

    mirror_health, mirror_counts = classify_mirror_health(
        repo, snapshot_1["mirror"].sha, snapshot_1["upstream"].sha
    )
    downstream_counts = rev_list_counts(repo, snapshot_1["upstream"].sha, snapshot_1["downstream"].sha)
    cherry_counts = cherry_counts_between(repo, snapshot_1["upstream"].sha, snapshot_1["downstream"].sha)
    merge_base = merge_base_sha(repo, snapshot_1["upstream"].sha, snapshot_1["downstream"].sha)

    registry_state = reconcile_registry(registry, code_items)
    annotated_all_items = annotate_diff_items(
        diff_items,
        registry_state["path_registry_ids"],
        registry_state["path_registry_surface_types"],
    )
    annotated_code_items = [item for item in annotated_all_items if item["is_code"]]
    annotated_non_code_items = [item for item in annotated_all_items if not item["is_code"]]

    snapshot_2 = resolve_snapshot(
        repo,
        args.downstream_remote,
        args.downstream_branch,
        args.mirror_remote,
        args.mirror_branch,
        args.upstream_remote,
        args.upstream_branch,
        args.downstream_ref,
    )

    unstable = snapshot_changed(snapshot_1, snapshot_2)
    invalid_mirror = mirror_health in {"illegal_ahead", "illegal_diverged"} or mirror_mismatch
    stale_registry = bool(registry_state["stale_entry_ids"])
    uncovered_live_code = bool(registry_state["uncovered_code_paths"])

    exit_code = EXIT_OK
    reasons: list[str] = []
    if unstable:
        exit_code = EXIT_UNSTABLE_SNAPSHOT
        reasons.append("live remote snapshot changed during audit")
    elif invalid_mirror:
        exit_code = EXIT_INVALID_MIRROR
        reasons.append("mirror state is not a valid exact mirror")
    elif uncovered_live_code:
        exit_code = EXIT_UNCOVERED_LIVE_CODE
        reasons.append("live code differences are not covered by the registry")
    elif stale_registry:
        exit_code = EXIT_STALE_REGISTRY
        reasons.append("registry contains stale live entries")

    audit = {
        "repo_root": str(repo),
        "captured_at": utc_now(),
        "snapshot": {
            "initial": snapshot_to_json(snapshot_1),
            "final": snapshot_to_json(snapshot_2),
            "stable": not unstable,
        },
        "mirror": {
            "health": mirror_health,
            "counts_vs_upstream": {
                "upstream_ahead": mirror_counts[0],
                "mirror_ahead": mirror_counts[1],
            },
            "tree_sha": mirror_tree,
            "expected_mirror_sha": args.expected_mirror_sha,
            "expected_mirror_matches": not mirror_mismatch,
            "usable_as_compare_baseline": mirror_health == "exact" and not unstable,
        },
        "tree_diff": {
            "comparison_basis": "mirror" if mirror_health == "exact" and not unstable else "upstream",
            "downstream_tree_sha": downstream_tree,
            "upstream_tree_sha": upstream_tree,
            "tree_equal": downstream_tree == upstream_tree,
            "counts_vs_upstream": {
                "upstream_ahead": downstream_counts[0],
                "downstream_ahead": downstream_counts[1],
            },
            "merge_base": merge_base,
            "patch_equivalent_downstream_commits": cherry_counts["patch_equivalent_downstream_commits"],
            "unique_downstream_commits": cherry_counts["unique_downstream_commits"],
            "all_paths": annotated_all_items,
            "code_paths": annotated_code_items,
            "non_code_paths": annotated_non_code_items,
        },
        "registry_reconciliation": registry_state,
        "verdict": {
            "exit_code": exit_code,
            "ok": exit_code == EXIT_OK,
            "reasons": reasons,
            "code_only": args.code_only,
        },
    }

    write_outputs(output_dir, args.format, audit)
    print_summary(audit)
    return exit_code


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Authoritative downstream divergence audit against live upstream and the synced mirror.",
    )
    parser.add_argument("--repo", default=".", help="Path to the repository root.")
    parser.add_argument("--downstream-remote", default="origin", help="Remote that hosts the downstream branch.")
    parser.add_argument("--downstream-branch", default="main", help="Downstream branch name.")
    parser.add_argument(
        "--downstream-ref",
        help=(
            "Local commit/ref to audit as downstream instead of resolving "
            "--downstream-remote/--downstream-branch. Useful for PR-head CI."
        ),
    )
    parser.add_argument("--mirror-remote", default="origin", help="Remote that hosts the upstream mirror branch.")
    parser.add_argument("--mirror-branch", default="upstream-main", help="Mirror branch name.")
    parser.add_argument("--upstream-remote", default="upstream", help="Remote that hosts live upstream.")
    parser.add_argument("--upstream-branch", default="main", help="Upstream branch name.")
    parser.add_argument("--expected-mirror-sha", help="Exact upstream SHA that the mirror should match after sync.")
    parser.add_argument(
        "--registry-path",
        default="docs/divergences/index.yaml",
        help="Registry of intentional downstream divergences.",
    )
    parser.add_argument(
        "--output-dir",
        default="target/downstream-divergence-audit",
        help="Directory for JSON and Markdown audit artifacts.",
    )
    parser.add_argument(
        "--format",
        choices=("json", "md", "both"),
        default="both",
        help="Artifact formats to write.",
    )
    parser.add_argument(
        "--code-only",
        action="store_true",
        default=True,
        help="Treat docs-only drift as informational and keep the verdict scoped to code paths.",
    )
    parser.add_argument(
        "--no-code-only",
        dest="code_only",
        action="store_false",
        help="Include docs-only drift in the rendered diff sections as well as code paths.",
    )
    parser.add_argument(
        "--enforce-registry",
        action="store_true",
        default=True,
        help="Fail on uncovered live code diffs or stale registry entries.",
    )
    parser.add_argument(
        "--no-enforce-registry",
        dest="enforce_registry",
        action="store_false",
        help="Report registry drift without failing the run.",
    )
    return parser


def resolve_repo_root(repo: Path) -> Path:
    result = run_git(repo, ["rev-parse", "--show-toplevel"], capture_stdout=True)
    return Path(result.stdout.strip())


def resolve_snapshot(
    repo: Path,
    downstream_remote: str,
    downstream_branch: str,
    mirror_remote: str,
    mirror_branch: str,
    upstream_remote: str,
    upstream_branch: str,
    downstream_ref: str | None = None,
) -> dict[str, RemoteTip]:
    downstream = (
        local_tip(repo, "downstream-ref", downstream_ref)
        if downstream_ref
        else live_tip(repo, downstream_remote, downstream_branch)
    )
    mirror = live_tip(repo, mirror_remote, mirror_branch)
    upstream = live_tip(repo, upstream_remote, upstream_branch)
    return {"downstream": downstream, "mirror": mirror, "upstream": upstream}


def live_tip(repo: Path, remote: str, branch: str) -> RemoteTip:
    ref = normalize_branch_ref(branch)
    result = run_git(
        repo,
        ["ls-remote", "--refs", remote, ref],
        capture_stdout=True,
        allow_failure=False,
    )
    sha = parse_single_sha(result.stdout, remote, ref)
    return RemoteTip(remote=remote, branch=branch, sha=sha)


def local_tip(repo: Path, remote_label: str, ref: str) -> RemoteTip:
    result = run_git(repo, ["rev-parse", f"{ref}^{{commit}}"], capture_stdout=True)
    sha = result.stdout.strip()
    return RemoteTip(remote=remote_label, branch=ref, sha=sha, local_ref=sha)


def normalize_branch_ref(branch: str) -> str:
    if branch.startswith("refs/"):
        return branch
    return f"refs/heads/{branch}"


def parse_single_sha(stdout: str, remote: str, ref: str) -> str:
    lines = [line.strip() for line in stdout.splitlines() if line.strip()]
    if not lines:
        raise RuntimeError(f"missing remote ref {remote} {ref}")
    sha, got_ref = lines[0].split("\t", 1)
    if got_ref != ref:
        raise RuntimeError(f"unexpected ref from {remote}: expected {ref}, got {got_ref}")
    return sha


def fetch_live_refs(repo: Path, snapshot: dict[str, RemoteTip]) -> None:
    grouped: dict[str, list[RemoteTip]] = {}
    for tip in snapshot.values():
        if tip.local_ref is not None:
            continue
        grouped.setdefault(tip.remote, []).append(tip)

    for remote, tips in grouped.items():
        branches = [tip.branch for tip in tips]
        run_git(repo, ["fetch", "--no-tags", "--prune", remote, *branches], capture_stdout=True)
        for tip in tips:
            local_sha = local_tracking_sha(repo, tip)
            if local_sha != tip.sha:
                raise SnapshotChanged(
                    f"{tip.remote}/{tip.branch} moved during fetch: expected {tip.sha}, got {local_sha}"
                )


def verify_local_tracking_refs(repo: Path, snapshot: dict[str, RemoteTip]) -> None:
    for tip in snapshot.values():
        local_sha = local_tracking_sha(repo, tip)
        if local_sha != tip.sha:
            raise SnapshotChanged(
                f"{tip.remote}/{tip.branch} moved during audit: expected {tip.sha}, got {local_sha}"
            )


def local_tracking_sha(repo: Path, tip: RemoteTip) -> str:
    result = run_git(repo, ["rev-parse", tip.local_tracking_ref], capture_stdout=True)
    return result.stdout.strip()


def tree_sha(repo: Path, commit_sha: str) -> str:
    result = run_git(repo, ["rev-parse", f"{commit_sha}^{{tree}}"], capture_stdout=True)
    return result.stdout.strip()


def diff_items_between(repo: Path, left_sha: str, right_sha: str) -> list[DiffItem]:
    result = run_git(
        repo,
        [
            "diff",
            "--raw",
            "-z",
            "--find-renames=50%",
            "--find-copies=50%",
            "--no-ext-diff",
            left_sha,
            right_sha,
        ],
        capture_stdout=True,
    )
    items: list[DiffItem] = []
    chunks = result.stdout.split("\0")
    index = 0
    while index < len(chunks):
        header = chunks[index]
        index += 1
        if not header:
            continue
        if not header.startswith(":"):
            raise RuntimeError(f"unexpected raw diff header: {header}")
        meta, status = header[1:].rsplit(" ", 1)
        old_mode, new_mode, _old_sha, _new_sha = meta.split(" ")
        rename_score = status[1:] if len(status) > 1 and status[0] in {"R", "C"} else None
        if status and status[0] in {"R", "C"}:
            if index + 1 >= len(chunks):
                raise RuntimeError(f"missing rename paths for diff entry: {header}")
            paths = (chunks[index], chunks[index + 1])
            index += 2
        else:
            if index >= len(chunks):
                raise RuntimeError(f"missing path for diff entry: {header}")
            paths = (chunks[index],)
            index += 1
        is_code = any(is_code_path(path) for path in paths)
        items.append(
            DiffItem(
                status=status,
                paths=paths,
                old_mode=old_mode,
                new_mode=new_mode,
                rename_score=rename_score,
                is_code=is_code,
            )
        )
    items.sort(key=lambda item: (item.display_path, item.status))
    return items


def is_code_path(path: str) -> bool:
    if path in NON_CODE_EXACT:
        return False
    if path.startswith(NON_CODE_PREFIXES):
        return False
    if any(path.endswith(suffix) for suffix in NON_CODE_SUFFIXES):
        return False
    return True


def rev_list_counts(repo: Path, left_sha: str, right_sha: str) -> tuple[int, int]:
    result = run_git(
        repo,
        ["rev-list", "--left-right", "--count", f"{left_sha}...{right_sha}"],
        capture_stdout=True,
    )
    left_str, right_str = result.stdout.strip().split()
    return int(left_str), int(right_str)


def cherry_counts_between(repo: Path, left_sha: str, right_sha: str) -> dict[str, int]:
    result = run_git(repo, ["cherry", left_sha, right_sha], capture_stdout=True)
    patch_equivalent = 0
    unique = 0
    for line in result.stdout.splitlines():
        if not line.strip():
            continue
        marker = line[0]
        if marker == "-":
            patch_equivalent += 1
        elif marker == "+":
            unique += 1
    return {
        "patch_equivalent_downstream_commits": patch_equivalent,
        "unique_downstream_commits": unique,
    }


def merge_base_sha(repo: Path, left_sha: str, right_sha: str) -> str:
    result = run_git(repo, ["merge-base", left_sha, right_sha], capture_stdout=True)
    return result.stdout.strip()


def classify_mirror_health(repo: Path, mirror_sha: str, upstream_sha: str) -> tuple[str, tuple[int, int]]:
    counts = rev_list_counts(repo, upstream_sha, mirror_sha)
    if mirror_sha == upstream_sha:
        return "exact", counts
    upstream_ahead, mirror_ahead = counts
    if mirror_ahead > 0 and upstream_ahead == 0:
        return "illegal_ahead", counts
    if upstream_ahead > 0 and mirror_ahead == 0:
        return "stale_ff_only", counts
    return "illegal_diverged", counts


def load_registry(path: Path, enforce_registry: bool) -> dict[str, Any]:
    if not path.exists():
        if enforce_registry:
            raise FileNotFoundError(f"registry file missing: {path}")
        return {"version": 1, "divergences": [], "_path": str(path)}
    data = json.loads(path.read_text(encoding="utf-8"))
    divergences = data.get("divergences")
    if not isinstance(divergences, list):
        raise ValueError("registry file must contain a divergences array")
    data["_path"] = str(path)
    return data


def reconcile_registry(registry: dict[str, Any], live_code_items: list[DiffItem]) -> dict[str, Any]:
    entries = registry.get("divergences", [])
    live_entries: list[dict[str, Any]] = []
    stale_entry_ids: list[str] = []
    path_registry_ids: dict[str, list[str]] = {}
    path_registry_surface_types: dict[str, list[str]] = {}
    live_code_paths = sorted({path for item in live_code_items for path in item.paths})
    uncovered_code_paths = sorted(live_code_paths)

    for entry in entries:
        entry_id = str(entry.get("id", ""))
        files = entry.get("files", [])
        matched_paths = sorted(
            {path for path in live_code_paths for spec in files if matches_spec(spec, path)}
        )
        status = str(entry.get("status", "live"))
        surface_type = entry.get("surface_type")
        if matched_paths:
            live_entries.append(
                {
                    "id": entry_id,
                    "title": entry.get("title", ""),
                    "status": "live",
                    "matched_paths": matched_paths,
                    "surface": entry.get("surface", []),
                    "surface_type": surface_type,
                    "category": entry.get("category", ""),
                }
            )
            uncovered_code_paths = [path for path in uncovered_code_paths if path not in matched_paths]
            for path in matched_paths:
                path_registry_ids.setdefault(path, []).append(entry_id)
                if surface_type:
                    path_registry_surface_types.setdefault(path, []).append(surface_type)
        else:
            derived_status = "upstream-equivalent" if status == "upstream-equivalent" else "stale"
            live_entries.append(
                {
                    "id": entry_id,
                    "title": entry.get("title", ""),
                    "status": derived_status,
                    "matched_paths": [],
                    "surface": entry.get("surface", []),
                    "surface_type": surface_type,
                    "category": entry.get("category", ""),
                }
            )
            if derived_status == "stale":
                stale_entry_ids.append(entry_id)

    return {
        "registry_path": str(registry.get("_path", "docs/divergences/index.yaml")),
        "entries": live_entries,
        "live_entry_ids": [entry["id"] for entry in live_entries if entry["status"] == "live"],
        "stale_entry_ids": stale_entry_ids,
        "uncovered_code_paths": uncovered_code_paths,
        "path_registry_ids": {path: sorted(set(ids)) for path, ids in path_registry_ids.items()},
        "path_registry_surface_types": {
            path: sorted(set(surface_types)) for path, surface_types in path_registry_surface_types.items()
        },
    }


def matches_spec(spec: Any, path: str) -> bool:
    if not isinstance(spec, str) or not spec:
        return False
    if spec.endswith("/"):
        return path.startswith(spec)
    if any(token in spec for token in ("*", "?", "[")):
        return fnmatch.fnmatch(path, spec)
    return path == spec


def snapshot_to_json(snapshot: dict[str, RemoteTip]) -> dict[str, Any]:
    return {
        name: {"remote": tip.remote, "branch": tip.branch, "sha": tip.sha}
        for name, tip in snapshot.items()
    }


def snapshot_changed(snapshot_1: dict[str, RemoteTip], snapshot_2: dict[str, RemoteTip]) -> bool:
    for key in snapshot_1:
        if snapshot_1[key].sha != snapshot_2[key].sha:
            return True
    return False


def diff_item_to_json(
    item: DiffItem,
    registry_ids: list[str] | tuple[str, ...] | None = None,
    surface_types: list[str] | tuple[str, ...] | None = None,
) -> dict[str, Any]:
    return {
        "status": item.status,
        "paths": list(item.paths),
        "display_path": item.display_path,
        "old_mode": item.old_mode,
        "new_mode": item.new_mode,
        "mode_change": item.mode_change,
        "rename_score": item.rename_score,
        "is_code": item.is_code,
        "registry_ids": list(registry_ids or item.registry_ids),
        "surface_types": list(surface_types or []),
    }


def annotate_diff_items(
    items: list[DiffItem],
    id_coverage: dict[str, list[str]],
    surface_coverage: dict[str, list[str]],
) -> list[dict[str, Any]]:
    return [
        diff_item_to_json(item, *coverage_for_item(item, id_coverage, surface_coverage))
        for item in items
    ]


def coverage_for_item(
    item: DiffItem,
    id_coverage: dict[str, list[str]],
    surface_coverage: dict[str, list[str]],
) -> tuple[list[str], list[str]]:
    ids: list[str] = []
    surface_types: list[str] = []
    for path in item.paths:
        ids.extend(id_coverage.get(path, []))
        surface_types.extend(surface_coverage.get(path, []))
    return sorted(set(ids)), sorted(set(surface_types))


def write_outputs(output_dir: Path, format_name: str, audit: dict[str, Any]) -> None:
    if format_name in {"json", "both"}:
        (output_dir / "downstream-divergence-audit.json").write_text(
            json.dumps(audit, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
    if format_name in {"md", "both"}:
        (output_dir / "downstream-divergence-audit.md").write_text(
            render_markdown(audit),
            encoding="utf-8",
        )


def render_markdown(audit: dict[str, Any]) -> str:
    lines: list[str] = []
    lines.append("# Downstream divergence audit")
    lines.append("")
    lines.append(f"- Repository: `{audit['repo_root']}`")
    lines.append(f"- Captured at: `{audit['captured_at']}`")
    snapshot = audit["snapshot"]["initial"]
    lines.append(f"- Downstream: `{snapshot['downstream']['sha']}`")
    lines.append(f"- Mirror: `{snapshot['mirror']['sha']}`")
    lines.append(f"- Upstream: `{snapshot['upstream']['sha']}`")
    lines.append(f"- Mirror health: `{audit['mirror']['health']}`")
    lines.append(f"- Comparison basis: `{audit['tree_diff']['comparison_basis']}`")
    lines.append(f"- Verdict: `{audit['verdict']['exit_code']}`")
    if audit["verdict"]["reasons"]:
        lines.append(f"- Reasons: {', '.join(f'`{reason}`' for reason in audit['verdict']['reasons'])}")
    lines.append("")
    lines.append("## Tree diff")
    lines.append("")
    lines.append(
        f"- Tree equal: `{audit['tree_diff']['tree_equal']}`"
        f" | Upstream ahead: `{audit['tree_diff']['counts_vs_upstream']['upstream_ahead']}`"
        f" | Downstream ahead: `{audit['tree_diff']['counts_vs_upstream']['downstream_ahead']}`"
    )
    lines.append(
        f"- Patch-equivalent downstream commits: `{audit['tree_diff']['patch_equivalent_downstream_commits']}`"
        f" | Unique downstream commits: `{audit['tree_diff']['unique_downstream_commits']}`"
    )
    lines.append("")
    lines.append("### Code paths")
    lines.extend(render_diff_table(audit["tree_diff"]["code_paths"]))
    lines.append("")
    lines.append("### Non-code paths")
    lines.extend(render_diff_table(audit["tree_diff"]["non_code_paths"]))
    lines.append("")
    lines.append("## Registry reconciliation")
    lines.append("")
    lines.extend(render_registry_table(audit["registry_reconciliation"]["entries"]))
    return "\n".join(lines).rstrip() + "\n"


def render_diff_table(items: list[dict[str, Any]]) -> list[str]:
    if not items:
        return ["- None"]
    rows = [
        "| Status | Mode | Rename | Paths | Registry | Surface |",
        "| --- | --- | --- | --- | --- | --- |",
    ]
    for item in items:
        registry_ids = ", ".join(item.get("registry_ids", [])) or "-"
        rename_score = f"`{item['rename_score']}`" if item.get("rename_score") else "-"
        surface_types = ", ".join(item.get("surface_types", [])) or "-"
        rows.append(
            f"| `{item['status']}` | `{item['mode_change']}` | {rename_score} | `{item['display_path']}` | `{registry_ids}` | `{surface_types}` |"
        )
    return rows


def render_registry_table(entries: list[dict[str, Any]]) -> list[str]:
    if not entries:
        return ["- None"]
    rows = ["| ID | Status | Surface type | Matched paths |", "| --- | --- | --- | --- |"]
    for entry in entries:
        matched = ", ".join(entry.get("matched_paths", [])) or "-"
        surface_type = entry.get("surface_type") or "-"
        rows.append(f"| `{entry['id']}` | `{entry['status']}` | `{surface_type}` | `{matched}` |")
    return rows


def print_summary(audit: dict[str, Any]) -> None:
    print(
        "downstream divergence audit: "
        f"mirror={audit['mirror']['health']}, "
        f"tree_equal={audit['tree_diff']['tree_equal']}, "
        f"exit={audit['verdict']['exit_code']}"
    )


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def run_git(
    repo: Path,
    args: list[str],
    *,
    capture_stdout: bool,
    allow_failure: bool = False,
) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    env["LC_ALL"] = "C"
    result = subprocess.run(
        ["git", "-C", str(repo), *args],
        capture_output=capture_stdout,
        text=True,
        env=env,
        check=False,
    )
    if result.returncode != 0 and not allow_failure:
        raise RuntimeError(
            "git command failed: "
            f"{' '.join(args)}\nstdout={result.stdout.strip()}\nstderr={result.stderr.strip()}"
        )
    return result


class SnapshotChanged(RuntimeError):
    pass


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except SnapshotChanged as exc:
        print(f"snapshot changed: {exc}", file=sys.stderr)
        raise SystemExit(EXIT_UNSTABLE_SNAPSHOT)
    except (FileNotFoundError, ValueError) as exc:
        print(f"registry error: {exc}", file=sys.stderr)
        raise SystemExit(EXIT_STALE_REGISTRY)
    except RuntimeError as exc:
        print(f"audit command error: {exc}", file=sys.stderr)
        raise SystemExit(EXIT_INVALID_MIRROR)
    except Exception as exc:  # noqa: BLE001
        print(f"downstream-divergence-audit failed: {exc}", file=sys.stderr)
        raise SystemExit(EXIT_INVALID_MIRROR)
