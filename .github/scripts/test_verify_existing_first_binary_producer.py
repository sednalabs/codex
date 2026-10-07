#!/usr/bin/env python3
"""Focused regression tests for exact cross-run package provenance checks."""

from __future__ import annotations

import copy
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock
import xml.etree.ElementTree as ET

from verify_existing_first_binary_producer import (
    ACCEPTED_INPUTS_PATH,
    ARCHES,
    BROWSER_DIAGNOSTIC_BASE_REF,
    BROWSER_DIAGNOSTIC_BASE_SHA,
    BROWSER_DIAGNOSTIC_FIELDS,
    BROWSER_DIAGNOSTIC_FIXTURE_SHA,
    BROWSER_DIAGNOSTIC_PRODUCT_SHA,
    BROWSER_DIAGNOSTIC_PROFILE,
    BROWSER_DIAGNOSTIC_PROPERTY,
    BROWSER_DIAGNOSTIC_SDK_SHA,
    BROWSER_DIAGNOSTIC_TEST_NAME,
    CROSS_RUN_ARTIFACTS,
    CROSS_RUN_PRODUCER,
    DIAGNOSTIC_RECORD,
    EXPECTED_STATE_NEGATIVE,
    EXPECTED_STATE_POSITIVE,
    CONSUME_EXISTING_TEST_PLANS,
    FOCUSED_REPAIR_PLAIN_TESTS,
    FULL_PLAIN_TESTS,
    Q2_FIXTURE_SHA,
    Q3_ADDITIONAL_PLAIN_TESTS,
    Q3_FIXTURE_SHA,
    Q4_FIXTURE_SHA,
    Q5_FIXTURE_SHA,
    Q11_FIXTURE_SHA,
    Q13_FIXTURE_SHA,
    Q14_FIXTURE_SHA,
    Q15_FIXTURE_SHA,
    Q16_FIXTURE_SHA,
    Q17_FIXTURE_SHA,
    Q18_FIXTURE_SHA,
    Q20_FIXTURE_SHA,
    Q22_FIXTURE_SHA,
    Q24_FIXTURE_SHA,
    Q25_FIXTURE_SHA,
    Q26_FIXTURE_SHA,
    Q57_FIXTURE_SHA,
    Q59_FIXTURE_SHA,
    Q60_FIXTURE_SHA,
    Q61_FIXTURE_SHA,
    Q62_FIXTURE_SHA,
    Q63_FIXTURE_SHA,
    Q64_FIXTURE_SHA,
    Q66_FIXTURE_SHA,
    Q67_FIXTURE_SHA,
    Q68_FIXTURE_SHA,
    Q69_FIXTURE_SHA,
    W14780_ARTIFACT_BASE_SHA,
    W14780_BASE_REF,
    W14780_BASE_SHA,
    W14780_FIXTURE_SHA,
    W14780_PRODUCER_BRANCH,
    W14780_PRODUCER_HOST_SHA,
    W14780_PRODUCER_JOB_NAMES,
    W14780_PRODUCER_RUN_ID,
    W14780_SDK_SHA,
    W14780_T4_PRODUCT_SHA,
    W14780_ADDITIONAL_PLAIN_TESTS,
    S0_SDK_SHA,
    S1_SDK_SHA,
    S2_SDK_SHA,
    S3_SDK_SHA,
    S4_SDK_SHA,
    TRUSTED_CONSUMER_BRANCH,
    H2_PRODUCER_JOB_NAMES,
    H2_PRODUCER_WORKFLOW_HOST_SHA,
    H9E58_PRODUCER_JOB_NAMES,
    H9E58_PRODUCER_RUN_ID,
    H9E58_PRODUCER_WORKFLOW_HOST_SHA,
    SDK_TEST_PLAN_BY_SHA,
    PRODUCER_JOB_CONTRACT,
    REPOSITORY,
    REPOSITORY_ID,
    WORKFLOW_ID,
    WORKFLOW_PATH,
    DIGEST,
    RUN_ID,
    SHA,
    verify_build_artifact,
    verify_current_consumer,
    verify_existing_producer,
    reconcile_consumer_results,
    browser_diagnostic_test_plan,
    consume_existing_test_plan,
    sdk_test_plan,
    select_accepted_record,
    _validate_manifest_record,
    _status_only_junit_projection,
    _json_object_without_duplicate_keys,
    _verify_trusted_consumer_ref,
    validate_runner_policy_inputs,
)


def hosted_job(name: str, conclusion: str, runner: str, *, status: str = "completed", ran: bool = True) -> dict[str, object]:
    return {
        "id": 1000 + len(name),
        "name": name,
        "status": status,
        "conclusion": conclusion,
        "labels": [runner],
        "runner_group_id": 0 if ran else None,
        "runner_group_name": "GitHub Actions" if ran else None,
        "runner_name": "GitHub Actions test-runner" if ran else None,
    }


def accepted_fixture_record() -> dict[str, object]:
    record = copy.deepcopy(DIAGNOSTIC_RECORD)
    record["record_id"] = "test-accepted-producer-control"
    record["disposition"] = "accepted"
    record["w14780_eligible"] = True
    identity = record["identity"]
    identity["product_sha"] = "6" * 40
    producer = record["producer"]
    producer["run_id"] = 36851180755
    producer["workflow_host_sha"] = "5" * 40
    record["artifacts"] = {
        "x86_64": {
            "id": 9001,
            "name": f"sedna-first-binary-{identity['product_sha']}-x86_64-{producer['run_id']}",
            "digest": "sha256:" + "a" * 64,
            "size_in_bytes": 1024,
        },
        "aarch64": {
            "id": 9002,
            "name": f"sedna-first-binary-{identity['product_sha']}-aarch64-{producer['run_id']}",
            "digest": "sha256:" + "b" * 64,
            "size_in_bytes": 2048,
        },
    }
    return record


def accepted_fixture_inputs() -> dict[str, object]:
    identity = accepted_fixture_record()["identity"]
    producer = accepted_fixture_record()["producer"]
    return {
        "product_sha": identity["product_sha"],
        "comparison_base_ref": identity["comparison_base_ref"],
        "comparison_base_sha": identity["comparison_base_sha"],
        "fixture_sha": identity["fixture_sha"],
        "sdk_sha": identity["sdk_sha"],
        "profile": identity["profile"],
        "producer_run_id": producer["run_id"],
        "producer_workflow_host_sha": producer["workflow_host_sha"],
    }


def accepted_producer_args() -> dict[str, object]:
    inputs = accepted_fixture_inputs()
    return {
        "producer_run_id": inputs["producer_run_id"],
        "producer_workflow_host_sha": inputs["producer_workflow_host_sha"],
        "product_sha": inputs["product_sha"],
        "base_ref": inputs["comparison_base_ref"],
        "base_sha": inputs["comparison_base_sha"],
    }


def producer_fixtures(
    accepted_record: dict[str, object] | None = None,
) -> tuple[dict[str, object], dict[str, object], dict[str, object], dict[str, object]]:
    record = copy.deepcopy(accepted_record) if accepted_record is not None else accepted_fixture_record()
    producer = record["producer"]
    run = {
        "id": producer["run_id"],
        "workflow_id": WORKFLOW_ID,
        "run_attempt": producer["run_attempt"],
        "event": producer["event"],
        "head_sha": producer["workflow_host_sha"],
        "head_branch": producer["branch"],
        "status": producer["status"],
        "conclusion": producer["conclusion"],
        "repository": {"id": REPOSITORY_ID, "full_name": REPOSITORY},
        "head_repository": {"id": REPOSITORY_ID, "full_name": REPOSITORY},
    }
    workflow = {"id": WORKFLOW_ID, "path": WORKFLOW_PATH, "state": "active"}
    jobs = {
        "total_count": len(record["jobs"]),
        "jobs": [],
    }
    for expected in record["jobs"]:
        jobs["jobs"].append(
            hosted_job(
                expected["name"], expected["conclusion"], expected["runner"],
                status=expected["status"], ran=expected["ran"],
            )
        )
    artifacts = {"total_count": len(record["artifacts"]), "artifacts": []}
    for expected in record["artifacts"].values():
        artifacts["artifacts"].append(
            {
                "id": expected["id"],
                "name": expected["name"],
                "digest": expected["digest"],
                "size_in_bytes": expected["size_in_bytes"],
                "expired": False,
                "workflow_run": {
                    "id": producer["run_id"],
                    "head_sha": producer["workflow_host_sha"],
                    "head_branch": producer["branch"],
                    "repository_id": REPOSITORY_ID,
                    "head_repository_id": REPOSITORY_ID,
                },
            }
        )
    return run, workflow, jobs, artifacts


def verify_fixture(
    run: dict[str, object],
    workflow: dict[str, object],
    jobs: dict[str, object],
    artifacts: dict[str, object],
    *,
    accepted_record: dict[str, object] | None = None,
) -> dict[str, object]:
    record = accepted_record if accepted_record is not None else accepted_fixture_record()
    identity = record["identity"]
    producer = record["producer"]
    return verify_existing_producer(
        run,
        workflow,
        jobs,
        artifacts,
        producer_run_id=producer["run_id"],
        producer_workflow_host_sha=producer["workflow_host_sha"],
        product_sha=identity["product_sha"],
        base_ref=identity["comparison_base_ref"],
        base_sha=identity["comparison_base_sha"],
        accepted_record=record,
    )


def current_consumer_fixtures() -> tuple[dict[str, object], dict[str, object], dict[str, object], dict[str, str]]:
    branch = "reconstruct/first-binary-consumer-20261001"
    ref = f"refs/heads/{branch}"
    host = "0123456789abcdef0123456789abcdef01234567"
    run_id = 39000000000
    run = {
        "id": run_id,
        "workflow_id": WORKFLOW_ID,
        "run_attempt": 1,
        "event": "workflow_dispatch",
        "head_sha": host,
        "head_branch": branch,
        "status": "in_progress",
        "conclusion": None,
        "repository": {"id": REPOSITORY_ID, "full_name": REPOSITORY},
        "head_repository": {"id": REPOSITORY_ID, "full_name": REPOSITORY},
    }
    workflow = {"id": WORKFLOW_ID, "path": WORKFLOW_PATH, "state": "active"}
    jobs = {
        "total_count": 1,
        "jobs": [
            hosted_job(
                ARCHES["x86_64"]["consumer_job"],
                None,
                "ubuntu-24.04",
                status="in_progress",
            )
        ],
    }
    env = {
        "GITHUB_REPOSITORY": REPOSITORY,
        "GITHUB_ACTIONS": "true",
        "GITHUB_EVENT_NAME": "workflow_dispatch",
        "RUNNER_OS": "Linux",
        "RUNNER_ARCH": "X64",
        "EXPECTED_RUNNER_LABEL": "ubuntu-24.04",
        "PRODUCT_ARCH": "x86_64",
        "RUST_TARGET": "x86_64-unknown-linux-gnu",
        "TARGET_SHA": CROSS_RUN_PRODUCER["product_sha"],
        "BASE_SHA": CROSS_RUN_PRODUCER["comparison_base_sha"],
        "GITHUB_RUN_ID": str(run_id),
        "GITHUB_RUN_ATTEMPT": "1",
        "GITHUB_SHA": host,
        "GITHUB_REF": ref,
        "GITHUB_REF_NAME": branch,
        "GITHUB_WORKFLOW_REF": f"{REPOSITORY}/{WORKFLOW_PATH}@{ref}",
    }
    return run, workflow, jobs, env


def _workflow_step_env(workflow: str, job_name: str, step_name: str) -> dict[str, str]:
    lines = workflow.splitlines()
    job_marker = f"  {job_name}:"
    try:
        job_start = lines.index(job_marker)
    except ValueError:
        raise ValueError(f"workflow is missing job {job_name}") from None
    job_end = next(
        (
            index
            for index in range(job_start + 1, len(lines))
            if lines[index].startswith("  ")
            and not lines[index].startswith("    ")
            and lines[index].strip().endswith(":")
        ),
        len(lines),
    )
    job_lines = lines[job_start:job_end]
    step_marker = f"      - name: {step_name}"
    try:
        step_start = job_lines.index(step_marker)
    except ValueError:
        raise ValueError(f"job {job_name} is missing step {step_name}") from None
    step_end = next(
        (index for index in range(step_start + 1, len(job_lines)) if job_lines[index].startswith("      - ")),
        len(job_lines),
    )
    step_lines = job_lines[step_start:step_end]
    try:
        env_start = step_lines.index("        env:")
    except ValueError:
        raise ValueError(f"step {step_name} in {job_name} has no env mapping") from None
    env: dict[str, str] = {}
    for line in step_lines[env_start + 1 :]:
        if not line.strip():
            continue
        if not line.startswith("          "):
            break
        key, separator, value = line.strip().partition(":")
        if separator:
            env[key] = value.strip()
    return env


def _assert_producer_verification_env(env: dict[str, str], product_arch: str) -> None:
    expected = {
        "MODE": "${{ inputs.mode }}",
        "PRODUCT_ARCH": product_arch,
        "TARGET_SHA": "${{ inputs.target_sha }}",
        "BASE_REF": "${{ inputs.base_ref }}",
        "BASE_SHA": "${{ inputs.base_sha }}",
        "API_URL": "${{ github.api_url }}",
        "GITHUB_TOKEN": "${{ github.token }}",
        "PRODUCER_RUN_ID": "${{ inputs.producer_run_id }}",
        "PRODUCER_WORKFLOW_HOST_SHA": "${{ inputs.producer_workflow_host_sha }}",
        "FIXTURE_SHA": "${{ inputs.fixture_sha }}",
        "SDK_SHA": "${{ inputs.sdk_sha }}",
        "CONSUMER_PROFILE": "${{ inputs.consumer_profile }}",
        "CONSUMER_CONTEXT_OUT": "${{ runner.temp }}/first-binary-consumer-context.json",
        "PRODUCER_EVIDENCE_OUT": "${{ runner.temp }}/first-binary-producer-evidence.json",
    }
    for key, value in expected.items():
        if env.get(key) != value:
            raise ValueError(f"producer verification env {key} is missing or misbound")


class VerifyExistingProducerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.run, self.workflow, self.jobs, self.artifacts = producer_fixtures()

    def test_exact_native_pair_is_accepted(self) -> None:
        selected = verify_fixture(self.run, self.workflow, self.jobs, self.artifacts)
        self.assertEqual(set(selected), {"x86_64", "aarch64"})
        self.assertEqual(selected["x86_64"]["id"], 9001)
        self.assertEqual(selected["aarch64"]["id"], 9002)

    def test_wrong_run_host_or_product_identity_is_rejected(self) -> None:
        for kwargs in (
            {"producer_run_id": accepted_fixture_inputs()["producer_run_id"] + 1},
            {"producer_workflow_host_sha": "1111111111111111111111111111111111111111"},
            {"product_sha": "2222222222222222222222222222222222222222"},
            {"base_sha": "3333333333333333333333333333333333333333"},
        ):
            with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                expected = accepted_producer_args()
                expected.update(kwargs)
                verify_existing_producer(
                    self.run, self.workflow, self.jobs, self.artifacts,
                    accepted_record=accepted_fixture_record(), **expected
                )

    def test_workflow_host_identity_and_run_event_are_exact(self) -> None:
        for field, value in (
            ("id", WORKFLOW_ID + 1),
            ("path", ".github/workflows/other.yml"),
            ("state", "disabled_manually"),
        ):
            changed = copy.deepcopy(self.workflow)
            changed[field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                verify_fixture(self.run, changed, self.jobs, self.artifacts)
        for field, value in (
            ("event", "push"),
            ("head_sha", "f" * 40),
            ("head_branch", "other-branch"),
            ("run_attempt", 2),
            ("status", "in_progress"),
            ("conclusion", "success"),
        ):
            changed = copy.deepcopy(self.run)
            changed[field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                verify_fixture(changed, self.workflow, self.jobs, self.artifacts)

    def test_repository_and_job_inventory_are_closed(self) -> None:
        changed = copy.deepcopy(self.run)
        changed["repository"] = {"id": 1, "full_name": "fork/codex"}
        with self.assertRaises(ValueError):
            verify_fixture(changed, self.workflow, self.jobs, self.artifacts)

        changed_jobs = copy.deepcopy(self.jobs)
        changed_jobs["jobs"].append(hosted_job("Unexpected job", "success", "ubuntu-24.04"))
        changed_jobs["total_count"] += 1
        with self.assertRaises(ValueError):
            verify_fixture(self.run, self.workflow, changed_jobs, self.artifacts)

    def test_unexpected_conclusions_and_runner_groups_fail_closed(self) -> None:
        cases = (
            ("Package native Linux x86_64", "conclusion", "cancelled"),
            ("Package native Linux ARM64", "labels", ["ubuntu-24.04"]),
            ("Package native Linux x86_64", "runner_group_name", "Paid custom group"),
            ("Package native Linux ARM64", "runner_group_id", 88),
            ("Verify standard runner graph and exact identities", "runner_name", "custom-runner"),
        )
        for job_name, field, value in cases:
            changed = copy.deepcopy(self.jobs)
            job = next(item for item in changed["jobs"] if item["name"] == job_name)
            job[field] = value
            with self.subTest(job=job_name, field=field), self.assertRaises(ValueError):
                verify_fixture(self.run, self.workflow, changed, self.artifacts)

    def test_prepare_job_must_be_skipped_without_runner_assignment(self) -> None:
        changed = copy.deepcopy(self.jobs)
        job = next(item for item in changed["jobs"] if item["name"].startswith("Prepare exact"))
        job["conclusion"] = "success"
        with self.assertRaises(ValueError):
            verify_fixture(self.run, self.workflow, changed, self.artifacts)
        changed = copy.deepcopy(self.jobs)
        job = next(item for item in changed["jobs"] if item["name"].startswith("Prepare exact"))
        job["runner_name"] = "GitHub Actions runner"
        with self.assertRaises(ValueError):
            verify_fixture(self.run, self.workflow, changed, self.artifacts)

    def test_package_artifacts_require_exact_pair_and_association(self) -> None:
        cases = []
        changed = copy.deepcopy(self.artifacts)
        changed["artifacts"].pop()
        changed["total_count"] -= 1
        cases.append(changed)

        changed = copy.deepcopy(self.artifacts)
        changed["artifacts"][0]["expired"] = True
        cases.append(changed)

        changed = copy.deepcopy(self.artifacts)
        changed["artifacts"][0]["digest"] = "sha256:" + "0" * 64
        cases.append(changed)

        changed = copy.deepcopy(self.artifacts)
        changed["artifacts"][0]["workflow_run"]["head_sha"] = "f" * 40
        cases.append(changed)

        changed = copy.deepcopy(self.artifacts)
        duplicate = copy.deepcopy(changed["artifacts"][0])
        duplicate["id"] += 1
        changed["artifacts"].append(duplicate)
        changed["total_count"] += 1
        cases.append(changed)

        changed = copy.deepcopy(self.artifacts)
        extra = copy.deepcopy(changed["artifacts"][0])
        test_inputs = accepted_fixture_inputs()
        extra["name"] = f"sedna-first-binary-{test_inputs['product_sha']}-unknown-{test_inputs['producer_run_id']}"
        extra["id"] += 2
        changed["artifacts"].append(extra)
        changed["total_count"] += 1
        cases.append(changed)

        for index, artifact_payload in enumerate(cases):
            with self.subTest(case=index), self.assertRaises(ValueError):
                verify_fixture(self.run, self.workflow, self.jobs, artifact_payload)

    def test_api_row_count_must_be_complete(self) -> None:
        changed = copy.deepcopy(self.artifacts)
        changed["total_count"] += 1
        with self.assertRaises(ValueError):
            verify_fixture(self.run, self.workflow, self.jobs, changed)

    def test_identity_grammars_require_exact_full_values(self) -> None:
        self.assertIsNotNone(SHA.fullmatch("a" * 40))
        self.assertIsNone(SHA.fullmatch("a" * 40 + "x"))
        self.assertIsNotNone(DIGEST.fullmatch("sha256:" + "b" * 64))
        self.assertIsNone(DIGEST.fullmatch("sha256:" + "b" * 64 + "x"))
        self.assertIsNotNone(RUN_ID.fullmatch("36800811941"))
        self.assertIsNone(RUN_ID.fullmatch("036800811941"))


class AcceptedInputManifestTests(unittest.TestCase):
    def test_frozen_manifest_keeps_prior_rows_and_adds_exact_q4_s2_pair_and_full_rows(self) -> None:
        manifest = json.loads(ACCEPTED_INPUTS_PATH.read_text(encoding="utf-8"))
        rows = manifest["records"]
        old_diagnostic = next(row for row in rows if row["record_id"] == "diagnostic-producer-36800811941")
        self.assertEqual("diagnostic", old_diagnostic["disposition"])
        self.assertFalse(old_diagnostic["w14780_eligible"])

        q2_rows = {
            row["identity"]["profile"]: row
            for row in rows
            if row["producer"]["run_id"] == 36851180755
            and row["disposition"] == "accepted"
            and row["identity"]["fixture_sha"] == Q2_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S0_SDK_SHA
        }
        self.assertEqual({"pair", "full"}, set(q2_rows))
        for profile, record in q2_rows.items():
            with self.subTest(profile=profile):
                self.assertTrue(record["w14780_eligible"])
                self.assertEqual("accepted", record["disposition"])
                identity = record["identity"]
                producer = record["producer"]
                selected = select_accepted_record(
                    manifest,
                    {
                        "product_sha": identity["product_sha"],
                        "comparison_base_ref": identity["comparison_base_ref"],
                        "comparison_base_sha": identity["comparison_base_sha"],
                        "fixture_sha": identity["fixture_sha"],
                        "sdk_sha": identity["sdk_sha"],
                        "profile": profile,
                        "producer_run_id": producer["run_id"],
                        "producer_workflow_host_sha": producer["workflow_host_sha"],
                    },
                )
                self.assertEqual(record["record_id"], selected["record_id"])

        q3_rows = [
            row for row in rows
            if row["producer"]["run_id"] == 36851180755
            and row["identity"]["fixture_sha"] == Q3_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S1_SDK_SHA
        ]
        self.assertEqual(1, len(q3_rows))
        q3_full = q3_rows[0]
        self.assertEqual("accepted-producer-36851180755-full-q3-s1", q3_full["record_id"])
        self.assertEqual("accepted", q3_full["disposition"])
        self.assertTrue(q3_full["w14780_eligible"])
        self.assertEqual("full", q3_full["identity"]["profile"])
        for field in ("product_sha", "comparison_base_ref", "comparison_base_sha"):
            self.assertEqual(q2_rows["full"]["identity"][field], q3_full["identity"][field])
        self.assertEqual(q2_rows["full"]["producer"], q3_full["producer"])
        self.assertEqual(q2_rows["full"]["jobs"], q3_full["jobs"])
        self.assertEqual(q2_rows["full"]["artifacts"], q3_full["artifacts"])
        q3_inputs = {
            "product_sha": q3_full["identity"]["product_sha"],
            "comparison_base_ref": q3_full["identity"]["comparison_base_ref"],
            "comparison_base_sha": q3_full["identity"]["comparison_base_sha"],
            "fixture_sha": Q3_FIXTURE_SHA,
            "sdk_sha": S1_SDK_SHA,
            "profile": "full",
            "producer_run_id": q3_full["producer"]["run_id"],
            "producer_workflow_host_sha": q3_full["producer"]["workflow_host_sha"],
        }
        self.assertEqual(q3_full["record_id"], select_accepted_record(manifest, q3_inputs)["record_id"])
        for field, value in (("fixture_sha", Q2_FIXTURE_SHA), ("sdk_sha", S0_SDK_SHA), ("profile", "pair")):
            changed = dict(q3_inputs)
            changed[field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                select_accepted_record(manifest, changed)

        q4_rows = {
            row["identity"]["profile"]: row
            for row in rows
            if row["producer"]["run_id"] == 36851180755
            and row["identity"]["fixture_sha"] == Q4_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S2_SDK_SHA
        }
        self.assertEqual({"pair", "full"}, set(q4_rows))
        for profile, record in q4_rows.items():
            with self.subTest(q4_profile=profile):
                self.assertEqual(f"accepted-producer-36851180755-{profile}-q4-s2", record["record_id"])
                self.assertEqual("accepted", record["disposition"])
                self.assertTrue(record["w14780_eligible"])
                self.assertEqual("c3a5d1135480efc61b6c24b275ea3322e0cfa14d", record["identity"]["product_sha"])
                self.assertEqual("4a1ecb1e26fa0c6e8933bb73188bc6735da18ee7", record["identity"]["comparison_base_sha"])
                self.assertEqual(q2_rows["full"]["producer"], record["producer"])
                self.assertEqual(q2_rows["full"]["jobs"], record["jobs"])
                self.assertEqual(q2_rows["full"]["artifacts"], record["artifacts"])
                q4_inputs = {
                    "product_sha": record["identity"]["product_sha"],
                    "comparison_base_ref": record["identity"]["comparison_base_ref"],
                    "comparison_base_sha": record["identity"]["comparison_base_sha"],
                    "fixture_sha": Q4_FIXTURE_SHA,
                    "sdk_sha": S2_SDK_SHA,
                    "profile": profile,
                    "producer_run_id": record["producer"]["run_id"],
                    "producer_workflow_host_sha": record["producer"]["workflow_host_sha"],
                }
                self.assertEqual(record["record_id"], select_accepted_record(manifest, q4_inputs)["record_id"])

        q5_rows = {
            row["identity"]["profile"]: row
            for row in rows
            if row["producer"]["run_id"] == 36851180755
            and row["identity"]["fixture_sha"] == Q5_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S2_SDK_SHA
        }
        self.assertEqual({"focused", "full"}, set(q5_rows))
        for profile, record in q5_rows.items():
            with self.subTest(q5_profile=profile):
                self.assertEqual(f"accepted-producer-36851180755-{profile}-q5-s2", record["record_id"])
                self.assertEqual("accepted", record["disposition"])
                self.assertTrue(record["w14780_eligible"])
                self.assertEqual(Q5_FIXTURE_SHA, record["identity"]["fixture_sha"])
                self.assertEqual(S2_SDK_SHA, record["identity"]["sdk_sha"])
                self.assertEqual(q2_rows["full"]["producer"], record["producer"])
                self.assertEqual(q2_rows["full"]["jobs"], record["jobs"])
                self.assertEqual(q2_rows["full"]["artifacts"], record["artifacts"])
                q5_inputs = {
                    "product_sha": record["identity"]["product_sha"],
                    "comparison_base_ref": record["identity"]["comparison_base_ref"],
                    "comparison_base_sha": record["identity"]["comparison_base_sha"],
                    "fixture_sha": Q5_FIXTURE_SHA,
                    "sdk_sha": S2_SDK_SHA,
                    "profile": profile,
                    "producer_run_id": record["producer"]["run_id"],
                    "producer_workflow_host_sha": record["producer"]["workflow_host_sha"],
                }
                self.assertEqual(record["record_id"], select_accepted_record(manifest, q5_inputs)["record_id"])

    def test_q11_s3_manifest_adds_only_focused_row_with_exact_producer(self) -> None:
        manifest = json.loads(ACCEPTED_INPUTS_PATH.read_text(encoding="utf-8"))
        rows = manifest["records"]
        q11_rows = [
            row for row in rows
            if row["identity"]["fixture_sha"] == Q11_FIXTURE_SHA
            or row["identity"]["sdk_sha"] == S3_SDK_SHA
        ]
        self.assertEqual(1, len(q11_rows))
        record = q11_rows[0]
        self.assertEqual("accepted-producer-36851180755-focused-q11-s3", record["record_id"])
        self.assertEqual("accepted", record["disposition"])
        self.assertTrue(record["w14780_eligible"])
        self.assertEqual(
            {
                "product_sha": "c3a5d1135480efc61b6c24b275ea3322e0cfa14d",
                "comparison_base_ref": "main",
                "comparison_base_sha": "4a1ecb1e26fa0c6e8933bb73188bc6735da18ee7",
                "fixture_sha": Q11_FIXTURE_SHA,
                "sdk_sha": S3_SDK_SHA,
                "profile": "focused",
            },
            record["identity"],
        )
        q2_full = next(
            row for row in rows
            if row["identity"]["fixture_sha"] == Q2_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S0_SDK_SHA
            and row["identity"]["profile"] == "full"
        )
        self.assertEqual(q2_full["producer"], record["producer"])
        self.assertEqual(q2_full["jobs"], record["jobs"])
        self.assertEqual(q2_full["artifacts"], record["artifacts"])
        inputs = {
            "product_sha": record["identity"]["product_sha"],
            "comparison_base_ref": record["identity"]["comparison_base_ref"],
            "comparison_base_sha": record["identity"]["comparison_base_sha"],
            "fixture_sha": Q11_FIXTURE_SHA,
            "sdk_sha": S3_SDK_SHA,
            "profile": "focused",
            "producer_run_id": record["producer"]["run_id"],
            "producer_workflow_host_sha": record["producer"]["workflow_host_sha"],
        }
        self.assertEqual(record["record_id"], select_accepted_record(manifest, inputs)["record_id"])
        mismatches = (
            ("product_sha", "9" * 40),
            ("comparison_base_sha", "8" * 40),
            ("fixture_sha", Q5_FIXTURE_SHA),
            ("sdk_sha", S2_SDK_SHA),
            ("profile", "pair"),
            ("profile", "full"),
            ("producer_run_id", 36800811941),
            ("producer_workflow_host_sha", "7" * 40),
        )
        for field, value in mismatches:
            changed = dict(inputs)
            changed[field] = value
            with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                select_accepted_record(manifest, changed)

    def test_q13_s4_manifest_adds_only_focused_row_with_exact_producer(self) -> None:
        manifest = json.loads(ACCEPTED_INPUTS_PATH.read_text(encoding="utf-8"))
        rows = manifest["records"]
        q13_rows = [
            row for row in rows
            if row["identity"]["fixture_sha"] == Q13_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S4_SDK_SHA
            and row["identity"]["profile"] == "focused"
        ]
        self.assertEqual(1, len(q13_rows))
        record = q13_rows[0]
        self.assertEqual("accepted-producer-36851180755-focused-q13-s4", record["record_id"])
        self.assertEqual("accepted", record["disposition"])
        self.assertTrue(record["w14780_eligible"])
        self.assertEqual(
            {
                "product_sha": "c3a5d1135480efc61b6c24b275ea3322e0cfa14d",
                "comparison_base_ref": "main",
                "comparison_base_sha": "4a1ecb1e26fa0c6e8933bb73188bc6735da18ee7",
                "fixture_sha": Q13_FIXTURE_SHA,
                "sdk_sha": S4_SDK_SHA,
                "profile": "focused",
            },
            record["identity"],
        )
        prior_q11 = next(
            row for row in rows
            if row["identity"]["fixture_sha"] == Q11_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S3_SDK_SHA
            and row["identity"]["profile"] == "focused"
        )
        self.assertEqual(prior_q11["producer"], record["producer"])
        self.assertEqual(prior_q11["jobs"], record["jobs"])
        self.assertEqual(prior_q11["artifacts"], record["artifacts"])
        inputs = {
            "product_sha": record["identity"]["product_sha"],
            "comparison_base_ref": record["identity"]["comparison_base_ref"],
            "comparison_base_sha": record["identity"]["comparison_base_sha"],
            "fixture_sha": Q13_FIXTURE_SHA,
            "sdk_sha": S4_SDK_SHA,
            "profile": "focused",
            "producer_run_id": record["producer"]["run_id"],
            "producer_workflow_host_sha": record["producer"]["workflow_host_sha"],
        }
        self.assertEqual(record["record_id"], select_accepted_record(manifest, inputs)["record_id"])
        mismatches = (
            ("product_sha", "9" * 40),
            ("comparison_base_sha", "8" * 40),
            ("fixture_sha", Q11_FIXTURE_SHA),
            ("sdk_sha", S3_SDK_SHA),
            ("profile", "pair"),
            ("profile", "full"),
            ("producer_run_id", 36800811941),
            ("producer_workflow_host_sha", "7" * 40),
        )
        for field, value in mismatches:
            changed = dict(inputs)
            changed[field] = value
            with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                select_accepted_record(manifest, changed)

    def test_q14_s4_manifest_adds_focused_row_without_changing_producer(self) -> None:
        manifest = json.loads(ACCEPTED_INPUTS_PATH.read_text(encoding="utf-8"))
        rows = manifest["records"]
        q14_rows = [
            row for row in rows if row["identity"]["fixture_sha"] == Q14_FIXTURE_SHA
        ]
        self.assertEqual(1, len(q14_rows))
        record = q14_rows[0]
        q13 = next(
            row for row in rows
            if row["identity"]["fixture_sha"] == Q13_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S4_SDK_SHA
            and row["identity"]["profile"] == "focused"
        )
        self.assertEqual("accepted-producer-36851180755-focused-q14-s4", record["record_id"])
        self.assertEqual("accepted", record["disposition"])
        self.assertTrue(record["w14780_eligible"])
        self.assertEqual(
            {
                "product_sha": "c3a5d1135480efc61b6c24b275ea3322e0cfa14d",
                "comparison_base_ref": "main",
                "comparison_base_sha": "4a1ecb1e26fa0c6e8933bb73188bc6735da18ee7",
                "fixture_sha": Q14_FIXTURE_SHA,
                "sdk_sha": S4_SDK_SHA,
                "profile": "focused",
            },
            record["identity"],
        )
        self.assertEqual(q13["producer"], record["producer"])
        self.assertEqual(q13["jobs"], record["jobs"])
        self.assertEqual(q13["artifacts"], record["artifacts"])
        inputs = {
            "product_sha": record["identity"]["product_sha"],
            "comparison_base_ref": record["identity"]["comparison_base_ref"],
            "comparison_base_sha": record["identity"]["comparison_base_sha"],
            "fixture_sha": Q14_FIXTURE_SHA,
            "sdk_sha": S4_SDK_SHA,
            "profile": "focused",
            "producer_run_id": record["producer"]["run_id"],
            "producer_workflow_host_sha": record["producer"]["workflow_host_sha"],
        }
        self.assertEqual(record["record_id"], select_accepted_record(manifest, inputs)["record_id"])
        for field, value in (
            ("fixture_sha", Q11_FIXTURE_SHA),
            ("sdk_sha", S3_SDK_SHA),
            ("profile", "pair"),
            ("profile", "full"),
        ):
            changed = dict(inputs)
            changed[field] = value
            with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                select_accepted_record(manifest, changed)

    def test_q15_s4_manifest_adds_focused_row_without_changing_producer(self) -> None:
        manifest = json.loads(ACCEPTED_INPUTS_PATH.read_text(encoding="utf-8"))
        rows = manifest["records"]
        q15_rows = [
            row for row in rows if row["identity"]["fixture_sha"] == Q15_FIXTURE_SHA
        ]
        self.assertEqual(1, len(q15_rows))
        record = q15_rows[0]
        q14 = next(
            row for row in rows
            if row["identity"]["fixture_sha"] == Q14_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S4_SDK_SHA
            and row["identity"]["profile"] == "focused"
        )
        self.assertEqual("accepted-producer-36851180755-focused-q15-s4", record["record_id"])
        self.assertEqual("accepted", record["disposition"])
        self.assertTrue(record["w14780_eligible"])
        self.assertEqual(
            {
                "product_sha": "c3a5d1135480efc61b6c24b275ea3322e0cfa14d",
                "comparison_base_ref": "main",
                "comparison_base_sha": "4a1ecb1e26fa0c6e8933bb73188bc6735da18ee7",
                "fixture_sha": Q15_FIXTURE_SHA,
                "sdk_sha": S4_SDK_SHA,
                "profile": "focused",
            },
            record["identity"],
        )
        self.assertEqual(q14["producer"], record["producer"])
        self.assertEqual(q14["jobs"], record["jobs"])
        self.assertEqual(q14["artifacts"], record["artifacts"])
        inputs = {
            "product_sha": record["identity"]["product_sha"],
            "comparison_base_ref": record["identity"]["comparison_base_ref"],
            "comparison_base_sha": record["identity"]["comparison_base_sha"],
            "fixture_sha": Q15_FIXTURE_SHA,
            "sdk_sha": S4_SDK_SHA,
            "profile": "focused",
            "producer_run_id": record["producer"]["run_id"],
            "producer_workflow_host_sha": record["producer"]["workflow_host_sha"],
        }
        self.assertEqual(record["record_id"], select_accepted_record(manifest, inputs)["record_id"])
        for field, value in (
            ("product_sha", "9" * 40),
            ("comparison_base_sha", "8" * 40),
            ("fixture_sha", Q11_FIXTURE_SHA),
            ("sdk_sha", S3_SDK_SHA),
            ("profile", "pair"),
            ("profile", "full"),
            ("producer_run_id", 36800811941),
            ("producer_workflow_host_sha", "7" * 40),
        ):
            changed = dict(inputs)
            changed[field] = value
            with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                select_accepted_record(manifest, changed)

    def test_q17_s4_manifest_adds_focused_row_without_changing_producer(self) -> None:
        manifest = json.loads(ACCEPTED_INPUTS_PATH.read_text(encoding="utf-8"))
        rows = manifest["records"]
        q17_rows = [
            row for row in rows if row["identity"]["fixture_sha"] == Q17_FIXTURE_SHA
        ]
        self.assertEqual(1, len(q17_rows))
        record = q17_rows[0]
        q15 = next(
            row for row in rows
            if row["identity"]["fixture_sha"] == Q15_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S4_SDK_SHA
            and row["identity"]["profile"] == "focused"
        )
        self.assertEqual("accepted-producer-36851180755-focused-q17-s4", record["record_id"])
        self.assertEqual("accepted", record["disposition"])
        self.assertTrue(record["w14780_eligible"])
        self.assertEqual(
            {
                "product_sha": "c3a5d1135480efc61b6c24b275ea3322e0cfa14d",
                "comparison_base_ref": "main",
                "comparison_base_sha": "4a1ecb1e26fa0c6e8933bb73188bc6735da18ee7",
                "fixture_sha": Q17_FIXTURE_SHA,
                "sdk_sha": S4_SDK_SHA,
                "profile": "focused",
            },
            record["identity"],
        )
        self.assertEqual(q15["producer"], record["producer"])
        self.assertEqual(q15["jobs"], record["jobs"])
        self.assertEqual(q15["artifacts"], record["artifacts"])
        inputs = {
            "product_sha": record["identity"]["product_sha"],
            "comparison_base_ref": record["identity"]["comparison_base_ref"],
            "comparison_base_sha": record["identity"]["comparison_base_sha"],
            "fixture_sha": Q17_FIXTURE_SHA,
            "sdk_sha": S4_SDK_SHA,
            "profile": "focused",
            "producer_run_id": record["producer"]["run_id"],
            "producer_workflow_host_sha": record["producer"]["workflow_host_sha"],
        }
        self.assertEqual(record["record_id"], select_accepted_record(manifest, inputs)["record_id"])
        for field, value in (
            ("product_sha", "9" * 40),
            ("comparison_base_sha", "8" * 40),
            ("fixture_sha", Q16_FIXTURE_SHA),
            ("sdk_sha", S3_SDK_SHA),
            ("profile", "pair"),
            ("profile", "full"),
            ("producer_run_id", 36800811941),
            ("producer_workflow_host_sha", "7" * 40),
        ):
            changed = dict(inputs)
            changed[field] = value
            with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                select_accepted_record(manifest, changed)

    def test_q18_s4_manifest_adds_focused_row_without_changing_producer(self) -> None:
        manifest = json.loads(ACCEPTED_INPUTS_PATH.read_text(encoding="utf-8"))
        rows = manifest["records"]
        q18_rows = [
            row for row in rows if row["identity"]["fixture_sha"] == Q18_FIXTURE_SHA
        ]
        self.assertEqual(1, len(q18_rows))
        record = q18_rows[0]
        q17 = next(
            row for row in rows
            if row["identity"]["fixture_sha"] == Q17_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S4_SDK_SHA
            and row["identity"]["profile"] == "focused"
        )
        self.assertEqual("accepted-producer-36851180755-focused-q18-s4", record["record_id"])
        self.assertEqual("accepted", record["disposition"])
        self.assertTrue(record["w14780_eligible"])
        self.assertEqual(
            {
                "product_sha": "c3a5d1135480efc61b6c24b275ea3322e0cfa14d",
                "comparison_base_ref": "main",
                "comparison_base_sha": "4a1ecb1e26fa0c6e8933bb73188bc6735da18ee7",
                "fixture_sha": Q18_FIXTURE_SHA,
                "sdk_sha": S4_SDK_SHA,
                "profile": "focused",
            },
            record["identity"],
        )
        self.assertEqual(q17["producer"], record["producer"])
        self.assertEqual(q17["jobs"], record["jobs"])
        self.assertEqual(q17["artifacts"], record["artifacts"])
        inputs = {
            "product_sha": record["identity"]["product_sha"],
            "comparison_base_ref": record["identity"]["comparison_base_ref"],
            "comparison_base_sha": record["identity"]["comparison_base_sha"],
            "fixture_sha": Q18_FIXTURE_SHA,
            "sdk_sha": S4_SDK_SHA,
            "profile": "focused",
            "producer_run_id": record["producer"]["run_id"],
            "producer_workflow_host_sha": record["producer"]["workflow_host_sha"],
        }
        self.assertEqual(record["record_id"], select_accepted_record(manifest, inputs)["record_id"])
        for field, value in (
            ("product_sha", "9" * 40),
            ("comparison_base_sha", "8" * 40),
            ("fixture_sha", Q16_FIXTURE_SHA),
            ("sdk_sha", S3_SDK_SHA),
            ("profile", "pair"),
            ("profile", "full"),
            ("producer_run_id", 36800811941),
            ("producer_workflow_host_sha", "7" * 40),
        ):
            changed = dict(inputs)
            changed[field] = value
            with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                select_accepted_record(manifest, changed)

    def test_q20_s4_manifest_adds_focused_row_without_changing_producer(self) -> None:
        manifest = json.loads(ACCEPTED_INPUTS_PATH.read_text(encoding="utf-8"))
        rows = manifest["records"]
        q20_rows = [
            row for row in rows if row["identity"]["fixture_sha"] == Q20_FIXTURE_SHA
        ]
        self.assertEqual(1, len(q20_rows))
        record = q20_rows[0]
        q18 = next(
            row for row in rows
            if row["identity"]["fixture_sha"] == Q18_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S4_SDK_SHA
            and row["identity"]["profile"] == "focused"
        )
        self.assertEqual("accepted-producer-36851180755-focused-q20-s4", record["record_id"])
        self.assertEqual("accepted", record["disposition"])
        self.assertTrue(record["w14780_eligible"])
        self.assertEqual(
            {
                "product_sha": "c3a5d1135480efc61b6c24b275ea3322e0cfa14d",
                "comparison_base_ref": "main",
                "comparison_base_sha": "4a1ecb1e26fa0c6e8933bb73188bc6735da18ee7",
                "fixture_sha": Q20_FIXTURE_SHA,
                "sdk_sha": S4_SDK_SHA,
                "profile": "focused",
            },
            record["identity"],
        )
        self.assertEqual(q18["producer"], record["producer"])
        self.assertEqual(q18["jobs"], record["jobs"])
        self.assertEqual(q18["artifacts"], record["artifacts"])
        inputs = {
            "product_sha": record["identity"]["product_sha"],
            "comparison_base_ref": record["identity"]["comparison_base_ref"],
            "comparison_base_sha": record["identity"]["comparison_base_sha"],
            "fixture_sha": Q20_FIXTURE_SHA,
            "sdk_sha": S4_SDK_SHA,
            "profile": "focused",
            "producer_run_id": record["producer"]["run_id"],
            "producer_workflow_host_sha": record["producer"]["workflow_host_sha"],
        }
        self.assertEqual(record["record_id"], select_accepted_record(manifest, inputs)["record_id"])
        for field, value in (
            ("product_sha", "9" * 40),
            ("comparison_base_sha", "8" * 40),
            ("fixture_sha", "0" * 40),
            ("sdk_sha", S3_SDK_SHA),
            ("profile", "pair"),
            ("profile", "full"),
            ("producer_run_id", 36800811941),
            ("producer_workflow_host_sha", "7" * 40),
        ):
            changed = dict(inputs)
            changed[field] = value
            with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                select_accepted_record(manifest, changed)

    def test_q22_s4_manifest_adds_focused_and_full_rows_without_changing_producer(self) -> None:
        manifest = json.loads(ACCEPTED_INPUTS_PATH.read_text(encoding="utf-8"))
        rows = manifest["records"]
        q22_rows = [
            row for row in rows if row["identity"]["fixture_sha"] == Q22_FIXTURE_SHA
        ]
        self.assertEqual(2, len(q22_rows))
        by_profile = {row["identity"]["profile"]: row for row in q22_rows}
        self.assertEqual({"focused", "full"}, set(by_profile))
        q20 = next(
            row for row in rows
            if row["identity"]["fixture_sha"] == Q20_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S4_SDK_SHA
            and row["identity"]["profile"] == "focused"
        )
        for profile, record_id in (
            ("focused", "accepted-producer-36851180755-focused-q22-s4"),
            ("full", "accepted-producer-36851180755-full-q22-s4"),
        ):
            record = by_profile[profile]
            self.assertEqual(record_id, record["record_id"])
            self.assertEqual("accepted", record["disposition"])
            self.assertTrue(record["w14780_eligible"])
            self.assertEqual(
                {
                    "product_sha": "c3a5d1135480efc61b6c24b275ea3322e0cfa14d",
                    "comparison_base_ref": "main",
                    "comparison_base_sha": "4a1ecb1e26fa0c6e8933bb73188bc6735da18ee7",
                    "fixture_sha": Q22_FIXTURE_SHA,
                    "sdk_sha": S4_SDK_SHA,
                    "profile": profile,
                },
                record["identity"],
            )
            self.assertEqual(q20["producer"], record["producer"])
            self.assertEqual(q20["jobs"], record["jobs"])
            self.assertEqual(q20["artifacts"], record["artifacts"])
            inputs = {
                "product_sha": record["identity"]["product_sha"],
                "comparison_base_ref": record["identity"]["comparison_base_ref"],
                "comparison_base_sha": record["identity"]["comparison_base_sha"],
                "fixture_sha": Q22_FIXTURE_SHA,
                "sdk_sha": S4_SDK_SHA,
                "profile": profile,
                "producer_run_id": record["producer"]["run_id"],
                "producer_workflow_host_sha": record["producer"]["workflow_host_sha"],
            }
            self.assertEqual(record_id, select_accepted_record(manifest, inputs)["record_id"])
            for field, value in (
                ("product_sha", "9" * 40),
                ("comparison_base_sha", "8" * 40),
                ("fixture_sha", "0" * 40),
                ("fixture_sha", "8b3728ff5af32882572c79da5437f50605093d20"),
                ("sdk_sha", S3_SDK_SHA),
                ("profile", "pair"),
                ("producer_run_id", 36800811941),
                ("producer_workflow_host_sha", "7" * 40),
            ):
                changed = dict(inputs)
                changed[field] = value
                with self.subTest(profile=profile, field=field), self.assertRaises(ValueError):
                    select_accepted_record(manifest, changed)

    def test_q24_s4_manifest_adds_focused_and_full_rows_without_changing_producer(self) -> None:
        manifest = json.loads(ACCEPTED_INPUTS_PATH.read_text(encoding="utf-8"))
        rows = manifest["records"]
        q24_rows = [
            row for row in rows if row["identity"]["fixture_sha"] == Q24_FIXTURE_SHA
        ]
        self.assertEqual(2, len(q24_rows))
        by_profile = {row["identity"]["profile"]: row for row in q24_rows}
        self.assertEqual({"focused", "full"}, set(by_profile))
        q22 = next(
            row for row in rows
            if row["identity"]["fixture_sha"] == Q22_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S4_SDK_SHA
            and row["identity"]["profile"] == "focused"
        )
        for profile, record_id in (
            ("focused", "accepted-producer-36851180755-focused-q24-s4"),
            ("full", "accepted-producer-36851180755-full-q24-s4"),
        ):
            record = by_profile[profile]
            self.assertEqual(record_id, record["record_id"])
            self.assertEqual("accepted", record["disposition"])
            self.assertTrue(record["w14780_eligible"])
            self.assertEqual(
                {
                    "product_sha": "c3a5d1135480efc61b6c24b275ea3322e0cfa14d",
                    "comparison_base_ref": "main",
                    "comparison_base_sha": "4a1ecb1e26fa0c6e8933bb73188bc6735da18ee7",
                    "fixture_sha": Q24_FIXTURE_SHA,
                    "sdk_sha": S4_SDK_SHA,
                    "profile": profile,
                },
                record["identity"],
            )
            self.assertEqual(q22["producer"], record["producer"])
            self.assertEqual(q22["jobs"], record["jobs"])
            self.assertEqual(q22["artifacts"], record["artifacts"])
            inputs = {
                "product_sha": record["identity"]["product_sha"],
                "comparison_base_ref": record["identity"]["comparison_base_ref"],
                "comparison_base_sha": record["identity"]["comparison_base_sha"],
                "fixture_sha": Q24_FIXTURE_SHA,
                "sdk_sha": S4_SDK_SHA,
                "profile": profile,
                "producer_run_id": record["producer"]["run_id"],
                "producer_workflow_host_sha": record["producer"]["workflow_host_sha"],
            }
            self.assertEqual(record_id, select_accepted_record(manifest, inputs)["record_id"])
            for field, value in (
                ("product_sha", "9" * 40),
                ("comparison_base_sha", "8" * 40),
                ("fixture_sha", "0" * 40),
                ("fixture_sha", "f" * 40),
                ("sdk_sha", S3_SDK_SHA),
                ("profile", "pair"),
                ("producer_run_id", 36800811941),
                ("producer_workflow_host_sha", "7" * 40),
            ):
                changed = dict(inputs)
                changed[field] = value
                with self.subTest(profile=profile, field=field), self.assertRaises(ValueError):
                    select_accepted_record(manifest, changed)

    def test_q25_s4_manifest_adds_focused_and_full_rows_without_changing_producer(self) -> None:
        manifest = json.loads(ACCEPTED_INPUTS_PATH.read_text(encoding="utf-8"))
        rows = manifest["records"]
        q25_rows = [
            row for row in rows if row["identity"]["fixture_sha"] == Q25_FIXTURE_SHA
        ]
        self.assertEqual(2, len(q25_rows))
        by_profile = {row["identity"]["profile"]: row for row in q25_rows}
        self.assertEqual({"focused", "full"}, set(by_profile))
        q24 = next(
            row for row in rows
            if row["identity"]["fixture_sha"] == Q24_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S4_SDK_SHA
            and row["identity"]["profile"] == "focused"
        )
        for profile, record_id in (
            ("focused", "accepted-producer-36851180755-focused-q25-s4"),
            ("full", "accepted-producer-36851180755-full-q25-s4"),
        ):
            record = by_profile[profile]
            self.assertEqual(record_id, record["record_id"])
            self.assertEqual("accepted", record["disposition"])
            self.assertTrue(record["w14780_eligible"])
            self.assertEqual(
                {
                    "product_sha": "c3a5d1135480efc61b6c24b275ea3322e0cfa14d",
                    "comparison_base_ref": "main",
                    "comparison_base_sha": "4a1ecb1e26fa0c6e8933bb73188bc6735da18ee7",
                    "fixture_sha": Q25_FIXTURE_SHA,
                    "sdk_sha": S4_SDK_SHA,
                    "profile": profile,
                },
                record["identity"],
            )
            self.assertEqual(q24["producer"], record["producer"])
            self.assertEqual(q24["jobs"], record["jobs"])
            self.assertEqual(q24["artifacts"], record["artifacts"])
            inputs = {
                "product_sha": record["identity"]["product_sha"],
                "comparison_base_ref": record["identity"]["comparison_base_ref"],
                "comparison_base_sha": record["identity"]["comparison_base_sha"],
                "fixture_sha": Q25_FIXTURE_SHA,
                "sdk_sha": S4_SDK_SHA,
                "profile": profile,
                "producer_run_id": record["producer"]["run_id"],
                "producer_workflow_host_sha": record["producer"]["workflow_host_sha"],
            }
            self.assertEqual(record_id, select_accepted_record(manifest, inputs)["record_id"])
            for field, value in (
                ("product_sha", "9" * 40),
                ("comparison_base_sha", "8" * 40),
                ("fixture_sha", "f" * 40),
                ("sdk_sha", S3_SDK_SHA),
                ("profile", "pair"),
                ("producer_run_id", 36800811941),
                ("producer_workflow_host_sha", "7" * 40),
            ):
                changed = dict(inputs)
                changed[field] = value
                with self.subTest(profile=profile, field=field), self.assertRaises(ValueError):
                    select_accepted_record(manifest, changed)

    def test_q26_s4_manifest_adds_focused_and_full_rows_without_changing_producer(self) -> None:
        manifest = json.loads(ACCEPTED_INPUTS_PATH.read_text(encoding="utf-8"))
        rows = manifest["records"]
        q26_rows = [
            row for row in rows if row["identity"]["fixture_sha"] == Q26_FIXTURE_SHA
        ]
        self.assertEqual(2, len(q26_rows))
        by_profile = {row["identity"]["profile"]: row for row in q26_rows}
        self.assertEqual({"focused", "full"}, set(by_profile))
        q25 = next(
            row for row in rows
            if row["identity"]["fixture_sha"] == Q25_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S4_SDK_SHA
            and row["identity"]["profile"] == "focused"
        )
        for profile, record_id in (
            ("focused", "accepted-producer-36851180755-focused-q26-s4"),
            ("full", "accepted-producer-36851180755-full-q26-s4"),
        ):
            record = by_profile[profile]
            self.assertEqual(record_id, record["record_id"])
            self.assertEqual("accepted", record["disposition"])
            self.assertTrue(record["w14780_eligible"])
            self.assertEqual(
                {
                    "product_sha": "c3a5d1135480efc61b6c24b275ea3322e0cfa14d",
                    "comparison_base_ref": "main",
                    "comparison_base_sha": "4a1ecb1e26fa0c6e8933bb73188bc6735da18ee7",
                    "fixture_sha": Q26_FIXTURE_SHA,
                    "sdk_sha": S4_SDK_SHA,
                    "profile": profile,
                },
                record["identity"],
            )
            self.assertEqual(q25["producer"], record["producer"])
            self.assertEqual(q25["jobs"], record["jobs"])
            self.assertEqual(q25["artifacts"], record["artifacts"])
            inputs = {
                "product_sha": record["identity"]["product_sha"],
                "comparison_base_ref": record["identity"]["comparison_base_ref"],
                "comparison_base_sha": record["identity"]["comparison_base_sha"],
                "fixture_sha": Q26_FIXTURE_SHA,
                "sdk_sha": S4_SDK_SHA,
                "profile": profile,
                "producer_run_id": record["producer"]["run_id"],
                "producer_workflow_host_sha": record["producer"]["workflow_host_sha"],
            }
            self.assertEqual(record_id, select_accepted_record(manifest, inputs)["record_id"])
            for field, value in (
                ("product_sha", "9" * 40),
                ("comparison_base_sha", "8" * 40),
                ("fixture_sha", "f" * 40),
                ("sdk_sha", S3_SDK_SHA),
                ("profile", "pair"),
                ("producer_run_id", 36800811941),
                ("producer_workflow_host_sha", "7" * 40),
            ):
                changed = dict(inputs)
                changed[field] = value
                with self.subTest(profile=profile, field=field), self.assertRaises(ValueError):
                    select_accepted_record(manifest, changed)

    def test_q57_s4_rows_bind_exact_h2_seven_job_producer_contract(self) -> None:
        manifest = json.loads(ACCEPTED_INPUTS_PATH.read_text(encoding="utf-8"))
        rows = manifest["records"]
        q57_rows = [
            row for row in rows
            if row["identity"]["fixture_sha"] == Q57_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S4_SDK_SHA
        ]
        self.assertEqual(2, len(q57_rows))
        by_profile = {row["identity"]["profile"]: row for row in q57_rows}
        self.assertEqual({"focused", "full"}, set(by_profile))

        expected_jobs = {
            "Verify standard runner graph and exact identities": (
                "completed", "success", "ubuntu-24.04", True,
            ),
            "Package native Linux x86_64": (
                "completed", "success", "ubuntu-24.04", True,
            ),
            "Package native Linux ARM64": (
                "completed", "success", "ubuntu-24.04-arm", True,
            ),
            "Prepare exact Cargo lock and app-server schema diff": (
                "completed", "skipped", "ubuntu-24.04", False,
            ),
            "Consume native Linux ARM64 package": (
                "completed", "failure", "ubuntu-24.04-arm", True,
            ),
            "Consume native Linux x86_64 package": (
                "completed", "failure", "ubuntu-24.04", True,
            ),
            "Verify exact SDK runtime-version parser selectors": (
                "completed", "skipped", "ubuntu-24.04", False,
            ),
        }
        self.assertEqual(set(expected_jobs), H2_PRODUCER_JOB_NAMES)
        self.assertEqual(7, len(H2_PRODUCER_JOB_NAMES))

        reference_jobs = None
        for profile, record_id in (
            ("focused", "accepted-producer-37126137319-focused-q57-s4"),
            ("full", "accepted-producer-37126137319-full-q57-s4"),
        ):
            record = by_profile[profile]
            self.assertEqual(record_id, record["record_id"])
            self.assertEqual("accepted", record["disposition"])
            self.assertTrue(record["w14780_eligible"])
            self.assertEqual(
                {
                    "product_sha": "2391be99dae78eab289324a2ba35660cbdcd328f",
                    "comparison_base_ref": "main",
                    "comparison_base_sha": "4a1ecb1e26fa0c6e8933bb73188bc6735da18ee7",
                    "fixture_sha": Q57_FIXTURE_SHA,
                    "sdk_sha": S4_SDK_SHA,
                    "profile": profile,
                },
                record["identity"],
            )
            self.assertEqual(
                {
                    "repository": REPOSITORY,
                    "repository_id": REPOSITORY_ID,
                    "workflow_id": WORKFLOW_ID,
                    "workflow_path": WORKFLOW_PATH,
                    "run_id": 37126137319,
                    "run_attempt": 1,
                    "event": "workflow_dispatch",
                    "workflow_host_sha": H2_PRODUCER_WORKFLOW_HOST_SHA,
                    "branch": "reconstruct/first-binary-package-workflow-20261001",
                    "status": "completed",
                    "conclusion": "failure",
                },
                record["producer"],
            )
            actual_jobs = {
                job["name"]: (
                    job["status"], job["conclusion"], job["runner"], job["ran"],
                )
                for job in record["jobs"]
            }
            self.assertEqual(expected_jobs, actual_jobs)
            self.assertEqual(
                {
                    "id": 11276316777,
                    "name": "sedna-first-binary-2391be99dae78eab289324a2ba35660cbdcd328f-x86_64-37126137319",
                    "digest": "sha256:f53dff7fd55b9917655b9a22dbfa9673ff4fb023ee8ed0c06dc99160f2142b04",
                    "size_in_bytes": 279940829,
                },
                record["artifacts"]["x86_64"],
            )
            self.assertEqual(
                {
                    "id": 11274859241,
                    "name": "sedna-first-binary-2391be99dae78eab289324a2ba35660cbdcd328f-aarch64-37126137319",
                    "digest": "sha256:d3899e48dc2b014cf522c6021d45c3aa369d4a378e4525a9d05589ee9bbae91a",
                    "size_in_bytes": 276358701,
                },
                record["artifacts"]["aarch64"],
            )
            _validate_manifest_record(record)
            inputs = {
                **record["identity"],
                "producer_run_id": record["producer"]["run_id"],
                "producer_workflow_host_sha": record["producer"]["workflow_host_sha"],
            }
            self.assertEqual(record_id, select_accepted_record(manifest, inputs)["record_id"])
            if reference_jobs is None:
                reference_jobs = record["jobs"]
            else:
                self.assertEqual(reference_jobs, record["jobs"])

        record = by_profile["focused"]
        run, workflow, jobs, artifacts = producer_fixtures(record)
        verified_artifacts = verify_fixture(run, workflow, jobs, artifacts, accepted_record=record)
        self.assertEqual(
            record["artifacts"]["x86_64"]["name"],
            verified_artifacts["x86_64"]["name"],
        )

        missing_sdk_job = copy.deepcopy(jobs)
        missing_sdk_job["jobs"] = [
            job for job in missing_sdk_job["jobs"]
            if job["name"] != "Verify exact SDK runtime-version parser selectors"
        ]
        missing_sdk_job["total_count"] -= 1
        with self.assertRaises(ValueError):
            verify_fixture(run, workflow, missing_sdk_job, artifacts, accepted_record=record)

        extra_live_job = copy.deepcopy(jobs)
        extra_live_job["jobs"].append(
            hosted_job("Unexpected producer job", "success", "ubuntu-24.04")
        )
        extra_live_job["total_count"] += 1
        with self.assertRaises(ValueError):
            verify_fixture(run, workflow, extra_live_job, artifacts, accepted_record=record)

        active_sdk_job = copy.deepcopy(jobs)
        sdk_job = next(
            job for job in active_sdk_job["jobs"]
            if job["name"] == "Verify exact SDK runtime-version parser selectors"
        )
        sdk_job["status"] = "in_progress"
        sdk_job["conclusion"] = None
        with self.assertRaises(ValueError):
            verify_fixture(run, workflow, active_sdk_job, artifacts, accepted_record=record)

        failed_sdk_contract = copy.deepcopy(record)
        failed_sdk_manifest_job = next(
            job for job in failed_sdk_contract["jobs"]
            if job["name"] == "Verify exact SDK runtime-version parser selectors"
        )
        failed_sdk_manifest_job["conclusion"] = "failure"
        failed_sdk_manifest_job["ran"] = True
        with self.assertRaises(ValueError):
            _validate_manifest_record(failed_sdk_contract)

        wrong_live_runner = copy.deepcopy(jobs)
        live_sdk_job = next(
            job for job in wrong_live_runner["jobs"]
            if job["name"] == "Verify exact SDK runtime-version parser selectors"
        )
        live_sdk_job["labels"] = ["ubuntu-24.04-arm"]
        with self.assertRaises(ValueError):
            verify_fixture(run, workflow, wrong_live_runner, artifacts, accepted_record=record)

        wrong_runner = copy.deepcopy(record)
        wrong_runner_sdk_job = next(
            job for job in wrong_runner["jobs"]
            if job["name"] == "Verify exact SDK runtime-version parser selectors"
        )
        wrong_runner_sdk_job["runner"] = "ubuntu-24.04-arm"
        with self.assertRaises(ValueError):
            _validate_manifest_record(wrong_runner)

        extra_manifest_job = copy.deepcopy(record)
        extra_manifest_job["jobs"].append(
            {
                "name": "Unexpected producer job",
                "status": "completed",
                "conclusion": "success",
                "runner": "ubuntu-24.04",
                "ran": True,
            }
        )
        with self.assertRaises(ValueError):
            _validate_manifest_record(extra_manifest_job)

        wrong_host_for_seven_jobs = copy.deepcopy(record)
        wrong_host_for_seven_jobs["producer"]["workflow_host_sha"] = "5" * 40
        with self.assertRaises(ValueError):
            _validate_manifest_record(wrong_host_for_seven_jobs)

        historical = next(
            row for row in rows if row["identity"]["fixture_sha"] == Q26_FIXTURE_SHA
        )
        self.assertEqual(6, len(historical["jobs"]))
        _validate_manifest_record(historical)

    def test_q59_s4_rows_bind_exact_h2_seven_job_producer_contract(self) -> None:
        manifest = json.loads(ACCEPTED_INPUTS_PATH.read_text(encoding="utf-8"))
        rows = manifest["records"]
        q57_rows_list = [
            row
            for row in rows
            if row["identity"]["fixture_sha"] == Q57_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S4_SDK_SHA
        ]
        q59_rows_list = [
            row
            for row in rows
            if row["identity"]["fixture_sha"] == Q59_FIXTURE_SHA
        ]
        self.assertEqual(2, len(q57_rows_list))
        self.assertEqual(2, len(q59_rows_list))
        q57_rows = {row["identity"]["profile"]: row for row in q57_rows_list}
        q59_rows = {row["identity"]["profile"]: row for row in q59_rows_list}
        self.assertEqual({"focused", "full"}, set(q57_rows))
        self.assertEqual({"focused", "full"}, set(q59_rows))

        for profile in ("focused", "full"):
            q57 = q57_rows[profile]
            q59 = q59_rows[profile]
            expected = {
                **q57,
                "record_id": f"accepted-producer-37126137319-{profile}-q59-s4",
                "identity": {
                    **q57["identity"],
                    "fixture_sha": Q59_FIXTURE_SHA,
                },
            }
            self.assertEqual(expected, q59)
            _validate_manifest_record(q59)
            inputs = {
                **q59["identity"],
                "producer_run_id": q59["producer"]["run_id"],
                "producer_workflow_host_sha": q59["producer"]["workflow_host_sha"],
            }
            self.assertEqual(
                q59["record_id"],
                select_accepted_record(manifest, inputs)["record_id"],
            )

    def test_q60_s4_rows_bind_exact_h2_seven_job_producer_contract(self) -> None:
        manifest = json.loads(ACCEPTED_INPUTS_PATH.read_text(encoding="utf-8"))
        rows = manifest["records"]
        q59_rows_list = [
            row
            for row in rows
            if row["identity"]["fixture_sha"] == Q59_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S4_SDK_SHA
        ]
        q60_rows_list = [
            row
            for row in rows
            if row["identity"]["fixture_sha"] == Q60_FIXTURE_SHA
        ]
        self.assertEqual(2, len(q59_rows_list))
        self.assertEqual(2, len(q60_rows_list))
        q59_rows = {row["identity"]["profile"]: row for row in q59_rows_list}
        q60_rows = {row["identity"]["profile"]: row for row in q60_rows_list}
        self.assertEqual({"focused", "full"}, set(q59_rows))
        self.assertEqual({"focused", "full"}, set(q60_rows))

        for profile in ("focused", "full"):
            q59 = q59_rows[profile]
            q60 = q60_rows[profile]
            expected = {
                **q59,
                "record_id": f"accepted-producer-37126137319-{profile}-q60-s4",
                "identity": {
                    **q59["identity"],
                    "fixture_sha": Q60_FIXTURE_SHA,
                },
            }
            self.assertEqual(expected, q60)
            _validate_manifest_record(q60)
            inputs = {
                **q60["identity"],
                "producer_run_id": q60["producer"]["run_id"],
                "producer_workflow_host_sha": q60["producer"]["workflow_host_sha"],
            }
            self.assertEqual(
                q60["record_id"],
                select_accepted_record(manifest, inputs)["record_id"],
            )

    def test_q61_s4_rows_bind_exact_h2_seven_job_producer_contract(self) -> None:
        manifest = json.loads(ACCEPTED_INPUTS_PATH.read_text(encoding="utf-8"))
        rows = manifest["records"]
        q60_rows_list = [
            row
            for row in rows
            if row["identity"]["fixture_sha"] == Q60_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S4_SDK_SHA
        ]
        q61_rows_list = [
            row
            for row in rows
            if row["identity"]["fixture_sha"] == Q61_FIXTURE_SHA
        ]
        self.assertEqual(2, len(q60_rows_list))
        self.assertEqual(2, len(q61_rows_list))
        q60_rows = {row["identity"]["profile"]: row for row in q60_rows_list}
        q61_rows = {row["identity"]["profile"]: row for row in q61_rows_list}
        self.assertEqual({"focused", "full"}, set(q60_rows))
        self.assertEqual({"focused", "full"}, set(q61_rows))

        for profile in ("focused", "full"):
            q60 = q60_rows[profile]
            q61 = q61_rows[profile]
            expected = {
                **q60,
                "record_id": f"accepted-producer-37126137319-{profile}-q61-s4",
                "identity": {
                    **q60["identity"],
                    "fixture_sha": Q61_FIXTURE_SHA,
                },
            }
            self.assertEqual(expected, q61)
            _validate_manifest_record(q61)
            inputs = {
                **q61["identity"],
                "producer_run_id": q61["producer"]["run_id"],
                "producer_workflow_host_sha": q61["producer"]["workflow_host_sha"],
            }
            self.assertEqual(
                q61["record_id"],
                select_accepted_record(manifest, inputs)["record_id"],
            )

    def test_q62_s4_rows_bind_exact_h2_seven_job_producer_contract(self) -> None:
        manifest = json.loads(ACCEPTED_INPUTS_PATH.read_text(encoding="utf-8"))
        rows = manifest["records"]
        q61_rows_list = [
            row
            for row in rows
            if row["identity"]["fixture_sha"] == Q61_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S4_SDK_SHA
        ]
        q62_rows_list = [
            row
            for row in rows
            if row["identity"]["fixture_sha"] == Q62_FIXTURE_SHA
        ]
        self.assertEqual(2, len(q61_rows_list))
        self.assertEqual(2, len(q62_rows_list))
        q61_rows = {row["identity"]["profile"]: row for row in q61_rows_list}
        q62_rows = {row["identity"]["profile"]: row for row in q62_rows_list}
        self.assertEqual({"focused", "full"}, set(q61_rows))
        self.assertEqual({"focused", "full"}, set(q62_rows))

        for profile in ("focused", "full"):
            q61 = q61_rows[profile]
            q62 = q62_rows[profile]
            expected = {
                **q61,
                "record_id": f"accepted-producer-37126137319-{profile}-q62-s4",
                "identity": {
                    **q61["identity"],
                    "fixture_sha": Q62_FIXTURE_SHA,
                },
            }
            self.assertEqual(expected, q62)
            _validate_manifest_record(q62)
            inputs = {
                **q62["identity"],
                "producer_run_id": q62["producer"]["run_id"],
                "producer_workflow_host_sha": q62["producer"]["workflow_host_sha"],
            }
            self.assertEqual(
                q62["record_id"],
                select_accepted_record(manifest, inputs)["record_id"],
            )

    def test_q63_s4_rows_bind_exact_h2_seven_job_producer_contract(self) -> None:
        manifest = json.loads(ACCEPTED_INPUTS_PATH.read_text(encoding="utf-8"))
        rows = manifest["records"]
        q62_rows_list = [
            row
            for row in rows
            if row["identity"]["fixture_sha"] == Q62_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S4_SDK_SHA
        ]
        q63_rows_list = [
            row
            for row in rows
            if row["identity"]["fixture_sha"] == Q63_FIXTURE_SHA
        ]
        self.assertEqual(2, len(q62_rows_list))
        self.assertEqual(2, len(q63_rows_list))
        q62_rows = {row["identity"]["profile"]: row for row in q62_rows_list}
        q63_rows = {row["identity"]["profile"]: row for row in q63_rows_list}
        self.assertEqual({"focused", "full"}, set(q62_rows))
        self.assertEqual({"focused", "full"}, set(q63_rows))

        for profile in ("focused", "full"):
            q62 = q62_rows[profile]
            q63 = q63_rows[profile]
            expected = {
                **q62,
                "record_id": f"accepted-producer-37126137319-{profile}-q63-s4",
                "identity": {
                    **q62["identity"],
                    "fixture_sha": Q63_FIXTURE_SHA,
                },
            }
            self.assertEqual(expected, q63)
            _validate_manifest_record(q63)
            inputs = {
                **q63["identity"],
                "producer_run_id": q63["producer"]["run_id"],
                "producer_workflow_host_sha": q63["producer"]["workflow_host_sha"],
            }
            self.assertEqual(
                q63["record_id"],
                select_accepted_record(manifest, inputs)["record_id"],
            )

    def test_q64_s4_rows_bind_exact_h2_seven_job_producer_contract(self) -> None:
        manifest = json.loads(ACCEPTED_INPUTS_PATH.read_text(encoding="utf-8"))
        rows = manifest["records"]
        q63_rows_list = [
            row
            for row in rows
            if row["identity"]["fixture_sha"] == Q63_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S4_SDK_SHA
        ]
        q64_rows_list = [
            row
            for row in rows
            if row["identity"]["fixture_sha"] == Q64_FIXTURE_SHA
        ]
        self.assertEqual(2, len(q63_rows_list))
        self.assertEqual(2, len(q64_rows_list))
        q63_rows = {row["identity"]["profile"]: row for row in q63_rows_list}
        q64_rows = {row["identity"]["profile"]: row for row in q64_rows_list}
        self.assertEqual({"focused", "full"}, set(q63_rows))
        self.assertEqual({"focused", "full"}, set(q64_rows))

        for profile in ("focused", "full"):
            q63 = q63_rows[profile]
            q64 = q64_rows[profile]
            expected = {
                **q63,
                "record_id": f"accepted-producer-37126137319-{profile}-q64-s4",
                "identity": {
                    **q63["identity"],
                    "fixture_sha": Q64_FIXTURE_SHA,
                },
            }
            self.assertEqual(expected, q64)
            _validate_manifest_record(q64)
            inputs = {
                **q64["identity"],
                "producer_run_id": q64["producer"]["run_id"],
                "producer_workflow_host_sha": q64["producer"]["workflow_host_sha"],
            }
            self.assertEqual(
                q64["record_id"],
                select_accepted_record(manifest, inputs)["record_id"],
            )

    def test_q66_s4_rows_bind_exact_h2_seven_job_producer_contract(self) -> None:
        manifest = json.loads(ACCEPTED_INPUTS_PATH.read_text(encoding="utf-8"))
        rows = manifest["records"]
        q64_rows_list = [
            row
            for row in rows
            if row["identity"]["fixture_sha"] == Q64_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S4_SDK_SHA
        ]
        q66_rows_list = [
            row
            for row in rows
            if row["identity"]["fixture_sha"] == Q66_FIXTURE_SHA
        ]
        self.assertEqual(2, len(q64_rows_list))
        self.assertEqual(2, len(q66_rows_list))
        q64_rows = {row["identity"]["profile"]: row for row in q64_rows_list}
        q66_rows = {row["identity"]["profile"]: row for row in q66_rows_list}
        self.assertEqual({"focused", "full"}, set(q64_rows))
        self.assertEqual({"focused", "full"}, set(q66_rows))

        for profile in ("focused", "full"):
            q64 = q64_rows[profile]
            q66 = q66_rows[profile]
            expected = {
                **q64,
                "record_id": f"accepted-producer-37126137319-{profile}-q66-s4",
                "identity": {
                    **q64["identity"],
                    "fixture_sha": Q66_FIXTURE_SHA,
                },
            }
            self.assertEqual(expected, q66)
            _validate_manifest_record(q66)
            inputs = {
                **q66["identity"],
                "producer_run_id": q66["producer"]["run_id"],
                "producer_workflow_host_sha": q66["producer"]["workflow_host_sha"],
            }
            self.assertEqual(
                q66["record_id"],
                select_accepted_record(manifest, inputs)["record_id"],
            )

    def test_q67_s4_rows_bind_exact_h2_seven_job_producer_contract(self) -> None:
        manifest = json.loads(ACCEPTED_INPUTS_PATH.read_text(encoding="utf-8"))
        rows = manifest["records"]
        q66_rows_list = [
            row
            for row in rows
            if row["identity"]["fixture_sha"] == Q66_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S4_SDK_SHA
        ]
        q67_rows_list = [
            row
            for row in rows
            if row["identity"]["fixture_sha"] == Q67_FIXTURE_SHA
        ]
        self.assertEqual(2, len(q66_rows_list))
        self.assertEqual(2, len(q67_rows_list))
        q66_rows = {row["identity"]["profile"]: row for row in q66_rows_list}
        q67_rows = {row["identity"]["profile"]: row for row in q67_rows_list}
        self.assertEqual({"focused", "full"}, set(q66_rows))
        self.assertEqual({"focused", "full"}, set(q67_rows))

        for profile in ("focused", "full"):
            q66 = q66_rows[profile]
            q67 = q67_rows[profile]
            expected = {
                **q66,
                "record_id": f"accepted-producer-37126137319-{profile}-q67-s4",
                "identity": {
                    **q66["identity"],
                    "fixture_sha": Q67_FIXTURE_SHA,
                },
            }
            self.assertEqual(expected, q67)
            _validate_manifest_record(q67)
            inputs = {
                **q67["identity"],
                "producer_run_id": q67["producer"]["run_id"],
                "producer_workflow_host_sha": q67["producer"]["workflow_host_sha"],
            }
            self.assertEqual(
                q67["record_id"],
                select_accepted_record(manifest, inputs)["record_id"],
            )

    def test_q68_s4_rows_bind_exact_h2_seven_job_producer_contract(self) -> None:
        manifest = json.loads(ACCEPTED_INPUTS_PATH.read_text(encoding="utf-8"))
        rows = manifest["records"]
        q67_rows_list = [
            row
            for row in rows
            if row["identity"]["fixture_sha"] == Q67_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S4_SDK_SHA
        ]
        q68_rows_list = [
            row
            for row in rows
            if row["identity"]["fixture_sha"] == Q68_FIXTURE_SHA
        ]
        self.assertEqual(2, len(q67_rows_list))
        self.assertEqual(2, len(q68_rows_list))
        q67_rows = {row["identity"]["profile"]: row for row in q67_rows_list}
        q68_rows = {row["identity"]["profile"]: row for row in q68_rows_list}
        self.assertEqual({"focused", "full"}, set(q67_rows))
        self.assertEqual({"focused", "full"}, set(q68_rows))

        for profile in ("focused", "full"):
            q67 = q67_rows[profile]
            q68 = q68_rows[profile]
            expected = {
                **q67,
                "record_id": f"accepted-producer-37126137319-{profile}-q68-s4",
                "identity": {
                    **q67["identity"],
                    "fixture_sha": Q68_FIXTURE_SHA,
                },
            }
            self.assertEqual(expected, q68)
            _validate_manifest_record(q68)
            inputs = {
                **q68["identity"],
                "producer_run_id": q68["producer"]["run_id"],
                "producer_workflow_host_sha": q68["producer"]["workflow_host_sha"],
            }
            self.assertEqual(
                q68["record_id"],
                select_accepted_record(manifest, inputs)["record_id"],
            )

    def test_q69_s4_rows_bind_exact_h2_seven_job_producer_contract(self) -> None:
        manifest = json.loads(ACCEPTED_INPUTS_PATH.read_text(encoding="utf-8"))
        rows = manifest["records"]
        q68_rows_list = [
            row
            for row in rows
            if row["identity"]["fixture_sha"] == Q68_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S4_SDK_SHA
        ]
        q69_rows_list = [
            row
            for row in rows
            if row["identity"]["fixture_sha"] == Q69_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S4_SDK_SHA
            and row["producer"]["run_id"] == 37126137319
        ]
        self.assertEqual(2, len(q68_rows_list))
        self.assertEqual(2, len(q69_rows_list))
        q68_rows = {row["identity"]["profile"]: row for row in q68_rows_list}
        q69_rows = {row["identity"]["profile"]: row for row in q69_rows_list}
        self.assertEqual({"focused", "full"}, set(q68_rows))
        self.assertEqual({"focused", "full"}, set(q69_rows))

        for profile in ("focused", "full"):
            q68 = q68_rows[profile]
            q69 = q69_rows[profile]
            expected = {
                **q68,
                "record_id": f"accepted-producer-37126137319-{profile}-q69-s4",
                "identity": {
                    **q68["identity"],
                    "fixture_sha": Q69_FIXTURE_SHA,
                },
            }
            self.assertEqual(expected, q69)
            _validate_manifest_record(q69)
            inputs = {
                **q69["identity"],
                "producer_run_id": q69["producer"]["run_id"],
                "producer_workflow_host_sha": q69["producer"]["workflow_host_sha"],
            }
            self.assertEqual(
                q69["record_id"],
                select_accepted_record(manifest, inputs)["record_id"],
            )

    def test_q69_s4_h9e58_rows_bind_exact_new_producer_and_reject_tampering(self) -> None:
        manifest = json.loads(ACCEPTED_INPUTS_PATH.read_text(encoding="utf-8"))
        rows = [
            row
            for row in manifest["records"]
            if row["identity"]["fixture_sha"] == Q69_FIXTURE_SHA
            and row["identity"]["sdk_sha"] == S4_SDK_SHA
            and row["producer"]["workflow_host_sha"] == H9E58_PRODUCER_WORKFLOW_HOST_SHA
        ]
        self.assertEqual(2, len(rows))
        by_profile = {row["identity"]["profile"]: row for row in rows}
        self.assertEqual({"focused", "full"}, set(by_profile))
        expected_jobs = {
            "Verify standard runner graph and exact identities": (
                "completed", "success", "ubuntu-24.04", True,
            ),
            "Package native Linux x86_64": (
                "completed", "success", "ubuntu-24.04", True,
            ),
            "Package native Linux ARM64": (
                "completed", "success", "ubuntu-24.04-arm", True,
            ),
            "Verify exact SDK runtime-version parser selectors": (
                "completed", "skipped", "ubuntu-24.04", False,
            ),
            "Prepare exact Cargo lock and app-server schema diff": (
                "completed", "skipped", "ubuntu-24.04", False,
            ),
            "Consume native Linux ARM64 package": (
                "completed", "failure", "ubuntu-24.04-arm", True,
            ),
            "Consume native Linux x86_64 package": (
                "completed", "failure", "ubuntu-24.04", True,
            ),
        }
        self.assertEqual(set(expected_jobs), H9E58_PRODUCER_JOB_NAMES)
        self.assertEqual(7, len(H9E58_PRODUCER_JOB_NAMES))

        expected_artifacts = {
            "x86_64": {
                "id": 11296039818,
                "name": "sedna-first-binary-8389b61d82cb6fb936e4e500b977f31682441ffe-x86_64-37182140778",
                "digest": "sha256:cef1413efcc803e28b32c2ebb149cafc60bdd936edd339a462a99b9237bbe495",
                "size_in_bytes": 279582045,
            },
            "aarch64": {
                "id": 11295794265,
                "name": "sedna-first-binary-8389b61d82cb6fb936e4e500b977f31682441ffe-aarch64-37182140778",
                "digest": "sha256:3774a8cd65543c241b0775d0b0ed77b3c88064bc7bb98bfb40a640c5bc2105eb",
                "size_in_bytes": 275216410,
            },
        }
        for profile, record in by_profile.items():
            with self.subTest(profile=profile):
                self.assertEqual(
                    f"accepted-producer-37182140778-{profile}-q69-s4-h9e58",
                    record["record_id"],
                )
                self.assertEqual(
                    {
                        "product_sha": "8389b61d82cb6fb936e4e500b977f31682441ffe",
                        "comparison_base_ref": "main",
                        "comparison_base_sha": "4a1ecb1e26fa0c6e8933bb73188bc6735da18ee7",
                        "fixture_sha": Q69_FIXTURE_SHA,
                        "sdk_sha": S4_SDK_SHA,
                        "profile": profile,
                    },
                    record["identity"],
                )
                self.assertEqual(
                    {
                        "repository": REPOSITORY,
                        "repository_id": REPOSITORY_ID,
                        "workflow_id": WORKFLOW_ID,
                        "workflow_path": WORKFLOW_PATH,
                        "run_id": H9E58_PRODUCER_RUN_ID,
                        "run_attempt": 1,
                        "event": "workflow_dispatch",
                        "workflow_host_sha": H9E58_PRODUCER_WORKFLOW_HOST_SHA,
                        "branch": "reconstruct/first-binary-package-workflow-20261001",
                        "status": "completed",
                        "conclusion": "failure",
                    },
                    record["producer"],
                )
                actual_jobs = {
                    job["name"]: (
                        job["status"], job["conclusion"], job["runner"], job["ran"],
                    )
                    for job in record["jobs"]
                }
                self.assertEqual(expected_jobs, actual_jobs)
                self.assertEqual(expected_artifacts, record["artifacts"])
                _validate_manifest_record(record)
                api_fixture = producer_fixtures(accepted_record=record)
                self.assertEqual(
                    set(expected_artifacts),
                    set(verify_fixture(*api_fixture, accepted_record=record)),
                )
                wrong_host_run = copy.deepcopy(api_fixture[0])
                wrong_host_run["head_sha"] = "f" * 40
                with self.assertRaises(ValueError):
                    verify_fixture(
                        wrong_host_run,
                        *api_fixture[1:],
                        accepted_record=record,
                    )
                inputs = {
                    **record["identity"],
                    "producer_run_id": record["producer"]["run_id"],
                    "producer_workflow_host_sha": record["producer"]["workflow_host_sha"],
                }
                self.assertEqual(
                    record["record_id"],
                    select_accepted_record(manifest, inputs)["record_id"],
                )
                wrong_inputs = dict(inputs)
                wrong_inputs["producer_workflow_host_sha"] = H2_PRODUCER_WORKFLOW_HOST_SHA
                with self.assertRaises(ValueError):
                    select_accepted_record(manifest, wrong_inputs)

                wrong_host = copy.deepcopy(record)
                wrong_host["producer"]["workflow_host_sha"] = "f" * 40
                with self.assertRaises(ValueError):
                    _validate_manifest_record(wrong_host)

                wrong_sdk_job = copy.deepcopy(record)
                sdk_job = next(
                    job
                    for job in wrong_sdk_job["jobs"]
                    if job["name"] == "Verify exact SDK runtime-version parser selectors"
                )
                sdk_job["conclusion"] = "success"
                sdk_job["ran"] = True
                with self.assertRaises(ValueError):
                    _validate_manifest_record(wrong_sdk_job)

                changed_artifact_record = copy.deepcopy(record)
                changed_artifact_record["artifacts"]["x86_64"]["digest"] = (
                    "sha256:" + "0" * 64
                )
                with self.assertRaises(ValueError):
                    verify_fixture(
                        *api_fixture,
                        accepted_record=changed_artifact_record,
                    )

    def test_fixture_sdk_generations_select_closed_package_and_sdk_inventories(self) -> None:
        q2_pair = consume_existing_test_plan(Q2_FIXTURE_SHA, S0_SDK_SHA, "pair")
        self.assertEqual(frozenset({"fresh", "bad_checksum"}), q2_pair["state"])
        self.assertEqual(frozenset(), q2_pair["plain"])
        q2_full = consume_existing_test_plan(Q2_FIXTURE_SHA, S0_SDK_SHA, "full")
        self.assertEqual(24, len(q2_full["state"]))
        self.assertEqual(FULL_PLAIN_TESTS, q2_full["plain"])

        q3_full = consume_existing_test_plan(Q3_FIXTURE_SHA, S1_SDK_SHA, "full")
        self.assertEqual(24, len(q3_full["state"]))
        self.assertEqual(FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS, q3_full["plain"])
        self.assertEqual(17, len(q3_full["plain"]))
        self.assertEqual(5, len(Q3_ADDITIONAL_PLAIN_TESTS))
        q4_pair = consume_existing_test_plan(Q4_FIXTURE_SHA, S2_SDK_SHA, "pair")
        self.assertEqual(frozenset({"fresh", "bad_checksum"}), q4_pair["state"])
        self.assertEqual(frozenset(), q4_pair["plain"])
        q4_full = consume_existing_test_plan(Q4_FIXTURE_SHA, S2_SDK_SHA, "full")
        self.assertEqual(24, len(q4_full["state"]))
        self.assertEqual(FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS, q4_full["plain"])
        self.assertEqual(17, len(q4_full["plain"]))
        q5_focused = consume_existing_test_plan(Q5_FIXTURE_SHA, S2_SDK_SHA, "focused")
        self.assertEqual(frozenset(), q5_focused["state"])
        self.assertEqual(FOCUSED_REPAIR_PLAIN_TESTS, q5_focused["plain"])
        self.assertEqual(6, len(q5_focused["plain"]))
        q5_full = consume_existing_test_plan(Q5_FIXTURE_SHA, S2_SDK_SHA, "full")
        self.assertEqual(24, len(q5_full["state"]))
        self.assertEqual(FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS, q5_full["plain"])
        self.assertEqual(17, len(q5_full["plain"]))
        q11_focused = consume_existing_test_plan(Q11_FIXTURE_SHA, S3_SDK_SHA, "focused")
        self.assertEqual(frozenset(), q11_focused["state"])
        self.assertEqual(FOCUSED_REPAIR_PLAIN_TESTS, q11_focused["plain"])
        self.assertEqual(6, len(q11_focused["plain"]))
        q13_focused = consume_existing_test_plan(Q13_FIXTURE_SHA, S4_SDK_SHA, "focused")
        self.assertEqual(frozenset(), q13_focused["state"])
        self.assertEqual(FOCUSED_REPAIR_PLAIN_TESTS, q13_focused["plain"])
        self.assertEqual(6, len(q13_focused["plain"]))
        q14_focused = consume_existing_test_plan(Q14_FIXTURE_SHA, S4_SDK_SHA, "focused")
        self.assertEqual(frozenset(), q14_focused["state"])
        self.assertEqual(FOCUSED_REPAIR_PLAIN_TESTS, q14_focused["plain"])
        self.assertEqual(6, len(q14_focused["plain"]))
        q15_focused = consume_existing_test_plan(Q15_FIXTURE_SHA, S4_SDK_SHA, "focused")
        self.assertEqual(frozenset(), q15_focused["state"])
        self.assertEqual(FOCUSED_REPAIR_PLAIN_TESTS, q15_focused["plain"])
        self.assertEqual(6, len(q15_focused["plain"]))
        q17_focused = consume_existing_test_plan(Q17_FIXTURE_SHA, S4_SDK_SHA, "focused")
        self.assertEqual(frozenset(), q17_focused["state"])
        self.assertEqual(FOCUSED_REPAIR_PLAIN_TESTS, q17_focused["plain"])
        self.assertEqual(6, len(q17_focused["plain"]))
        q18_focused = consume_existing_test_plan(Q18_FIXTURE_SHA, S4_SDK_SHA, "focused")
        self.assertEqual(frozenset(), q18_focused["state"])
        self.assertEqual(FOCUSED_REPAIR_PLAIN_TESTS, q18_focused["plain"])
        self.assertEqual(6, len(q18_focused["plain"]))
        q20_focused = consume_existing_test_plan(Q20_FIXTURE_SHA, S4_SDK_SHA, "focused")
        self.assertEqual(frozenset(), q20_focused["state"])
        self.assertEqual(FOCUSED_REPAIR_PLAIN_TESTS, q20_focused["plain"])
        self.assertEqual(6, len(q20_focused["plain"]))
        q22_focused = consume_existing_test_plan(Q22_FIXTURE_SHA, S4_SDK_SHA, "focused")
        self.assertEqual(frozenset(), q22_focused["state"])
        self.assertEqual(FOCUSED_REPAIR_PLAIN_TESTS, q22_focused["plain"])
        self.assertEqual(6, len(q22_focused["plain"]))
        q22_full = consume_existing_test_plan(Q22_FIXTURE_SHA, S4_SDK_SHA, "full")
        self.assertEqual(EXPECTED_STATE_POSITIVE | EXPECTED_STATE_NEGATIVE, q22_full["state"])
        self.assertEqual(FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS, q22_full["plain"])
        self.assertEqual(17, len(q22_full["plain"]))
        q24_focused = consume_existing_test_plan(Q24_FIXTURE_SHA, S4_SDK_SHA, "focused")
        self.assertEqual(frozenset(), q24_focused["state"])
        self.assertEqual(FOCUSED_REPAIR_PLAIN_TESTS, q24_focused["plain"])
        self.assertEqual(6, len(q24_focused["plain"]))
        q24_full = consume_existing_test_plan(Q24_FIXTURE_SHA, S4_SDK_SHA, "full")
        self.assertEqual(EXPECTED_STATE_POSITIVE | EXPECTED_STATE_NEGATIVE, q24_full["state"])
        self.assertEqual(FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS, q24_full["plain"])
        self.assertEqual(17, len(q24_full["plain"]))
        q25_focused = consume_existing_test_plan(Q25_FIXTURE_SHA, S4_SDK_SHA, "focused")
        self.assertEqual(frozenset(), q25_focused["state"])
        self.assertEqual(FOCUSED_REPAIR_PLAIN_TESTS, q25_focused["plain"])
        self.assertEqual(6, len(q25_focused["plain"]))
        q25_full = consume_existing_test_plan(Q25_FIXTURE_SHA, S4_SDK_SHA, "full")
        self.assertEqual(EXPECTED_STATE_POSITIVE | EXPECTED_STATE_NEGATIVE, q25_full["state"])
        self.assertEqual(FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS, q25_full["plain"])
        self.assertEqual(17, len(q25_full["plain"]))
        q26_focused = consume_existing_test_plan(Q26_FIXTURE_SHA, S4_SDK_SHA, "focused")
        self.assertEqual(frozenset(), q26_focused["state"])
        self.assertEqual(FOCUSED_REPAIR_PLAIN_TESTS, q26_focused["plain"])
        self.assertEqual(6, len(q26_focused["plain"]))
        q26_full = consume_existing_test_plan(Q26_FIXTURE_SHA, S4_SDK_SHA, "full")
        self.assertEqual(EXPECTED_STATE_POSITIVE | EXPECTED_STATE_NEGATIVE, q26_full["state"])
        self.assertEqual(FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS, q26_full["plain"])
        self.assertEqual(17, len(q26_full["plain"]))
        q57_focused = consume_existing_test_plan(Q57_FIXTURE_SHA, S4_SDK_SHA, "focused")
        self.assertEqual(frozenset(), q57_focused["state"])
        self.assertEqual(FOCUSED_REPAIR_PLAIN_TESTS, q57_focused["plain"])
        self.assertEqual(6, len(q57_focused["plain"]))
        q57_full = consume_existing_test_plan(Q57_FIXTURE_SHA, S4_SDK_SHA, "full")
        self.assertEqual(EXPECTED_STATE_POSITIVE | EXPECTED_STATE_NEGATIVE, q57_full["state"])
        self.assertEqual(FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS, q57_full["plain"])
        self.assertEqual(17, len(q57_full["plain"]))
        q59_focused = consume_existing_test_plan(Q59_FIXTURE_SHA, S4_SDK_SHA, "focused")
        self.assertEqual(frozenset(), q59_focused["state"])
        self.assertEqual(FOCUSED_REPAIR_PLAIN_TESTS, q59_focused["plain"])
        self.assertEqual(6, len(q59_focused["plain"]))
        q59_full = consume_existing_test_plan(Q59_FIXTURE_SHA, S4_SDK_SHA, "full")
        self.assertEqual(EXPECTED_STATE_POSITIVE | EXPECTED_STATE_NEGATIVE, q59_full["state"])
        self.assertEqual(FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS, q59_full["plain"])
        self.assertEqual(17, len(q59_full["plain"]))
        q60_focused = consume_existing_test_plan(Q60_FIXTURE_SHA, S4_SDK_SHA, "focused")
        self.assertEqual(frozenset(), q60_focused["state"])
        self.assertEqual(FOCUSED_REPAIR_PLAIN_TESTS, q60_focused["plain"])
        self.assertEqual(6, len(q60_focused["plain"]))
        q60_full = consume_existing_test_plan(Q60_FIXTURE_SHA, S4_SDK_SHA, "full")
        self.assertEqual(EXPECTED_STATE_POSITIVE | EXPECTED_STATE_NEGATIVE, q60_full["state"])
        self.assertEqual(FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS, q60_full["plain"])
        self.assertEqual(17, len(q60_full["plain"]))
        q61_focused = consume_existing_test_plan(Q61_FIXTURE_SHA, S4_SDK_SHA, "focused")
        self.assertEqual(frozenset(), q61_focused["state"])
        self.assertEqual(FOCUSED_REPAIR_PLAIN_TESTS, q61_focused["plain"])
        self.assertEqual(6, len(q61_focused["plain"]))
        q61_full = consume_existing_test_plan(Q61_FIXTURE_SHA, S4_SDK_SHA, "full")
        self.assertEqual(EXPECTED_STATE_POSITIVE | EXPECTED_STATE_NEGATIVE, q61_full["state"])
        self.assertEqual(FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS, q61_full["plain"])
        self.assertEqual(17, len(q61_full["plain"]))
        q62_focused = consume_existing_test_plan(Q62_FIXTURE_SHA, S4_SDK_SHA, "focused")
        self.assertEqual(frozenset(), q62_focused["state"])
        self.assertEqual(FOCUSED_REPAIR_PLAIN_TESTS, q62_focused["plain"])
        self.assertEqual(6, len(q62_focused["plain"]))
        q62_full = consume_existing_test_plan(Q62_FIXTURE_SHA, S4_SDK_SHA, "full")
        self.assertEqual(EXPECTED_STATE_POSITIVE | EXPECTED_STATE_NEGATIVE, q62_full["state"])
        self.assertEqual(FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS, q62_full["plain"])
        self.assertEqual(17, len(q62_full["plain"]))
        q63_focused = consume_existing_test_plan(Q63_FIXTURE_SHA, S4_SDK_SHA, "focused")
        self.assertEqual(frozenset(), q63_focused["state"])
        self.assertEqual(FOCUSED_REPAIR_PLAIN_TESTS, q63_focused["plain"])
        self.assertEqual(6, len(q63_focused["plain"]))
        q63_full = consume_existing_test_plan(Q63_FIXTURE_SHA, S4_SDK_SHA, "full")
        self.assertEqual(EXPECTED_STATE_POSITIVE | EXPECTED_STATE_NEGATIVE, q63_full["state"])
        self.assertEqual(FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS, q63_full["plain"])
        self.assertEqual(17, len(q63_full["plain"]))
        q64_focused = consume_existing_test_plan(Q64_FIXTURE_SHA, S4_SDK_SHA, "focused")
        self.assertEqual(frozenset(), q64_focused["state"])
        self.assertEqual(FOCUSED_REPAIR_PLAIN_TESTS, q64_focused["plain"])
        self.assertEqual(6, len(q64_focused["plain"]))
        q64_full = consume_existing_test_plan(Q64_FIXTURE_SHA, S4_SDK_SHA, "full")
        self.assertEqual(EXPECTED_STATE_POSITIVE | EXPECTED_STATE_NEGATIVE, q64_full["state"])
        self.assertEqual(FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS, q64_full["plain"])
        self.assertEqual(17, len(q64_full["plain"]))
        q66_focused = consume_existing_test_plan(Q66_FIXTURE_SHA, S4_SDK_SHA, "focused")
        self.assertEqual(frozenset(), q66_focused["state"])
        self.assertEqual(FOCUSED_REPAIR_PLAIN_TESTS, q66_focused["plain"])
        self.assertEqual(6, len(q66_focused["plain"]))
        q66_full = consume_existing_test_plan(Q66_FIXTURE_SHA, S4_SDK_SHA, "full")
        self.assertEqual(EXPECTED_STATE_POSITIVE | EXPECTED_STATE_NEGATIVE, q66_full["state"])
        self.assertEqual(FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS, q66_full["plain"])
        self.assertEqual(17, len(q66_full["plain"]))
        q67_focused = consume_existing_test_plan(Q67_FIXTURE_SHA, S4_SDK_SHA, "focused")
        self.assertEqual(frozenset(), q67_focused["state"])
        self.assertEqual(FOCUSED_REPAIR_PLAIN_TESTS, q67_focused["plain"])
        self.assertEqual(6, len(q67_focused["plain"]))
        q67_full = consume_existing_test_plan(Q67_FIXTURE_SHA, S4_SDK_SHA, "full")
        self.assertEqual(EXPECTED_STATE_POSITIVE | EXPECTED_STATE_NEGATIVE, q67_full["state"])
        self.assertEqual(FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS, q67_full["plain"])
        self.assertEqual(17, len(q67_full["plain"]))
        q68_focused = consume_existing_test_plan(Q68_FIXTURE_SHA, S4_SDK_SHA, "focused")
        self.assertEqual(frozenset(), q68_focused["state"])
        self.assertEqual(FOCUSED_REPAIR_PLAIN_TESTS, q68_focused["plain"])
        self.assertEqual(6, len(q68_focused["plain"]))
        q68_full = consume_existing_test_plan(Q68_FIXTURE_SHA, S4_SDK_SHA, "full")
        self.assertEqual(EXPECTED_STATE_POSITIVE | EXPECTED_STATE_NEGATIVE, q68_full["state"])
        self.assertEqual(FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS, q68_full["plain"])
        self.assertEqual(17, len(q68_full["plain"]))
        q69_focused = consume_existing_test_plan(Q69_FIXTURE_SHA, S4_SDK_SHA, "focused")
        self.assertEqual(frozenset(), q69_focused["state"])
        self.assertEqual(FOCUSED_REPAIR_PLAIN_TESTS, q69_focused["plain"])
        self.assertEqual(6, len(q69_focused["plain"]))
        q69_full = consume_existing_test_plan(Q69_FIXTURE_SHA, S4_SDK_SHA, "full")
        self.assertEqual(EXPECTED_STATE_POSITIVE | EXPECTED_STATE_NEGATIVE, q69_full["state"])
        self.assertEqual(FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS, q69_full["plain"])
        self.assertEqual(17, len(q69_full["plain"]))
        for fixture_sha, sdk_sha, profile in (
            (Q3_FIXTURE_SHA, S1_SDK_SHA, "pair"),
            (Q3_FIXTURE_SHA, S0_SDK_SHA, "full"),
            (Q2_FIXTURE_SHA, S1_SDK_SHA, "full"),
            (Q4_FIXTURE_SHA, S1_SDK_SHA, "full"),
            (Q4_FIXTURE_SHA, S2_SDK_SHA, "unknown"),
            (Q4_FIXTURE_SHA, S2_SDK_SHA, "focused"),
            (Q5_FIXTURE_SHA, S1_SDK_SHA, "focused"),
            (Q5_FIXTURE_SHA, S0_SDK_SHA, "full"),
            (Q5_FIXTURE_SHA, S2_SDK_SHA, "pair"),
            (Q11_FIXTURE_SHA, S3_SDK_SHA, "pair"),
            (Q11_FIXTURE_SHA, S3_SDK_SHA, "full"),
            (Q11_FIXTURE_SHA, S2_SDK_SHA, "focused"),
            (Q5_FIXTURE_SHA, S3_SDK_SHA, "focused"),
            (Q13_FIXTURE_SHA, S3_SDK_SHA, "focused"),
            (Q14_FIXTURE_SHA, S3_SDK_SHA, "focused"),
            (Q11_FIXTURE_SHA, S4_SDK_SHA, "focused"),
            (Q13_FIXTURE_SHA, S4_SDK_SHA, "pair"),
            (Q13_FIXTURE_SHA, S4_SDK_SHA, "full"),
            (Q14_FIXTURE_SHA, S4_SDK_SHA, "pair"),
            (Q14_FIXTURE_SHA, S4_SDK_SHA, "full"),
            (Q15_FIXTURE_SHA, S3_SDK_SHA, "focused"),
            (Q15_FIXTURE_SHA, S4_SDK_SHA, "pair"),
            (Q15_FIXTURE_SHA, S4_SDK_SHA, "full"),
            (Q16_FIXTURE_SHA, S4_SDK_SHA, "focused"),
            (Q17_FIXTURE_SHA, S3_SDK_SHA, "focused"),
            (Q17_FIXTURE_SHA, S4_SDK_SHA, "pair"),
            (Q17_FIXTURE_SHA, S4_SDK_SHA, "full"),
            (Q18_FIXTURE_SHA, S3_SDK_SHA, "focused"),
            (Q18_FIXTURE_SHA, S4_SDK_SHA, "pair"),
            (Q18_FIXTURE_SHA, S4_SDK_SHA, "full"),
            (Q20_FIXTURE_SHA, S3_SDK_SHA, "focused"),
            (Q20_FIXTURE_SHA, S4_SDK_SHA, "pair"),
            (Q20_FIXTURE_SHA, S4_SDK_SHA, "full"),
            (Q22_FIXTURE_SHA, S3_SDK_SHA, "focused"),
            (Q22_FIXTURE_SHA, S3_SDK_SHA, "full"),
            (Q22_FIXTURE_SHA, S4_SDK_SHA, "pair"),
            ("1f742078f5bd5c45df92ed05a84458aced0a38fa", S4_SDK_SHA, "focused"),
            (Q24_FIXTURE_SHA, S3_SDK_SHA, "focused"),
            (Q24_FIXTURE_SHA, S4_SDK_SHA, "pair"),
            (Q25_FIXTURE_SHA, S3_SDK_SHA, "focused"),
            (Q25_FIXTURE_SHA, S4_SDK_SHA, "pair"),
            (Q26_FIXTURE_SHA, S3_SDK_SHA, "focused"),
            (Q26_FIXTURE_SHA, S4_SDK_SHA, "pair"),
            (Q57_FIXTURE_SHA, S3_SDK_SHA, "focused"),
            (Q57_FIXTURE_SHA, S4_SDK_SHA, "pair"),
            (Q59_FIXTURE_SHA, S3_SDK_SHA, "focused"),
            (Q59_FIXTURE_SHA, S4_SDK_SHA, "pair"),
            (Q60_FIXTURE_SHA, S3_SDK_SHA, "focused"),
            (Q60_FIXTURE_SHA, S4_SDK_SHA, "pair"),
            (Q61_FIXTURE_SHA, S3_SDK_SHA, "focused"),
            (Q61_FIXTURE_SHA, S4_SDK_SHA, "pair"),
            (Q62_FIXTURE_SHA, S3_SDK_SHA, "focused"),
            (Q62_FIXTURE_SHA, S4_SDK_SHA, "pair"),
            (Q63_FIXTURE_SHA, S3_SDK_SHA, "focused"),
            (Q63_FIXTURE_SHA, S4_SDK_SHA, "pair"),
            (Q64_FIXTURE_SHA, S3_SDK_SHA, "focused"),
            (Q64_FIXTURE_SHA, S4_SDK_SHA, "pair"),
            (Q66_FIXTURE_SHA, S3_SDK_SHA, "focused"),
            (Q66_FIXTURE_SHA, S4_SDK_SHA, "pair"),
            (Q67_FIXTURE_SHA, S3_SDK_SHA, "focused"),
            (Q67_FIXTURE_SHA, S4_SDK_SHA, "pair"),
            (Q68_FIXTURE_SHA, S3_SDK_SHA, "focused"),
            (Q68_FIXTURE_SHA, S4_SDK_SHA, "pair"),
            (Q69_FIXTURE_SHA, S3_SDK_SHA, "focused"),
            (Q69_FIXTURE_SHA, S4_SDK_SHA, "pair"),
            ("8" * 40, S0_SDK_SHA, "full"),
        ):
            with self.subTest(fixture_sha=fixture_sha, sdk_sha=sdk_sha, profile=profile), self.assertRaises(ValueError):
                consume_existing_test_plan(fixture_sha, sdk_sha, profile)

        s0 = sdk_test_plan(S0_SDK_SHA)
        self.assertEqual(2, len(s0["selectors"]))
        self.assertEqual(46, sum(s0["expected_test_cases"].values()))
        s1 = sdk_test_plan(S1_SDK_SHA)
        self.assertEqual(9, len(s1["selectors"]))
        self.assertEqual(s0["selectors"], s1["selectors"][:2])
        self.assertEqual(54, sum(s1["expected_test_cases"].values()))
        s2 = sdk_test_plan(S2_SDK_SHA)
        self.assertEqual(10, len(s2["selectors"]))
        self.assertEqual(s1["selectors"], s2["selectors"][:9])
        self.assertEqual(55, sum(s2["expected_test_cases"].values()))
        s3 = sdk_test_plan(S3_SDK_SHA)
        self.assertEqual(s2, s3)
        self.assertEqual(10, len(s3["selectors"]))
        self.assertEqual(55, sum(s3["expected_test_cases"].values()))
        s4 = sdk_test_plan(S4_SDK_SHA)
        self.assertEqual(s3, s4)
        self.assertEqual(10, len(s4["selectors"]))
        self.assertEqual(55, sum(s4["expected_test_cases"].values()))
        self.assertEqual(
            "sdk/python/tests/test_app_server_harness_request_routing.py::test_used_route_does_not_make_one_new_matching_route_ambiguous",
            s2["selectors"][-1],
        )
        self.assertEqual(
            [
                "sdk/python/tests/test_app_server_harness_request_routing.py::test_fifo_response_selection_remains_the_default",
                "sdk/python/tests/test_app_server_harness_request_routing.py::test_request_routes_match_exact_requests_and_wait_outside_selector_lock",
                "sdk/python/tests/test_app_server_harness_request_routing.py::test_bad_request_route_sets_fail_and_are_reported_on_teardown",
                "sdk/python/tests/test_app_server_harness_request_routing.py::test_unused_one_shot_routes_fail_at_teardown",
                "sdk/python/tests/test_app_server_harness_request_routing.py::test_one_shot_route_cannot_be_reused",
                "sdk/python/tests/test_app_server_harness_request_routing.py::test_fifo_and_request_matched_modes_cannot_be_mixed",
                "sdk/python/tests/test_app_server_harness_request_routing.py::test_server_teardown_releases_a_gated_request",
            ],
            s1["selectors"][2:],
        )
        self.assertEqual(8, sum(s1["expected_test_cases"][name] for name in s1["expected_test_cases"] if name not in s0["expected_test_cases"]))
        self.assertEqual(2, s1["expected_test_cases"]["test_bad_request_route_sets_fail_and_are_reported_on_teardown"])
        with self.assertRaisesRegex(ValueError, "SDK source SHA"):
            sdk_test_plan("9" * 40)

    def test_workflow_restores_userns_for_all_consumer_profiles_and_runs_exact_sdk_inventory(self) -> None:
        workflow_path = ACCEPTED_INPUTS_PATH.parent / "workflows" / "sedna-branch-build.yml"
        workflow = workflow_path.read_text(encoding="utf-8")
        self.assertEqual(2, workflow.count("Temporarily enable Linux user namespaces for existing-package consumers"))
        self.assertEqual(2, workflow.count("Restore original Linux user-namespace settings"))
        self.assertEqual(2, workflow.count("Check accepted package fixture modules for undefined names"))
        witness_dir_init = (
            'if [[ ("$MODE" == "consume-existing" && "$CONSUMER_PROFILE" == "focused") || '
            '("$MODE" == "consume-existing" && "$CONSUMER_PROFILE" == "full") || '
            '("$MODE" == "build" && "$CONSUMER_PROFILE" == "browser-diagnostic") ]]; then\n'
            '            mkdir -m 700 -- "$RUNNER_TEMP/state-history-witnesses"'
        )
        self.assertEqual(2, workflow.count(witness_dir_init))
        consumer_jobs = (
            (
                "consume-linux-x86_64",
                "ubuntu-24.04",
                workflow.split("  consume-linux-x86_64:", 1)[1].split("\n  sdk-parser:", 1)[0],
            ),
            (
                "consume-linux-aarch64",
                "ubuntu-24.04-arm",
                workflow.split("  consume-linux-aarch64:", 1)[1],
            ),
        )
        for job_name, expected_runner, job in consumer_jobs:
            with self.subTest(job=job_name):
                self.assertIn(f"runs-on: {expected_runner}", job)
                self.assertIn(
                    "inputs.consumer_profile == 'pair' || inputs.consumer_profile == 'full' || inputs.consumer_profile == 'focused'",
                    job,
                )
                self.assertIn("runner.environment == 'github-hosted'", job)
                self.assertIn("Restore original Linux user-namespace settings", job)
                lint_marker = "      - name: Check accepted package fixture modules for undefined names"
                lint_start = job.index(lint_marker)
                lint_end = job.find("\n      - name:", lint_start + len(lint_marker))
                lint_step = job[lint_start:] if lint_end < 0 else job[lint_start:lint_end]
                userns_start = job.index("      - name: Temporarily enable Linux user namespaces for existing-package consumers")
                consumer_start = job.index("      - name: Run existing-package consumers with explicit producer provenance")
                restore_start = job.index("      - name: Restore original Linux user-namespace settings")
                self.assertLess(lint_start, userns_start)
                self.assertLess(lint_start, consumer_start)
                self.assertLess(userns_start, consumer_start)
                self.assertLess(consumer_start, restore_start)
                enable_step = job[userns_start:job.index("\n      - name:", userns_start + 1)]
                restore_end = job.find("\n      - name:", restore_start + 1)
                restore_step = job[restore_start:] if restore_end < 0 else job[restore_start:restore_end]
                # Ruff has its own focused guard; verify the named userns steps instead.
                self.assertEqual(
                    [line.strip() for line in enable_step.splitlines() if line.lstrip().startswith("if: ")],
                    [
                        "if: ${{ inputs.mode == 'consume-existing' && "
                        "(inputs.consumer_profile == 'pair' || inputs.consumer_profile == 'full' || "
                        "inputs.consumer_profile == 'focused') && runner.os == 'Linux' && "
                        "runner.environment == 'github-hosted' }}"
                    ],
                )
                self.assertEqual(
                    [line.strip() for line in restore_step.splitlines() if line.lstrip().startswith("if: ")],
                    [
                        "if: ${{ always() && inputs.mode == 'consume-existing' && "
                        "(inputs.consumer_profile == 'pair' || inputs.consumer_profile == 'full' || "
                        "inputs.consumer_profile == 'focused') && runner.os == 'Linux' && "
                        "runner.environment == 'github-hosted' }}"
                    ],
                )
                self.assertIn(
                    "if: ${{ inputs.mode == 'consume-existing' && (inputs.consumer_profile == 'focused' || inputs.consumer_profile == 'full') }}",
                    lint_step,
                )
                self.assertIn("UV_PATH: ${{ steps.setup_uv.outputs.uv-path }}", lint_step)
                self.assertIn('--project "${SDK_ROOT}/sdk/python"', lint_step)
                self.assertIn("--locked", lint_step)
                self.assertIn("--python 3.12.14", lint_step)
                self.assertIn("ruff check", lint_step)
                self.assertNotIn(Q26_FIXTURE_SHA, lint_step)
                ruff_arguments = [
                    line.strip().removesuffix("\\").strip()
                    for line in lint_step.split("ruff check \\", 1)[1].splitlines()
                    if line.strip()
                ]
                self.assertEqual(
                    [
                        "--select F821,F822,F823",
                        "--output-format=github",
                        '"${FIXTURE_ROOT}/scripts/codex_package/smoke_tests/first_binary/test_agent_control_model_acceptance.py"',
                        '"${FIXTURE_ROOT}/scripts/codex_package/smoke_tests/first_binary/test_agent_control_tui_acceptance.py"',
                        '"${FIXTURE_ROOT}/scripts/codex_package/smoke_tests/first_binary/tui_pty.py"',
                    ],
                    ruff_arguments,
                )
                consumer_step = job[consumer_start:restore_start]
                self.assertIn("ARTIFACT_BASE_SHA: ${{ steps.verified_artifact.outputs.artifact_base_sha }}", consumer_step)
                self.assertIn('--base-sha "${ARTIFACT_BASE_SHA}"', consumer_step)
                self.assertIn('pytest -q \\', consumer_step)
                self.assertIn('pytest_addopts="--tb=no --show-capture=no"', consumer_step)

        sdk_job = workflow.split("  sdk-parser:", 1)[1].split("\n  consume-linux-aarch64:", 1)[0]
        self.assertIn("runs-on: ubuntu-24.04", sdk_job)
        self.assertIn("version: \"0.12.17\"", sdk_job)
        self.assertIn("python-version: \"3.12.14\"", sdk_job)
        self.assertIn("SDK_SHA: ${{ inputs.sdk_sha }}", sdk_job)
        self.assertEqual(3, sdk_job.count("from verify_existing_first_binary_producer import sdk_test_plan"))
        self.assertIn("sdk_test_plan(os.environ[\"SDK_SHA\"])", sdk_job)
        self.assertIn("sdk_test_plan(sdk_sha)", sdk_job)
        self.assertIn('"selectors": plan["selectors"]', sdk_job)
        self.assertIn('"expected_test_cases": plan["expected_test_cases"]', sdk_job)
        self.assertIn("plan[\"expected_test_cases\"]", sdk_job)
        self.assertIn("*(str(sdk_root / selector) for selector in plan[\"selectors\"])", sdk_job)
        self.assertIn('"pytest_exit_code": completed.returncode', sdk_job)
        self.assertIn("first-binary-sdk-parser-status.json", sdk_job)
        self.assertIn("skipped != 0 or failed != 0 or errored != 0", sdk_job)
        self.assertIn("first-binary-sdk-parser-results.xml", sdk_job)

        focused_selectors = (
            "test_packaged_model_nested_spawn_recovery_and_list_after_resume",
            "test_packaged_model_queue_only_message_does_not_wake_until_followup_and_exact_join",
            "test_packaged_model_goal_continuation_and_terminal_transition",
            "test_packaged_tui_agents_details_render_configured_identity_and_unknown_effective_identity",
            "test_actual_tui_agents_entry_has_initial_empty_search",
            "test_actual_tui_nested_filter_clear_live_rename_and_replay",
        )
        self.assertEqual(FOCUSED_REPAIR_PLAIN_TESTS, frozenset(focused_selectors))
        self.assertIn("- focused", workflow)
        self.assertIn("case \"${CONSUMER_PROFILE}\" in pair|focused|full)", workflow)
        self.assertEqual(2, workflow.count("focused)"))
        for selector in focused_selectors:
            for job_name, _expected_runner, job in consumer_jobs:
                with self.subTest(job=job_name, selector=selector):
                    self.assertEqual(1, job.count(f"::{selector}\""))

    def test_existing_consumer_verifier_env_binds_every_required_input(self) -> None:
        workflow_path = ACCEPTED_INPUTS_PATH.parent / "workflows" / "sedna-branch-build.yml"
        workflow = workflow_path.read_text(encoding="utf-8")
        step_name = "Verify producer API evidence and write current consumer context"
        consumers = (
            ("consume-linux-x86_64", "x86_64"),
            ("consume-linux-aarch64", "aarch64"),
        )
        for job_name, product_arch in consumers:
            env = _workflow_step_env(workflow, job_name, step_name)
            with self.subTest(job=job_name):
                _assert_producer_verification_env(env, product_arch)
            for field, wrong_value in (
                ("FIXTURE_SHA", None),
                ("FIXTURE_SHA", "${{ inputs.target_sha }}"),
                ("SDK_SHA", None),
                ("SDK_SHA", "${{ inputs.target_sha }}"),
                ("CONSUMER_PROFILE", None),
                ("CONSUMER_PROFILE", "${{ inputs.mode }}"),
            ):
                changed = dict(env)
                if wrong_value is None:
                    changed.pop(field, None)
                else:
                    changed[field] = wrong_value
                with self.subTest(job=job_name, field=field, wrong_value=wrong_value), self.assertRaisesRegex(
                    ValueError, field
                ):
                    _assert_producer_verification_env(changed, product_arch)

    def test_duplicate_manifest_keys_are_rejected(self) -> None:
        with self.assertRaisesRegex(ValueError, "duplicate JSON key"):
            _json_object_without_duplicate_keys([("w14780_eligible", False), ("w14780_eligible", True)])

    def test_exact_good_tuple_selects_eligible_record(self) -> None:
        record = accepted_fixture_record()
        selected = select_accepted_record(
            {"schema_version": "sedna-first-binary-accepted-inputs-v1", "records": [record]},
            accepted_fixture_inputs(),
        )
        self.assertEqual(selected["record_id"], record["record_id"])

    def test_diagnostic_record_is_never_eligible(self) -> None:
        identity = DIAGNOSTIC_RECORD["identity"]
        producer = DIAGNOSTIC_RECORD["producer"]
        diagnostic_inputs = {
            "product_sha": identity["product_sha"],
            "comparison_base_ref": identity["comparison_base_ref"],
            "comparison_base_sha": identity["comparison_base_sha"],
            "fixture_sha": identity["fixture_sha"],
            "sdk_sha": identity["sdk_sha"],
            "profile": identity["profile"],
            "producer_run_id": producer["run_id"],
            "producer_workflow_host_sha": producer["workflow_host_sha"],
        }
        with self.assertRaisesRegex(ValueError, "diagnostic"):
            select_accepted_record(
                {"schema_version": "sedna-first-binary-accepted-inputs-v1", "records": [copy.deepcopy(DIAGNOSTIC_RECORD)]},
                diagnostic_inputs,
            )
        promoted = copy.deepcopy(DIAGNOSTIC_RECORD)
        promoted["disposition"] = "accepted"
        promoted["w14780_eligible"] = True
        with self.assertRaisesRegex(ValueError, "permanently ineligible"):
            select_accepted_record(
                {"schema_version": "sedna-first-binary-accepted-inputs-v1", "records": [promoted]},
                accepted_fixture_inputs(),
            )

    def test_wrong_t_b_q_s_producer_or_profile_is_rejected(self) -> None:
        record = accepted_fixture_record()
        wrong_values = {
            "product_sha": "1" * 40,
            "comparison_base_ref": "release",
            "comparison_base_sha": "2" * 40,
            "fixture_sha": "3" * 40,
            "sdk_sha": "4" * 40,
            "profile": "full",
            "producer_run_id": accepted_fixture_inputs()["producer_run_id"] + 1,
            "producer_workflow_host_sha": "7" * 40,
        }
        for field, value in wrong_values.items():
            inputs = accepted_fixture_inputs()
            inputs[field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                select_accepted_record(
                    {"schema_version": "sedna-first-binary-accepted-inputs-v1", "records": [record]},
                    inputs,
                )

    def test_runner_policy_requires_trusted_host_and_exact_manifest_tuple(self) -> None:
        record = accepted_fixture_record()
        branch = "reconstruct/first-binary-package-workflow-20261001"
        ref = f"refs/heads/{branch}"
        host = "7" * 40
        inputs = accepted_fixture_inputs()
        env = {
            "MODE": "consume-existing",
            "GITHUB_REPOSITORY": REPOSITORY,
            "GITHUB_EVENT_NAME": "workflow_dispatch",
            "GITHUB_REF": ref,
            "GITHUB_REF_NAME": branch,
            "GITHUB_WORKFLOW_REF": f"{REPOSITORY}/{WORKFLOW_PATH}@{ref}",
            "GITHUB_SHA": host,
            "EXPECTED_H": host,
            "TARGET_SHA": inputs["product_sha"],
            "BASE_REF": inputs["comparison_base_ref"],
            "BASE_SHA": inputs["comparison_base_sha"],
            "FIXTURE_SHA": inputs["fixture_sha"],
            "SDK_SHA": inputs["sdk_sha"],
            "CONSUMER_PROFILE": inputs["profile"],
            "PRODUCER_RUN_ID": str(inputs["producer_run_id"]),
            "PRODUCER_WORKFLOW_HOST_SHA": inputs["producer_workflow_host_sha"],
        }
        manifest = {"schema_version": "sedna-first-binary-accepted-inputs-v1", "records": [record]}
        with mock.patch("verify_existing_first_binary_producer.subprocess.check_output", return_value=host), mock.patch(
            "verify_existing_first_binary_producer._read_manifest", return_value=manifest
        ):
            validate_runner_policy_inputs(env)
        for key, value in (("FIXTURE_SHA", "3" * 40), ("SDK_SHA", "4" * 40), ("GITHUB_REF_NAME", "other")):
            changed = dict(env)
            changed[key] = value
            with mock.patch("verify_existing_first_binary_producer.subprocess.check_output", return_value=host), mock.patch(
                "verify_existing_first_binary_producer._read_manifest", return_value=manifest
            ), self.subTest(key=key), self.assertRaises(ValueError):
                validate_runner_policy_inputs(changed)

    def test_manifest_cannot_promote_diagnostic_or_mutate_artifact_tuple(self) -> None:
        mutations = (
            ("id", 123456),
            ("name", "wrong-artifact-name"),
            ("digest", "sha256:" + "0" * 64),
            ("size_in_bytes", 1),
        )
        for field, value in mutations:
            record = accepted_fixture_record()
            record["artifacts"]["x86_64"][field] = value
            run, workflow, jobs, artifacts = producer_fixtures()
            with self.subTest(field=field), self.assertRaises(ValueError):
                verify_existing_producer(
                    run,
                    workflow,
                    jobs,
                    artifacts,
                    **accepted_producer_args(),
                    accepted_record=record,
                )


class ExactW14780RouteTests(unittest.TestCase):
    def test_h6_record_binds_distinct_package_and_consumer_bases(self) -> None:
        manifest = json.loads(ACCEPTED_INPUTS_PATH.read_text(encoding="utf-8"))
        rows = [row for row in manifest["records"] if row["producer"]["run_id"] == W14780_PRODUCER_RUN_ID]
        self.assertEqual(1, len(rows))
        record = rows[0]
        _validate_manifest_record(record)
        self.assertEqual(W14780_PRODUCER_HOST_SHA, record["producer"]["workflow_host_sha"])
        self.assertEqual(W14780_PRODUCER_BRANCH, record["producer"]["branch"])
        self.assertEqual(W14780_ARTIFACT_BASE_SHA, record["producer"]["artifact_base_sha"])
        self.assertEqual(W14780_BASE_SHA, record["identity"]["comparison_base_sha"])
        self.assertNotEqual(record["producer"]["artifact_base_sha"], record["identity"]["comparison_base_sha"])
        self.assertEqual(
            {
                "product_sha": W14780_T4_PRODUCT_SHA,
                "comparison_base_ref": W14780_BASE_REF,
                "comparison_base_sha": W14780_BASE_SHA,
                "fixture_sha": W14780_FIXTURE_SHA,
                "sdk_sha": W14780_SDK_SHA,
                "profile": "full",
            },
            record["identity"],
        )
        self.assertEqual(7, len(W14780_PRODUCER_JOB_NAMES))
        producer_jobs = {
            "total_count": len(record["jobs"]),
            "jobs": [
                {
                    **hosted_job(job["name"], job["conclusion"], job["runner"], ran=job["ran"]),
                    "status": job["status"],
                }
                for job in record["jobs"]
            ],
        }
        producer_run = {
            "id": W14780_PRODUCER_RUN_ID,
            "workflow_id": WORKFLOW_ID,
            "head_sha": W14780_PRODUCER_HOST_SHA,
            "head_branch": W14780_PRODUCER_BRANCH,
            "event": "workflow_dispatch",
            "status": "completed",
            "conclusion": "failure",
            "run_attempt": 1,
            "repository": {"id": REPOSITORY_ID, "full_name": REPOSITORY},
            "head_repository": {"id": REPOSITORY_ID, "full_name": REPOSITORY},
        }
        producer_workflow = {"id": WORKFLOW_ID, "path": WORKFLOW_PATH, "state": "active"}
        producer_artifacts = {
            "total_count": 2,
            "artifacts": [
                {
                    **artifact,
                    "expired": False,
                    "workflow_run": {
                        "id": W14780_PRODUCER_RUN_ID,
                        "head_sha": W14780_PRODUCER_HOST_SHA,
                        "head_branch": W14780_PRODUCER_BRANCH,
                        "repository_id": REPOSITORY_ID,
                        "head_repository_id": REPOSITORY_ID,
                    },
                }
                for artifact in record["artifacts"].values()
            ],
        }
        selected_artifacts = verify_existing_producer(
            producer_run,
            producer_workflow,
            producer_jobs,
            producer_artifacts,
            producer_run_id=W14780_PRODUCER_RUN_ID,
            producer_workflow_host_sha=W14780_PRODUCER_HOST_SHA,
            product_sha=W14780_T4_PRODUCT_SHA,
            base_ref=W14780_BASE_REF,
            base_sha=W14780_BASE_SHA,
            accepted_record=record,
        )
        self.assertEqual({"x86_64", "aarch64"}, set(selected_artifacts))
        inputs = {
            **record["identity"],
            "producer_run_id": record["producer"]["run_id"],
            "producer_workflow_host_sha": record["producer"]["workflow_host_sha"],
        }
        self.assertEqual(record["record_id"], select_accepted_record(manifest, inputs)["record_id"])
        for field, value in (
            ("artifact_base_sha", W14780_BASE_SHA),
            ("workflow_host_sha", "f" * 40),
        ):
            changed_record = copy.deepcopy(record)
            changed_record["producer"][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                _validate_manifest_record(changed_record)
        changed = dict(inputs)
        changed["comparison_base_sha"] = W14780_ARTIFACT_BASE_SHA
        with self.assertRaises(ValueError):
            select_accepted_record(manifest, changed)

    def test_h6_full_plan_includes_only_bound_additions_and_expanded_case_count(self) -> None:
        plan = consume_existing_test_plan(W14780_FIXTURE_SHA, W14780_SDK_SHA, "full")
        self.assertEqual(FULL_PLAIN_TESTS | Q3_ADDITIONAL_PLAIN_TESTS | W14780_ADDITIONAL_PLAIN_TESTS, plan["plain"])
        self.assertEqual(21, plan["plain_case_count"])
        with self.assertRaises(ValueError):
            consume_existing_test_plan(W14780_FIXTURE_SHA, W14780_SDK_SHA, "focused")

    def test_status_projection_keeps_case_status_and_discards_failure_payload(self) -> None:
        source = ET.fromstring(
            '<testsuite tests="2" failures="1" errors="0" skipped="0">'
            '<testcase name="test_pass"><system-out>PRIVATE_SCREEN</system-out></testcase>'
            '<testcase name="test_fail"><failure message="PRIVATE_ASSERTION">PRIVATE_TRACE</failure>'
            '<system-err>PRIVATE_REQUEST</system-err></testcase></testsuite>'
        )
        projected, statuses, counts, invalid_name = _status_only_junit_projection(list(source.iter("testcase")))
        text = ET.tostring(projected, encoding="unicode")
        self.assertFalse(invalid_name)
        self.assertEqual(
            [
                {"name": "test_pass", "status": "passed"},
                {"name": "test_fail", "status": "failed"},
            ],
            statuses,
        )
        self.assertEqual({"passed": 1, "failed": 1, "error": 0, "skipped": 0}, counts)
        for private_text in ("PRIVATE_SCREEN", "PRIVATE_ASSERTION", "PRIVATE_TRACE", "PRIVATE_REQUEST"):
            self.assertNotIn(private_text, text)


class CurrentConsumerIdentityTests(unittest.TestCase):
    def test_exact_current_consumer_api_identity_is_accepted(self) -> None:
        run, workflow, jobs, env = current_consumer_fixtures()
        with mock.patch("verify_existing_first_binary_producer.platform.system", return_value="Linux"), mock.patch(
            "verify_existing_first_binary_producer.platform.machine", return_value="x86_64"
        ):
            context = verify_current_consumer(
                run,
                workflow,
                jobs,
                env=env,
                architecture="x86_64",
                product_sha=CROSS_RUN_PRODUCER["product_sha"],
                base_sha=CROSS_RUN_PRODUCER["comparison_base_sha"],
            )
        self.assertEqual(context["run_id"], int(env["GITHUB_RUN_ID"]))
        self.assertEqual(context["workflow_host_sha"], env["GITHUB_SHA"])
        self.assertEqual(context["workflow_ref"], env["GITHUB_WORKFLOW_REF"])

    def test_consume_existing_requires_trusted_current_host_and_api_head(self) -> None:
        run, workflow, jobs, env = current_consumer_fixtures()
        branch = "reconstruct/first-binary-package-workflow-20261001"
        ref = f"refs/heads/{branch}"
        env["MODE"] = "consume-existing"
        env["EXPECTED_H"] = env["GITHUB_SHA"]
        env["GITHUB_REF"] = ref
        env["GITHUB_REF_NAME"] = branch
        env["GITHUB_WORKFLOW_REF"] = f"{REPOSITORY}/{WORKFLOW_PATH}@{ref}"
        run["head_branch"] = branch
        with mock.patch("verify_existing_first_binary_producer.platform.system", return_value="Linux"), mock.patch(
            "verify_existing_first_binary_producer.platform.machine", return_value="x86_64"
        ):
            verify_current_consumer(
                run,
                workflow,
                jobs,
                env=env,
                architecture="x86_64",
                product_sha=CROSS_RUN_PRODUCER["product_sha"],
                base_sha=CROSS_RUN_PRODUCER["comparison_base_sha"],
            )
        for key, value in (("GITHUB_REF", "refs/heads/other"), ("EXPECTED_H", "f" * 40)):
            bad_env = dict(env)
            bad_env[key] = value
            with mock.patch("verify_existing_first_binary_producer.platform.system", return_value="Linux"), mock.patch(
                "verify_existing_first_binary_producer.platform.machine", return_value="x86_64"
            ), self.subTest(key=key), self.assertRaises(ValueError):
                verify_current_consumer(
                    run,
                    workflow,
                    jobs,
                    env=bad_env,
                    architecture="x86_64",
                    product_sha=CROSS_RUN_PRODUCER["product_sha"],
                    base_sha=CROSS_RUN_PRODUCER["comparison_base_sha"],
                )

    def test_workflow_host_checkout_must_equal_github_sha(self) -> None:
        branch = "reconstruct/first-binary-package-workflow-20261001"
        ref = f"refs/heads/{branch}"
        host = "7" * 40
        env = {
            "GITHUB_EVENT_NAME": "workflow_dispatch",
            "GITHUB_REF": ref,
            "GITHUB_REF_NAME": branch,
            "GITHUB_WORKFLOW_REF": f"{REPOSITORY}/{WORKFLOW_PATH}@{ref}",
            "GITHUB_SHA": host,
            "EXPECTED_H": host,
        }
        _verify_trusted_consumer_ref(env, checkout_sha=host)
        with self.assertRaisesRegex(ValueError, "checked-out workflow source"):
            _verify_trusted_consumer_ref(env, checkout_sha="8" * 40)

    def test_current_consumer_env_and_runner_must_match_api(self) -> None:
        for key, value in (
            ("GITHUB_SHA", "f" * 40),
            ("GITHUB_RUN_ID", "39000000001"),
            ("GITHUB_REF", "refs/pull/2/merge"),
            ("GITHUB_WORKFLOW_REF", "other/path@refs/heads/x"),
            ("RUNNER_ARCH", "ARM64"),
            ("EXPECTED_RUNNER_LABEL", "self-hosted"),
        ):
            run, workflow, jobs, env = current_consumer_fixtures()
            env[key] = value
            with mock.patch("verify_existing_first_binary_producer.platform.system", return_value="Linux"), mock.patch(
                "verify_existing_first_binary_producer.platform.machine", return_value="x86_64"
            ), self.subTest(key=key), self.assertRaises(ValueError):
                verify_current_consumer(
                    run,
                    workflow,
                    jobs,
                    env=env,
                    architecture="x86_64",
                    product_sha=CROSS_RUN_PRODUCER["product_sha"],
                    base_sha=CROSS_RUN_PRODUCER["comparison_base_sha"],
                )

    def test_current_runner_api_label_and_group_must_match(self) -> None:
        run, workflow, jobs, env = current_consumer_fixtures()
        jobs["jobs"][0]["runner_group_id"] = 99
        with mock.patch("verify_existing_first_binary_producer.platform.system", return_value="Linux"), mock.patch(
            "verify_existing_first_binary_producer.platform.machine", return_value="x86_64"
        ), self.assertRaises(ValueError):
            verify_current_consumer(
                run,
                workflow,
                jobs,
                env=env,
                architecture="x86_64",
                product_sha=CROSS_RUN_PRODUCER["product_sha"],
                base_sha=CROSS_RUN_PRODUCER["comparison_base_sha"],
            )


class BackwardsCompatibleBuildModeTests(unittest.TestCase):
    def test_same_run_native_build_artifact_is_still_accepted(self) -> None:
        architecture = "x86_64"
        branch = "reconstruct/first-increment-20261001"
        run_id = 39000000001
        host = "abcdef0123456789abcdef0123456789abcdef01"
        product = "1234567890abcdef1234567890abcdef12345678"
        base = "8765432109abcdef8765432109abcdef87654321"
        run = {
            "id": run_id,
            "workflow_id": WORKFLOW_ID,
            "head_sha": host,
            "head_branch": branch,
            "event": "workflow_dispatch",
            "status": "in_progress",
            "repository": {"id": REPOSITORY_ID, "full_name": REPOSITORY},
            "head_repository": {"id": REPOSITORY_ID, "full_name": REPOSITORY},
        }
        workflow = {"id": WORKFLOW_ID, "path": WORKFLOW_PATH, "state": "active"}
        jobs = {
            "total_count": 3,
            "jobs": [
                hosted_job("Verify standard runner graph and exact identities", "success", "ubuntu-24.04"),
                hosted_job(ARCHES[architecture]["package_job"], "success", ARCHES[architecture]["runner"]),
                hosted_job(ARCHES[architecture]["consumer_job"], None, ARCHES[architecture]["runner"], status="in_progress"),
            ],
        }
        artifact = {
            "id": 42,
            "name": f"sedna-first-binary-{product}-{architecture}-{run_id}",
            "digest": "sha256:" + "a" * 64,
            "size_in_bytes": 1024,
            "expired": False,
            "workflow_run": {
                "id": run_id,
                "head_sha": host,
                "head_branch": branch,
                "repository_id": REPOSITORY_ID,
                "head_repository_id": REPOSITORY_ID,
            },
        }
        result = verify_build_artifact(
            run,
            workflow,
            jobs,
            {"total_count": 1, "artifacts": [artifact]},
            run_id=run_id,
            workflow_host_sha=host,
            branch=branch,
            product_sha=product,
            base_sha=base,
            architecture=architecture,
        )
        self.assertEqual(result["id"], 42)

    def test_same_run_wrong_runner_is_rejected(self) -> None:
        architecture = "x86_64"
        branch = "branch"
        run_id = 39000000001
        host = "abcdef0123456789abcdef0123456789abcdef01"
        product = "1234567890abcdef1234567890abcdef12345678"
        base = "8765432109abcdef8765432109abcdef87654321"
        run = {
            "id": run_id,
            "workflow_id": WORKFLOW_ID,
            "head_sha": host,
            "head_branch": branch,
            "event": "workflow_dispatch",
            "status": "in_progress",
            "repository": {"id": REPOSITORY_ID, "full_name": REPOSITORY},
            "head_repository": {"id": REPOSITORY_ID, "full_name": REPOSITORY},
        }
        workflow = {"id": WORKFLOW_ID, "path": WORKFLOW_PATH, "state": "active"}
        jobs = {
            "total_count": 3,
            "jobs": [
                hosted_job("Verify standard runner graph and exact identities", "success", "ubuntu-24.04"),
                hosted_job(ARCHES[architecture]["package_job"], "success", "larger-runner"),
                hosted_job(ARCHES[architecture]["consumer_job"], None, ARCHES[architecture]["runner"], status="in_progress"),
            ],
        }
        artifact = {
            "id": 42,
            "name": f"sedna-first-binary-{product}-{architecture}-{run_id}",
            "digest": "sha256:" + "a" * 64,
            "size_in_bytes": 1024,
            "expired": False,
            "workflow_run": {
                "id": run_id,
                "head_sha": host,
                "head_branch": branch,
                "repository_id": REPOSITORY_ID,
                "head_repository_id": REPOSITORY_ID,
            },
        }
        with self.assertRaises(ValueError):
            verify_build_artifact(
                run, workflow, jobs, {"total_count": 1, "artifacts": [artifact]},
                run_id=run_id, workflow_host_sha=host, branch=branch,
                product_sha=product, base_sha=base, architecture=architecture,
            )


class ConsumerResultTests(unittest.TestCase):
    def _write_fixture(
        self,
        root: Path,
        *,
        profile: str,
        fixture_sha: str,
        sdk_sha: str,
    ) -> tuple[Path, Path, Path, Path, Path]:
        runner_temp = root / "runner-temp"
        runner_temp.mkdir()
        witness_dir = runner_temp / "state-history-witnesses"
        witness_dir.mkdir(mode=0o700)
        os.chmod(witness_dir, 0o700)
        q_sha = fixture_sha
        consumer = {
            "schema_version": "sedna-first-binary-consumer-api-v1",
            "repository": REPOSITORY,
            "workflow_path": WORKFLOW_PATH,
            "event": "workflow_dispatch",
            "workflow_host_sha": "7" * 40,
            "run_id": 39000000000,
            "run_attempt": 1,
            "ref": "refs/heads/reconstruct/consumer",
            "branch": "reconstruct/consumer",
            "workflow_ref": f"{REPOSITORY}/{WORKFLOW_PATH}@refs/heads/reconstruct/consumer",
            "product_sha": CROSS_RUN_PRODUCER["product_sha"],
            "comparison_base_sha": CROSS_RUN_PRODUCER["comparison_base_sha"],
            "target": ARCHES["x86_64"]["target"],
            "architecture": "x86_64",
            "runner_label": "ubuntu-24.04",
        }
        producer = {
            "schema_version": "sedna-first-binary-producer-api-v1",
            "repository": REPOSITORY,
            "workflow_path": WORKFLOW_PATH,
            "event": "workflow_dispatch",
            "workflow_host_sha": CROSS_RUN_PRODUCER["workflow_host_sha"],
            "run_id": CROSS_RUN_PRODUCER["run_id"],
            "branch": CROSS_RUN_PRODUCER["branch"],
            "product_sha": CROSS_RUN_PRODUCER["product_sha"],
            "comparison_base_ref": "main",
            "comparison_base_sha": CROSS_RUN_PRODUCER["comparison_base_sha"],
            "artifact_id": CROSS_RUN_ARTIFACTS["x86_64"]["id"],
            "artifact_name": CROSS_RUN_ARTIFACTS["x86_64"]["name"],
            "artifact_digest": CROSS_RUN_ARTIFACTS["x86_64"]["digest"],
            "artifact_size_bytes": CROSS_RUN_ARTIFACTS["x86_64"]["size_in_bytes"],
            "architecture": "x86_64",
            "target": ARCHES["x86_64"]["target"],
            "runner_label": "ubuntu-24.04",
        }
        consumer_path = runner_temp / "consumer.json"
        producer_path = runner_temp / "producer.json"
        junit_path = runner_temp / "results.xml"
        result_path = runner_temp / "result.json"
        consumer_path.write_text(json.dumps(consumer), encoding="utf-8")
        producer_path.write_text(json.dumps(producer), encoding="utf-8")
        selected_plan = consume_existing_test_plan(q_sha, sdk_sha, profile)
        state_cases = sorted(selected_plan["state"])
        plain_cases = sorted(selected_plan["plain"])
        testcase_xml = [
            f'<testcase name="test_packaged_historical_upgrade_and_reopen[{case_name}]"/>'
            for case_name in state_cases
            if case_name in EXPECTED_STATE_POSITIVE
        ]
        testcase_xml.extend(
            f'<testcase name="test_packaged_historical_rejection_preserves_preimage[{case_name}]"/>'
            for case_name in state_cases
            if case_name in EXPECTED_STATE_NEGATIVE
        )
        testcase_xml.extend(f'<testcase name="{name}"/>' for name in plain_cases)
        if (q_sha, sdk_sha) == (W14780_FIXTURE_SHA, W14780_SDK_SHA) and profile == "full":
            testcase_xml.append(
                '<testcase name="test_packaged_weekly_pacing_uses_account_usage_across_sparse_update_and_resume[ratio-case]"/>'
            )
        junit_path.write_text(f"<testsuite>{''.join(testcase_xml)}</testsuite>", encoding="utf-8")
        for case_name in state_cases:
            name = f"{case_name}.json"
            witness = {
                "case": case_name,
                "product_target_sha": producer["product_sha"],
                "comparison_base_sha": producer["comparison_base_sha"],
                "fixture_source_sha": q_sha,
                "producer_workflow_host_sha": producer["workflow_host_sha"],
                "producer_run_id": producer["run_id"],
                "consumer_workflow_host_sha": consumer["workflow_host_sha"],
                "consumer_run_id": consumer["run_id"],
                "consumer_run_attempt": consumer["run_attempt"],
                "target": producer["target"],
                "artifact_id": producer["artifact_id"],
                "artifact_name": producer["artifact_name"],
                "package_archive_sha256": "a" * 64,
                "package_version": "0.160.0-dev.sedna.1",
            }
            (witness_dir / name).write_text(json.dumps(witness), encoding="utf-8")
        self._q_sha = q_sha
        self._sdk_sha = sdk_sha
        return junit_path, consumer_path, producer_path, witness_dir, result_path

    def _write_browser_diagnostic_fixture(self, root: Path, *, failed: bool = False):
        runner_temp = root / "runner-temp"
        runner_temp.mkdir()
        witness_dir = runner_temp / "state-history-witnesses"
        witness_dir.mkdir(mode=0o700)
        os.chmod(witness_dir, 0o700)
        run_id = 39000000000
        host_sha = "7" * 40
        consumer = {
            "schema_version": "sedna-first-binary-consumer-api-v1",
            "repository": REPOSITORY,
            "workflow_path": WORKFLOW_PATH,
            "workflow_host_sha": host_sha,
            "run_id": run_id,
            "ref": f"refs/heads/{TRUSTED_CONSUMER_BRANCH}",
            "branch": TRUSTED_CONSUMER_BRANCH,
            "workflow_ref": f"{REPOSITORY}/{WORKFLOW_PATH}@refs/heads/{TRUSTED_CONSUMER_BRANCH}",
            "product_sha": BROWSER_DIAGNOSTIC_PRODUCT_SHA,
            "comparison_base_sha": BROWSER_DIAGNOSTIC_BASE_SHA,
            "architecture": "x86_64",
            "runner_label": "ubuntu-24.04",
        }
        producer = {
            "schema_version": "sedna-first-binary-producer-api-v1",
            "repository": REPOSITORY,
            "workflow_path": WORKFLOW_PATH,
            "workflow_host_sha": host_sha,
            "run_id": run_id,
            "branch": TRUSTED_CONSUMER_BRANCH,
            "product_sha": BROWSER_DIAGNOSTIC_PRODUCT_SHA,
            "comparison_base_ref": BROWSER_DIAGNOSTIC_BASE_REF,
            "comparison_base_sha": BROWSER_DIAGNOSTIC_BASE_SHA,
            "artifact_id": 42,
            "artifact_name": f"sedna-first-binary-{BROWSER_DIAGNOSTIC_PRODUCT_SHA}-x86_64-{run_id}",
            "artifact_digest": "sha256:" + "a" * 64,
            "artifact_size_bytes": 1024,
            "architecture": "x86_64",
            "target": ARCHES["x86_64"]["target"],
            "runner_label": "ubuntu-24.04",
        }
        diagnostic = {
            "fixture_function_call_emitted": True,
            "responses_request_count": 2,
            "second_request_observed": True,
            "second_request_input_available": True,
            "function_call_output_count": 0,
            "function_call_output_call_ids": [],
            "function_call_output_tool_names": [],
            "synthetic_browser_provider_invoked": False,
            "synthetic_browser_provider_tool_name": None,
            "source_stage_observations": [
                {
                    "stage": "browser_provider",
                    "call_id_matches_fixture": True,
                    "provider_process_exit_success": True,
                    "provider_json_parse_success": True,
                    "provider_content_item_count": 3,
                },
                {
                    "stage": "app_server_response",
                    "call_id_matches_fixture": True,
                    "app_server_response_accepted": True,
                    "app_server_response_submitted": True,
                    "app_server_accepted_item_count": 3,
                },
                {
                    "stage": "core_function_output",
                    "call_id_matches_fixture": True,
                    "core_response_received": True,
                    "core_function_output_constructed": True,
                    "core_output_item_count": 3,
                },
            ],
        }
        self.assertEqual(BROWSER_DIAGNOSTIC_FIELDS, frozenset(diagnostic))
        consumer_path = runner_temp / "consumer.json"
        producer_path = runner_temp / "producer.json"
        junit_path = runner_temp / "results.xml"
        result_path = runner_temp / "result.json"
        consumer_path.write_text(json.dumps(consumer), encoding="utf-8")
        producer_path.write_text(json.dumps(producer), encoding="utf-8")
        suite = ET.Element("testsuite")
        case = ET.SubElement(suite, "testcase", {"name": BROWSER_DIAGNOSTIC_TEST_NAME})
        properties = ET.SubElement(case, "properties")
        ET.SubElement(properties, "property", {
            "name": BROWSER_DIAGNOSTIC_PROPERTY,
            "value": json.dumps(diagnostic, sort_keys=True, separators=(",", ":")),
        })
        if failed:
            ET.SubElement(case, "failure", {"message": "fixture observed missing call output"})
        ET.ElementTree(suite).write(junit_path, encoding="unicode")
        return junit_path, consumer_path, producer_path, witness_dir, result_path

    def test_w14780_full_fixture_reconciles_parameterized_pacing_cases(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                Path(temporary),
                profile="full",
                fixture_sha=W14780_FIXTURE_SHA,
                sdk_sha=W14780_SDK_SHA,
            )
            result = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="full",
                fixture_sha=W14780_FIXTURE_SHA,
                sdk_sha=W14780_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual(45, result["executed_cases"])
            self.assertEqual(0, result["failures"])
            self.assertEqual([], result["issues"])

    def test_browser_diagnostic_profile_is_closed_to_exact_pair_and_route(self) -> None:
        plan = browser_diagnostic_test_plan(
            mode="build",
            profile=BROWSER_DIAGNOSTIC_PROFILE,
            product_sha=BROWSER_DIAGNOSTIC_PRODUCT_SHA,
            base_ref=BROWSER_DIAGNOSTIC_BASE_REF,
            base_sha=BROWSER_DIAGNOSTIC_BASE_SHA,
            fixture_sha=BROWSER_DIAGNOSTIC_FIXTURE_SHA,
            sdk_sha=BROWSER_DIAGNOSTIC_SDK_SHA,
        )
        self.assertEqual({BROWSER_DIAGNOSTIC_TEST_NAME}, set(plan["plain"]))
        self.assertEqual(set(), set(plan["state"]))
        for field, value in (
            ("fixture_sha", "f" * 40),
            ("sdk_sha", "s" * 40),
            ("product_sha", "t" * 40),
            ("base_ref", "main"),
            ("base_sha", "b" * 40),
        ):
            inputs = {
                "mode": "build",
                "profile": BROWSER_DIAGNOSTIC_PROFILE,
                "product_sha": BROWSER_DIAGNOSTIC_PRODUCT_SHA,
                "base_ref": BROWSER_DIAGNOSTIC_BASE_REF,
                "base_sha": BROWSER_DIAGNOSTIC_BASE_SHA,
                "fixture_sha": BROWSER_DIAGNOSTIC_FIXTURE_SHA,
                "sdk_sha": BROWSER_DIAGNOSTIC_SDK_SHA,
            }
            inputs[field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                browser_diagnostic_test_plan(**inputs)

    def test_browser_diagnostic_result_retains_sanitized_signal_on_test_failure(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            junit, consumer, producer, witnesses, result_path = self._write_browser_diagnostic_fixture(
                Path(temporary), failed=True
            )
            result = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=1,
                mode="build",
                profile=BROWSER_DIAGNOSTIC_PROFILE,
                fixture_sha=BROWSER_DIAGNOSTIC_FIXTURE_SHA,
                sdk_sha=BROWSER_DIAGNOSTIC_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual(1, result["executed_cases"])
            self.assertEqual(1, result["failures"])
            self.assertEqual(2, result["browser_output_diagnostic"]["responses_request_count"])
            self.assertEqual([], result["browser_output_diagnostic"]["function_call_output_call_ids"])
            self.assertEqual(
                ["browser_provider", "app_server_response", "core_function_output"],
                [
                    observation["stage"]
                    for observation in result["browser_output_diagnostic"]["source_stage_observations"]
                ],
            )
            self.assertTrue(all(
                observation["call_id_matches_fixture"]
                for observation in result["browser_output_diagnostic"]["source_stage_observations"]
            ))
            self.assertIn("pytest exited with status 1", result["issues"])
            self.assertIn("JUnit reports failures=1 errors=0", result["issues"])

    def test_browser_diagnostic_result_rejects_extra_case_or_unsafe_observation(self) -> None:
        for mutation in ("extra", "raw_call_id"):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                junit, consumer, producer, witnesses, result_path = self._write_browser_diagnostic_fixture(
                    Path(temporary)
                )
                tree = ET.parse(junit)
                root = tree.getroot()
                if mutation == "extra":
                    ET.SubElement(root, "testcase", {"name": "test_unadmitted_case"})
                else:
                    prop = root.find("./testcase/properties/property")
                    diagnostic = json.loads(prop.attrib["value"])
                    diagnostic["function_call_output_count"] = 1
                    diagnostic["function_call_output_call_ids"] = ["unredacted-unexpected-id"]
                    diagnostic["function_call_output_tool_names"] = ["browser_observe"]
                    prop.attrib["value"] = json.dumps(diagnostic, sort_keys=True, separators=(",", ":"))
                tree.write(junit, encoding="unicode")
                result = reconcile_consumer_results(
                    junit_path=junit,
                    consumer_context_path=consumer,
                    producer_evidence_path=producer,
                    witness_dir=witnesses,
                    result_path=result_path,
                    pytest_exit=0,
                    mode="build",
                    profile=BROWSER_DIAGNOSTIC_PROFILE,
                    fixture_sha=BROWSER_DIAGNOSTIC_FIXTURE_SHA,
                    sdk_sha=BROWSER_DIAGNOSTIC_SDK_SHA,
                    runner_temp=witnesses.parent,
                )
                if mutation == "extra":
                    self.assertTrue(any("plain-test inventory differs" in issue for issue in result["issues"]))
                else:
                    self.assertIsNone(result["browser_output_diagnostic"])
                    self.assertTrue(any("call identifiers or tool names are malformed" in issue for issue in result["issues"]))

    def test_exact_pair_result_and_witness_join_are_accepted(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="pair", fixture_sha=Q2_FIXTURE_SHA, sdk_sha=S0_SDK_SHA
            )
            result = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="pair",
                fixture_sha=self._q_sha,
                sdk_sha=self._sdk_sha,
                runner_temp=witnesses.parent,
            )
            self.assertEqual(result["issues"], [])
            self.assertEqual(result["executed_cases"], 2)
            self.assertEqual(result["state_history_witnesses"], 2)

    def test_q3_s1_full_result_accepts_exact_inventory_and_all_witnesses(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="full", fixture_sha=Q3_FIXTURE_SHA, sdk_sha=S1_SDK_SHA
            )
            result = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="full",
                fixture_sha=Q3_FIXTURE_SHA,
                sdk_sha=S1_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual([], result["issues"])
            self.assertEqual(41, result["executed_cases"])
            self.assertEqual(24, result["state_history_witnesses"])

    def test_q4_s2_pair_and_full_results_accept_exact_inventory(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="pair", fixture_sha=Q4_FIXTURE_SHA, sdk_sha=S2_SDK_SHA
            )
            pair = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="pair",
                fixture_sha=Q4_FIXTURE_SHA,
                sdk_sha=S2_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual([], pair["issues"])
            self.assertEqual(2, pair["executed_cases"])
            self.assertEqual(2, pair["state_history_witnesses"])

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="full", fixture_sha=Q4_FIXTURE_SHA, sdk_sha=S2_SDK_SHA
            )
            full = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="full",
                fixture_sha=Q4_FIXTURE_SHA,
                sdk_sha=S2_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual([], full["issues"])
            self.assertEqual(41, full["executed_cases"])
            self.assertEqual(24, full["state_history_witnesses"])

    def test_q5_s2_focused_result_accepts_only_exact_repair_cases(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="focused", fixture_sha=Q5_FIXTURE_SHA, sdk_sha=S2_SDK_SHA
            )
            focused = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="focused",
                fixture_sha=Q5_FIXTURE_SHA,
                sdk_sha=S2_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual([], focused["issues"])
            self.assertEqual(6, focused["executed_cases"])
            self.assertEqual(0, focused["state_history_witnesses"])

    def test_q5_s2_focused_result_rejects_missing_extra_skipped_and_wrong_generation(self) -> None:
        for mutation in ("missing", "extra", "skipped", "wrong_generation"):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                junit, consumer, producer, witnesses, result_path = self._write_fixture(
                    root, profile="focused", fixture_sha=Q5_FIXTURE_SHA, sdk_sha=S2_SDK_SHA
                )
                if mutation == "missing":
                    test_name = sorted(FOCUSED_REPAIR_PLAIN_TESTS)[0]
                    xml = junit.read_text(encoding="utf-8")
                    junit.write_text(xml.replace(f'<testcase name="{test_name}"/>', "", 1), encoding="utf-8")
                elif mutation == "extra":
                    xml = junit.read_text(encoding="utf-8").replace(
                        "</testsuite>", '<testcase name="test_unadmitted_focused_case"/></testsuite>'
                    )
                    junit.write_text(xml, encoding="utf-8")
                elif mutation == "skipped":
                    xml = junit.read_text(encoding="utf-8").replace(
                        "/>", "><skipped/></testcase>", 1
                    )
                    junit.write_text(xml, encoding="utf-8")
                selected_sdk_sha = S1_SDK_SHA if mutation == "wrong_generation" else S2_SDK_SHA
                result = reconcile_consumer_results(
                    junit_path=junit,
                    consumer_context_path=consumer,
                    producer_evidence_path=producer,
                    witness_dir=witnesses,
                    result_path=result_path,
                    pytest_exit=0,
                    mode="consume-existing",
                    profile="focused",
                    fixture_sha=Q5_FIXTURE_SHA,
                    sdk_sha=selected_sdk_sha,
                    runner_temp=witnesses.parent,
                )
                if mutation == "skipped":
                    self.assertTrue(any("skipped" in issue for issue in result["issues"]))
                elif mutation == "wrong_generation":
                    self.assertTrue(any("no exact consume-existing test inventory" in issue for issue in result["issues"]))
                else:
                    self.assertTrue(any("plain-test inventory differs" in issue for issue in result["issues"]))

    def test_q11_s3_focused_result_accepts_exact_six_cases_without_witnesses(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="focused", fixture_sha=Q11_FIXTURE_SHA, sdk_sha=S3_SDK_SHA
            )
            focused = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="focused",
                fixture_sha=Q11_FIXTURE_SHA,
                sdk_sha=S3_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual([], focused["issues"])
            self.assertEqual(6, focused["executed_cases"])
            self.assertEqual(0, focused["state_history_witnesses"])

    def test_q11_s3_focused_result_rejects_missing_extra_skipped_and_wrong_generation(self) -> None:
        for mutation in ("missing", "extra", "skipped", "wrong_generation", "pytest_failure"):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                junit, consumer, producer, witnesses, result_path = self._write_fixture(
                    root, profile="focused", fixture_sha=Q11_FIXTURE_SHA, sdk_sha=S3_SDK_SHA
                )
                if mutation == "missing":
                    test_name = sorted(FOCUSED_REPAIR_PLAIN_TESTS)[0]
                    xml = junit.read_text(encoding="utf-8")
                    junit.write_text(xml.replace(f'<testcase name="{test_name}"/>', "", 1), encoding="utf-8")
                elif mutation == "extra":
                    xml = junit.read_text(encoding="utf-8").replace(
                        "</testsuite>", '<testcase name="test_unadmitted_focused_case"/></testsuite>'
                    )
                    junit.write_text(xml, encoding="utf-8")
                elif mutation == "skipped":
                    xml = junit.read_text(encoding="utf-8").replace(
                        "/>", "><skipped/></testcase>", 1
                    )
                    junit.write_text(xml, encoding="utf-8")
                selected_sdk_sha = S2_SDK_SHA if mutation == "wrong_generation" else S3_SDK_SHA
                result = reconcile_consumer_results(
                    junit_path=junit,
                    consumer_context_path=consumer,
                    producer_evidence_path=producer,
                    witness_dir=witnesses,
                    result_path=result_path,
                    pytest_exit=1 if mutation == "pytest_failure" else 0,
                    mode="consume-existing",
                    profile="focused",
                    fixture_sha=Q11_FIXTURE_SHA,
                    sdk_sha=selected_sdk_sha,
                    runner_temp=witnesses.parent,
                )
                if mutation == "skipped":
                    self.assertTrue(any("skipped" in issue for issue in result["issues"]))
                elif mutation == "pytest_failure":
                    self.assertIn("pytest exited with status 1", result["issues"])
                elif mutation == "wrong_generation":
                    self.assertTrue(any("no exact consume-existing test inventory" in issue for issue in result["issues"]))
                else:
                    self.assertTrue(any("plain-test inventory differs" in issue for issue in result["issues"]))

    def test_q13_s4_focused_result_accepts_exact_six_cases_without_witnesses(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="focused", fixture_sha=Q13_FIXTURE_SHA, sdk_sha=S4_SDK_SHA
            )
            focused = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="focused",
                fixture_sha=Q13_FIXTURE_SHA,
                sdk_sha=S4_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual([], focused["issues"])
            self.assertEqual(6, focused["executed_cases"])
            self.assertEqual(0, focused["state_history_witnesses"])

    def test_q13_s4_focused_result_rejects_missing_extra_skipped_and_wrong_generation(self) -> None:
        for mutation in ("missing", "extra", "skipped", "wrong_generation", "pytest_failure"):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                junit, consumer, producer, witnesses, result_path = self._write_fixture(
                    root, profile="focused", fixture_sha=Q13_FIXTURE_SHA, sdk_sha=S4_SDK_SHA
                )
                if mutation == "missing":
                    test_name = sorted(FOCUSED_REPAIR_PLAIN_TESTS)[0]
                    xml = junit.read_text(encoding="utf-8")
                    junit.write_text(xml.replace(f'<testcase name="{test_name}"/>', "", 1), encoding="utf-8")
                elif mutation == "extra":
                    xml = junit.read_text(encoding="utf-8").replace(
                        "</testsuite>", '<testcase name="test_unadmitted_focused_case"/></testsuite>'
                    )
                    junit.write_text(xml, encoding="utf-8")
                elif mutation == "skipped":
                    xml = junit.read_text(encoding="utf-8").replace(
                        "/>", "><skipped/></testcase>", 1
                    )
                    junit.write_text(xml, encoding="utf-8")
                selected_sdk_sha = S3_SDK_SHA if mutation == "wrong_generation" else S4_SDK_SHA
                result = reconcile_consumer_results(
                    junit_path=junit,
                    consumer_context_path=consumer,
                    producer_evidence_path=producer,
                    witness_dir=witnesses,
                    result_path=result_path,
                    pytest_exit=1 if mutation == "pytest_failure" else 0,
                    mode="consume-existing",
                    profile="focused",
                    fixture_sha=Q13_FIXTURE_SHA,
                    sdk_sha=selected_sdk_sha,
                    runner_temp=witnesses.parent,
                )
                if mutation == "skipped":
                    self.assertTrue(any("skipped" in issue for issue in result["issues"]))
                elif mutation == "pytest_failure":
                    self.assertIn("pytest exited with status 1", result["issues"])
                elif mutation == "wrong_generation":
                    self.assertTrue(any("no exact consume-existing test inventory" in issue for issue in result["issues"]))
                else:
                    self.assertTrue(any("plain-test inventory differs" in issue for issue in result["issues"]))

    def test_q14_s4_focused_result_requires_exact_six_cases_and_no_witnesses(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="focused", fixture_sha=Q14_FIXTURE_SHA, sdk_sha=S4_SDK_SHA
            )
            accepted = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="focused",
                fixture_sha=Q14_FIXTURE_SHA,
                sdk_sha=S4_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual([], accepted["issues"])
            self.assertEqual(6, accepted["executed_cases"])
            self.assertEqual(0, accepted["state_history_witnesses"])

            rejected = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="focused",
                fixture_sha=Q14_FIXTURE_SHA,
                sdk_sha=S3_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertTrue(
                any("no exact consume-existing test inventory" in issue for issue in rejected["issues"])
            )

    def test_q15_s4_focused_result_requires_exact_six_cases_and_no_witnesses(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="focused", fixture_sha=Q15_FIXTURE_SHA, sdk_sha=S4_SDK_SHA
            )
            accepted = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="focused",
                fixture_sha=Q15_FIXTURE_SHA,
                sdk_sha=S4_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual([], accepted["issues"])
            self.assertEqual(6, accepted["executed_cases"])
            self.assertEqual(0, accepted["state_history_witnesses"])

            rejected = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="focused",
                fixture_sha=Q15_FIXTURE_SHA,
                sdk_sha=S3_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertTrue(
                any("no exact consume-existing test inventory" in issue for issue in rejected["issues"])
            )

    def test_q17_s4_focused_result_requires_exact_six_cases_and_no_witnesses(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="focused", fixture_sha=Q17_FIXTURE_SHA, sdk_sha=S4_SDK_SHA
            )
            accepted = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="focused",
                fixture_sha=Q17_FIXTURE_SHA,
                sdk_sha=S4_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual([], accepted["issues"])
            self.assertEqual(6, accepted["executed_cases"])
            self.assertEqual(0, accepted["state_history_witnesses"])

            rejected = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="focused",
                fixture_sha=Q17_FIXTURE_SHA,
                sdk_sha=S3_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertTrue(
                any("no exact consume-existing test inventory" in issue for issue in rejected["issues"])
            )

    def test_q18_s4_focused_result_requires_exact_six_cases_and_no_witnesses(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="focused", fixture_sha=Q18_FIXTURE_SHA, sdk_sha=S4_SDK_SHA
            )
            accepted = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="focused",
                fixture_sha=Q18_FIXTURE_SHA,
                sdk_sha=S4_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual([], accepted["issues"])
            self.assertEqual(6, accepted["executed_cases"])
            self.assertEqual(0, accepted["state_history_witnesses"])

            rejected = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="focused",
                fixture_sha=Q18_FIXTURE_SHA,
                sdk_sha=S3_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertTrue(
                any("no exact consume-existing test inventory" in issue for issue in rejected["issues"])
            )

    def test_q20_s4_focused_result_requires_exact_six_cases_and_no_witnesses(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="focused", fixture_sha=Q20_FIXTURE_SHA, sdk_sha=S4_SDK_SHA
            )
            accepted = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="focused",
                fixture_sha=Q20_FIXTURE_SHA,
                sdk_sha=S4_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual([], accepted["issues"])
            self.assertEqual(6, accepted["executed_cases"])
            self.assertEqual(0, accepted["state_history_witnesses"])

            rejected = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="focused",
                fixture_sha=Q20_FIXTURE_SHA,
                sdk_sha=S3_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertTrue(
                any("no exact consume-existing test inventory" in issue for issue in rejected["issues"])
            )

    def test_q22_s4_focused_result_requires_exact_six_cases_without_witnesses(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="focused", fixture_sha=Q22_FIXTURE_SHA, sdk_sha=S4_SDK_SHA
            )
            accepted = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="focused",
                fixture_sha=Q22_FIXTURE_SHA,
                sdk_sha=S4_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual([], accepted["issues"])
            self.assertEqual(6, accepted["executed_cases"])
            self.assertEqual(0, accepted["state_history_witnesses"])

    def test_q22_s4_full_result_requires_exact_inventory_and_all_witnesses(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="full", fixture_sha=Q22_FIXTURE_SHA, sdk_sha=S4_SDK_SHA
            )
            accepted = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="full",
                fixture_sha=Q22_FIXTURE_SHA,
                sdk_sha=S4_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual([], accepted["issues"])
            self.assertEqual(41, accepted["executed_cases"])
            self.assertEqual(24, accepted["state_history_witnesses"])

    def test_q24_s4_focused_result_requires_exact_six_cases_without_witnesses(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="focused", fixture_sha=Q24_FIXTURE_SHA, sdk_sha=S4_SDK_SHA
            )
            accepted = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="focused",
                fixture_sha=Q24_FIXTURE_SHA,
                sdk_sha=S4_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual([], accepted["issues"])
            self.assertEqual(6, accepted["executed_cases"])
            self.assertEqual(0, accepted["state_history_witnesses"])

    def test_q24_s4_full_result_requires_exact_inventory_and_all_witnesses(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="full", fixture_sha=Q24_FIXTURE_SHA, sdk_sha=S4_SDK_SHA
            )
            accepted = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="full",
                fixture_sha=Q24_FIXTURE_SHA,
                sdk_sha=S4_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual([], accepted["issues"])
            self.assertEqual(41, accepted["executed_cases"])
            self.assertEqual(24, accepted["state_history_witnesses"])

    def test_q63_s4_focused_result_requires_exact_six_cases_without_witnesses(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="focused", fixture_sha=Q63_FIXTURE_SHA, sdk_sha=S4_SDK_SHA
            )
            accepted = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="focused",
                fixture_sha=Q63_FIXTURE_SHA,
                sdk_sha=S4_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual([], accepted["issues"])
            self.assertEqual(6, accepted["executed_cases"])
            self.assertEqual(0, accepted["state_history_witnesses"])

    def test_q63_s4_full_result_requires_exact_inventory_and_all_witnesses(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="full", fixture_sha=Q63_FIXTURE_SHA, sdk_sha=S4_SDK_SHA
            )
            accepted = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="full",
                fixture_sha=Q63_FIXTURE_SHA,
                sdk_sha=S4_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual([], accepted["issues"])
            self.assertEqual(41, accepted["executed_cases"])
            self.assertEqual(24, accepted["state_history_witnesses"])

    def test_q64_s4_focused_result_requires_exact_six_cases_without_witnesses(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="focused", fixture_sha=Q64_FIXTURE_SHA, sdk_sha=S4_SDK_SHA
            )
            accepted = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="focused",
                fixture_sha=Q64_FIXTURE_SHA,
                sdk_sha=S4_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual([], accepted["issues"])
            self.assertEqual(6, accepted["executed_cases"])
            self.assertEqual(0, accepted["state_history_witnesses"])

    def test_q64_s4_full_result_requires_exact_inventory_and_all_witnesses(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="full", fixture_sha=Q64_FIXTURE_SHA, sdk_sha=S4_SDK_SHA
            )
            accepted = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="full",
                fixture_sha=Q64_FIXTURE_SHA,
                sdk_sha=S4_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual([], accepted["issues"])
            self.assertEqual(41, accepted["executed_cases"])
            self.assertEqual(24, accepted["state_history_witnesses"])

    def test_q66_s4_focused_result_requires_exact_six_cases_without_witnesses(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="focused", fixture_sha=Q66_FIXTURE_SHA, sdk_sha=S4_SDK_SHA
            )
            accepted = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="focused",
                fixture_sha=Q66_FIXTURE_SHA,
                sdk_sha=S4_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual([], accepted["issues"])
            self.assertEqual(6, accepted["executed_cases"])
            self.assertEqual(0, accepted["state_history_witnesses"])

    def test_q66_s4_full_result_requires_exact_inventory_and_all_witnesses(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="full", fixture_sha=Q66_FIXTURE_SHA, sdk_sha=S4_SDK_SHA
            )
            accepted = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="full",
                fixture_sha=Q66_FIXTURE_SHA,
                sdk_sha=S4_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual([], accepted["issues"])
            self.assertEqual(41, accepted["executed_cases"])
            self.assertEqual(24, accepted["state_history_witnesses"])

    def test_q67_s4_focused_result_requires_exact_six_cases_without_witnesses(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="focused", fixture_sha=Q67_FIXTURE_SHA, sdk_sha=S4_SDK_SHA
            )
            accepted = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="focused",
                fixture_sha=Q67_FIXTURE_SHA,
                sdk_sha=S4_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual([], accepted["issues"])
            self.assertEqual(6, accepted["executed_cases"])
            self.assertEqual(0, accepted["state_history_witnesses"])

    def test_q67_s4_full_result_requires_exact_inventory_and_all_witnesses(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="full", fixture_sha=Q67_FIXTURE_SHA, sdk_sha=S4_SDK_SHA
            )
            accepted = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="full",
                fixture_sha=Q67_FIXTURE_SHA,
                sdk_sha=S4_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual([], accepted["issues"])
            self.assertEqual(41, accepted["executed_cases"])
            self.assertEqual(24, accepted["state_history_witnesses"])

    def test_q68_s4_focused_result_requires_exact_six_cases_without_witnesses(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="focused", fixture_sha=Q68_FIXTURE_SHA, sdk_sha=S4_SDK_SHA
            )
            accepted = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="focused",
                fixture_sha=Q68_FIXTURE_SHA,
                sdk_sha=S4_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual([], accepted["issues"])
            self.assertEqual(6, accepted["executed_cases"])
            self.assertEqual(0, accepted["state_history_witnesses"])

    def test_q68_s4_full_result_requires_exact_inventory_and_all_witnesses(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="full", fixture_sha=Q68_FIXTURE_SHA, sdk_sha=S4_SDK_SHA
            )
            accepted = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="full",
                fixture_sha=Q68_FIXTURE_SHA,
                sdk_sha=S4_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual([], accepted["issues"])
            self.assertEqual(41, accepted["executed_cases"])
            self.assertEqual(24, accepted["state_history_witnesses"])

    def test_q69_s4_focused_result_requires_exact_six_cases_without_witnesses(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="focused", fixture_sha=Q69_FIXTURE_SHA, sdk_sha=S4_SDK_SHA
            )
            accepted = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="focused",
                fixture_sha=Q69_FIXTURE_SHA,
                sdk_sha=S4_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual([], accepted["issues"])
            self.assertEqual(6, accepted["executed_cases"])
            self.assertEqual(0, accepted["state_history_witnesses"])

    def test_q69_s4_full_result_requires_exact_inventory_and_all_witnesses(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="full", fixture_sha=Q69_FIXTURE_SHA, sdk_sha=S4_SDK_SHA
            )
            accepted = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="full",
                fixture_sha=Q69_FIXTURE_SHA,
                sdk_sha=S4_SDK_SHA,
                runner_temp=witnesses.parent,
            )
            self.assertEqual([], accepted["issues"])
            self.assertEqual(41, accepted["executed_cases"])
            self.assertEqual(24, accepted["state_history_witnesses"])

    def test_q3_s1_full_result_rejects_missing_extra_skipped_and_wrong_generation(self) -> None:
        for mutation in ("missing", "extra", "skipped", "wrong_generation"):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                junit, consumer, producer, witnesses, result_path = self._write_fixture(
                    root, profile="full", fixture_sha=Q3_FIXTURE_SHA, sdk_sha=S1_SDK_SHA
                )
                if mutation == "missing":
                    test_name = sorted(Q3_ADDITIONAL_PLAIN_TESTS)[0]
                    xml = junit.read_text(encoding="utf-8")
                    junit.write_text(xml.replace(f'<testcase name="{test_name}"/>', "", 1), encoding="utf-8")
                elif mutation == "extra":
                    xml = junit.read_text(encoding="utf-8").replace(
                        "</testsuite>", '<testcase name="test_unadmitted_fixture_generation_case"/></testsuite>'
                    )
                    junit.write_text(xml, encoding="utf-8")
                elif mutation == "skipped":
                    xml = junit.read_text(encoding="utf-8")
                    junit.write_text(xml.replace("/>", "><skipped/></testcase>", 1), encoding="utf-8")

                selected_sdk_sha = S0_SDK_SHA if mutation == "wrong_generation" else S1_SDK_SHA
                result = reconcile_consumer_results(
                    junit_path=junit,
                    consumer_context_path=consumer,
                    producer_evidence_path=producer,
                    witness_dir=witnesses,
                    result_path=result_path,
                    pytest_exit=0,
                    mode="consume-existing",
                    profile="full",
                    fixture_sha=Q3_FIXTURE_SHA,
                    sdk_sha=selected_sdk_sha,
                    runner_temp=witnesses.parent,
                )
                if mutation == "skipped":
                    self.assertTrue(any("skipped" in issue for issue in result["issues"]))
                elif mutation == "wrong_generation":
                    self.assertTrue(any("no exact consume-existing test inventory" in issue for issue in result["issues"]))
                else:
                    self.assertTrue(any("plain-test inventory differs" in issue for issue in result["issues"]))

    def test_skips_or_wrong_witness_identity_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_fixture(
                root, profile="pair", fixture_sha=Q2_FIXTURE_SHA, sdk_sha=S0_SDK_SHA
            )
            with (witnesses / "fresh.json").open(encoding="utf-8") as stream:
                changed = json.load(stream)
            changed["producer_run_id"] += 1
            (witnesses / "fresh.json").write_text(json.dumps(changed), encoding="utf-8")
            junit.write_text(
                "<testsuite><testcase name='test_packaged_historical_upgrade_and_reopen[fresh]'><skipped/></testcase>"
                "<testcase name='test_packaged_historical_rejection_preserves_preimage[bad_checksum]'/></testsuite>",
                encoding="utf-8",
            )
            result = reconcile_consumer_results(
                junit_path=junit,
                consumer_context_path=consumer,
                producer_evidence_path=producer,
                witness_dir=witnesses,
                result_path=result_path,
                pytest_exit=0,
                mode="consume-existing",
                profile="pair",
                fixture_sha=self._q_sha,
                sdk_sha=self._sdk_sha,
                runner_temp=witnesses.parent,
            )
            self.assertTrue(any("skipped" in issue for issue in result["issues"]))
            self.assertTrue(any("identity mismatch" in issue for issue in result["issues"]))


if __name__ == "__main__":
    unittest.main()
