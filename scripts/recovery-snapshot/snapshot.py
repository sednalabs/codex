#!/usr/bin/env python3
"""Build and restore-test a single, encrypted recovery snapshot package."""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import subprocess
import tempfile
from pathlib import Path


def run(*args: str, cwd: Path | None = None, stdout=None) -> str:
    p = subprocess.run(args, cwd=cwd, check=True, text=True, stdout=stdout,
                       stderr=subprocess.PIPE)
    return p.stdout if stdout is None else ""


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for block in iter(lambda: f.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def fail(msg: str) -> None:
    raise SystemExit(f"recovery snapshot: {msg}")


def package(ns: argparse.Namespace) -> None:
    repo = Path(ns.repo_dir).resolve()
    output = Path(ns.output).resolve()
    refs = [line.strip() for line in Path(ns.refs_file).read_text().splitlines() if line.strip()]
    if not refs or any(not (r.startswith("refs/heads/") or r.startswith("refs/tags/")) for r in refs):
        fail("refs file must contain one or more explicit refs/heads/* or refs/tags/* names")
    if len(set(refs)) != len(refs):
        fail("duplicate refs are not allowed")
    if len(ns.candidate_sha) != 40 or any(c not in "0123456789abcdefABCDEF" for c in ns.candidate_sha):
        fail("candidate SHA must be a full 40-character hexadecimal object ID")
    run("git", "cat-file", "-e", f"{ns.candidate_sha}^{{commit}}", cwd=repo)
    ref_manifest_hash = ""
    with tempfile.TemporaryDirectory(prefix="recovery-snapshot-") as td:
        root = Path(td)
        bare = root / "repository.git"
        shutil.copytree(repo, bare)
        # Keep only selected public refs; GitHub-managed and upstream refs are excluded.
        for ref in run("git", "for-each-ref", "--format=%(refname)", cwd=bare).splitlines():
            if ref and ref not in refs:
                run("git", "update-ref", "-d", ref, cwd=bare)
        manifest = []
        for ref in refs:
            oid = run("git", "rev-parse", ref, cwd=bare).strip()
            typ = run("git", "cat-file", "-t", oid, cwd=bare).strip()
            peeled = run("git", "rev-parse", f"{ref}^{{commit}}", cwd=bare).strip() if ref.startswith("refs/tags/") else None
            item = {"ref": ref, "object": oid, "type": typ, "peeled_commit": peeled}
            if ref.startswith("refs/tags/") and typ == "tag":
                raw = run("git", "cat-file", "-p", oid, cwd=bare)
                headers, _, message = raw.partition("\n\n")
                fields = dict(line.split(" ", 1) for line in headers.splitlines() if " " in line)
                item.update({"tagger": fields.get("tagger"), "tag_message": message.rstrip("\n"),
                             "signature_status": "valid" if subprocess.run(
                                 ("git", "verify-tag", "--raw", oid), cwd=bare,
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0 else "unverified"})
            manifest.append(item)
        (root / "ref-manifest.json").write_text(json.dumps({"refs": manifest}, sort_keys=True, indent=2) + "\n")
        ref_manifest_hash = sha256(root / "ref-manifest.json")
        metadata = root / "github-metadata"
        metadata.mkdir()
        src = Path(ns.metadata_dir)
        for name in ("repository.json", "pull-requests.json", "releases.json", "rulesets.json", "default-branch.json"):
            candidate = src / name
            if not candidate.is_file():
                fail(f"missing metadata file {name}")
            shutil.copyfile(candidate, metadata / name)
        (root / "exclusions.json").write_text(json.dumps({"excluded": [
            "upstream ref names and metadata", "GitHub-managed refs", "private keys and credentials",
            "plaintext package after encryption", "public release, R2, force-push, ref-delete, protection or main mutations"
        ], "shared_ancestor_objects": "remain when reachable from selected refs; this package does not erase Git objects"}, indent=2) + "\n")
        run("git", "bundle", "create", str(root / "refs.bundle"), "--all", cwd=bare)
        output.parent.mkdir(parents=True, exist_ok=True)
        if output.exists(): output.unlink()
        run("tar", "--sort=name", "--mtime=@0", "--owner=0", "--group=0", "--numeric-owner", "-czf", str(output),
            "refs.bundle", "ref-manifest.json", "github-metadata", "exclusions.json", cwd=root)
    print(json.dumps({"archive": str(output), "inner_sha256": sha256(output), "inner_size": output.stat().st_size,
                      "ref_manifest_sha256": ref_manifest_hash}, sort_keys=True))


def restore(ns: argparse.Namespace) -> None:
    archive = Path(ns.archive).resolve()
    if not archive.is_file() or archive.stat().st_size == 0:
        fail("archive is missing or empty")
    with tempfile.TemporaryDirectory(prefix="recovery-restore-") as td:
        root = Path(td)
        run("tar", "-xzf", str(archive), "-C", str(root))
        manifest = json.loads((root / "ref-manifest.json").read_text())
        if not isinstance(manifest.get("refs"), list) or not manifest["refs"]:
            fail("ref manifest is invalid")
        bundle = root / "refs.bundle"
        run("git", "bundle", "verify", str(bundle))
        restored = root / "restored.git"
        run("git", "clone", "--bare", str(bundle), str(restored))
        for item in manifest["refs"]:
            actual = run("git", "rev-parse", item["ref"], cwd=restored).strip()
            if actual != item["object"]:
                fail(f"restore ref mismatch for {item['ref']}")
            if item["type"] == "tag":
                peeled = run("git", "rev-parse", f"{item['ref']}^{{commit}}", cwd=restored).strip()
                if peeled != item["peeled_commit"]:
                    fail(f"restore peeled tag mismatch for {item['ref']}")
        for name in ("github-metadata/repository.json", "github-metadata/pull-requests.json", "github-metadata/releases.json", "github-metadata/rulesets.json", "github-metadata/default-branch.json", "exclusions.json"):
            json.loads((root / name).read_text())
    print(json.dumps({"restore_test": "passed", "archive_sha256": sha256(archive)}, sort_keys=True))


parser = argparse.ArgumentParser()
sub = parser.add_subparsers(dest="command", required=True)
p = sub.add_parser("package"); p.add_argument("--repo-dir", required=True); p.add_argument("--refs-file", required=True); p.add_argument("--metadata-dir", required=True); p.add_argument("--candidate-sha", required=True); p.add_argument("--output", required=True); p.set_defaults(func=package)
r = sub.add_parser("restore-test"); r.add_argument("--archive", required=True); r.set_defaults(func=restore)
ns = parser.parse_args()
try: ns.func(ns)
except subprocess.CalledProcessError as exc: fail(exc.stderr.strip() or "command failed")
