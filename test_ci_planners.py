"""Small, deterministic checks for the hosted validation-lane contract."""

import json
from pathlib import Path
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

    def test_required_lanes_are_present_and_hosted_only(self):
        policy = self.config["runner_policy"]
        forbidden = tuple(policy["forbidden_constructs"])
        self.assertEqual(policy["provider"], "github-hosted")
        for lane in self.config["required_lanes"]:
            workflow = ROOT / lane["workflow"]
            self.assertTrue(workflow.is_file(), lane["workflow"])
            self.assertTrue(lane["hosted_only"], lane["id"])
            text = workflow.read_text(encoding="utf-8")
            for construct in forbidden:
                self.assertNotIn(construct, text, f"{construct}: {lane['workflow']}")

    def test_windows_is_not_provisionally_accepted(self):
        windows = self.config["windows_acceptance"]
        self.assertTrue(windows["required_hosted"])
        self.assertEqual(windows["status"], "not_yet_observed")
        self.assertFalse(windows["provisional"])


if __name__ == "__main__":
    unittest.main()
