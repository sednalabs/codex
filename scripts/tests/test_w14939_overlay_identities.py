import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).parents[1] / "w14939-overlay-identities.py"
spec = importlib.util.spec_from_file_location("overlay_identities", SCRIPT)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class OverlayIdentitiesTest(unittest.TestCase):
    def test_matches_synthetic_rust_vector(self):
        plan = {"edits": [{"op": "remove_exact_if_present", "text": "synthetic target", "expected_matches": 1}]}
        self.assertEqual(module.identity(plan, b"synthetic target"), (
            "a1311114618394433a7ff0e8f01ce4af655464f5e66bb46dbd7d270dcbc4c65f",
            "0f9f8fd711af2773cfb0ac61b62e189d568a24997c24e476681445eacebe9bcc",
        ))

    def test_rejects_unsafe_or_ambiguous_edits(self):
        for edit in (
            {"op": "replace", "text": "synthetic", "expected_matches": 1},
            {"op": "remove_exact", "text": "synthetic", "expected_matches": 2},
            {"op": "remove_exact", "text": "", "expected_matches": 1},
        ):
            with self.subTest(edit=edit), self.assertRaises(ValueError):
                module.identity({"edits": [edit]}, b"synthetic")

    def test_cli_does_not_echo_synthetic_inputs(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "plan.toml"
            path.write_text('[model_baseline]\nedits = [{ op = "remove_exact_if_present", text = "synthetic private text", expected_matches = 1 }]\n')
            result = subprocess.run([sys.executable, str(SCRIPT), "--plan-file", str(path)], input=b"synthetic baseline", capture_output=True, check=True)
            self.assertNotIn(b"synthetic", result.stdout + result.stderr)
            self.assertEqual(len(result.stdout.splitlines()), 2)
            path.write_text('[model_baseline]\nedits = [{ op = "replace", text = "synthetic private text", expected_matches = 1 }]\n')
            failed = subprocess.run([sys.executable, str(SCRIPT), "--plan-file", str(path)], input=b"synthetic baseline", capture_output=True)
            self.assertNotEqual(failed.returncode, 0)
            self.assertNotIn(b"synthetic", failed.stdout + failed.stderr)


if __name__ == "__main__":
    unittest.main()
