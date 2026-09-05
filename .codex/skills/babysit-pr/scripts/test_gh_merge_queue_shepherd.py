import importlib.util
import json
from pathlib import Path

import pytest


MODULE_PATH = Path(__file__).with_name("gh_merge_queue_shepherd.py")
MODULE_SPEC = importlib.util.spec_from_file_location("gh_merge_queue_shepherd", MODULE_PATH)
queue = importlib.util.module_from_spec(MODULE_SPEC)
assert MODULE_SPEC.loader is not None
MODULE_SPEC.loader.exec_module(queue)


def sample_pr(**overrides):
    result = {
        "repository": "sednalabs/codex",
        "number": 750,
        "user": {"login": "branch-owner"},
        "headRefOid": "head-750",
        "baseRefOid": "base-main",
        "baseRefName": "main",
    }
    result.update(overrides)
    return result


def sample_queue(**overrides):
    result = {
        "id": "queue-entry-750",
        "state": "AWAITING_CHECKS",
        "position": 1,
        "headSha": "merge-group-750",
        "baseSha": "base-main",
        "baseRefName": "main",
        "pullRequests": [
            {
                "number": 750,
                "author": {"login": "branch-owner"},
                "headSha": "head-750",
                "state": "AWAITING_CHECKS",
                "queueEntryId": "queue-entry-750",
                "mergeGroupSha": "merge-group-750",
            }
        ],
    }
    result.update(overrides)
    return result


def sample_ruleset():
    return [
        {
            "id": 11,
            "name": "protected-main",
            "updated_at": "2026-09-05T00:00:00Z",
            "target": "branch",
            "enforcement": "active",
            "rules": [{"type": "required_status_checks"}],
        }
    ]


def sample_runs(head_sha="merge-group-750"):
    return [
        {
            "id": 1001,
            "head_sha": head_sha,
            "status": "completed",
            "conclusion": "success",
        }
    ]


def snapshot(**kwargs):
    values = {
        "pr": sample_pr(),
        "queue_entry": sample_queue(),
        "candidates": sample_queue()["pullRequests"],
        "workflow_runs": sample_runs(),
        "rulesets": sample_ruleset(),
    }
    values.update(kwargs)
    return queue.reconcile_snapshot(**values)


def test_ruleset_generation_is_stable_and_changes_on_readback_change():
    first = queue.ruleset_generation(sample_ruleset())
    second = queue.ruleset_generation(sample_ruleset())
    changed = queue.ruleset_generation(
        [{**sample_ruleset()[0], "updated_at": "2026-09-05T01:00:00Z"}]
    )
    assert first == second
    assert first != changed


def test_merge_group_head_sha_wins_over_queue_entry_head_sha():
    normalized = queue.normalize_queue_entry(
        {
            "id": "entry-750",
            "headSha": "pr-head-750",
            "mergeGroup": {"headSha": "synthetic-G-750", "baseSha": "base-main"},
        }
    )
    assert normalized["synthetic_sha"] == "synthetic-G-750"
    assert normalized["base_sha"] == "base-main"


def test_owner_unmergeable_is_scoped_and_independent_entry_is_continuable():
    independent = {
        "number": 754,
        "author": {"login": "other-owner"},
        "state": "UNMERGEABLE",
        "queueEntryId": "queue-entry-754",
        "mergeGroupSha": "merge-group-754",
    }
    current = snapshot(
        candidates=sample_queue()["pullRequests"]
        + [{**independent, "state": "UNMERGEABLE"}],
        queue_entry={**sample_queue(), "state": "UNMERGEABLE"},
    )
    assert current["actions"] == [queue.OWNER_UNMERGEABLE_ACTION]
    assert current["candidates"]["owner"][0]["scope"] == "owner"
    assert current["candidates"]["independent"][0]["scope"] == "independent"
    assert current["continuation"]["independent_entries_continue"] is True
    assert current["continuation"]["provider_mutation"] is False


def test_independent_unmergeable_does_not_interrupt_owner():
    independent = {
        "number": 754,
        "author": {"login": "other-owner"},
        "state": "UNMERGEABLE",
        "queueEntryId": "queue-entry-754",
        "mergeGroupSha": "merge-group-754",
    }
    current = snapshot(
        candidates=[{**sample_queue()["pullRequests"][0], "state": "AWAITING_CHECKS"}, independent]
    )
    assert current["actions"] == [queue.IDLE_ACTION]
    assert current["disposition"] == "independent_unmergeable_only"


def test_head_replacement_invalidates_prior_identity_and_runs():
    before = snapshot()
    after = snapshot(pr=sample_pr(headRefOid="replacement-head"), previous_binding=before["binding"])
    assert after["actions"] == [queue.HEAD_REPLACED_ACTION]
    assert after["identity"]["comparison"]["head_replaced"] is True
    assert after["identity"]["comparison"]["invalidated_workflow_run_ids"] == ["1001"]


def test_ruleset_generation_mismatch_fails_closed():
    generation = queue.ruleset_generation(sample_ruleset())
    current = snapshot(observed_ruleset_generation="stale-generation")
    assert current["actions"] == [queue.IDENTITY_MISMATCH_ACTION]
    assert current["identity"]["valid"] is False
    assert current["ruleset"]["generation"] == generation
    assert current["ruleset"]["matches_observed"] is False


def test_queue_entry_replacement_fails_closed_even_when_pr_head_is_same():
    before = snapshot()
    replacement = sample_queue(id="queue-entry-new", headSha="merge-group-new")
    after = snapshot(queue_entry=replacement, previous_binding=before["binding"])
    assert after["actions"] == [queue.IDENTITY_MISMATCH_ACTION]
    assert after["identity"]["comparison"]["queue_identity_changed"] is True


def test_workflow_run_for_wrong_merge_group_sha_is_not_evidence():
    current = snapshot(workflow_runs=sample_runs(head_sha="some-other-sha"))
    assert current["actions"] == [queue.IDENTITY_MISMATCH_ACTION]
    assert current["disposition"] == "workflow_identity_mismatch"
    assert current["identity"]["workflow_mismatches"]


def test_workflow_run_without_head_sha_is_not_evidence():
    current = snapshot(workflow_runs=[{"id": 1001, "status": "completed", "conclusion": "success"}])
    assert current["actions"] == [queue.IDENTITY_MISMATCH_ACTION]
    assert current["disposition"] == "workflow_identity_mismatch"


def test_missing_queue_identity_fails_closed_by_default():
    current = snapshot(queue_entry={"state": "AWAITING_CHECKS"}, candidates=[])
    assert current["actions"] == [queue.IDENTITY_MISMATCH_ACTION]
    assert "queue_entry_id" in current["identity"]["missing"]
    assert "merge_group_sha" in current["identity"]["missing"]


def test_missing_ruleset_readback_fails_closed():
    current = snapshot(rulesets=[])
    assert current["actions"] == [queue.IDENTITY_MISMATCH_ACTION]
    assert current["identity"]["valid"] is False
    assert "active_ruleset" in current["identity"]["missing"]


def test_empty_queue_can_be_explicitly_reported_without_claiming_valid_identity():
    current = snapshot(queue_entry=None, candidates=[], require_queue=False)
    assert current["actions"] == [queue.IDENTITY_MISMATCH_ACTION]
    assert current["identity"]["valid"] is False
    assert current["identity"]["missing"] == []


class FakeProvider:
    def __init__(self):
        self.calls = []

    def read_pr(self):
        self.calls.append("pr")
        return sample_pr()

    def read_queue_entry(self):
        self.calls.append("queue")
        return sample_queue()

    def read_workflow_runs(self, head_sha):
        self.calls.append(("runs", head_sha))
        return sample_runs(head_sha)

    def read_rulesets(self):
        self.calls.append("rulesets")
        return sample_ruleset()


def test_snapshot_provider_reads_each_surface_once_in_order():
    provider = FakeProvider()
    current = queue.snapshot_from_provider(provider)
    assert current["identity"]["valid"] is True
    assert provider.calls == ["pr", "queue", ("runs", "merge-group-750"), "rulesets"]


def test_rest_pr_shape_is_enriched_with_exact_provider_repository():
    provider = queue.ReadOnlyGitHubProvider(
        "sednalabs/codex",
        750,
        runner=lambda _command: {
            "number": 750,
            "user": {"login": "branch-owner"},
            "head": {"sha": "head-750"},
            "base": {"ref": "main", "sha": "base-main"},
        },
    )
    identity = queue.owner_identity_from_pr(provider.read_pr())
    assert identity == {
        "repository": "sednalabs/codex",
        "pr_number": 750,
        "owner": "branch-owner",
        "head_sha": "head-750",
        "base_sha": "base-main",
        "base_ref": "main",
    }


def test_graphql_queue_adapter_uses_supported_entry_schema_and_keeps_independent_entries():
    calls = []

    def runner(command):
        calls.append(command)
        if command[:1] == ["api"] and "pulls/750" in command[1]:
            return {
                "number": 750,
                "user": {"login": "branch-owner"},
                "head": {"sha": "head-750"},
                "base": {"ref": "main", "sha": "base-main"},
            }
        return {
            "data": {
                "repository": {
                    "mergeQueue": {
                        "entries": {
                            "nodes": [
                                {
                                    "id": "queue-entry-750",
                                    "position": 1,
                                    "state": "AWAITING_CHECKS",
                                    "baseCommit": {"oid": "base-main"},
                                    "headCommit": {"oid": "merge-group-750"},
                                    "pullRequest": {
                                        "number": 750,
                                        "author": {"login": "branch-owner"},
                                        "headRefOid": "head-750",
                                        "baseRefOid": "base-main",
                                        "baseRefName": "main",
                                    },
                                },
                                {
                                    "id": "queue-entry-754",
                                    "position": 2,
                                    "state": "UNMERGEABLE",
                                    "baseCommit": {"oid": "base-main"},
                                    "headCommit": {"oid": "merge-group-754"},
                                    "pullRequest": {
                                        "number": 754,
                                        "author": {"login": "other-owner"},
                                        "headRefOid": "head-754",
                                        "baseRefOid": "base-main",
                                        "baseRefName": "main",
                                    },
                                },
                            ]
                        }
                    }
                }
            }
        }

    provider = queue.ReadOnlyGitHubProvider("sednalabs/codex", 750, runner=runner)
    entry = provider.read_queue_entry()
    assert entry["headSha"] == "merge-group-750"
    assert [row["number"] for row in entry["pullRequests"]] == [750, 754]
    query = next(command[command.index("query=") + 0][6:] for command in calls if any("graphql" == value for value in command))
    assert "baseCommit{oid}" in query
    assert "headCommit{oid}" in query
    assert "mergeGroup{" not in query


def test_gh_adapter_rejects_non_get_api_commands():
    with pytest.raises(queue.QueueObserverError, match="forbids gh api method POST"):
        queue.run_gh_json(["api", "repos/example/repo/issues", "--method", "POST"])


def test_delegate_uses_one_blocking_helper_invocation_and_decodes_receipt():
    calls = []

    class Completed:
        stdout = json.dumps({"exit_reason": "action_required"})

    def fake_run(command, **kwargs):
        calls.append((command, kwargs))
        return Completed()

    receipt = queue.delegate_bounded_watcher(
        "https://github.com/sednalabs/codex/pull/750",
        runner=fake_run,
    )
    assert receipt == {"exit_reason": "action_required"}
    assert len(calls) == 1
    assert calls[0][0][-1] == "--watch-until-action"
    assert calls[0][1]["check"] is True
