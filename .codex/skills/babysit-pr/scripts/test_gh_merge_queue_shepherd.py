import importlib.util
from pathlib import Path

import pytest


MODULE_PATH = Path(__file__).with_name("gh_merge_queue_shepherd.py")
MODULE_SPEC = importlib.util.spec_from_file_location("gh_merge_queue_shepherd", MODULE_PATH)
queue = importlib.util.module_from_spec(MODULE_SPEC)
assert MODULE_SPEC.loader is not None
MODULE_SPEC.loader.exec_module(queue)

PR_HEAD = "1" * 40
BASE_SHA = "2" * 40
SYNTHETIC_G = "3" * 40
OTHER_PR_HEAD = "4" * 40
OTHER_SYNTHETIC_G = "5" * 40


def sample_pr(**overrides):
    result = {
        "repository": "sednalabs/codex",
        "number": 750,
        "user": {"login": "branch-owner"},
        "headRefOid": PR_HEAD,
        "baseRefOid": BASE_SHA,
        "baseRefName": "main",
        "default_branch": "main",
    }
    result.update(overrides)
    return result


def sample_queue(**overrides):
    result = {
        "id": "queue-entry-750",
        "queue_entry_ref": "refs/heads/gh-readonly-queue/750",
        "state": "AWAITING_CHECKS",
        "position": 1,
        "attempt": 1,
        "merge_group_sha": SYNTHETIC_G,
        "baseSha": BASE_SHA,
        "merge_group_source": "MergeQueueEntry.headCommit.oid",
        "baseRefName": "main",
        "ancestry": {
            "source": "hosted-static-ancestry-v1",
            "pr_head_sha": PR_HEAD,
            "base_sha": BASE_SHA,
            "synthetic_sha": SYNTHETIC_G,
            "contains_pr_head": True,
            "contains_base": True,
            "complete": True,
            "verified": True,
        },
        "pullRequests": [
            {
                "number": 750,
                "author": {"login": "branch-owner"},
                "headSha": PR_HEAD,
                "baseRefOid": BASE_SHA,
                "baseRefName": "main",
                "state": "AWAITING_CHECKS",
                "queueEntryId": "queue-entry-750",
                "mergeGroupSha": SYNTHETIC_G,
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
            "conditions": {"ref_name": {"include": ["refs/heads/main"]}},
            "rules": [{"type": "required_status_checks"}],
        }
    ]


def sample_runs(head_sha=SYNTHETIC_G):
    return [
        {
            "id": 1001,
            "workflow": "CI required",
            "event": "merge_group",
            "head_sha": head_sha,
            "status": "completed",
            "conclusion": "success",
            "run_attempt": 1,
        },
        {
            "id": 1002,
            "workflow": "CodeQL required",
            "event": "merge_group",
            "head_sha": head_sha,
            "status": "completed",
            "conclusion": "success",
            "run_attempt": 1,
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


def test_allgreen_requires_exact_merge_group_required_workflows_and_attempt():
    current = snapshot()
    assert current["allgreen"] is True
    assert current["labels"] == ["ALLGREEN"]
    assert current["workflow_evidence"]["run_set"] == ["1001", "1002"]
    assert current["workflow_evidence"]["attempt"] == 1


def test_unrelated_or_empty_workflow_run_fails_closed():
    current = snapshot(
        workflow_runs=sample_runs()
        + [{"id": 1003, "workflow": "other", "event": "push"}]
    )
    assert current["allgreen"] is False
    assert current["actions"] == [queue.IDENTITY_MISMATCH_ACTION]
    assert "unrelated_workflow" in current["workflow_evidence"]["reasons"]


def test_queue_id_is_not_a_pr_identity_or_queue_ref():
    normalized = queue.normalize_queue_entry({"id": "queue-entry-750"})
    assert normalized["queue_entry_ref"] == ""
    assert queue.candidate_is_owner(
        {"queue_entry_id": "queue-entry-750"},
        {"pr_number": 750, "queue_entry_id": "queue-entry-750"},
    ) is False


def test_default_branch_ruleset_token_only_applies_to_declared_default():
    ruleset = {**sample_ruleset()[0], "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"]}}}
    assert queue.normalize_ruleset_readback(
        [ruleset], base_ref="main", default_branch="main"
    )["active_ruleset_count"] == 1
    assert queue.normalize_ruleset_readback(
        [ruleset], base_ref="release", default_branch="main"
    )["active_ruleset_count"] == 0
    assert queue.normalize_ruleset_readback(
        [ruleset], base_ref="main"
    )["active_ruleset_count"] == 0


def test_missing_structural_ancestry_never_claims_allgreen():
    current = snapshot(queue_entry={**sample_queue(), "ancestry": {}})
    assert current["allgreen"] is False
    assert "ancestry" in current["identity"]["missing"]


def test_merge_group_head_sha_wins_over_queue_entry_head_sha():
    normalized = queue.normalize_queue_entry(
        {
            "id": "entry-750",
            "headSha": PR_HEAD,
            "mergeGroup": {"headSha": SYNTHETIC_G, "baseSha": BASE_SHA},
        }
    )
    assert normalized["synthetic_sha"] == SYNTHETIC_G
    assert normalized["base_sha"] == BASE_SHA


def test_owner_unmergeable_is_scoped_and_independent_entry_is_continuable():
    independent = {
        "number": 754,
        "author": {"login": "other-owner"},
        "state": "UNMERGEABLE",
        "queueEntryId": "queue-entry-754",
        "headSha": OTHER_PR_HEAD,
        "baseRefOid": BASE_SHA,
        "baseRefName": "main",
        "mergeGroupSha": OTHER_SYNTHETIC_G,
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
        "headSha": OTHER_PR_HEAD,
        "baseRefOid": BASE_SHA,
        "baseRefName": "main",
        "mergeGroupSha": OTHER_SYNTHETIC_G,
    }
    current = snapshot(
        candidates=[{**sample_queue()["pullRequests"][0], "state": "AWAITING_CHECKS"}, independent]
    )
    assert current["actions"] == [queue.IDLE_ACTION]
    assert current["disposition"] == "independent_unmergeable_only"


def test_owner_candidate_author_mismatch_fails_closed():
    current = snapshot(
        candidates=[
            {
                **sample_queue()["pullRequests"][0],
                "author": {"login": "wrong-owner"},
            }
        ]
    )
    assert current["actions"] == [queue.IDENTITY_MISMATCH_ACTION]
    assert current["identity"]["valid"] is False


def test_head_replacement_invalidates_prior_identity_and_runs():
    before = snapshot()
    after = snapshot(pr=sample_pr(headRefOid="6" * 40), previous_binding=before["binding"])
    assert after["actions"] == [queue.HEAD_REPLACED_ACTION]
    assert after["identity"]["comparison"]["head_replaced"] is True
    assert after["identity"]["comparison"]["invalidated_workflow_run_ids"] == ["1001", "1002"]


def test_ruleset_generation_mismatch_fails_closed():
    generation = queue.ruleset_generation(sample_ruleset())
    current = snapshot(observed_ruleset_generation="stale-generation")
    assert current["actions"] == [queue.IDENTITY_MISMATCH_ACTION]
    assert current["identity"]["valid"] is False
    assert current["ruleset"]["generation"] == generation
    assert current["ruleset"]["matches_observed"] is False


def test_queue_entry_replacement_fails_closed_even_when_pr_head_is_same():
    before = snapshot()
    replacement = sample_queue(id="queue-entry-new", merge_group_sha=OTHER_SYNTHETIC_G)
    after = snapshot(queue_entry=replacement, previous_binding=before["binding"])
    assert after["actions"] == [queue.IDENTITY_MISMATCH_ACTION]
    assert after["identity"]["comparison"]["queue_identity_changed"] is True


def test_workflow_run_for_wrong_merge_group_sha_is_not_evidence():
    current = snapshot(workflow_runs=sample_runs(head_sha=OTHER_SYNTHETIC_G))
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


def test_unrelated_or_inactive_ruleset_does_not_bind_main_queue():
    current = snapshot(
        rulesets=[
            {
                **sample_ruleset()[0],
                "enforcement": "disabled",
                "conditions": {"ref_name": {"include": ["refs/heads/other"]}},
            }
        ]
    )
    assert current["actions"] == [queue.IDENTITY_MISMATCH_ACTION]
    assert current["ruleset"]["active_ruleset_count"] == 0


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
    assert provider.calls == ["pr", "queue", ("runs", SYNTHETIC_G), "rulesets"]


def test_rest_pr_shape_is_enriched_with_exact_provider_repository():
    provider = queue.ReadOnlyGitHubProvider(
        "sednalabs/codex",
        750,
        runner=lambda _command: {
            "number": 750,
            "user": {"login": "branch-owner"},
            "head": {"sha": PR_HEAD},
            "base": {"ref": "main", "sha": BASE_SHA},
        },
    )
    identity = queue.owner_identity_from_pr(provider.read_pr())
    assert identity == {
        "repository": "sednalabs/codex",
        "pr_number": 750,
        "owner": "branch-owner",
        "head_sha": PR_HEAD,
        "base_sha": BASE_SHA,
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
                "head": {"sha": PR_HEAD},
                "base": {"ref": "main", "sha": BASE_SHA},
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
                                    "baseCommit": {"oid": BASE_SHA},
                                    "headCommit": {"oid": SYNTHETIC_G},
                                    "pullRequest": {
                                        "number": 750,
                                        "author": {"login": "branch-owner"},
                                        "headRefOid": PR_HEAD,
                                        "baseRefOid": BASE_SHA,
                                        "baseRefName": "main",
                                    },
                                },
                                {
                                    "id": "queue-entry-754",
                                    "position": 2,
                                    "state": "UNMERGEABLE",
                                    "baseCommit": {"oid": BASE_SHA},
                                    "headCommit": {"oid": OTHER_SYNTHETIC_G},
                                    "pullRequest": {
                                        "number": 754,
                                        "author": {"login": "other-owner"},
                                        "headRefOid": OTHER_PR_HEAD,
                                        "baseRefOid": BASE_SHA,
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
    assert entry["merge_group_sha"] == SYNTHETIC_G
    assert [row["number"] for row in entry["pullRequests"]] == [750, 754]
    query = next(value[6:] for command in calls for value in command if value.startswith("query="))
    assert "baseCommit { oid }" in query
    assert "headCommit { oid }" in query
    assert "mergeGroup{" not in query


def test_gh_adapter_rejects_non_get_api_commands():
    with pytest.raises(queue.QueueObserverError, match="forbids gh api method POST"):
        queue.run_gh_json(["api", "repos/example/repo/issues", "--method", "POST"])


def test_graphql_errors_fail_closed_before_partial_queue_projection():
    provider = queue.ReadOnlyGitHubProvider(
        "sednalabs/codex",
        750,
        runner=lambda _command: {"errors": [{"message": "partial data"}], "data": {}},
    )
    with pytest.raises(queue.QueueObserverError, match="GraphQL response contained errors"):
        provider.read_queue_entry()


def test_pr_provider_rejects_mismatched_endpoint_identity():
    provider = queue.ReadOnlyGitHubProvider(
        "sednalabs/codex",
        750,
        runner=lambda _command: {
            "number": 751,
            "repository": {"full_name": "other/repo"},
        },
    )
    with pytest.raises(queue.QueueObserverError, match="different PR number"):
        provider.read_pr()


def test_observer_is_one_shot_and_does_not_accept_watch_delegation():
    args = queue.parse_args(["--pr", "750", "--repo", "sednalabs/codex"])
    assert args.once is True
    with pytest.raises(SystemExit):
        queue.parse_args(
            ["--pr", "750", "--repo", "sednalabs/codex", "--watch-until-action"]
        )


def test_provider_projection_leaves_unsupported_queue_evidence_unbound():
    provider = queue.ReadOnlyGitHubProvider(
        "sednalabs/codex",
        750,
        runner=lambda command: {
            "number": 750,
            "user": {"login": "branch-owner"},
            "head": {"sha": PR_HEAD},
            "base": {"ref": "main", "sha": BASE_SHA},
        }
        if "pulls/750" in command[1]
        else {
            "data": {
                "repository": {
                    "mergeQueue": {
                        "entries": {
                            "nodes": [
                                {
                                    "id": "queue-entry-750",
                                    "position": 1,
                                    "state": "AWAITING_CHECKS",
                                    "baseCommit": {"oid": BASE_SHA},
                                    "headCommit": {"oid": SYNTHETIC_G},
                                    "pullRequest": {
                                        "number": 750,
                                        "author": {"login": "branch-owner"},
                                        "headRefOid": PR_HEAD,
                                        "baseRefOid": BASE_SHA,
                                        "baseRefName": "main",
                                    },
                                }
                            ]
                        }
                    }
                }
            }
        },
    )
    entry = provider.read_queue_entry()
    assert "queueEntryRef" not in entry
    assert "attempt" not in entry
    assert "ancestryEvidence" not in entry
    current = queue.reconcile_snapshot(
        pr=sample_pr(),
        queue_entry=entry,
        candidates=entry["pullRequests"],
        workflow_runs=sample_runs(),
        rulesets=sample_ruleset(),
    )
    assert current["allgreen"] is False
    assert current["identity"]["external_evidence_required"] == [
        "queue_entry_ref",
        "queue_attempt",
        "ancestry",
    ]


def test_duplicate_or_malformed_run_ids_are_rejected_before_workflow_selection():
    runs = [
        {**sample_runs()[0], "id": 1001},
        {**sample_runs()[1], "id": 1001, "workflow": "CodeQL required"},
    ]
    evidence = queue.workflow_evidence(runs, SYNTHETIC_G)
    assert evidence["valid"] is False
    assert "duplicate_run_id" in evidence["reasons"]
    assert evidence["selected"] == []

    malformed = queue.workflow_evidence(
        [{**sample_runs()[0], "id": "run-1001"}, sample_runs()[1]], SYNTHETIC_G
    )
    assert malformed["valid"] is False
    assert "run_id_malformed" in malformed["reasons"]
    assert "CI required" in malformed["missing_workflows"]


def test_conflicting_queue_aliases_fail_closed_instead_of_first_nonempty_value():
    current = snapshot(
        queue_entry={
            **sample_queue(),
            "merge_group_base_sha": "6" * 40,
            "mergeGroupSha": "7" * 40,
        }
    )
    assert current["allgreen"] is False
    assert "queue_alias_conflict" in current["identity"]["missing"]
    assert "queue_alias_conflict_base_sha" in current["identity"]["missing"]
    assert "queue_alias_conflict_synthetic_sha" in current["identity"]["missing"]


def test_unallowlisted_ancestry_source_fails_closed():
    current = snapshot(
        queue_entry={
            **sample_queue(),
            "ancestry": {**sample_queue()["ancestry"], "source": "fabricated"},
        }
    )
    assert current["allgreen"] is False
    assert "ancestry_source_untrusted" in current["identity"]["missing"]


def test_queue_entry_ref_must_be_distinct_from_provider_queue_id():
    current = snapshot(queue_entry={**sample_queue(), "queue_entry_ref": "queue-entry-750"})
    assert current["allgreen"] is False
    assert "queue_entry_ref_not_distinct" in current["identity"]["missing"]


def test_conflicting_workflow_aliases_are_not_selected():
    runs = [{**sample_runs()[0], "headSha": OTHER_SYNTHETIC_G} , sample_runs()[1]]
    evidence = queue.workflow_evidence(runs, SYNTHETIC_G)
    assert evidence["valid"] is False
    assert "workflow_field_alias_conflict" in evidence["reasons"]


def test_workflow_provider_requests_complete_pagination():
    provider = queue.ReadOnlyGitHubProvider(
        "sednalabs/codex", 750,
        runner=lambda command: [{"workflow_runs": sample_runs()}, {"workflow_runs": []}],
    )
    runs = provider.read_workflow_runs(SYNTHETIC_G)
    assert len(runs) == 2


def test_incomplete_workflow_page_fails_closed():
    provider = queue.ReadOnlyGitHubProvider(
        "sednalabs/codex", 750,
        runner=lambda command: {"workflow_runs": [], "pagination_complete": False},
    )
    with pytest.raises(queue.QueueObserverError, match="pagination was incomplete"):
        provider.read_workflow_runs(SYNTHETIC_G)
