#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path


SCRIPT_PATH = Path(__file__).with_name("supersede_automation_prs.py")
SPEC = importlib.util.spec_from_file_location("supersede_automation_prs", SCRIPT_PATH)
assert SPEC and SPEC.loader
supersede = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = supersede
SPEC.loader.exec_module(supersede)


def pull_request(
    number: int,
    *,
    version: str | None = None,
    dependency: str = "example-package",
    created_at: str = "2026-08-01T00:00:00Z",
    head_ref: str | None = None,
    head_repo: str = "sednalabs/codex",
    author: str = "dependabot[bot]",
) -> supersede.PullRequest:
    body = f"Bumps [{dependency}]\n<!-- dependency-version: {version} -->" if version else f"Bumps [{dependency}]"
    return supersede.PullRequest(
        number=number,
        title=f"Bump {dependency} from old to {version or 'new'}",
        body=body,
        head_ref=head_ref or f"dependabot/example/{dependency}-{number}",
        head_repo=head_repo,
        created_at=created_at,
        author=author,
        html_url=f"https://github.com/example/repository/pull/{number}",
    )


def action(number: int, winner: int, dependency: str = "example-package") -> dict[str, object]:
    return {
        "number": number,
        "message": (
            f"Closing this superseded automation PR. The newer PR #{winner} "
            f"is the current {dependency} update: https://github.com/example/repository/pull/{winner}."
        ),
    }


class SupersessionPlanTest(unittest.TestCase):
    def test_dependabot_whole_plan_cases(self) -> None:
        cases = {
            "newer stable": (
                [pull_request(1, version="1.2.3"), pull_request(2, version="1.3.0")],
                [action(1, 2)],
            ),
            "stable supersedes prerelease of same release": (
                [pull_request(3, version="2.0.0-rc.1"), pull_request(4, version="2.0.0")],
                [action(3, 4)],
            ),
            "newer stable supersedes prerelease of older release": (
                [pull_request(30, version="2.0.0-rc.1"), pull_request(31, version="2.1.0")],
                [action(30, 31)],
            ),
            "equal versions": (
                [pull_request(5, version="3.1.0"), pull_request(6, version="3.1.0")],
                [],
            ),
            "semantically equal stable versions": (
                [pull_request(50, version="1.0"), pull_request(51, version="1.0.0")],
                [],
            ),
            "missing metadata": (
                [pull_request(7, version="4.0.0"), pull_request(8)],
                [],
            ),
            "ambiguous version form": (
                [pull_request(9, version="5.0.0"), pull_request(10, version="v6.0.0")],
                [],
            ),
            "unsupported metadata suffix": (
                [pull_request(90, version="1.2.3"), pull_request(91, version="1.2.4:custom")],
                [],
            ),
            "ambiguous prerelease ordering": (
                [pull_request(11, version="7.0.0-alpha.1"), pull_request(12, version="7.0.0-beta.1")],
                [],
            ),
        }
        for name, (pull_requests, expected) in cases.items():
            with self.subTest(name=name):
                self.assertEqual(supersede.supersession_plan(pull_requests, "dependabot"), expected)

    def test_models_json_uses_created_time_ordering(self) -> None:
        pull_requests = [
            pull_request(
                20,
                created_at="2026-08-02T00:00:00Z",
                head_ref="bot/sync-models-json-older-number",
                author="github-actions[bot]",
            ),
            pull_request(
                21,
                created_at="2026-08-01T00:00:00Z",
                head_ref="bot/sync-models-json-older-time",
                author="github-actions[bot]",
            ),
            pull_request(
                22,
                created_at="2026-08-02T00:00:00Z",
                head_ref="bot/sync-models-json-newest",
                author="github-actions[bot]",
            ),
            pull_request(
                23,
                created_at="2026-08-03T00:00:00Z",
                head_ref="bot/sync-models-json-fork-lookalike",
                head_repo="untrusted/fork",
                author="github-actions[bot]",
            ),
            pull_request(
                24,
                created_at="2026-08-04T00:00:00Z",
                head_ref="bot/sync-models-json-author-lookalike",
                author="untrusted-user",
            ),
            pull_request(
                25,
                created_at="2026-08-02T00:00:00Z",
                head_ref="bot/sync-models-json-case-insensitive",
                head_repo="SednaLabs/Codex",
                author="GitHub-Actions[bot]",
            ),
        ]

        self.assertEqual(
            supersede.supersession_plan(pull_requests, "models-json"),
            [action(22, 25, "models.json"), action(20, 25, "models.json"), action(21, 25, "models.json")],
        )


if __name__ == "__main__":
    unittest.main()
