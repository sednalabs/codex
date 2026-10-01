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

from verify_existing_first_binary_producer import (
    ARCHES,
    CROSS_RUN_ARTIFACTS,
    CROSS_RUN_PRODUCER,
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


def producer_fixtures() -> tuple[dict[str, object], dict[str, object], dict[str, object], dict[str, object]]:
    run = {
        "id": CROSS_RUN_PRODUCER["run_id"],
        "workflow_id": WORKFLOW_ID,
        "run_attempt": CROSS_RUN_PRODUCER["run_attempt"],
        "event": CROSS_RUN_PRODUCER["event"],
        "head_sha": CROSS_RUN_PRODUCER["workflow_host_sha"],
        "head_branch": CROSS_RUN_PRODUCER["branch"],
        "status": "completed",
        "conclusion": "failure",
        "repository": {"id": REPOSITORY_ID, "full_name": REPOSITORY},
        "head_repository": {"id": REPOSITORY_ID, "full_name": REPOSITORY},
    }
    workflow = {"id": WORKFLOW_ID, "path": WORKFLOW_PATH, "state": "active"}
    jobs = {
        "total_count": len(PRODUCER_JOB_CONTRACT),
        "jobs": [],
    }
    for name, (conclusion, runner, ran) in PRODUCER_JOB_CONTRACT.items():
        jobs["jobs"].append(hosted_job(name, conclusion, runner, ran=ran))
    artifacts = {"total_count": len(CROSS_RUN_ARTIFACTS), "artifacts": []}
    for arch, expected in CROSS_RUN_ARTIFACTS.items():
        artifacts["artifacts"].append(
            {
                "id": expected["id"],
                "name": expected["name"],
                "digest": expected["digest"],
                "size_in_bytes": expected["size_in_bytes"],
                "expired": False,
                "workflow_run": {
                    "id": CROSS_RUN_PRODUCER["run_id"],
                    "head_sha": CROSS_RUN_PRODUCER["workflow_host_sha"],
                    "head_branch": CROSS_RUN_PRODUCER["branch"],
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
) -> dict[str, object]:
    return verify_existing_producer(
        run,
        workflow,
        jobs,
        artifacts,
        producer_run_id=CROSS_RUN_PRODUCER["run_id"],
        producer_workflow_host_sha=CROSS_RUN_PRODUCER["workflow_host_sha"],
        product_sha=CROSS_RUN_PRODUCER["product_sha"],
        base_ref=CROSS_RUN_PRODUCER["comparison_base_ref"],
        base_sha=CROSS_RUN_PRODUCER["comparison_base_sha"],
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


class VerifyExistingProducerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.run, self.workflow, self.jobs, self.artifacts = producer_fixtures()

    def test_exact_native_pair_is_accepted(self) -> None:
        selected = verify_fixture(self.run, self.workflow, self.jobs, self.artifacts)
        self.assertEqual(set(selected), {"x86_64", "aarch64"})
        self.assertEqual(selected["x86_64"]["id"], 11137590827)
        self.assertEqual(selected["aarch64"]["id"], 11135444900)

    def test_wrong_run_host_or_product_identity_is_rejected(self) -> None:
        for kwargs in (
            {"producer_run_id": 36800811942},
            {"producer_workflow_host_sha": "1111111111111111111111111111111111111111"},
            {"product_sha": "2222222222222222222222222222222222222222"},
            {"base_sha": "3333333333333333333333333333333333333333"},
        ):
            with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                expected = dict(
                    producer_run_id=CROSS_RUN_PRODUCER["run_id"],
                    producer_workflow_host_sha=CROSS_RUN_PRODUCER["workflow_host_sha"],
                    product_sha=CROSS_RUN_PRODUCER["product_sha"],
                    base_ref=CROSS_RUN_PRODUCER["comparison_base_ref"],
                    base_sha=CROSS_RUN_PRODUCER["comparison_base_sha"],
                )
                expected.update(kwargs)
                verify_existing_producer(
                    self.run, self.workflow, self.jobs, self.artifacts, **expected
                )

    def test_workflow_host_identity_and_run_event_are_exact(self) -> None:
        for field, value in (
            ("workflow_id", WORKFLOW_ID + 1),
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
        extra["name"] = f"sedna-first-binary-{CROSS_RUN_PRODUCER['product_sha']}-unknown-36800811941"
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
    def _write_pair_fixture(self, root: Path) -> tuple[Path, Path, Path, Path, Path]:
        runner_temp = root / "runner-temp"
        runner_temp.mkdir()
        witness_dir = runner_temp / "state-history-witnesses"
        witness_dir.mkdir(mode=0o700)
        os.chmod(witness_dir, 0o700)
        q_sha = "9" * 40
        sdk_sha = "8" * 40
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
        junit_path.write_text(
            "<testsuite><testcase name='test_packaged_historical_upgrade_and_reopen[fresh]'/>"
            "<testcase name='test_packaged_historical_rejection_preserves_preimage[bad_checksum]'/></testsuite>",
            encoding="utf-8",
        )
        for name, case_name in (("fresh.json", "fresh"), ("bad_checksum.json", "bad_checksum")):
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

    def test_exact_pair_result_and_witness_join_are_accepted(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_pair_fixture(root)
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

    def test_skips_or_wrong_witness_identity_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            junit, consumer, producer, witnesses, result_path = self._write_pair_fixture(root)
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
