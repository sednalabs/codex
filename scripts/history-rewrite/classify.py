#!/usr/bin/env python3
"""Bounded, identity-only history occurrence classification."""
import hashlib
import json
import gzip
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

REPO, POLICY, OUTPUT = map(Path, sys.argv[1:])
MAX_ROWS = 1000000
MAX_MATCHES = 2000000
policy = json.loads(POLICY.read_text(encoding="utf-8"))
def compile_pattern(pattern, label):
    rx = re.compile(pattern, re.MULTILINE)
    if rx.search("") is not None:
        raise SystemExit(f"zero-width regex is not allowed: {label}")
    return rx

base_patterns = [(p["id"], compile_pattern(p["pattern"], p["id"]), p["classification"], p["rationale"], p.get("priority", 0), {"commit_subject", "commit_body", "path", "blob"}) for p in policy.get("patterns", [])]
rewrite_patterns = []
for rule in policy.get("rules", []):
    if rule.get("scope") in {"commit_subject", "commit_body", "path", "blob"}:
        if not rule.get("old") or not rule.get("new"):
            raise SystemExit("rewrite rules require non-empty old and new literals")
        scope = {rule["scope"]}
        rewrite_patterns.append((rule["id"] + ":old", compile_pattern(re.escape(rule["old"]), rule["id"] + ":old"), "rewrite_rule_old", rule.get("proof", ""), rule.get("priority", 50), scope))
        rewrite_patterns.append((rule["id"] + ":new", compile_pattern(re.escape(rule["new"]), rule["id"] + ":new"), "rewrite_rule_new", rule.get("proof", ""), rule.get("priority", 50), scope))

ACCEPTED_PATTERN_COUNT = len(base_patterns) + len(rewrite_patterns)
if ACCEPTED_PATTERN_COUNT == 0:
    raise SystemExit("pattern family is required")

def git(*args):
    return subprocess.check_output(["git", "-C", str(REPO), *args], text=True, errors="replace")

class GitCatFileBatch:
    def __init__(self):
        self.process = subprocess.Popen(
            ["git", "-C", str(REPO), "cat-file", "--batch"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )

    def read(self, identity):
        self.process.stdin.write((identity + "\n").encode("ascii"))
        self.process.stdin.flush()
        header = self.process.stdout.readline()
        if not header:
            raise RuntimeError("git cat-file --batch terminated unexpectedly")
        fields = header.rstrip(b"\n").split()
        if len(fields) == 2 and fields[1] == b"missing":
            return None
        if len(fields) != 3:
            raise RuntimeError("malformed git cat-file --batch response")
        try:
            size = int(fields[2])
        except ValueError as exc:
            raise RuntimeError("malformed git cat-file --batch object size") from exc
        value = self.process.stdout.read(size)
        if len(value) != size or self.process.stdout.read(1) != b"\n":
            raise RuntimeError("truncated git cat-file --batch response")
        return value

    def close(self):
        if self.process.stdin is not None:
            try:
                self.process.stdin.close()
            except OSError:
                pass
        if self.process.poll() is None:
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.terminate()
                try:
                    self.process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    self.process.kill()
                    self.process.wait()
        if self.process.stdout is not None:
            self.process.stdout.close()

class OutputWriter:
    """Write complete metadata rows to plain and deterministic gzip streams."""
    def __init__(self, output):
        self.output = output
        self.tempdir = output.parent
        self.plain_temp = None
        self.gzip_temp = None
        self.plain = None
        self.compressed = None
        self.plain_hash = hashlib.sha256()
        self.compressed_hash = hashlib.sha256()

    def __enter__(self):
        self.plain_temp = tempfile.NamedTemporaryFile("wb", dir=self.tempdir, prefix=f".{self.output.name}.", suffix=".tmp", delete=False)
        self.gzip_temp = tempfile.NamedTemporaryFile("wb", dir=self.tempdir, prefix=f".{self.output.name}.", suffix=".gz.tmp", delete=False)
        self.plain = self.plain_temp
        self.compressed = gzip.GzipFile(fileobj=self.gzip_temp, mode="wb", filename="", mtime=0)
        self.write("kind\tidentity\tpath\tpattern_class\tclassification\trationale_sha256\tproof_sha256\tmatch_count\n")
        return self

    def write(self, text):
        data = text.encode("utf-8")
        self.plain.write(data)
        self.compressed.write(data)
        self.plain_hash.update(data)

    def close(self):
        self.compressed.close()
        self.gzip_temp.flush()
        self.gzip_temp.close()
        self.plain.flush()
        self.plain.close()
        with open(self.gzip_temp.name, "rb") as compressed:
            for chunk in iter(lambda: compressed.read(1024 * 1024), b""):
                self.compressed_hash.update(chunk)

    def commit(self):
        os.replace(self.plain_temp.name, self.output)
        compressed_output = Path(str(self.output) + ".gz")
        os.replace(self.gzip_temp.name, compressed_output)
        return compressed_output

    def cleanup(self):
        for path in (self.plain_temp, self.gzip_temp):
            if path is not None:
                try:
                    os.unlink(path.name)
                except FileNotFoundError:
                    pass

row_count = 0
total_matches = 0
scanned_fields = 0
scanned_utf8_bytes = 0
admitted_fields = 0
per_pattern = {}
per_kind = {}
def match_metadata(kind, value):
    metadata = []
    for pid, rx, classification, rationale, _priority, scopes in rewrite_patterns:
        if kind in scopes and rx.search(value):
            count = len(rx.findall(value))
            metadata.append((pid, classification, hashlib.sha256(rationale.encode()).hexdigest(), count))
    matches = [(pid, rx, classification, rationale, priority) for pid, rx, classification, rationale, priority, scopes in base_patterns if kind in scopes and rx.search(value)]
    if matches:
        top = max(x[4] for x in matches)
        winners = [x for x in matches if x[4] == top]
        if len({x[2] for x in winners}) > 1:
            raise SystemExit("overlapping classification patterns require explicit priority")
        for pid, rx, classification, rationale, _ in winners:
            count = len(rx.findall(value))
            metadata.append((pid, classification, hashlib.sha256(rationale.encode()).hexdigest(), count))
    proof = hashlib.sha256(value.encode("utf-8", "replace")).hexdigest() if metadata else ""
    return proof, metadata

def emit(kind, identity, path, proof, metadata):
    global total_matches, admitted_fields, row_count
    if metadata:
        admitted_fields += 1
    per_kind.setdefault(kind, {"scanned_fields": 0, "admitted_fields": 0, "rows": 0, "matches": 0})
    if metadata:
        per_kind[kind]["admitted_fields"] += 1
    for pid, classification, rationale_sha256, count in metadata:
        total_matches += count
        row = (kind, identity, path, pid, classification, rationale_sha256, proof, str(count))
        row_count += 1
        writer.write("\t".join(row) + "\n")
        per_pattern.setdefault(pid, {"rows": 0, "matches": 0})
        per_pattern[pid]["rows"] += 1
        per_pattern[pid]["matches"] += count
        per_kind[kind]["rows"] += 1
        per_kind[kind]["matches"] += count
        if row_count > MAX_ROWS:
            raise SystemExit(f"classification row bound exceeded: rows={row_count} max_rows={MAX_ROWS}; narrow the pattern family")
        if total_matches > MAX_MATCHES:
            raise SystemExit(f"classification match bound exceeded: total_matches={total_matches} max_matches={MAX_MATCHES}; narrow the pattern family")

def scan(kind, identity, value, path=""):
    global scanned_fields, scanned_utf8_bytes
    encoded = value.encode("utf-8", "replace")
    account_scan(kind, len(encoded))
    proof, metadata = match_metadata(kind, value)
    emit(kind, identity, path, proof, metadata)

def account_scan(kind, byte_count):
    global scanned_fields, scanned_utf8_bytes
    scanned_fields += 1
    scanned_utf8_bytes += byte_count
    per_kind.setdefault(kind, {"scanned_fields": 0, "admitted_fields": 0, "rows": 0, "matches": 0})
    per_kind[kind]["scanned_fields"] += 1

OUTPUT.parent.mkdir(parents=True, exist_ok=True)
writer = OutputWriter(OUTPUT)
writer.__enter__()

for identity in git("rev-list", "--all").splitlines():
    record = git("show", "-s", "--format=%H%x00%s%x00%b", identity)
    fields = record.split("\x00", 2)
    if len(fields) == 3:
        scan("commit_subject", fields[0], fields[1])
        scan("commit_body", fields[0], fields[2])

blob_match_cache = {}
batch = GitCatFileBatch()
try:
    for commit in git("rev-list", "--all").splitlines():
        for record in subprocess.check_output(["git", "-C", str(REPO), "ls-tree", "-r", "-z", commit], text=False).split(b"\0"):
            if not record: continue
            head, path_bytes = record.split(b"\t", 1)
            mode, typ, identity = head.split()
            path = path_bytes.decode("utf-8", "replace")
            identity = identity.decode()
            scan("path", identity, path, path)
            if typ.decode() != "blob": continue
            cached = blob_match_cache.get(identity)
            if cached is None:
                blob = batch.read(identity)
                if blob is None:
                    raise RuntimeError(f"git cat-file --batch missing blob {identity}")
                proof, metadata = match_metadata("blob", blob.decode("utf-8", "replace"))
                cached = (proof, metadata, len(blob))
                blob_match_cache[identity] = cached
                byte_count = len(blob)
            else:
                byte_count = cached[2]
            account_scan("blob", byte_count)
            emit("blob", identity, path, cached[0], cached[1])
finally:
    batch.close()

try:
    for pid in sorted(per_pattern):
        writer.write(f"#count\t{pid}\t{per_pattern[pid]['matches']}\n")
    writer.close()
except Exception:
    writer.cleanup()
    raise

row_capacity = scanned_fields * ACCEPTED_PATTERN_COUNT
match_capacity = scanned_utf8_bytes * ACCEPTED_PATTERN_COUNT
if row_count > row_capacity or total_matches > match_capacity:
    writer.cleanup()
    raise SystemExit("classification capacity accounting mismatch")
if sum(item["scanned_fields"] for item in per_kind.values()) != scanned_fields or sum(item["admitted_fields"] for item in per_kind.values()) != admitted_fields:
    raise SystemExit("classification field counter mismatch")
if sum(item["rows"] for item in per_kind.values()) != row_count or sum(item["matches"] for item in per_kind.values()) != total_matches:
    raise SystemExit("classification row/match counter mismatch")

def file_digest(path):
    digest = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()

plain_temp_path = Path(writer.plain_temp.name)
compressed_temp_path = Path(writer.gzip_temp.name)
if file_digest(plain_temp_path) != writer.plain_hash.hexdigest() or file_digest(compressed_temp_path) != writer.compressed_hash.hexdigest():
    raise SystemExit("classification output digest mismatch")
plain_bytes = plain_temp_path.stat().st_size
compressed_bytes = compressed_temp_path.stat().st_size
compressed_output = Path(str(OUTPUT) + ".gz")
writer.commit()
metadata = {
    "schema": 2,
    "accepted_pattern_count": ACCEPTED_PATTERN_COUNT,
    "capacities": {
        "rows_formula": "scanned_fields * accepted_pattern_count",
        "matches_formula": "scanned_utf8_bytes * accepted_pattern_count",
        "rows": row_capacity,
        "matches": match_capacity,
    },
    "scanned": {"fields": scanned_fields, "utf8_bytes": scanned_utf8_bytes},
    "admitted": {"fields": admitted_fields, "rows": row_count, "matches": total_matches},
    "per_kind": per_kind,
    "per_pattern": per_pattern,
    "digests": {
        "plain": {"path": OUTPUT.name, "sha256": writer.plain_hash.hexdigest(), "bytes": plain_bytes},
        "compressed": {"path": compressed_output.name, "sha256": writer.compressed_hash.hexdigest(), "bytes": compressed_bytes},
    },
}
metadata_path = Path(str(OUTPUT) + ".metadata.json")
metadata_temp = tempfile.NamedTemporaryFile("w", encoding="utf-8", dir=metadata_path.parent, prefix=f".{metadata_path.name}.", suffix=".tmp", delete=False)
try:
    with metadata_temp as fh:
        json.dump(metadata, fh, sort_keys=True, separators=(",", ":"))
        fh.write("\n")
    os.replace(metadata_temp.name, metadata_path)
except Exception:
    try:
        os.unlink(metadata_temp.name)
    except FileNotFoundError:
        pass
    raise
