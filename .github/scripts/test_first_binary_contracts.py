"""Hosted regression controls for selected static consumer contracts."""

import copy
import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import check_first_binary_contracts as checker
import verify_existing_first_binary_producer as producer_verifier


OPTION_SOURCE = """def pytest_addoption(parser):
    group = parser.getgroup('Selected consumer')
    for option in ('artifact-dir', 'artifact-name', 'target-sha', 'base-ref', 'base-sha'):
        group.addoption(f'--{option}', required=True)
    group.addoption('--artifact-id', required=True, type=int)
    group.addoption('--workflow-host-sha')
    group.addoption('--run-id', type=int)
    group.addoption('--expected-producer-workflow-host-sha')
    group.addoption('--expected-producer-run-id', type=int)
    group.addoption('--consumer-context-file', required=True, type=Path)
    group.addoption('--fixture-sha')
"""
BROWSER_DIAGNOSTIC_OPTION_SOURCE = """def pytest_addoption(parser):
    group = parser.getgroup('Sedna first binary package')
    for option in ('artifact-dir', 'artifact-name', 'target-sha', 'base-ref', 'base-sha', 'workflow-host-sha'):
        group.addoption(f'--{option}', required=True)
    group.addoption('--artifact-id', required=True, type=int)
    group.addoption('--run-id', required=True, type=int)
"""
TEST_RUN = '''test_root="${FIXTURE_ROOT}/scripts/codex_package/smoke_tests/first_binary"
case "${CONSUMER_PROFILE}" in
  focused)
    test_args=("${test_root}/test_selected.py::test_selected")
    ;;
  *) exit 1 ;;
esac
pytest -q \\
  --confcutdir="${test_root}" \\
  "${test_args[@]}" \\
  --artifact-dir pkg \\
  --artifact-id 1 \\
  --artifact-name pkg \\
  --target-sha target \\
  --base-ref main \\
  --base-sha base \\
  --expected-producer-workflow-host-sha producer \\
  --expected-producer-run-id 7 \\
  --consumer-context-file context \\
  --fixture-sha fixture \\
  --junitxml junit
'''
PAIR_RUN = TEST_RUN.replace("focused)", "pair)").replace(
    '"${test_root}/test_selected.py::test_selected"',
    '"${test_root}/state_history/test_state_history.py::test_packaged_historical_upgrade_and_reopen[fresh]" '
    '"${test_root}/state_history/test_state_history.py::test_packaged_historical_rejection_preserves_preimage[bad_checksum]"',
)
BUILD_RUN = '''TEST_ROOT="${FIXTURE_ROOT}/scripts/codex_package/smoke_tests/first_binary"
case "${MODE}:${CONSUMER_PROFILE}" in
  build:*)
    TEST_ARGS=("${TEST_ROOT}")
    PRODUCER_ARGS=(--workflow-host-sha "${GITHUB_SHA}" --run-id "${GITHUB_RUN_ID}")
    FIXTURE_ARGS=()
    ;;
  *) exit 1 ;;
esac
pytest -q \\
  --confcutdir="${TEST_ROOT}" \\
  "${TEST_ARGS[@]}" \\
  --artifact-dir pkg \\
  --artifact-id 1 \\
  --artifact-name pkg \\
  --target-sha target \\
  --base-ref main \\
  --base-sha base \\
  "${PRODUCER_ARGS[@]}" \\
  "${FIXTURE_ARGS[@]}" \\
  --consumer-context-file context \\
  --junitxml junit
'''
BROWSER_DIAGNOSTIC_BUILD_RUN = '''TEST_ROOT="${FIXTURE_ROOT}/scripts/codex_package/smoke_tests/first_binary"
case "${MODE}:${CONSUMER_PROFILE}" in
  build:browser-diagnostic)
    TEST_ARGS=("${TEST_ROOT}/test_tui_agents_acceptance.py::test_packaged_tui_browser_output_keeps_images_and_manifest_metadata_separate")
    PRODUCER_ARGS=(--workflow-host-sha "${GITHUB_SHA}" --run-id "${GITHUB_RUN_ID}")
    FIXTURE_ARGS=()
    ;;
  *) exit 1 ;;
esac
pytest -q \\
  --confcutdir="${TEST_ROOT}" \\
  "${TEST_ARGS[@]}" \\
  --artifact-dir pkg \\
  --artifact-id 1 \\
  --artifact-name pkg \\
  --target-sha target \\
  --base-ref validation/interrupt-guardian-fixture-8389-20261004 \\
  --base-sha 8389b61d82cb6fb936e4e500b977f31682441ffe \\
  "${PRODUCER_ARGS[@]}" \\
  "${FIXTURE_ARGS[@]}" \\
  --junitxml junit
'''


class CheckerFixture(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.workflow = self.root / "host"
        self.source = self.root / "fixture"
        self.sdk = self.root / "sdk"
        self.producer = self.root / "producer"
        self.test_dir = self.source / checker.TEST_ROOT
        self.test_dir.mkdir(parents=True)
        (self.test_dir / "conftest.py").write_text(OPTION_SOURCE, encoding="utf-8")
        (self.test_dir / "test_selected.py").write_text("def test_selected(): pass\n", encoding="utf-8")
        (self.test_dir / "test_agent_control_tui_acceptance.py").write_text("", encoding="utf-8")
        (self.test_dir / "test_tui_agents_acceptance.py").write_text(
            "def test_packaged_tui_browser_output_keeps_images_and_manifest_metadata_separate(): pass\n",
            encoding="utf-8",
        )
        (self.sdk / "sdk/python/tests").mkdir(parents=True)
        (self.sdk / "sdk/python/tests/test_sdk.py").write_text("def test_sdk_selected(): pass\n", encoding="utf-8")
        (self.producer / ".github/workflows").mkdir(parents=True)
        self.write_producer(["verify", "package_x", "package_arm", "prepare", "consume_x", "consume_arm"])

    def write_producer(self, job_ids):
        lines = ["jobs:"]
        lines.extend("  {}:\n    name: Job {}".format(job, job) for job in job_ids)
        (self.producer / checker.WORKFLOW).write_text("\n".join(lines) + "\n", encoding="utf-8")

    def verifier(self, profile="focused", plain=None):
        record_jobs = [{"name": "Job {}".format(job)} for job in ["verify", "package_x", "package_arm", "prepare", "consume_x", "consume_arm"]]
        return mock.Mock(
            consume_existing_test_plan=mock.Mock(return_value={"plain": plain or {"test_selected"}, "state": {"fresh", "bad_checksum"} if profile == "pair" else set()}),
            browser_diagnostic_test_plan=mock.Mock(return_value={
                "plain": {"test_packaged_tui_browser_output_keeps_images_and_manifest_metadata_separate"},
                "state": set(),
            }),
            sdk_test_plan=mock.Mock(return_value={"selectors": ("sdk/python/tests/test_sdk.py::test_sdk_selected",)}),
            select_accepted_record=mock.Mock(return_value={"record_id": "selected-record", "jobs": record_jobs}),
            _read_manifest=mock.Mock(return_value={}),
            _accepted_inputs_from_env=mock.Mock(return_value={}),
        )

    def run_checks(self, mode="consume-existing", profile="focused", step_run=None, verifier=None, check_head=None, env_override=None):
        run_text = step_run or (BUILD_RUN if mode == "build" else PAIR_RUN if profile == "pair" else TEST_RUN)
        build_run = step_run or (BROWSER_DIAGNOSTIC_BUILD_RUN if profile == "browser-diagnostic" else BUILD_RUN)
        steps = {
            checker.STEP_BUILD: [("Package native Linux x86_64", build_run), ("Package native Linux ARM64", build_run)],
            checker.STEP_EXISTING: [("Consume native Linux x86_64 package", run_text), ("Consume native Linux ARM64 package", run_text)],
        }
        args = mock.Mock(workflow_root=self.workflow, source_root=self.source, sdk_root=self.sdk, producer_root=self.producer)
        env = {
            "MODE": mode, "CONSUMER_PROFILE": profile,
            "EXPECTED_H": "h" * 40, "TARGET_SHA": "t" * 40, "FIXTURE_SHA": "q" * 40,
            "SDK_SHA": "s" * 40, "BASE_REF": "main", "BASE_SHA": "b" * 40,
            "PRODUCER_RUN_ID": "7", "PRODUCER_WORKFLOW_HOST_SHA": "p" * 40,
        }
        if mode == "build" and profile == "browser-diagnostic":
            (self.test_dir / "conftest.py").write_text(BROWSER_DIAGNOSTIC_OPTION_SOURCE, encoding="utf-8")
            env.update(
                TARGET_SHA=producer_verifier.BROWSER_DIAGNOSTIC_PRODUCT_SHA,
                FIXTURE_SHA=producer_verifier.BROWSER_DIAGNOSTIC_FIXTURE_SHA,
                SDK_SHA=producer_verifier.BROWSER_DIAGNOSTIC_SDK_SHA,
                BASE_REF=producer_verifier.BROWSER_DIAGNOSTIC_BASE_REF,
                BASE_SHA=producer_verifier.BROWSER_DIAGNOSTIC_BASE_SHA,
                PRODUCER_RUN_ID="",
                PRODUCER_WORKFLOW_HOST_SHA="",
            )
        if env_override:
            env.update(env_override)
        verifier = verifier or self.verifier(profile)
        with mock.patch.object(checker, "_git_head", side_effect=check_head or (lambda *args: True)), mock.patch.object(
            checker, "_workflow_steps", return_value=steps
        ), mock.patch.object(checker, "_load_verifier", return_value=verifier):
            return checker.run(args, env)


class OptionContractTests(CheckerFixture):
    def test_build_source_without_context_registration_rejects_workflow_argument(self):
        registration = "    group.addoption('--consumer-context-file', required=True, type=Path)\n"
        self.assertEqual(OPTION_SOURCE.count(registration), 1)
        source = OPTION_SOURCE.replace(registration, "")
        self.assertNotIn("consumer-context-file", source)
        (self.test_dir / "conftest.py").write_text(
            source, encoding="utf-8")
        result = self.run_checks(mode="build", profile="full")
        self.assertTrue(any("unknown pytest option --consumer-context-file" in error for error in result.errors))

    def test_unsupported_dynamic_registration_fails_closed(self):
        with self.assertRaisesRegex(ValueError, "literal long option"):
            checker._registered_options("def pytest_addoption(parser):\n    group = parser.getgroup('x')\n    group.addoption(option_name)\n")
        with self.assertRaisesRegex(ValueError, "pytest_addoption"):
            checker._registered_options("def other(parser):\n    group = parser.getgroup('x')\n")


class SelectedRunTests(CheckerFixture):
    def test_actual_shaped_build_and_consume_invocations_pass(self):
        seen = []
        def check(root, expected, mode, label, diagnostics):
            seen.append((label, expected))
            return True
        build = self.run_checks(mode="build", profile="full", check_head=check)
        consume = self.run_checks(check_head=check)
        self.assertEqual(build.errors, [])
        self.assertEqual(consume.errors, [])
        self.assertIn(("source", "t" * 40), seen)
        self.assertIn(("source", "q" * 40), seen)
        self.assertIn(("sdk", "s" * 40), seen)
        self.assertIn(("producer", "p" * 40), seen)

    def test_browser_diagnostic_profile_binds_exact_source_pair_and_nodeid(self):
        seen = []

        def check(root, expected, mode, label, diagnostics):
            seen.append((label, expected))
            return True

        result = self.run_checks(
            mode="build",
            profile="browser-diagnostic",
            verifier=producer_verifier,
            check_head=check,
        )
        self.assertEqual([], result.errors)
        self.assertIn(("source", producer_verifier.BROWSER_DIAGNOSTIC_FIXTURE_SHA), seen)
        self.assertIn(("sdk", producer_verifier.BROWSER_DIAGNOSTIC_SDK_SHA), seen)

    def test_browser_diagnostic_profile_rejects_source_pair_and_selector_substitutions(self):
        for mutation in ("fixture", "sdk", "base", "extra_selector", "extra_args"):
            with self.subTest(mutation=mutation):
                run = BROWSER_DIAGNOSTIC_BUILD_RUN
                env_override = {}
                if mutation == "fixture":
                    env_override["FIXTURE_SHA"] = "f" * 40
                elif mutation == "sdk":
                    env_override["SDK_SHA"] = "s" * 40
                elif mutation == "base":
                    env_override["BASE_REF"] = "main"
                if mutation == "extra_selector":
                    run = run.replace(
                        'TEST_ARGS=("${TEST_ROOT}/test_tui_agents_acceptance.py::test_packaged_tui_browser_output_keeps_images_and_manifest_metadata_separate")',
                        'TEST_ARGS=("${TEST_ROOT}/test_tui_agents_acceptance.py::test_packaged_tui_browser_output_keeps_images_and_manifest_metadata_separate" "${TEST_ROOT}/test_selected.py::test_selected")',
                    )
                elif mutation == "extra_args":
                    run = run.replace(
                        "FIXTURE_ARGS=()",
                        'FIXTURE_ARGS=(--fixture-sha "${FIXTURE_SHA}" --consumer-context-file context)',
                    )
                result = self.run_checks(
                    mode="build",
                    profile="browser-diagnostic",
                    step_run=run,
                    verifier=producer_verifier,
                    env_override=env_override,
                )
                if mutation == "fixture":
                    self.assertTrue(any("fixture SHA is not admitted" in error for error in result.errors))
                elif mutation == "sdk":
                    self.assertTrue(any("SDK SHA is not admitted" in error for error in result.errors))
                elif mutation == "base":
                    self.assertTrue(any("comparison ref is not admitted" in error for error in result.errors))
                elif mutation == "extra_selector":
                    self.assertTrue(any("select exactly its admitted nodeid" in error for error in result.errors))
                else:
                    self.assertTrue(any("unknown pytest option --consumer-context-file" in error for error in result.errors))
                    self.assertTrue(any("unknown pytest option --fixture-sha" in error for error in result.errors))

    def test_consume_fixture_root_uses_fixture_sha_and_wrong_head_blocks(self):
        seen = []
        def check(root, expected, mode, label, diagnostics):
            seen.append((label, expected))
            return expected != "wrong"
        result = self.run_checks(check_head=check)
        self.assertEqual(result.errors, [])
        self.assertIn(("source", "q" * 40), seen)

        def wrong(root, expected, mode, label, diagnostics):
            if label == "source":
                diagnostics.error(mode, label, str(root), "HEAD mismatch")
                return False
            return True
        result = self.run_checks(check_head=wrong)
        self.assertTrue(any("source" in error and "HEAD" in error for error in result.errors))

    def test_focused_nodeid_must_match_expected_file_and_function(self):
        wrong_file = TEST_RUN.replace("test_selected.py::test_selected", "wrong_file.py::test_selected")
        result = self.run_checks(step_run=wrong_file)
        self.assertTrue(any("module/function mismatch" in error for error in result.errors))
        self.assertFalse(any("selected nodeid missing expected test" in error for error in result.errors))
        wrong_name = TEST_RUN.replace("test_selected.py::test_selected", "test_selected.py::test_unplanned")
        result = self.run_checks(step_run=wrong_name)
        self.assertTrue(any("selected nodeid missing expected test" in error for error in result.errors))
        self.assertTrue(any("not in verifier plan" in error for error in result.errors))

    def test_pair_uses_exact_parameter_labels_not_only_function_stems(self):
        state = self.test_dir / "state_history"
        state.mkdir()
        (state / "test_state_history.py").write_text(
            "def test_packaged_historical_upgrade_and_reopen(): pass\n"
            "def test_packaged_historical_rejection_preserves_preimage(): pass\n", encoding="utf-8")
        bad = PAIR_RUN.replace("[fresh]", "[stale]")
        result = self.run_checks(profile="pair", step_run=bad, verifier=self.verifier("pair", set()))
        self.assertTrue(any("exact selected state labels" in error for error in result.errors))

    def test_six_job_selected_record_does_not_match_seven_static_names(self):
        self.write_producer(["verify", "package_x", "package_arm", "prepare", "consume_x", "consume_arm", "extra"])
        result = self.run_checks()
        self.assertTrue(any("selected-record" in error and "extra" in error for error in result.errors))

    def test_missing_sdk_selector_is_not_accepted_from_a_stale_plan_name(self):
        verifier = self.verifier()
        verifier.sdk_test_plan.return_value = {"selectors": ("sdk/python/tests/test_sdk.py::test_missing",)}
        result = self.run_checks(verifier=verifier)
        self.assertTrue(any("SDK selector function missing: test_missing" in error for error in result.errors))

    def test_older_fixture_without_optional_observer_keeps_its_selected_contract(self):
        (self.test_dir / "test_agent_control_tui_acceptance.py").unlink()
        self.assertEqual(self.run_checks().errors, [])

    def test_malformed_manifest_batches_with_option_diagnostics(self):
        bad = TEST_RUN.replace("--consumer-context-file context", "--not-registered context")
        args = mock.Mock(workflow_root=self.workflow, source_root=self.source, sdk_root=self.sdk, producer_root=self.producer)
        steps = {checker.STEP_BUILD: [("build", BUILD_RUN)] * 2, checker.STEP_EXISTING: [("Consume native Linux x86_64 package", bad), ("Consume native Linux ARM64 package", bad)]}
        with mock.patch.object(checker, "_git_head", return_value=True), mock.patch.object(
            checker, "_workflow_steps", return_value=steps
        ), mock.patch.object(checker, "_load_verifier", side_effect=ValueError("malformed manifest row")):
            result = checker.run(args, {"MODE": "consume-existing", "CONSUMER_PROFILE": "focused", "EXPECTED_H": "h"*40,
                "FIXTURE_SHA": "q"*40, "TARGET_SHA": "t"*40, "SDK_SHA": "s"*40,
                "PRODUCER_WORKFLOW_HOST_SHA": "p"*40, "PRODUCER_RUN_ID": "7"})
        joined = "\n".join(result.errors)
        self.assertIn("all-record validation", joined)
        self.assertIn("unknown pytest option --not-registered", joined)
        self.assertIn("missing required pytest option --consumer-context-file", joined)
        self.assertIn("Consume native Linux x86_64 package", joined)
        self.assertIn("Consume native Linux ARM64 package", joined)

    def test_unknown_mode_is_blocking_not_a_clean_noop(self):
        args = mock.Mock(workflow_root=self.workflow, source_root=self.source, sdk_root=None, producer_root=None)
        result = checker.run(args, {"MODE": ""})
        self.assertTrue(any("unsupported or missing MODE" in error for error in result.errors))


class WorkflowAndObserverTests(CheckerFixture):
    def test_checked_in_workflow_all_selected_command_shapes(self):
        workflow = Path(__file__).resolve().parents[1] / "workflows/sedna-branch-build.yml"
        steps = checker._workflow_steps(workflow)
        for _, script in steps[checker.STEP_BUILD]:
            argv = checker._selected_pytest_argv(script, "build", "full", checker.STEP_BUILD)
            self.assertIn("workflow-host-sha", checker._custom_options(argv))
            self.assertEqual(checker._selected_nodeids(argv), [checker.TEST_ROOT])
        for _, script in steps[checker.STEP_EXISTING]:
            for profile in ("pair", "focused", "full"):
                argv = checker._selected_pytest_argv(script, "consume-existing", profile, checker.STEP_EXISTING)
                self.assertTrue(checker._selected_nodeids(argv))
                self.assertIn("expected-producer-workflow-host-sha", checker._custom_options(argv))
                if profile == "full":
                    self.assertEqual(checker._selected_nodeids(argv), [checker.TEST_ROOT])

    def test_workflow_rejects_wrong_architecture_or_mode(self):
        workflow = Path(__file__).resolve().parents[1] / "workflows/sedna-branch-build.yml"
        original = workflow.read_text(encoding="utf-8")
        for source in (
            original.replace("consume-linux-aarch64:", "duplicate-consumer:"),
            original.replace("Consume native Linux ARM64 package", "Consume native Linux x86_64 package"),
            original.replace("if: ${{ inputs.mode == 'build' }}", "if: ${{ inputs.mode == 'prepare-only' }}"),
        ):
            with mock.patch.object(Path, "read_text", return_value=source), self.assertRaises(ValueError):
                checker._workflow_steps(workflow)

    def test_checked_in_workflow_rejects_changed_root_and_parser_bindings(self):
        workflow = Path(__file__).resolve().parents[1] / "workflows/sedna-branch-build.yml"
        for step, invocations in checker._workflow_steps(workflow).items():
            mode = "build" if step == checker.STEP_BUILD else "consume-existing"
            for _, script in invocations:
                if '${FIXTURE_ROOT}/' in script:
                    wrong = script.replace('${FIXTURE_ROOT}/', '${FIXTURE_ROOT}/wrong/', 1)
                    with self.assertRaisesRegex(ValueError, "selected test root"):
                        checker._selected_pytest_argv(wrong, mode, "full", step)
                wrong = script.replace('--confcutdir="', '--confcutdir="wrong/', 1)
                with self.assertRaisesRegex(ValueError, "confcutdir"):
                    checker._selected_pytest_argv(wrong, mode, "full", step)

    def test_pytest_control_operators_and_duplicate_commands_are_rejected(self):
        for suffix in (" || true", " > output", "; true", "\npytest -q"):
            with self.assertRaises(ValueError):
                checker._pytest_argv(TEST_RUN.rstrip() + suffix)

    def test_expanded_array_cannot_override_parser_root(self):
        for script, mode, profile, step, array in (
            (TEST_RUN, "consume-existing", "focused", checker.STEP_EXISTING, "test_args"),
            (BUILD_RUN, "build", "full", checker.STEP_BUILD, "PRODUCER_ARGS"),
        ):
            for override in ("--confcutdir=wrong ", "--confcutdir wrong "):
                wrong = script.replace(array + "=(", array + "=(" + override, 1)
                with self.assertRaisesRegex(ValueError, "confcutdir"):
                    checker._selected_pytest_argv(wrong, mode, profile, step)

    def test_root_binding_must_precede_selected_case_and_pytest(self):
        for script, mode, profile, step in (
            (TEST_RUN, "consume-existing", "focused", checker.STEP_EXISTING),
            (BUILD_RUN, "build", "full", checker.STEP_BUILD),
        ):
            binding, rest = script.split("\n", 1)
            for wrong in (rest + "\n" + binding, rest.replace("esac\n", "esac\n" + binding + "\n")):
                with self.assertRaisesRegex(ValueError, "selected test root"):
                    checker._selected_pytest_argv(wrong, mode, profile, step)

    def test_existing_authority_rejects_malformed_base_sha(self):
        import verify_existing_first_binary_producer as verifier
        manifest = copy.deepcopy(verifier._read_manifest())
        record = manifest["records"][0]
        record["identity"]["comparison_base_sha"] = "b" * 38
        path = self.root / "malformed-manifest.json"
        path.write_text(json.dumps(manifest), encoding="utf-8")
        with self.assertRaises(ValueError) as failure:
            verifier._read_manifest(path)
        self.assertIn("record " + record["record_id"] + " rejected", str(failure.exception))
        self.assertIn("comparison_base_sha is invalid", str(failure.exception))
        record["record_id"] = "unsafe\nidentifier"
        path.write_text(json.dumps(manifest), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "record index 0 rejected"):
            verifier._read_manifest(path)

    def test_producer_names_require_one_literal_per_top_level_job(self):
        self.assertEqual(len(checker._producer_job_names(self.producer / checker.WORKFLOW)), 6)
        path = self.producer / checker.WORKFLOW
        path.write_text(path.read_text(encoding="utf-8") + "  nameless_job:\n    runs-on: ubuntu-latest\n", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "missing a literal name"):
            checker._producer_job_names(path)

    def _observer(self, before_cr, marker="TUI_RICH_ROOT_MARKER: start one synthetic worker."):
        path = self.root / "observer.py"
        before = 'tui.send("\\r")\n    ' if before_cr else ""
        after = "" if before_cr else '\n    tui.send("\\r")'
        path.write_text("with PackagedTui(columns=40) as tui:\n    tui.send({!r})\n    {}tui.until({!r}){}\n".format(marker, before, marker, after), encoding="utf-8")
        diagnostics = checker.Diagnostics()
        checker._observer_warnings(path, "consume-existing", "consumer", diagnostics)
        return diagnostics

    def test_historical_long_literal_warns_without_failing(self):
        result = self._observer(False)
        self.assertEqual(result.errors, [])
        self.assertEqual(len(result.warnings), 1)
        self.assertIn("raw contiguous soft-wrap may still match", result.warnings[0])

    def test_short_marker_is_not_warned_and_carriage_return_stops_search(self):
        self.assertEqual(self._observer(False, "ready").warnings, [])
        self.assertEqual(self._observer(True).warnings, [])

    def test_warnings_alone_keep_cli_exit_success(self):
        with mock.patch.object(checker, "run", return_value=checker.Diagnostics()) as run:
            self.assertEqual(checker.main(["--workflow-root", str(self.workflow), "--source-root", str(self.source)]), 0)
            run.return_value.warning("build", "consumer", "workflow", "soft-wrap may match raw bytes")
            self.assertEqual(checker.main(["--workflow-root", str(self.workflow), "--source-root", str(self.source)]), 0)
            run.return_value.error("build", "consumer", "workflow", "missing option")
            self.assertEqual(checker.main(["--workflow-root", str(self.workflow), "--source-root", str(self.source)]), 1)


if __name__ == "__main__":
    unittest.main()
