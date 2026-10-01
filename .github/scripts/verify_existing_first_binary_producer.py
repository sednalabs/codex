#!/usr/bin/env python3
"""Verify the one admitted first-binary producer run and current consumer identity.

This helper deliberately supports one preserved cross-run producer. The build
mode path remains same-run and resolves only the just-built native artifact.
All provider/API material is passed through typed environment fields; secrets
are never printed or written to the evidence records.
"""

from __future__ import annotations

import hashlib
import json
import os
import platform
import re
import sys
from pathlib import Path
import xml.etree.ElementTree as ET
from typing import Any, Mapping
from urllib.error import HTTPError, URLError
from urllib.parse import urlsplit
from urllib.request import Request, urlopen

REPOSITORY = "sednalabs/codex"
REPOSITORY_ID = 1152496647
WORKFLOW_ID = 250252262
WORKFLOW_PATH = ".github/workflows/sedna-branch-build.yml"
SHA = re.compile(r"[0-9a-f]{40}\Z")
DIGEST = re.compile(r"sha256:[0-9a-f]{64}\Z")
RUN_ID = re.compile(r"[1-9][0-9]*\Z")
ARCHES = {
    "x86_64": {
        "target": "x86_64-unknown-linux-gnu",
        "runner": "ubuntu-24.04",
        "run_arch": "X64",
        "package_job": "Package native Linux x86_64",
        "consumer_job": "Consume native Linux x86_64 package",
    },
    "aarch64": {
        "target": "aarch64-unknown-linux-gnu",
        "runner": "ubuntu-24.04-arm",
        "run_arch": "ARM64",
        "package_job": "Package native Linux ARM64",
        "consumer_job": "Consume native Linux ARM64 package",
    },
}
CROSS_RUN_PRODUCER = {
    "repository": REPOSITORY,
    "repository_id": REPOSITORY_ID,
    "workflow_id": WORKFLOW_ID,
    "workflow_path": WORKFLOW_PATH,
    "run_id": 36800811941,
    "run_attempt": 1,
    "event": "workflow_dispatch",
    "workflow_host_sha": "954251aea3d0602c72679c81aac35d88f4b350fe",
    "branch": "reconstruct/first-binary-package-workflow-20261001",
    "product_sha": "d0ea2de6eae3ab59d3c76787cad4d79975b71bfe",
    "comparison_base_ref": "main",
    "comparison_base_sha": "4a1ecb1e26fa0c6e8933bb73188bc6735da18ee7",
}
CROSS_RUN_ARTIFACTS = {
    "x86_64": {
        "id": 11137590827,
        "name": "sedna-first-binary-d0ea2de6eae3ab59d3c76787cad4d79975b71bfe-x86_64-36800811941",
        "digest": "sha256:1092ce38c57acc32762fda825c2593d34d4cf81a8e7c4164737550273a0912a9",
        "size_in_bytes": 279686623,
    },
    "aarch64": {
        "id": 11135444900,
        "name": "sedna-first-binary-d0ea2de6eae3ab59d3c76787cad4d79975b71bfe-aarch64-36800811941",
        "digest": "sha256:eeeead930077314435ac0bdef6c09254118c0106b15b24f5e4126f2941ecbde7",
        "size_in_bytes": 275553014,
    },
}
PRODUCER_JOB_CONTRACT = {
    "Verify standard runner graph and exact identities": ("success", "ubuntu-24.04", True),
    "Package native Linux x86_64": ("success", "ubuntu-24.04", True),
    "Package native Linux ARM64": ("success", "ubuntu-24.04-arm", True),
    "Prepare exact Cargo lock and app-server schema diff": ("skipped", "ubuntu-24.04", False),
    "Consume native Linux ARM64 package": ("failure", "ubuntu-24.04-arm", True),
    "Consume native Linux x86_64 package": ("failure", "ubuntu-24.04", True),
}


def _require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def _is_int(value: object, *, minimum: int = 1) -> bool:
    return type(value) is int and value >= minimum


def _object(value: object, message: str) -> Mapping[str, Any]:
    _require(isinstance(value, dict), message)
    return value


def _validate_hosted_job(job: Mapping[str, Any], label: str, *, ran: bool) -> None:
    _require(job.get("labels") == [label], "Actions job runner label is not an exact standard label")
    if ran:
        _require(job.get("runner_group_id") == 0, "Actions job is not in the GitHub-hosted runner group")
        _require(job.get("runner_group_name") == "GitHub Actions", "Actions job runner group is not GitHub-hosted")
        runner_name = job.get("runner_name")
        _require(
            isinstance(runner_name, str) and runner_name.startswith("GitHub Actions "),
            "Actions job does not identify a GitHub-hosted runner",
        )
    else:
        _require(job.get("runner_group_id") is None, "skipped Actions job unexpectedly acquired a runner")
        _require(job.get("runner_group_name") is None, "skipped Actions job unexpectedly acquired a runner group")
        _require(job.get("runner_name") is None, "skipped Actions job unexpectedly acquired a runner")


def _complete_rows(payload: Mapping[str, Any], key: str, label: str) -> list[Mapping[str, Any]]:
    rows = payload.get(key)
    total = payload.get("total_count")
    _require(isinstance(rows, list), f"{label} API response omitted its row list")
    _require(type(total) is int and total == len(rows), f"{label} API response is incomplete")
    return [_object(row, f"{label} API row is malformed") for row in rows]


def _workflow_matches(workflow: Mapping[str, Any], *, workflow_id: int) -> None:
    _require(workflow.get("id") == workflow_id == WORKFLOW_ID, "workflow API ID mismatch")
    _require(workflow.get("path") == WORKFLOW_PATH, "workflow API canonical path mismatch")
    _require(workflow.get("state") == "active", "registered producer workflow is not active")


def _repository_matches(run: Mapping[str, Any]) -> None:
    repository = run.get("repository")
    head_repository = run.get("head_repository")
    _require(isinstance(repository, dict), "Actions run omitted repository identity")
    _require(
        repository.get("id") == REPOSITORY_ID and repository.get("full_name") == REPOSITORY,
        "Actions run repository identity mismatch",
    )
    _require(isinstance(head_repository, dict), "Actions run omitted head repository identity")
    _require(
        head_repository.get("id") == REPOSITORY_ID and head_repository.get("full_name") == REPOSITORY,
        "Actions run head repository identity mismatch",
    )


def _workflow_run_matches(
    run: Mapping[str, Any],
    *,
    run_id: int,
    workflow_host_sha: str,
    branch: str,
    event: str,
    workflow_id: int,
) -> None:
    _require(run.get("id") == run_id, "Actions run ID mismatch")
    _require(run.get("workflow_id") == workflow_id == WORKFLOW_ID, "Actions run workflow ID mismatch")
    _require(run.get("head_sha") == workflow_host_sha and SHA.fullmatch(workflow_host_sha), "Actions run host SHA mismatch")
    _require(run.get("head_branch") == branch and isinstance(branch, str), "Actions run branch mismatch")
    _require(run.get("event") == event == "workflow_dispatch", "Actions run event is not workflow_dispatch")
    _repository_matches(run)


def _job_map(payload: Mapping[str, Any], *, expected_names: set[str] | None = None) -> dict[str, Mapping[str, Any]]:
    rows = _complete_rows(payload, "jobs", "jobs")
    names = [row.get("name") for row in rows]
    _require(all(isinstance(name, str) for name in names), "Actions job name is malformed")
    _require(len(names) == len(set(names)), "Actions API returned duplicate job names")
    result = {str(row["name"]): row for row in rows}
    if expected_names is not None:
        _require(set(result) == expected_names, "producer run job inventory differs from the frozen contract")
    return result


def _verify_producer_job_set(payload: Mapping[str, Any]) -> None:
    jobs = _job_map(payload, expected_names=set(PRODUCER_JOB_CONTRACT))
    for name, (conclusion, runner, ran) in PRODUCER_JOB_CONTRACT.items():
        job = jobs[name]
        _require(job.get("status") == "completed", f"producer job {name} did not complete")
        _require(job.get("conclusion") == conclusion, f"producer job {name} has an unexpected conclusion")
        _validate_hosted_job(job, runner, ran=ran)


def _artifact_map(payload: Mapping[str, Any]) -> list[Mapping[str, Any]]:
    return _complete_rows(payload, "artifacts", "artifact")


def _verify_artifact_association(
    artifact: Mapping[str, Any],
    *,
    run_id: int,
    workflow_host_sha: str,
    branch: str,
) -> None:
    association = artifact.get("workflow_run")
    _require(isinstance(association, dict), "artifact API omitted workflow-run association")
    _require(
        association.get("id") == run_id
        and association.get("head_sha") == workflow_host_sha
        and association.get("head_branch") == branch
        and association.get("repository_id") == REPOSITORY_ID
        and association.get("head_repository_id") == REPOSITORY_ID,
        "artifact association differs from the exact producer run",
    )


def verify_existing_producer(
    run: Mapping[str, Any],
    workflow: Mapping[str, Any],
    jobs_payload: Mapping[str, Any],
    artifacts_payload: Mapping[str, Any],
    *,
    producer_run_id: int,
    producer_workflow_host_sha: str,
    product_sha: str,
    base_ref: str,
    base_sha: str,
) -> dict[str, Mapping[str, Any]]:
    expected = CROSS_RUN_PRODUCER
    _require(producer_run_id == expected["run_id"], "producer run is outside the admitted exact run")
    _require(producer_workflow_host_sha == expected["workflow_host_sha"], "producer host is outside the admitted exact SHA")
    _require(product_sha == expected["product_sha"], "product target is outside the admitted exact SHA")
    _require(base_ref == expected["comparison_base_ref"], "comparison base ref differs from the admitted base")
    _require(base_sha == expected["comparison_base_sha"], "comparison base SHA differs from the admitted base")
    _workflow_matches(workflow, workflow_id=expected["workflow_id"])
    _workflow_run_matches(
        run,
        run_id=expected["run_id"],
        workflow_host_sha=expected["workflow_host_sha"],
        branch=expected["branch"],
        event=expected["event"],
        workflow_id=expected["workflow_id"],
    )
    _require(run.get("run_attempt") == expected["run_attempt"], "producer run attempt differs from the admitted attempt")
    _require(run.get("status") == "completed" and run.get("conclusion") == "failure", "producer run is not the exact completed diagnostic build")
    _verify_producer_job_set(jobs_payload)

    artifacts = _artifact_map(artifacts_payload)
    prefix = f"sedna-first-binary-{product_sha}-"
    package_artifacts = [item for item in artifacts if isinstance(item.get("name"), str) and item["name"].startswith(prefix)]
    _require(len(package_artifacts) == 2, "producer package artifact inventory is not exactly the native pair")
    by_name = {str(item.get("name")): item for item in package_artifacts}
    _require(len(by_name) == 2, "producer package artifacts contain duplicate names")
    selected: dict[str, Mapping[str, Any]] = {}
    for arch, expected_artifact in CROSS_RUN_ARTIFACTS.items():
        artifact = by_name.get(expected_artifact["name"])
        _require(isinstance(artifact, dict), f"producer {arch} artifact name mismatch")
        _require(artifact.get("id") == expected_artifact["id"], f"producer {arch} artifact ID mismatch")
        _require(artifact.get("digest") == expected_artifact["digest"], f"producer {arch} artifact digest mismatch")
        _require(artifact.get("size_in_bytes") == expected_artifact["size_in_bytes"], f"producer {arch} artifact size mismatch")
        _require(artifact.get("expired") is False, f"producer {arch} artifact is expired")
        _require(type(artifact.get("size_in_bytes")) is int and artifact["size_in_bytes"] > 0, f"producer {arch} artifact is empty")
        _verify_artifact_association(
            artifact,
            run_id=expected["run_id"],
            workflow_host_sha=expected["workflow_host_sha"],
            branch=expected["branch"],
        )
        selected[arch] = artifact
    return selected


def verify_build_artifact(
    run: Mapping[str, Any],
    workflow: Mapping[str, Any],
    jobs_payload: Mapping[str, Any],
    artifacts_payload: Mapping[str, Any],
    *,
    run_id: int,
    workflow_host_sha: str,
    branch: str,
    product_sha: str,
    base_sha: str,
    architecture: str,
) -> Mapping[str, Any]:
    arch = ARCHES[architecture]
    _workflow_matches(workflow, workflow_id=WORKFLOW_ID)
    _workflow_run_matches(
        run,
        run_id=run_id,
        workflow_host_sha=workflow_host_sha,
        branch=branch,
        event="workflow_dispatch",
        workflow_id=WORKFLOW_ID,
    )
    _require(run.get("status") == "in_progress", "same-run consumer is not in the active build workflow")
    _require(product_sha and SHA.fullmatch(product_sha), "product SHA is invalid")
    _require(base_sha and SHA.fullmatch(base_sha), "comparison base SHA is invalid")
    jobs = _job_map(jobs_payload)
    for name, expected_runner in (
        ("Verify standard runner graph and exact identities", "ubuntu-24.04"),
        (arch["package_job"], arch["runner"]),
        (arch["consumer_job"], arch["runner"]),
    ):
        job = jobs.get(name)
        _require(isinstance(job, dict), f"required same-run job is missing: {name}")
        if name == arch["consumer_job"]:
            _require(job.get("status") == "in_progress" and job.get("conclusion") is None, "same-run consumer job is not active")
        else:
            _require(job.get("status") == "completed" and job.get("conclusion") == "success", f"required same-run job did not succeed: {name}")
        _validate_hosted_job(job, expected_runner, ran=True)

    artifacts = _artifact_map(artifacts_payload)
    expected_name = f"sedna-first-binary-{product_sha}-{architecture}-{run_id}"
    matches = [item for item in artifacts if item.get("name") == expected_name]
    _require(len(matches) == 1, "same-run API did not return exactly one native package artifact")
    artifact = matches[0]
    _require(type(artifact.get("id")) is int and artifact["id"] > 0, "same-run artifact ID is invalid")
    _require(artifact.get("expired") is False, "same-run package artifact is expired")
    _require(type(artifact.get("size_in_bytes")) is int and artifact["size_in_bytes"] > 0, "same-run package artifact is empty")
    _require(isinstance(artifact.get("digest"), str) and DIGEST.fullmatch(artifact["digest"]), "same-run artifact digest is invalid")
    _verify_artifact_association(artifact, run_id=run_id, workflow_host_sha=workflow_host_sha, branch=branch)
    return artifact


def verify_current_consumer(
    run: Mapping[str, Any],
    workflow: Mapping[str, Any],
    jobs_payload: Mapping[str, Any],
    *,
    env: Mapping[str, str],
    architecture: str,
    product_sha: str,
    base_sha: str,
) -> dict[str, Any]:
    arch = ARCHES[architecture]
    repository = env.get("GITHUB_REPOSITORY", "")
    _require(repository == REPOSITORY, "current Actions repository identity mismatch")
    _require(env.get("GITHUB_ACTIONS") == "true", "hosted GitHub Actions execution is required")
    _require(env.get("GITHUB_EVENT_NAME") == "workflow_dispatch", "consumer route requires workflow_dispatch")
    _require(env.get("RUNNER_OS") == "Linux" and platform.system() == "Linux", "consumer runner is not Linux")
    _require(env.get("RUNNER_ARCH") == arch["run_arch"], "consumer RUNNER_ARCH differs from the native target")
    _require(platform.machine().lower() == architecture, "consumer process architecture differs from the native target")
    _require(env.get("EXPECTED_RUNNER_LABEL") == arch["runner"], "consumer job label is not the exact standard runner")
    _require(env.get("PRODUCT_ARCH") == architecture, "consumer architecture input mismatch")
    _require(env.get("RUST_TARGET") == arch["target"], "consumer target input mismatch")
    _require(product_sha and SHA.fullmatch(product_sha), "consumer product SHA is invalid")
    _require(base_sha and SHA.fullmatch(base_sha), "consumer base SHA is invalid")

    run_id_text = env.get("GITHUB_RUN_ID", "")
    attempt_text = env.get("GITHUB_RUN_ATTEMPT", "")
    _require(RUN_ID.fullmatch(run_id_text) is not None, "current run ID is invalid")
    _require(RUN_ID.fullmatch(attempt_text) is not None, "current run attempt is invalid")
    run_id = int(run_id_text)
    attempt = int(attempt_text)
    host_sha = env.get("GITHUB_SHA", "")
    ref = env.get("GITHUB_REF", "")
    branch = env.get("GITHUB_REF_NAME", "")
    workflow_ref = env.get("GITHUB_WORKFLOW_REF", "")
    _require(SHA.fullmatch(host_sha) is not None, "current workflow host SHA is invalid")
    _require(ref.startswith("refs/heads/") and bool(branch), "consumer run is not on a branch ref")
    _require(run.get("status") == "in_progress", "current consumer Actions run is not in progress")
    _workflow_run_matches(
        run,
        run_id=run_id,
        workflow_host_sha=host_sha,
        branch=branch,
        event="workflow_dispatch",
        workflow_id=WORKFLOW_ID,
    )
    _workflow_matches(workflow, workflow_id=WORKFLOW_ID)
    _require(run.get("run_attempt") == attempt, "current consumer run attempt mismatch")
    _require(ref == f"refs/heads/{branch}", "current consumer ref and branch disagree")
    _require(workflow_ref == f"{REPOSITORY}/{WORKFLOW_PATH}@{ref}", "current consumer workflow_ref mismatch")
    _require(run.get("conclusion") is None, "current consumer run has a terminal conclusion")

    job_name = arch["consumer_job"]
    jobs = _job_map(jobs_payload)
    matches = [job for job in jobs.values() if job.get("name") == job_name]
    _require(len(matches) == 1, "current consumer job is absent or duplicated in Actions API")
    job = matches[0]
    _require(job.get("status") == "in_progress" and job.get("conclusion") is None, "current consumer job is not active")
    _validate_hosted_job(job, arch["runner"], ran=True)

    return {
        "schema_version": "sedna-first-binary-consumer-api-v1",
        "repository": REPOSITORY,
        "workflow_path": WORKFLOW_PATH,
        "event": "workflow_dispatch",
        "workflow_host_sha": host_sha,
        "run_id": run_id,
        "run_attempt": attempt,
        "ref": ref,
        "branch": branch,
        "workflow_ref": workflow_ref,
        "product_sha": product_sha,
        "comparison_base_sha": base_sha,
        "target": arch["target"],
        "architecture": architecture,
        "runner_label": arch["runner"],
    }


def reconcile_consumer_results(
    *,
    junit_path: Path,
    consumer_context_path: Path,
    producer_evidence_path: Path,
    witness_dir: Path,
    result_path: Path,
    pytest_exit: int,
    mode: str,
    profile: str,
    fixture_sha: str,
    sdk_sha: str,
    runner_temp: Path,
) -> dict[str, Any]:
    """Write a separate, exact-consumer result receipt without mutating inputs."""
    issues: list[str] = []
    try:
        consumer = _object(
            json.loads(consumer_context_path.read_text(encoding="utf-8")),
            "consumer API context is malformed",
        )
    except (OSError, json.JSONDecodeError, ValueError):
        consumer = {}
        issues.append("current consumer API context is missing or malformed")
    try:
        producer = _object(
            json.loads(producer_evidence_path.read_text(encoding="utf-8")),
            "producer API evidence is malformed",
        )
    except (OSError, json.JSONDecodeError, ValueError):
        producer = {}
        issues.append("producer API evidence is missing or malformed")

    cases: list[ET.Element] = []
    try:
        cases = list(ET.parse(junit_path).getroot().iter("testcase"))
    except (OSError, ET.ParseError):
        issues.append("pytest did not produce a parseable JUnit report")
    failures = sum(len(case.findall("failure")) for case in cases)
    errors = sum(len(case.findall("error")) for case in cases)
    skipped = sum(len(case.findall("skipped")) for case in cases)
    if pytest_exit != 0:
        issues.append(f"pytest exited with status {pytest_exit}")
    if not cases:
        issues.append("JUnit report contains zero executed test cases")
    if failures or errors:
        issues.append(f"JUnit reports failures={failures} errors={errors}")
    if skipped:
        issues.append(f"JUnit reports {skipped} skipped cases")

    positive_prefix = "test_packaged_historical_upgrade_and_reopen"
    negative_prefix = "test_packaged_historical_rejection_preserves_preimage"
    expected_positive = {
        "fresh", "u23", "u55", "u56", "u57", "u58", "f56", "f57",
        "f58", "f_full", "f_alias_pair", "shift24", "shift29", "shift38",
        "shift45", "shift50",
    }
    expected_negative = {
        "bad_checksum", "mixed_ids", "missing_middle", "failed_row",
        "incomplete_f58", "partial_alias", "partial_upstream_schema", "unknown_id",
    }

    def labels(prefix: str) -> list[str]:
        result: list[str] = []
        for case in cases:
            name = case.attrib.get("name", "")
            if name.startswith(prefix):
                if not name.startswith(prefix + "[") or not name.endswith("]"):
                    issues.append("state-history JUnit name is not an exact parameterized case")
                    continue
                result.append(name[len(prefix) + 1 : -1])
        return result

    positive = labels(positive_prefix)
    negative = labels(negative_prefix)
    if mode == "build":
        profile = "full"
    pair = profile == "pair" and mode == "consume-existing"
    if mode not in {"build", "consume-existing"} or profile not in {"pair", "full"}:
        issues.append("consumer result has an unsupported mode/profile")
    if pair:
        expected_state = {"fresh", "bad_checksum"}
        if positive != ["fresh"] or negative != ["bad_checksum"]:
            issues.append("pair profile did not execute its exact positive and negative state cases")
        expected_plain: set[str] = set()
    else:
        expected_state = expected_positive | expected_negative
        if len(positive) != 16 or set(positive) != expected_positive:
            issues.append("JUnit does not contain the exact 16 positive state-history cases")
        if len(negative) != 8 or set(negative) != expected_negative:
            issues.append("JUnit does not contain the exact 8 negative state-history cases")
        expected_plain = {
            "test_exact_single_artifact_and_native_layout",
            "test_wrong_source_manifest_is_rejected_before_unpack",
            "test_wrong_target_and_missing_helper_are_rejected",
            "test_banner_native_client_and_same_source_proxy_execution",
            "test_persistent_code_mode_and_reopen_from_real_package",
            "test_missing_host_cannot_qualify_code_mode",
            "test_model_receives_and_executes_root_only_agent_list",
            "test_model_spawn_hidden_metadata_has_no_private_fields",
            "test_visible_model_spawn_and_resumed_list_keep_stable_identity",
            "test_actual_tui_agents_entry_has_initial_empty_search",
            "test_actual_tui_nested_filter_clear_live_rename_and_replay",
            "test_real_response_usage_joins_standard_rate_scenario_after_resume",
        }
    plain_names = {
        case.attrib.get("name", "") for case in cases
        if not case.attrib.get("name", "").startswith((positive_prefix, negative_prefix))
    }
    if plain_names != expected_plain:
        issues.append("JUnit plain-test inventory differs from the exact selected profile")
    if len(cases) != len(expected_state) + len(expected_plain):
        issues.append("JUnit executed-case count differs from the exact selected profile")

    try:
        resolved_temp = runner_temp.resolve(strict=True)
        if witness_dir.is_symlink() or witness_dir.resolve(strict=True).parent != resolved_temp:
            raise ValueError("state-history witnesses are not in the exact runner temp directory")
        if witness_dir.stat().st_mode & 0o077:
            raise ValueError("state-history witness directory is not private")
        witness_paths = sorted(witness_dir.glob("*.json"))
    except (OSError, ValueError):
        witness_paths = []
        issues.append("state-history witness directory is missing, unsafe, or outside RUNNER_TEMP")

    witness_hashes: dict[str, str] = {}
    witness_cases: set[str] = set()
    witness_versions: set[str] = set()
    witness_archive_digests: set[str] = set()
    expected_fixture_sha = fixture_sha or str(producer.get("product_sha", ""))
    for path in witness_paths:
        try:
            witness = _object(
                json.loads(path.read_text(encoding="utf-8")), "witness is malformed"
            )
        except (OSError, json.JSONDecodeError, ValueError):
            issues.append("state-history witness JSON is missing or malformed")
            continue
        case_name = witness.get("case")
        if not isinstance(case_name, str) or case_name not in expected_state:
            issues.append("state-history witness has an unexpected case identity")
            continue
        witness_cases.add(case_name)
        expected_fields = {
            "product_target_sha": producer.get("product_sha"),
            "comparison_base_sha": producer.get("comparison_base_sha"),
            "fixture_source_sha": expected_fixture_sha,
            "producer_workflow_host_sha": producer.get("workflow_host_sha"),
            "producer_run_id": producer.get("run_id"),
            "consumer_workflow_host_sha": consumer.get("workflow_host_sha"),
            "consumer_run_id": consumer.get("run_id"),
            "consumer_run_attempt": consumer.get("run_attempt"),
            "target": producer.get("target"),
            "artifact_id": producer.get("artifact_id"),
            "artifact_name": producer.get("artifact_name"),
        }
        if any(witness.get(key) != value for key, value in expected_fields.items()):
            issues.append(f"state-history witness {case_name} producer/consumer identity mismatch")
        archive_digest = witness.get("package_archive_sha256")
        if not isinstance(archive_digest, str) or DIGEST.fullmatch(f"sha256:{archive_digest}") is None:
            issues.append(f"state-history witness {case_name} package digest is malformed")
        else:
            witness_archive_digests.add(archive_digest)
        version = witness.get("package_version")
        if not isinstance(version, str) or not version:
            issues.append(f"state-history witness {case_name} package version is missing")
        else:
            witness_versions.add(version)
        witness_hashes[case_name] = hashlib.sha256(path.read_bytes()).hexdigest()
    if len(witness_paths) != len(expected_state) or witness_cases != expected_state:
        issues.append("state-history witness inventory differs from the exact selected profile")
    if len(witness_versions) > 1 or len(witness_archive_digests) > 1:
        issues.append("state-history witnesses report inconsistent package identity")

    result = {
        "schema_version": "sedna-first-binary-consumer-result-v1",
        "mode": mode,
        "profile": profile,
        "fixture_source_sha": expected_fixture_sha,
        "sdk_source_sha": sdk_sha or str(producer.get("product_sha", "")),
        "consumer": consumer,
        "producer": producer,
        "pytest_exit_code": pytest_exit,
        "consumer_context_sha256": (
            hashlib.sha256(consumer_context_path.read_bytes()).hexdigest()
            if consumer_context_path.is_file() else None
        ),
        "producer_evidence_sha256": (
            hashlib.sha256(producer_evidence_path.read_bytes()).hexdigest()
            if producer_evidence_path.is_file() else None
        ),
        "junit_sha256": (
            hashlib.sha256(junit_path.read_bytes()).hexdigest() if junit_path.is_file() else None
        ),
        "executed_cases": len(cases),
        "failures": failures,
        "errors": errors,
        "skipped": skipped,
        "state_history_positive_cases": len(positive),
        "state_history_negative_cases": len(negative),
        "state_history_witnesses": len(witness_hashes),
        "state_history_witness_sha256": dict(sorted(witness_hashes.items())),
        "issues": issues,
    }
    result_path.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return result


def _fetch_json(api_url: str, token: str, path: str) -> dict[str, Any]:
    request = Request(
        f"{api_url.rstrip('/')}{path}",
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {token}",
            "X-GitHub-Api-Version": "2026-03-10",
        },
    )
    try:
        with urlopen(request, timeout=30) as response:
            final_url = urlsplit(response.geturl())
            expected_url = urlsplit(api_url)
            _require(
                final_url.scheme == "https" and final_url.netloc == expected_url.netloc,
                "GitHub API redirected outside its verified HTTPS origin",
            )
            value = json.loads(response.read())
    except HTTPError as error:
        raise ValueError(f"GitHub Actions API returned HTTP {error.code} for a required identity read") from None
    except URLError as error:
        raise ValueError(f"GitHub Actions API request failed: {error.reason}") from None
    except json.JSONDecodeError:
        raise ValueError("GitHub Actions API returned malformed JSON") from None
    return dict(_object(value, "GitHub Actions API response is not an object"))


def _write_private_json(path_text: str, runner_temp_text: str, value: Mapping[str, Any]) -> None:
    runner_temp = Path(runner_temp_text).resolve(strict=True)
    path = Path(path_text)
    _require(path.is_absolute(), "identity output path must be absolute")
    _require(path.parent.resolve(strict=True) == runner_temp, "identity output must be directly under RUNNER_TEMP")
    _require(not path.exists() and not path.is_symlink(), "identity output already exists")
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    descriptor = os.open(path, flags, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
        json.dump(value, stream, indent=2, sort_keys=True)
        stream.write("\n")


def _required_env(env: Mapping[str, str], name: str) -> str:
    value = env.get(name)
    if value is None:
        raise ValueError(f"required workflow environment value is missing: {name}")
    return value


def main() -> int:
    env = os.environ
    mode = _required_env(env, "MODE")
    architecture = _required_env(env, "PRODUCT_ARCH")
    if architecture not in ARCHES:
        raise ValueError("unsupported native consumer architecture")
    product_sha = _required_env(env, "TARGET_SHA")
    base_ref = _required_env(env, "BASE_REF")
    base_sha = _required_env(env, "BASE_SHA")
    if not SHA.fullmatch(product_sha) or not SHA.fullmatch(base_sha):
        raise ValueError("workflow target/base must be full lowercase SHAs")
    if base_ref != "main":
        raise ValueError("consumer route requires the pinned main comparison ref")

    api_url = _required_env(env, "API_URL")
    api = urlsplit(api_url)
    if api.scheme != "https" or api.netloc != "api.github.com":
        raise ValueError("consumer route requires the official HTTPS GitHub API")
    token = _required_env(env, "GITHUB_TOKEN")
    _require(bool(token), "GitHub Actions API token is empty")
    repository = _required_env(env, "GITHUB_REPOSITORY")
    _require(repository == REPOSITORY, "workflow repository is not the admitted public repository")

    current_run_id = int(_required_env(env, "GITHUB_RUN_ID"))
    current_run = _fetch_json(api_url, token, f"/repos/{REPOSITORY}/actions/runs/{current_run_id}")
    current_workflow_id = current_run.get("workflow_id")
    _require(type(current_workflow_id) is int, "current run omitted a numeric workflow ID")
    current_workflow = _fetch_json(api_url, token, f"/repos/{REPOSITORY}/actions/workflows/{current_workflow_id}")
    current_jobs = _fetch_json(api_url, token, f"/repos/{REPOSITORY}/actions/runs/{current_run_id}/jobs?per_page=100")
    context = verify_current_consumer(
        current_run,
        current_workflow,
        current_jobs,
        env=env,
        architecture=architecture,
        product_sha=product_sha,
        base_sha=base_sha,
    )

    if mode == "consume-existing":
        producer_run_text = _required_env(env, "PRODUCER_RUN_ID")
        producer_host = _required_env(env, "PRODUCER_WORKFLOW_HOST_SHA")
        _require(RUN_ID.fullmatch(producer_run_text) is not None, "producer run ID is invalid")
        _require(SHA.fullmatch(producer_host) is not None, "producer workflow host SHA is invalid")
        producer_run_id = int(producer_run_text)
        _require(context["run_id"] != producer_run_id, "cross-run consumer must use a distinct current run")
        producer_run = _fetch_json(api_url, token, f"/repos/{REPOSITORY}/actions/runs/{producer_run_id}")
        producer_workflow_id = producer_run.get("workflow_id")
        _require(type(producer_workflow_id) is int, "producer run omitted a numeric workflow ID")
        producer_workflow = _fetch_json(api_url, token, f"/repos/{REPOSITORY}/actions/workflows/{producer_workflow_id}")
        producer_jobs = _fetch_json(api_url, token, f"/repos/{REPOSITORY}/actions/runs/{producer_run_id}/jobs?per_page=100")
        producer_artifacts = _fetch_json(api_url, token, f"/repos/{REPOSITORY}/actions/runs/{producer_run_id}/artifacts?per_page=100")
        selected = verify_existing_producer(
            producer_run,
            producer_workflow,
            producer_jobs,
            producer_artifacts,
            producer_run_id=producer_run_id,
            producer_workflow_host_sha=producer_host,
            product_sha=product_sha,
            base_ref=base_ref,
            base_sha=base_sha,
        )
        artifact = selected[architecture]
    elif mode == "build":
        _require(not env.get("PRODUCER_RUN_ID") and not env.get("PRODUCER_WORKFLOW_HOST_SHA"), "build mode must not accept cross-run producer inputs")
        _require(not env.get("FIXTURE_SHA") and not env.get("SDK_SHA"), "build mode must not accept separate fixture/SDK revisions")
        producer_run_id = context["run_id"]
        producer_host = context["workflow_host_sha"]
        jobs_payload = current_jobs
        artifacts_payload = _fetch_json(api_url, token, f"/repos/{REPOSITORY}/actions/runs/{producer_run_id}/artifacts?per_page=100")
        artifact = verify_build_artifact(
            current_run,
            current_workflow,
            jobs_payload,
            artifacts_payload,
            run_id=producer_run_id,
            workflow_host_sha=producer_host,
            branch=context["branch"],
            product_sha=product_sha,
            base_sha=base_sha,
            architecture=architecture,
        )
    else:
        raise ValueError("consumer jobs may run only for build or consume-existing modes")

    expected_name = f"sedna-first-binary-{product_sha}-{architecture}-{producer_run_id}"
    _require(artifact.get("name") == expected_name, "selected artifact name is not canonical")
    producer_record = {
        "schema_version": "sedna-first-binary-producer-api-v1",
        "repository": REPOSITORY,
        "workflow_path": WORKFLOW_PATH,
        "event": "workflow_dispatch",
        "workflow_host_sha": producer_host,
        "run_id": producer_run_id,
        "branch": (CROSS_RUN_PRODUCER["branch"] if mode == "consume-existing" else context["branch"]),
        "product_sha": product_sha,
        "comparison_base_ref": base_ref,
        "comparison_base_sha": base_sha,
        "artifact_id": artifact["id"],
        "artifact_name": artifact["name"],
        "artifact_digest": artifact["digest"],
        "artifact_size_bytes": artifact["size_in_bytes"],
        "architecture": architecture,
        "target": ARCHES[architecture]["target"],
        "runner_label": ARCHES[architecture]["runner"],
    }
    _write_private_json(_required_env(env, "CONSUMER_CONTEXT_OUT"), _required_env(env, "RUNNER_TEMP"), context)
    _write_private_json(_required_env(env, "PRODUCER_EVIDENCE_OUT"), _required_env(env, "RUNNER_TEMP"), producer_record)

    output_path = Path(_required_env(env, "GITHUB_OUTPUT"))
    with output_path.open("a", encoding="utf-8") as output:
        output.write(f"artifact_id={artifact['id']}\n")
        output.write(f"artifact_name={artifact['name']}\n")
        output.write(f"artifact_digest={artifact['digest']}\n")
        output.write(f"producer_run_id={producer_run_id}\n")
        output.write(f"producer_workflow_host_sha={producer_host}\n")
    return 0


if __name__ == "__main__":
    try:
        if sys.argv[1:] == ["--verify-consumer-results"]:
            environment = os.environ
            result = reconcile_consumer_results(
                junit_path=Path(_required_env(environment, "CONSUMER_JUNIT")),
                consumer_context_path=Path(_required_env(environment, "CONSUMER_CONTEXT")),
                producer_evidence_path=Path(_required_env(environment, "PRODUCER_EVIDENCE")),
                witness_dir=Path(_required_env(environment, "STATE_WITNESS_DIR")),
                result_path=Path(_required_env(environment, "CONSUMER_RESULT")),
                pytest_exit=int(_required_env(environment, "PYTEST_EXIT_CODE")),
                mode=_required_env(environment, "MODE"),
                profile=environment.get("CONSUMER_PROFILE", "full"),
                fixture_sha=environment.get("FIXTURE_SHA", ""),
                sdk_sha=environment.get("SDK_SHA", ""),
                runner_temp=Path(_required_env(environment, "RUNNER_TEMP")),
            )
            print(
                "consumer test gate: "
                f"exit={result['pytest_exit_code']} cases={result['executed_cases']} "
                f"failures={result['failures']} errors={result['errors']} "
                f"skipped={result['skipped']} "
                f"state_witnesses={result['state_history_witnesses']}"
            )
            for issue in result["issues"]:
                print(f"consumer test gate failed: {issue}", file=sys.stderr)
            raise SystemExit(1 if result["issues"] else 0)
        raise SystemExit(main())
    except (ValueError, KeyError, TypeError, OSError) as error:
        print(f"first-binary producer verification failed: {error}", file=sys.stderr)
        raise SystemExit(1)
