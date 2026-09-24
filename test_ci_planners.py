"""Small, deterministic checks for the hosted validation-lane contract."""

import json
import os
from pathlib import Path
import re
import subprocess
import unittest


ROOT = Path(__file__).resolve().parent
CONFIG_PATH = ROOT / "validation-lanes.json"


class ValidationLaneContractTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.config = json.loads(CONFIG_PATH.read_text(encoding="utf-8"))

    def test_candidate_identity_is_exact(self):
        candidate = self.config["candidate"]
        self.assertTrue(candidate["sha_required"])
        self.assertTrue(candidate["tree_required"])
        self.assertFalse(candidate["overlay_proof_is_product_proof"])
        runtime = candidate["runtime_identity"]
        self.assertEqual(runtime["sha_env"], "VALIDATION_TARGET_SHA")
        self.assertEqual(runtime["tree_env"], "VALIDATION_TARGET_TREE")

        expected_sha = os.environ.get(runtime["sha_env"], "")
        expected_tree = os.environ.get(runtime["tree_env"], "")
        if expected_sha or expected_tree:
            self.assertRegex(expected_sha, r"^[0-9a-f]{40}$")
            self.assertRegex(expected_tree, r"^[0-9a-f]{40}$")
            actual_sha = subprocess.check_output(
                ["git", "rev-parse", "HEAD"], text=True
            ).strip()
            actual_tree = subprocess.check_output(
                ["git", "rev-parse", "HEAD^{tree}"], text=True
            ).strip()
            self.assertEqual(expected_sha, actual_sha)
            self.assertEqual(expected_tree, actual_tree)

    def test_required_lanes_are_present_and_hosted_only(self):
        policy = self.config["runner_policy"]
        forbidden = tuple(policy["forbidden_constructs"])
        self.assertEqual(policy["provider"], "github-hosted")
        for lane in self.config["required_lanes"]:
            workflow = ROOT / lane["workflow"]
            self.assertTrue(workflow.is_file(), lane["workflow"])
            self.assertTrue(lane["hosted_only"], lane["id"])
            text = workflow.read_text(encoding="utf-8")
            self.assertNotIn("self-hosted", text, lane["workflow"])
            self.assertNotIn("-runners", text, lane["workflow"])
            for line in text.splitlines():
                match = re.match(r"\s*(?:runs-on|runner|os):\s*([A-Za-z0-9_.-]+)\s*$", line)
                if match:
                    self.assertIn(match.group(1), policy["allowed_labels"], line)

            lines = text.splitlines()
            for index, line in enumerate(lines):
                match = re.match(r"^(\s*)runs-on:\s*$", line)
                if not match:
                    continue
                indent = len(match.group(1))
                for nested in lines[index + 1 :]:
                    if nested.strip() and len(nested) - len(nested.lstrip()) <= indent:
                        break
                    self.assertIsNone(
                        re.match(r"\s*(?:group|labels|runner_group|runner_labels):", nested),
                        f"nested runner group/label: {lane['workflow']}",
                    )

            dispatch_inputs = lane.get("dispatch_inputs", [])
            if dispatch_inputs:
                dispatch = re.search(r"(?ms)^\s*workflow_dispatch:\s*\n(.*?)(?=^\S|\Z)", text)
                self.assertIsNotNone(dispatch, lane["workflow"])
                for input_name in dispatch_inputs:
                    self.assertRegex(
                        dispatch.group(1), rf"(?m)^\s+{input_name}:\s*$", input_name
                    )

    def test_windows_is_not_provisionally_accepted(self):
        windows = self.config["windows_acceptance"]
        self.assertTrue(windows["required_hosted"])
        self.assertEqual(windows["status"], "not_yet_observed")
        self.assertFalse(windows["provisional"])


if __name__ == "__main__":
    unittest.main()
