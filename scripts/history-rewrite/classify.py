#!/usr/bin/env python3
"""Bounded, identity-only history occurrence classification."""
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path

REPO, POLICY, OUTPUT = map(Path, sys.argv[1:])
MAX_ROWS = 5000
MAX_MATCHES = 100000
policy = json.loads(POLICY.read_text(encoding="utf-8"))
base_patterns = [(p["id"], re.compile(p["pattern"], re.MULTILINE), p["classification"], p["rationale"], p.get("priority", 0), {"commit_subject", "commit_body", "path", "blob"}) for p in policy.get("patterns", [])]
rewrite_patterns = []
for rule in policy.get("rules", []):
    if rule.get("scope") in {"commit_subject", "commit_body", "path", "blob"}:
        if not rule.get("old") or not rule.get("new"):
            raise SystemExit("rewrite rules require non-empty old and new literals")
        scope = {rule["scope"]}
        rewrite_patterns.append((rule["id"] + ":old", re.compile(re.escape(rule["old"]), re.MULTILINE), "rewrite_rule_old", rule.get("proof", ""), rule.get("priority", 50), scope))
        rewrite_patterns.append((rule["id"] + ":new", re.compile(re.escape(rule["new"]), re.MULTILINE), "rewrite_rule_new", rule.get("proof", ""), rule.get("priority", 50), scope))

def git(*args):
    return subprocess.check_output(["git", "-C", str(REPO), *args], text=True, errors="replace")

rows = []
total_matches = 0
def scan(kind, identity, value, path=""):
    global total_matches
    for pid, rx, classification, rationale, _priority, scopes in rewrite_patterns:
        if kind in scopes and rx.search(value):
            proof = hashlib.sha256(value.encode("utf-8", "replace")).hexdigest()
            count = len(rx.findall(value))
            total_matches += count
            rows.append((kind, identity, path, pid, classification, hashlib.sha256(rationale.encode()).hexdigest(), proof, str(count)))
            if len(rows) > MAX_ROWS or total_matches > MAX_MATCHES:
                raise SystemExit("classification row or occurrence bound exceeded; narrow the pattern family")
    matches = [(pid, rx, classification, rationale, priority) for pid, rx, classification, rationale, priority, scopes in base_patterns if kind in scopes and rx.search(value)]
    if matches:
        top = max(x[4] for x in matches)
        winners = [x for x in matches if x[4] == top]
        if len({x[2] for x in winners}) > 1:
            raise SystemExit("overlapping classification patterns require explicit priority")
        for pid, rx, classification, rationale, _ in winners:
            proof = hashlib.sha256(value.encode("utf-8", "replace")).hexdigest()
            count = len(rx.findall(value))
            total_matches += count
            rows.append((kind, identity, path, pid, classification, hashlib.sha256(rationale.encode()).hexdigest(), proof, str(count)))
            if len(rows) > MAX_ROWS or total_matches > MAX_MATCHES:
                raise SystemExit("classification row or occurrence bound exceeded; narrow the pattern family")

for identity in git("rev-list", "--all").splitlines():
    record = git("show", "-s", "--format=%H%x00%s%x00%b", identity)
    fields = record.split("\x00", 2)
    if len(fields) == 3:
        scan("commit_subject", fields[0], fields[1])
        scan("commit_body", fields[0], fields[2])

for commit in git("rev-list", "--all").splitlines():
    for record in subprocess.check_output(["git", "-C", str(REPO), "ls-tree", "-r", "-z", commit], text=False).split(b"\0"):
        if not record: continue
        head, path_bytes = record.split(b"\t", 1)
        mode, typ, identity = head.split()
        path = path_bytes.decode("utf-8", "replace")
        identity = identity.decode()
        scan("path", identity, path, path)
        if typ.decode() != "blob": continue
        if subprocess.check_output(["git", "-C", str(REPO), "cat-file", "-t", identity], text=True).strip() != "blob":
            continue
        try:
            blob = subprocess.check_output(["git", "-C", str(REPO), "cat-file", "-p", identity], text=False, stderr=subprocess.DEVNULL)
        except subprocess.CalledProcessError:
            continue
        scan("blob", identity, blob.decode("utf-8", "replace"), path)

OUTPUT.parent.mkdir(parents=True, exist_ok=True)
with OUTPUT.open("w", encoding="utf-8") as fh:
    fh.write("kind\tidentity\tpath\tpattern_class\tclassification\trationale_sha256\tproof_sha256\tmatch_count\n")
    for row in rows: fh.write("\t".join(row) + "\n")
    counts = {}
    for row in rows: counts[row[3]] = counts.get(row[3], 0) + int(row[7])
    for pid in sorted(counts): fh.write(f"#count\t{pid}\t{counts[pid]}\n")
