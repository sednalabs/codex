"""Run historical state fixtures through the downloaded package, not a build tree.

The existing package smoke workflow is unaffected unless the dedicated hosted
qualification sets CODEX_STATE_HISTORY_ACCEPTANCE=1. That run must retain
nonzero JUnit case counts and upload the generated witness JSON files.
"""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
from dataclasses import asdict
from pathlib import Path

import pytest
from app_server_harness import MockResponsesServer
from artifact import FirstBinaryEvidence
from fixtures import SmokePackage
from package_acceptance import open_and_reopen
from package_acceptance import start_once_expect_failure

from .acceptance import StateWitness, assert_idempotent_reopen, assert_positive_state
from .acceptance import assert_rejected_without_mutation
from .fixture import NEGATIVE_CASES, POSITIVE_CASES, prepare_case
from .sources import HISTORICAL_FORK_SHA, UPSTREAM_SHA, UPSTREAM_TREE

pytestmark = pytest.mark.skipif(
    os.environ.get("CODEX_STATE_HISTORY_ACCEPTANCE") != "1",
    reason="dedicated exact-artifact historical-state qualification only",
)
SOURCE_ROOT = Path(__file__).resolve().parents[5]


@pytest.fixture(scope="session")
def state_history_provenance(
    artifact_evidence: FirstBinaryEvidence,
) -> FirstBinaryEvidence:
    head = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=SOURCE_ROOT,
        capture_output=True,
        text=True,
        check=True,
        timeout=10,
    ).stdout.strip()
    assert head == artifact_evidence.target_sha, "fixture source is not the package target SHA"
    return artifact_evidence


def _write_witness(
    package: SmokePackage,
    case_name: str,
    provenance: FirstBinaryEvidence,
    payload: dict[str, object],
) -> None:
    destination = package.directory / "state-history-witnesses"
    destination.mkdir(exist_ok=True)
    body = {
        "case": case_name,
        "target_sha": provenance.target_sha,
        "base_sha": provenance.base_sha,
        "upstream_sha": UPSTREAM_SHA,
        "upstream_tree": UPSTREAM_TREE,
        "workflow_host_sha": provenance.workflow_host_sha,
        "run_id": provenance.run_id,
        "target": provenance.target,
        "artifact_id": provenance.artifact_id,
        "artifact_name": provenance.artifact_name,
        "package_archive_sha256": provenance.digests[provenance.archive.name],
        "package_version": provenance.version,
        "historical_fork_sha": HISTORICAL_FORK_SHA,
        **payload,
    }
    (destination / f"{case_name}.json").write_text(
        json.dumps(body, sort_keys=True, indent=2) + "\n", encoding="utf-8"
    )


@pytest.mark.parametrize("case_name", POSITIVE_CASES)
def test_packaged_historical_upgrade_and_reopen(
    package: SmokePackage,
    responses_server: MockResponsesServer,
    state_history_provenance: FirstBinaryEvidence,
    case_name: str,
) -> None:
    home = package.directory / f"state-history-{case_name}"
    fixture = prepare_case(SOURCE_ROOT, home, case_name)
    first: StateWitness | None = None

    def capture_first_closed_process() -> None:
        nonlocal first
        first = assert_positive_state(fixture, "first_open")

    receipt = open_and_reopen(
        package, home, responses_server, on_first_close=capture_first_closed_process
    )
    assert first is not None, "first packaged process did not reach the witness hook"
    second = assert_positive_state(fixture, "second_open")
    assert_idempotent_reopen(first, second)
    assert receipt["first_thread_id"] == receipt["second_thread_id"]
    assert receipt["first_user_agent"] and receipt["second_user_agent"]
    assert Path(receipt["packaged_rg_path"]).is_relative_to(package.cli_root)
    assert first.canonical_ledger_rows >= 65
    assert first.fork_effects == 7
    if case_name in {"f56", "f57", "f58", "f_full"} or case_name.startswith("shift"):
        assert fixture.known_bad_collision_detected, "collision control was not exercised"
    public_receipt = {
        "first_thread_id": receipt["first_thread_id"],
        "second_thread_id": receipt["second_thread_id"],
        "first_user_agent_sha256": hashlib.sha256(
            receipt["first_user_agent"].encode("utf-8")
        ).hexdigest(),
        "second_user_agent_sha256": hashlib.sha256(
            receipt["second_user_agent"].encode("utf-8")
        ).hexdigest(),
    }
    public_receipt["packaged_rg_relative_path"] = str(
        Path(receipt["packaged_rg_path"]).relative_to(package.cli_root)
    )
    _write_witness(
        package,
        case_name,
        state_history_provenance,
        {
            "kind": "positive",
            "historical_rows": fixture.historical_rows,
            "seed_thread_rows": fixture.seed_thread_rows,
            "seed_phase2_baseline_rows": fixture.seed_phase2_baseline_rows,
            "seed_phase2_root_rows": fixture.seed_phase2_root_rows,
            "marked_alias_rows": fixture.marked_alias_rows,
            "known_bad_collision_detected": fixture.known_bad_collision_detected,
            "known_bad_collision_rows": fixture.known_bad_collision_rows,
            "first": {
                key: value
                for key, value in asdict(first).items()
                if key not in {"ledger_identity", "receipt_identity"}
            },
            "second": {
                key: value
                for key, value in asdict(second).items()
                if key not in {"ledger_identity", "receipt_identity"}
            },
            "package_consumer": public_receipt,
            "executed_packaged_starts": 2,
            "nonzero_state_witnesses": 2,
        },
    )


EXPECTED_MIGRATION_FAILURE = {
    "bad_checksum": "unknown identity",
    "mixed_ids": "mixed upstream",
    "missing_middle": "gap at 27",
    "failed_row": "failed; refusing",
    "incomplete_f58": "fork migration 58 is recorded",
    "partial_alias": "alias pair is incomplete",
    "partial_upstream_schema": "creator identity columns disagree",
    "unknown_id": "unknown identity",
}


@pytest.mark.parametrize("case_name", NEGATIVE_CASES)
def test_packaged_historical_rejection_preserves_preimage(
    package: SmokePackage,
    responses_server: MockResponsesServer,
    state_history_provenance: FirstBinaryEvidence,
    case_name: str,
) -> None:
    home = package.directory / f"state-history-{case_name}"
    fixture = prepare_case(SOURCE_ROOT, home, case_name)
    assert fixture.pre_start_semantic_sha256 is not None
    failure = start_once_expect_failure(package, home, responses_server)
    assert failure["executable"] == str(package.cli)
    error = str(failure["error"]).lower()
    assert "timeout" not in error, "timeout is not a migration rejection"
    expected = EXPECTED_MIGRATION_FAILURE[case_name]
    assert expected in error, (
        f"{case_name}: expected migration rejection {expected!r}; "
        f"observed error type {failure['error_type']}"
    )
    preserved = assert_rejected_without_mutation(fixture)
    _write_witness(
        package,
        case_name,
        state_history_provenance,
        {
            "kind": "negative",
            "historical_rows": fixture.historical_rows,
            "expected_rejection": expected,
            "observed_error_type": failure["error_type"],
            "observed_error_sha256": hashlib.sha256(
                str(failure["error"]).encode("utf-8")
            ).hexdigest(),
            "semantic_preimage_sha256": fixture.pre_start_semantic_sha256,
            "semantic_after_sha256": preserved,
            "executed_packaged_starts": 1,
            "nonzero_rejection_witnesses": 1,
        },
    )
