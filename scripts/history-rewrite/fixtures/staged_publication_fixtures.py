#!/usr/bin/env python3
"""Hosted plan, lifecycle and actual-CLI proof for explicit staged publication."""
from __future__ import annotations

import copy
from datetime import datetime, timedelta, timezone
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import publication as p
import protected_handoff as h
import staged_publication as s
from publication_fixtures import MockApi, base_manifest, observer_fixture_documents, publisher_fixture_documents


def reject(function) -> None:
    try:
        function()
    except (s.PlanError, p.PublicationError):
        return
    raise AssertionError("negative staged fixture unexpectedly accepted")


def plan_fixtures() -> None:
    selected = {f"refs/heads/branch-{index:03}": "1" * 40 for index in range(205)}
    selected.update({"refs/heads/main": "1" * 40, "refs/heads/unchanged": "2" * 40})
    output = {ref: "3" * 40 for ref in selected}
    output["refs/heads/unchanged"] = "2" * 40
    manifest = base_manifest(selected, output)
    required = {"refs/heads/main"}
    plan = s.build_plan(selected, output, canary_ref="refs/heads/branch-000", final_refs=sorted(required), required_final_refs=required)
    assert [len(batch["refs"]) for batch in plan["batches"]] == [1, 100, 100, 4, 1]
    manifest.update(publication_plan=plan, publication_plan_sha256=s.digest(plan))
    assert p.staged_plan(manifest) == plan
    maps = s.prefix_maps(plan, selected, output)
    assert [s.locate_prefix(maps, value) for value in maps] == list(range(6))
    assert maps[-1] == output
    invalid = []
    for value in (None, [], "staged", True, {}):
        invalid.append({**manifest, "publication_plan": value})
    invalid.append({**manifest, "publication_plan_sha256": "0" * 64})
    for key, value in (("batch_size", 50), ("mode", "atomic"), ("extra", True),
                       ("canary_ref", "refs/heads/main"), ("final_refs", [])):
        changed = copy.deepcopy(plan); changed[key] = value
        invalid.append({**manifest, "publication_plan": changed, "publication_plan_sha256": s.digest(changed)})
    reordered = copy.deepcopy(plan)
    reordered["batches"][1], reordered["batches"][2] = reordered["batches"][2], reordered["batches"][1]
    invalid.append({**manifest, "publication_plan": reordered, "publication_plan_sha256": s.digest(reordered)})
    for item in invalid:
        reject(lambda item=item: p.staged_plan(item))
    default = {key: value for key, value in manifest.items() if not key.startswith("publication_plan")}
    assert p.staged_plan(default) is None
    for actual in ({**maps[1], "refs/heads/foreign": "4" * 40},
                   {ref: oid for ref, oid in maps[1].items() if ref != "refs/heads/main"},
                   {**maps[1], "refs/heads/branch-204": output["refs/heads/branch-204"]}):
        reject(lambda actual=actual: s.locate_prefix(maps, actual))
        reject(lambda actual=actual: s.reverse_lease_plan(plan, selected, output, actual))
    reverse = s.reverse_lease_plan(plan, selected, output, maps[3])
    assert [step["to_prefix"] for step in reverse] == [2, 1, 0]
    assert all(update["expected_old_oid"] == output[update["ref"]]
               and update["new_oid"] == selected[update["ref"]]
               for step in reverse for update in step["leased_updates"])
    print("staged-plan: deterministic100/canary/final/default/schema/unique-prefix/reverse-leases passed")


def state_fixtures() -> None:
    selected = {ref: "1" * 40 for ref in ("refs/heads/canary", "refs/heads/middle", "refs/heads/main")}
    output = {ref: "2" * 40 for ref in selected}
    plan = s.build_plan(selected, output, canary_ref="refs/heads/canary", final_refs=["refs/heads/main"],
                        required_final_refs={"refs/heads/main"})
    for mode in ("expire", "rejected", "response-loss", "capture-loss", "checkpoint-loss", "readback-loss", "foreign"):
        actual = dict(selected)
        calls = []
        checkpoints = []
        gate_calls = []

        def read():
            if mode == "readback-loss" and calls:
                raise p.PublicationError("fixture readback unavailable")
            return dict(actual)

        def push(batch, before, after):
            assert actual == before
            calls.append(batch["index"])
            if mode == "rejected":
                raise p.PublicationError("fixture server rejected the transaction")
            actual.update(after)
            if mode == "foreign":
                actual["refs/heads/foreign"] = "3" * 40
            if mode in {"response-loss", "readback-loss"}:
                raise p.PublicationError("fixture transport response lost")

        def gate(*, observe):
            gate_calls.append(observe)
            if mode == "expire" and calls:
                raise p.PublicationError("fixture authority expired")

        def checkpoint(value):
            if mode == "checkpoint-loss" and calls:
                raise OSError("fixture checkpoint disk unavailable")
            checkpoints.append(copy.deepcopy(value))

        def execute():
            return s.run_batches(plan, selected, output, read_refs=read, push=push, check_gate=gate,
                capture_complete=lambda: not (mode == "capture-loss" and calls), checkpoint=checkpoint,
                error_type=p.PublicationError)

        if mode in {"readback-loss", "foreign"}:
            reject(execute)
        else:
            result = execute()
            expected_calls = [0, 1, 2] if mode == "response-loss" else [0]
            assert calls == expected_calls
            assert result.outcome == ("success" if mode == "response-loss" else "checkpointed")
            assert result.after_refs == actual
            assert result.progress["completed_prefix"] == (3 if mode == "response-loss" else 0 if mode == "rejected" else 1)
        assert len(calls) == len(set(calls))
        assert gate_calls[:2] == [True, False]
    print("staged-state: expiry/rejection/response-loss/capture-loss/checkpoint-loss/readback-loss/foreign passed")


class CliFixture:
    def __init__(self, root: Path):
        self.root = root
        self.source = root / "source.git"
        self.real_git = shutil.which("git")
        assert self.real_git
        self.env = {**os.environ, "HISTORY_REWRITE_PUBLICATION_FIXTURE": "1",
                    "GIT_AUTHOR_NAME": "Fixture", "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
                    "GIT_COMMITTER_NAME": "Fixture", "GIT_COMMITTER_EMAIL": "fixture@example.invalid"}
        self.command("init", "--bare", str(self.source))
        tree = self.git(self.source, "mktree", input=b"").strip()
        old = self.git(self.source, "commit-tree", tree, "-m", "before").strip()
        new = self.git(self.source, "commit-tree", tree, "-p", old, "-m", "after").strip()
        refs = {*p.PROTECTED_REFS, f"refs/heads/{p.PUBLICATION_BRANCH}",
                "refs/heads/canary", "refs/heads/middle-a", "refs/heads/middle-b", "refs/tags/fixture-release"}
        self.selected = {ref: old for ref in refs}
        self.output = {ref: new for ref in refs}
        self.manifest = base_manifest(self.selected, self.output)
        final = sorted(set(p.PROTECTED_REFS) | {f"refs/heads/{p.PUBLICATION_BRANCH}", "refs/tags/fixture-release"})
        self.plan = s.build_plan(self.selected, self.output, canary_ref="refs/heads/canary", final_refs=final,
                                 required_final_refs=set(final))
        self.manifest.update(publication_plan=self.plan, publication_plan_sha256=s.digest(self.plan))
        for ref, oid in self.output.items():
            self.git(self.source, "update-ref", ref, oid)
        key = root / "fixture-age-identity"
        keygen = os.environ["HISTORY_REWRITE_CAPTURE_KEYGEN"]
        subprocess.run([keygen, "-o", str(key)], check=True, capture_output=True)
        self.recipient = subprocess.check_output([keygen, "-y", str(key)]).decode().strip()
        wrapper = root / "bin"; wrapper.mkdir()
        script = wrapper / "git"
        script.write_text(
            f"#!{sys.executable}\nimport json, os, signal, subprocess, sys\nfrom pathlib import Path\n"
            f"real={self.real_git!r}\nargs=sys.argv[1:]\n"
            "if len(args)>2 and args[0]=='-C' and args[2]=='push':\n"
            "    log=Path(os.environ['STAGED_FIXTURE_PUSH_LOG'])\n"
            "    with log.open('a') as stream: stream.write(json.dumps(args)+'\\n')\n"
            "    result=subprocess.run([real,*args], capture_output=True)\n"
            "    count=len(log.read_text().splitlines())\n"
            "    if result.returncode==0 and count==1:\n"
            "        if os.environ.get('STAGED_FIXTURE_MODE')=='crash':\n"
            "            os.kill(os.getppid(), signal.SIGKILL)\n"
            "            sys.exit(0)\n"
            "        if os.environ.get('STAGED_FIXTURE_MODE')=='lost-response':\n"
            "            sys.stderr.buffer.write(b'fixture response lost after receive\\n')\n"
            "            sys.exit(72)\n"
            "    sys.stdout.buffer.write(result.stdout);sys.stderr.buffer.write(result.stderr)\n"
            "    sys.exit(result.returncode)\n"
            "os.execv(real,[real,*args])\n")
        script.chmod(0o700)
        self.env["PATH"] = str(wrapper) + os.pathsep + self.env.get("PATH", "")

    def command(self, *args, input=None):
        return subprocess.run([self.real_git, *args], input=input, capture_output=True, check=True, env=self.env).stdout.decode()

    def git(self, repo, *args, input=None):
        return self.command("-C", str(repo), *args, input=input)

    def remote(self, name):
        remote = self.root / f"{name}.git"
        self.command("init", "--bare", str(remote))
        self.git(self.source, "push", "--atomic", str(remote), *(f"{oid}:{ref}" for ref, oid in self.selected.items()))
        return remote

    def attempt(self, label: str, remote: Path, *, run_id: int, mode: str = "normal", bad_artifact=False):
        root = self.root / label; root.mkdir()
        manifest = self.manifest
        p.write_json(root / "manifest.json", manifest)
        api_dir = root / "api"; api_dir.mkdir()
        protection = manifest["controls"]["maintenance_plan"]["administrator_after"]
        mock = MockApi({}, protection=protection)
        workflows = {str(identifier): {"id": identifier, "path": path, "state": "disabled_manually"}
                     for path, identifier in p.WRITER_WORKFLOWS.items()}
        workflows[str(p.MIRROR_WORKFLOW_ID)] = {"id": p.MIRROR_WORKFLOW_ID,
            "path": ".github/workflows/sedna-sync-upstream.yml", "state": "disabled_manually"}
        for identifier, value in workflows.items():
            p.write_json(api_dir / f"workflow-{identifier}.json", value)
            if int(identifier) != p.MIRROR_WORKFLOW_ID:
                p.write_json(api_dir / f"writer-runs-{identifier}.json", [
                    {"requested_status": state, "response": {"total_count": 0, "workflow_runs": []}}
                    for state in sorted(p.ACTIVE_RUN_STATES)])
        preflight = root / "preflight"
        p.record_live_preflight(manifest, api_dir=api_dir, output=preflight)
        p.write_restoration_intent(manifest, preflight_path=preflight / "preflight.json",
            control_plan=preflight / "control-plan.json", intent_path=root / "publication-restoration-intent.json")
        now = datetime.now(timezone.utc)
        prepared = {"phase": "publication", "run_id": 77, "run_attempt": 1}
        artifact = {"artifact_id": 777, "run_id": 77, "head_sha": manifest["harness_sha"],
                    "artifact_api_digest": "c" * 64, "artifact_size": 1234}
        binding = h.approval_binding(manifest, prepared, artifact, run_id=run_id, attempt=1, phase="publication")
        attestation = {"schema": h.ATTESTATION_SCHEMA, "binding_sha256": p.digest(binding),
            "administrator_snapshot_sha256": p.digest(protection),
            "observed_at": (now - timedelta(seconds=1)).isoformat(),
            "expires_at": (now + timedelta(seconds=599)).isoformat()}
        run = {"id": run_id, "run_attempt": 1, "head_sha": manifest["harness_sha"],
            "repository": {"id": p.REPOSITORY_ID}, "event": "workflow_dispatch", "head_branch": p.PUBLICATION_BRANCH,
            "path": ".github/workflows/history-rewrite-candidate.yml", "run_started_at": (now - timedelta(seconds=3)).isoformat()}
        approval = {"state": "approved", "user": {"id": p.REVIEWER_ID, "login": p.REVIEWER_LOGIN},
            "environments": [{"id": h.ENVIRONMENT_ID, "name": p.ENVIRONMENT_NAME}], "comment": p.canonical_json(attestation).decode()}
        documents = {**observer_fixture_documents(), **publisher_fixture_documents(),
            "workflows": workflows,
            "writer_runs": {str(value): {"total_count": 0, "workflow_runs": []} for value in p.WRITER_WORKFLOWS.values()},
            "branch_protection_document": mock.post_graphql(h.SCALAR_QUERY, {}),
            "rulesets": [{"id": row["id"]} for row in protection["repository_rulesets"]],
            "ruleset_documents": {str(row["id"]): mock.get(f"/repos/{p.REPOSITORY}/rulesets/{row['id']}")
                                  for row in protection["repository_rulesets"]},
            "responses": {f"/repos/{p.REPOSITORY}/actions/runs/{run_id}/attempts/1": run,
                          f"/repos/{p.REPOSITORY}/actions/runs/{run_id}/approvals": [approval]}}
        handoff = h.verify_current(manifest, binding, p.FixtureApi(documents, principal="workflow"),
                                    p.FixtureApi(documents, principal="observer"), now=now)
        p.write_json(root / "handoff.json", handoff)
        intent = p.staged_intent(manifest, handoff, preflight / "preflight.json", preflight / "control-plan.json",
                                 root / "publication-restoration-intent.json")
        p.write_json(root / "publication-staged-intent.json", intent)
        archive = root / "intent.zip"
        with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as output:
            for path in (root / "manifest.json", root / "publication-staged-intent.json",
                         root / "publication-restoration-intent.json", preflight / "preflight.json", preflight / "control-plan.json"):
                output.write(path, path.relative_to(root))
        identifier = run_id + 1000
        metadata = {"id": identifier, "name": f"history-rewrite-publication-intent-{run_id}", "expired": False,
            "size_in_bytes": archive.stat().st_size, "digest": "sha256:" + p.file_digest(archive),
            "workflow_run": {"id": run_id, "head_sha": manifest["harness_sha"], "repository_id": p.REPOSITORY_ID}}
        if bad_artifact:
            metadata["workflow_run"]["id"] = run_id - 1
        documents["responses"][f"/repos/{p.REPOSITORY}/actions/artifacts/{identifier}"] = metadata
        p.write_json(root / "fixture-api.json", documents)
        log = root / "pushes.jsonl"
        environment = {**self.env, "GITHUB_RUN_ID": str(run_id), "GITHUB_RUN_ATTEMPT": "1",
                       "STAGED_FIXTURE_PUSH_LOG": str(log), "STAGED_FIXTURE_MODE": mode}
        command = [sys.executable, str(Path(p.__file__)), "publish", str(root / "manifest.json"),
            "--repo", str(self.source), "--remote-url", str(remote), "--receipt", str(root / "receipt.json"),
            "--frozen-sha", manifest["harness_sha"], "--frozen-tree", manifest["harness_tree"],
            "--manifest-sha256", p.digest(manifest), "--current-run-id", str(run_id),
            "--preflight", str(preflight / "preflight.json"), "--control-plan", str(preflight / "control-plan.json"),
            "--intent", str(root / "publication-restoration-intent.json"), "--handoff", str(root / "handoff.json"),
            "--intent-artifact-id", str(identifier), "--fixture-intent-zip", str(archive),
            "--fixture-api", str(root / "fixture-api.json"), "--checkpoint", str(root / "checkpoint.json"),
            "--capture-root", str(root / "capture"), "--capture-age", os.environ["HISTORY_REWRITE_CAPTURE_AGE"],
            "--capture-recipient", self.recipient]
        result = subprocess.run(command, env=environment, capture_output=True)
        pushes = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
        return root, result, pushes


def cli_fixtures(root: Path) -> None:
    fixture = CliFixture(root)
    remote = fixture.remote("lost-response")
    attempt, result, pushes = fixture.attempt("lost-response-attempt", remote, run_id=501, mode="lost-response")
    assert result.returncode == 0, result.stderr.decode(errors="replace")
    receipt = p.load_object(attempt / "receipt.json")
    assert receipt["outcome"] == "success" and receipt["after_refs"] == fixture.output
    assert len(pushes) == len(fixture.plan["batches"])
    for args, batch in zip(pushes, fixture.plan["batches"], strict=True):
        assert "--atomic" in args
        assert {arg for arg in args if arg.startswith("--force-with-lease=")} == {
            f"--force-with-lease={ref}:{fixture.selected[ref]}" for ref in batch["refs"]}
    assert receipt["diagnostics"][0]["exit_code"] == 72
    print("staged-actual-cli: lost successful response resolved without retry; exact leases and final clean fetch passed")

    remote = fixture.remote("crash")
    attempt, result, pushes = fixture.attempt("crash-attempt", remote, run_id=502, mode="crash")
    assert result.returncode == -9 and len(pushes) == 1
    maps = s.prefix_maps(fixture.plan, fixture.selected, fixture.output)
    assert p.advertised_refs(str(remote)) == maps[1]
    assert maps[1][f"refs/heads/{p.PUBLICATION_BRANCH}"] == fixture.selected[f"refs/heads/{p.PUBLICATION_BRANCH}"]
    (attempt / "checkpoint.json").unlink()
    resumed, result, pushes = fixture.attempt("resumed-attempt", remote, run_id=503)
    assert result.returncode == 0, result.stderr.decode(errors="replace")
    receipt = p.load_object(resumed / "receipt.json")
    assert receipt["after_refs"] == fixture.output
    assert receipt["staged_progress"]["start_prefix"] == 1
    assert receipt["staged_progress"]["prior_run_capture_custody"] == "requires-independent-reconciliation"
    assert len(pushes) == len(fixture.plan["batches"]) - 1
    print("staged-actual-cli: hard process loss plus deleted checkpoint resumes from full namespace under new run/approval")

    remote = fixture.remote("wrong-artifact")
    _, result, pushes = fixture.attempt("wrong-artifact-attempt", remote, run_id=504, bad_artifact=True)
    assert result.returncode != 0 and not pushes and p.advertised_refs(str(remote)) == fixture.selected
    remote = fixture.remote("foreign")
    fixture.git(remote, "update-ref", "refs/heads/foreign", fixture.selected["refs/heads/main"])
    before = p.advertised_refs(str(remote))
    _, result, pushes = fixture.attempt("foreign-attempt", remote, run_id=505)
    assert result.returncode != 0 and not pushes and p.advertised_refs(str(remote)) == before
    print("staged-actual-cli: wrong durable artifact and foreign namespace rejected with zero pushes")


def main() -> None:
    plan_fixtures()
    state_fixtures()
    with tempfile.TemporaryDirectory(prefix="staged-publication-fixtures-") as temporary:
        cli_fixtures(Path(temporary))


if __name__ == "__main__":
    main()
