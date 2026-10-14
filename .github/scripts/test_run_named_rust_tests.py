import importlib.util
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch


spec = importlib.util.spec_from_file_location(
    "named_rust_tests", Path(__file__).with_name("run_named_rust_tests.py")
)
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class NestedTargetSummaryTests(unittest.TestCase):
    def test_nested_summary_uses_the_enclosing_target_counts(self):
        output = (
            "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 234 filtered out\n"
            "test result: ok. 235 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out\n"
        )
        self.assertEqual(runner.test_result_counts(output), {
            "passed": 235, "failed": 0, "ignored": 1, "measured": 0, "filtered": 0
        })
        self.assertIsNone(runner.test_result_counts("no target summary"))

    def test_nested_summary_preserves_named_outcome_and_exit_guards(self):
        request = {
            "schema_version": "rust-tests-v1", "profile": "rust_integration",
            "package": "fixture", "target_kind": "lib", "target": "",
            "tests": ["fixture::selected"],
        }
        inventory, execution = runner.expected_commands("fixture", "lib", "")
        record = {
            "package": "fixture", "target_kind": "lib", "target": "",
            "profiles": ("rust_integration",),
            "inventory_argv": tuple(inventory), "execution_argv": tuple(execution),
        }
        for code, outcome, expected in [(0, "ok", "success"), (1, "ok", "failure"), (0, "ignored", "failure")]:
            with self.subTest(code=code, outcome=outcome):
                output = (
                    "test child::fixture ... ok\n"
                    "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 234 filtered out\n"
                    f"test fixture::selected ... {outcome}\n"
                    "test result: ok. 235 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out\n"
                )
                results = [
                    subprocess.CompletedProcess(inventory, 0, "fixture::selected: test\n", ""),
                    subprocess.CompletedProcess(execution, code, output, ""),
                ]
                with patch.object(runner, "load_manifest", return_value={runner.target_key("fixture", "lib", ""): record}), patch.object(runner.subprocess, "run", side_effect=results) as run:
                    result = runner.run_request(request, Path("/synthetic"))
                self.assertEqual(result["status"], expected)
                self.assertEqual(result["tests"][0]["result_counts"], {
                    "passed": 235, "failed": 0, "ignored": 1, "measured": 0, "filtered": 0
                })
                self.assertEqual(result["tests"][0]["observed_outcomes"], [outcome])
                self.assertEqual([call.args[0] for call in run.call_args_list], [inventory, execution])


if __name__ == "__main__":
    unittest.main()
