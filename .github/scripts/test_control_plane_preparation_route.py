#!/usr/bin/env python3
"""Hosted-only producer-shaped controls for the closed preparation route."""

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import re
import subprocess
import unittest
from unittest import mock
import zipfile

import control_plane_preparation_route as route
import test_prepare_control_plane as original_controls


ROOT = Path(__file__).resolve().parents[1]
CANARY = "private-body credential-token-canary /private/internal/path https://private.invalid"


class RouteFlowTests(unittest.TestCase):
    def setUp(self):
        # Reuse the original valid producer fixture; Git remains real throughout.
        self.fixture = original_controls.PrepareFlowTests(methodName="runTest")
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        fixture = self.fixture
        renamed = fixture.root / ".workflow-src"
        fixture.helper.rename(renamed)
        fixture.helper = renamed
        self.preparation = original_controls.prepare_control_plane
        for relative in self.preparation.SOURCE_PATHS:
            path = fixture.product / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("candidate\n")
        fixture.git(fixture.product, "add", "-A")
        fixture.git(fixture.product, "commit", "--quiet", "-m", "complete source fixture")
        fixture.target_sha = fixture.git(fixture.product, "rev-parse", "HEAD").decode().strip()
        self.addCleanup(mock.patch.stopall)
        mock.patch.object(route, "preparation", self.preparation).start()
        base_tree = fixture.git(fixture.base, "rev-parse", "HEAD^{tree}").decode().strip()
        target_tree = fixture.git(fixture.product, "rev-parse", "HEAD^{tree}").decode().strip()
        for key, value in (("BASE_SHA", fixture.base_sha), ("BASE_TREE", base_tree),
                           ("TARGET_SHA", fixture.target_sha), ("TARGET_TREE", target_tree)):
            mock.patch.object(route, key, value).start()
        temporary = fixture.root / "runner-temp"
        temporary.mkdir()
        self.env = {
            "PATH": os.environ["PATH"], "RUNNER_TEMP": str(temporary),
            "GITHUB_WORKSPACE": str(fixture.root), "PYTHONDONTWRITEBYTECODE": "1",
            "GITHUB_REPOSITORY": route.REPOSITORY, "GITHUB_SHA": fixture.helper_sha,
            "GITHUB_WORKFLOW_SHA": fixture.helper_sha,
            "GITHUB_REF": "refs/heads/validation/control-plane-fixture",
            "GITHUB_WORKFLOW_REF": route.REPOSITORY + "/" + route.CALLER
                + "@refs/heads/validation/control-plane-fixture",
            "GITHUB_RUN_ID": "12345", "GITHUB_RUN_ATTEMPT": "2", "GITHUB_JOB": "prepare",
        }
        mock.patch.dict(os.environ, self.env, clear=True).start()
        args = fixture.args()
        self.args = argparse.Namespace(**{key: getattr(args, key) for key in (
            "product", "base", "helper", "helper_sha", "target_sha", "base_sha", "base_ref",
        )}, receipt=str(temporary / "control-plane-preparation-12345-2.json"),
            patch=str(temporary / "control-plane-preparation-12345-2.patch"))
        self.calls = []
        self.launches = []

    def platform(self):
        return dict(self.env, workflow_id=route.WORKFLOW_ID, workflow_path=route.CALLER,
                    helper_tree=self.fixture.helper_tree, comparison_ref=self.args.base_ref,
                    comparison_ref_sha=self.args.base_sha,
                    inputs={"mode": "prepare-only", "preparation_profile": "control-plane",
                        "consumer_profile": "full", "target_sha": self.args.target_sha,
                        "base_sha": self.args.base_sha, "base_ref": self.args.base_ref,
                        "producer_run_id": "", "producer_workflow_host_sha": "",
                        "fixture_sha": "", "sdk_sha": ""},
                    run_conclusion="success", job_conclusion="success", step_conclusion="success",
                    job_id=123, step_number=18, step_name="Prepare exact candidate",
                    receipt_artifact_id=345, patch_artifact_id=346,
                    receipt_artifact_name="control-plane-preparation-receipt-12345-2",
                    patch_artifact_name="control-plane-preparation-patch-12345-2")

    def fake_helper(self, args, env, *, generate_path=None, return_override=None):
        self.assertEqual(args, self.args)
        self.assertEqual(route.helper_argv(args), (
            "python3", str(self.fixture.helper / ".github/scripts/prepare_control_plane.py"),
            "--product", args.product, "--base", args.base, "--helper", args.helper,
            "--target-sha", args.target_sha, "--base-sha", args.base_sha,
            "--helper-sha", args.helper_sha, "--base-ref", args.base_ref,
            "--patch", args.patch, "--receipt", args.receipt,
        ))
        self.assertEqual(env, route.helper_env(route.platform_context(self.env, args.helper_sha)))
        self.launches.append(route.helper_argv(args))
        fixed = self.fixture.mocked_run(calls=self.calls, generate_path=generate_path)
        with mock.patch.dict(os.environ, env, clear=True), mock.patch.object(
            self.preparation, "run", side_effect=fixed,
        ):
            result = self.preparation.prepare(args)
        self.assertEqual(result, 0)
        # Validate the actual whole producer bytes independently, before any mutation.
        patch = Path(args.patch).read_bytes()
        expected = self.fixture.expected_complete_receipt(args, b"", 0)
        expected["inventory"]["candidate_path_count"] = 25
        if generate_path is not None:
            self.assertEqual(generate_path, "MODULE.bazel.lock")
            blob = hashlib.sha1(b"blob 10\0generated\n").hexdigest()
            known_patch = (
                b"diff --git a/MODULE.bazel.lock b/MODULE.bazel.lock\nnew file mode 100644\n"
                + f"index {'0' * 40}..{blob}\n".encode()
                + b"--- /dev/null\n+++ b/MODULE.bazel.lock\n@@ -0,0 +1 @@\n+generated\n"
            )
            self.assertEqual(patch, known_patch)
            expected["inventory"]["changed_path_count"] = 1
            expected["patch"] = {"status": "emitted", "bytes": len(known_patch),
                                 "sha256": route.digest(known_patch)}
        else:
            self.assertEqual(patch, b"")
        self.assertEqual(Path(args.receipt).read_bytes(), self.fixture.serialize_expected(expected))
        phases = {argv for _, _, argv in self.preparation.PHASES}
        phase_calls = [(argv, cwd) for argv, cwd in self.calls if argv in phases]
        self.assertEqual(phase_calls, [(argv, self.preparation.phase_cwd(self.fixture.product, cwd))
                                      for _, cwd, argv in self.preparation.PHASES])
        for fixed in (self.preparation.SOURCE_STYLE_ARGV, self.preparation.SELF_TEST_ARGV):
            self.assertEqual([cwd for argv, cwd in self.calls if argv == fixed], [self.fixture.helper])
        return result if return_override is None else return_override

    def produce(self, generate_path=None, return_override=None):
        self.assertEqual(route.execute(self.args, preflight_only=True), 0)
        with mock.patch.object(route, "launch_helper", side_effect=lambda args, env:
                self.fake_helper(args, env, generate_path=generate_path, return_override=return_override)):
            return route.execute(self.args)

    def preflight(self, args=None, fault=None):
        args = self.args if args is None else args
        real_run = self.preparation.run
        roots = {str(self.fixture.helper), str(self.fixture.base), str(self.fixture.product)}
        suffixes = {("rev-parse", "HEAD^{commit}"), ("rev-parse", "HEAD^{tree}"),
                    ("status", "--porcelain=v1", "--untracked-files=all"),
                    ("rev-parse", args.base_sha + "^{tree}"),
                    ("diff", "--no-renames", "--raw", "-z", args.base_sha, args.target_sha)}
        calls = []
        def run(argv, cwd, env):
            self.assertEqual(argv[:2], ("git", "-C"))
            self.assertIn(argv[2], roots)
            self.assertIn(argv[3:], suffixes, "unexpected preflight command")
            self.assertEqual(cwd, Path(argv[2]))
            self.assertEqual(env, self.preparation.minimal_env())
            calls.append(argv)
            replacement = None if fault is None else fault(argv)
            return real_run(argv, cwd, env) if replacement is None else replacement
        with mock.patch.object(self.preparation, "run", side_effect=run), \
                mock.patch.object(route, "launch_helper", side_effect=AssertionError("must not launch")) as launch, \
                mock.patch("sys.stderr", new_callable=io.StringIO) as stderr, \
                mock.patch("sys.stdout", new_callable=io.StringIO) as stdout:
            result = route.execute(args, preflight_only=True)
        launch.assert_not_called()
        self.assertEqual(stdout.getvalue(), "")
        return result, stderr.getvalue(), calls

    def assert_preflight_diagnostic(self, code, phase, args=None, fault=None, witness=True):
        result, diagnostic, calls = self.preflight(args, fault)
        self.assertEqual((result, diagnostic), (1, json.dumps({"code": code, "phase": phase},
                         sort_keys=True, separators=(",", ":")) + "\n"))
        self.assertNotIn(CANARY, diagnostic)
        if witness:
            persisted = route.witness_path(self.args if args is None else args).read_bytes()
            self.assertNotIn(CANARY.encode(), persisted)
            outer = json.loads(persisted)
            outer_code = "checkout_preflight_failed" if code in route.PREFLIGHT_PREPARATION_CODES else code
            self.assertEqual((outer["status"], outer["complete"], outer["failure_code"],
                              outer["helper_exit"], outer["receipt"], outer["patch"]),
                             ("incomplete", False, outer_code, None, None, None))
            self.assertFalse(Path(self.args.receipt).exists())
            self.assertFalse(Path(self.args.patch).exists())
        return calls

    def members(self):
        return (Path(self.args.receipt).read_bytes(), route.witness_path(self.args).read_bytes(),
                Path(self.args.patch).read_bytes())

    def consume(self, members=None, platform=None):
        return route.validate_bundle(*(self.members() if members is None else members),
                                     self.args, self.platform() if platform is None else platform)

    def mutate_receipt(self, change):
        receipt, witness, patch = self.members()
        inner, outer = json.loads(receipt), json.loads(witness)
        change(inner)
        receipt = (json.dumps(inner, sort_keys=True, separators=(",", ":")) + "\n").encode()
        outer["receipt"] = {"bytes": len(receipt), "sha256": route.digest(receipt)}
        return receipt, route.encode_witness(outer), patch

    def test_complete_empty_bundle_is_consumed_as_patch_relative_to_target(self):
        self.assertEqual(self.produce(), 0)
        receipt, witness, patch = self.members()
        outer = json.loads(witness)
        expected = route.make_witness(self.args, route.platform_context(self.env, self.args.helper_sha))
        expected.update(status="complete", complete=True, identity={
            "helper_sha": self.args.helper_sha, "helper_tree": self.fixture.helper_tree,
            "base_sha": self.args.base_sha, "base_tree": route.BASE_TREE,
            "target_sha": self.args.target_sha, "target_tree": route.TARGET_TREE,
        }, helper_exit=0, receipt={"bytes": len(receipt), "sha256": route.digest(receipt)},
            patch={"bytes": 0, "sha256": route.digest(b"")})
        self.assertEqual(witness, route.encode_witness(expected))
        self.assertLessEqual(len(witness), 4096)
        self.assertEqual(outer["platform"]["caller_path"], route.CALLER)
        self.assertEqual(json.loads(receipt)["identity"]["workflow_path"], route.CALLEE)
        self.assertEqual(self.consume(), {"status": "accepted", "patch_base_sha": self.args.target_sha,
            "prepared_successor": "not_materialized", "helper_exit": 0,
            "receipt_sha256": route.digest(receipt), "patch_sha256": route.digest(patch)})
        self.assertEqual(len(self.launches), 1)

    def test_complete_generated_bundle_and_both_full_archives_are_consumed(self):
        self.assertEqual(self.produce(generate_path="MODULE.bazel.lock"), 0)
        receipt, witness, patch = self.members()
        def archive(members):
            stream = io.BytesIO()
            with zipfile.ZipFile(stream, "w") as bundle:
                for name, data in members.items():
                    bundle.writestr(name, data)
            return stream.getvalue()
        receipts = archive({Path(self.args.receipt).name: receipt, route.witness_path(self.args).name: witness})
        patches = archive({Path(self.args.patch).name: patch})
        platform = self.platform()
        platform.update(receipt_artifact_digest="sha256:" + route.digest(receipts),
                        patch_artifact_digest="sha256:" + route.digest(patches))
        result = route.consume_artifacts(receipts, patches, self.args, platform)
        self.assertEqual(result, dict(self.consume(), receipt_archive_sha256=route.digest(receipts),
                                      patch_archive_sha256=route.digest(patches),
                                      platform_digest_coverage={"receipt": "verified", "patch": "verified"}))
        unexposed = route.consume_artifacts(receipts, patches, self.args, self.platform())
        self.assertEqual(unexposed["platform_digest_coverage"], {"receipt": "not_exposed", "patch": "not_exposed"})
        wrong = dict(platform, patch_artifact_digest="sha256:" + "0" * 64)
        with self.assertRaises(route.RouteError):
            route.consume_artifacts(receipts, patches, self.args, wrong)
        # The actual patch applies to T, not the comparison checkout.
        (self.fixture.product / "MODULE.bazel.lock").unlink()
        self.fixture.git(self.fixture.product, "apply", "--check", self.args.patch)

    def test_stale_complete_inner_after_nonzero_helper_is_rejected(self):
        self.assertEqual(self.produce(return_override=1), 1)
        self.assertEqual(json.loads(Path(self.args.receipt).read_bytes())["status"], "complete")
        witness = json.loads(route.witness_path(self.args).read_bytes())
        self.assertEqual((witness["helper_exit"], witness["failure_code"], witness["complete"]),
                         (1, "helper_failed", False))
        with self.assertRaises(route.RouteError):
            self.consume()

    def test_platform_failure_cancellation_missing_or_conflicting_identity_rejects(self):
        self.assertEqual(self.produce(), 0)
        self.consume()
        mutations = {"run_conclusion": "cancelled", "job_conclusion": "failure",
            "step_conclusion": "skipped", "step_name": "wrong-step", "workflow_id": 1,
            "workflow_path": route.CALLEE, "job_id": None, "step_number": None,
            "GITHUB_JOB": "wrong-job", "GITHUB_RUN_ID": "6789", "GITHUB_RUN_ATTEMPT": "3",
            "GITHUB_WORKFLOW_SHA": "a" * 40, "comparison_ref_sha": "b" * 40,
            "comparison_ref": "wrong-ref", "helper_tree": "c" * 40,
            "receipt_artifact_id": 346, "patch_artifact_name": "stale-artifact"}
        for key, value in mutations.items():
            with self.subTest(key=key), self.assertRaises(route.RouteError):
                self.consume(platform=dict(self.platform(), **{key: value}))
        absent = self.platform()
        del absent["step_conclusion"]
        with self.assertRaises(route.RouteError):
            self.consume(platform=absent)
        wrong_inputs = self.platform()
        wrong_inputs["inputs"]["producer_run_id"] = "99"
        with self.assertRaises(route.RouteError):
            self.consume(platform=wrong_inputs)

    def test_manual_caller_is_truthful_but_not_registered_route_acceptance(self):
        os.environ["GITHUB_WORKFLOW_REF"] = route.REPOSITORY + "/" + route.CALLEE + "@" + self.env["GITHUB_REF"]
        self.env["GITHUB_WORKFLOW_REF"] = os.environ["GITHUB_WORKFLOW_REF"]
        self.assertEqual(self.produce(), 0)
        self.assertEqual(json.loads(self.members()[1])["platform"]["caller_path"], route.CALLEE)
        with self.assertRaises(route.RouteError):
            self.consume()

    def test_whole_inner_schema_and_required_phase_conservation_reject_mutations(self):
        self.assertEqual(self.produce(), 0)
        self.consume()
        mutations = [
            lambda inner: inner.update(extra=CANARY),
            lambda inner: inner["identity"].update(workflow_path=route.CALLER),
            lambda inner: inner["identity"].update(target_tree="a" * 40),
            lambda inner: inner.update(request_fingerprint="a" * 64),
            lambda inner: inner.update(catalog_fingerprint="a" * 64),
            lambda inner: inner.update(omissions=[CANARY]),
            lambda inner: inner["inventory"].update(omitted_path_count=1),
            lambda inner: inner["inventory"].update(candidate_path_count=24),
            lambda inner: inner["source_style"].update(status="failed"),
            lambda inner: inner["self_tests"].update(exit_code=1),
            lambda inner: inner["phases"]["metadata"].update(status="unknown"),
            lambda inner: inner["phases"].pop("clippy"),
            lambda inner: inner["fixed_commands"][0]["argv"].append("--unsafe"),
            lambda inner: inner["toolchain"]["observed"].update(rust="old"),
            lambda inner: inner["toolchain"]["commands"]["bazel"].update(exit_code=1),
            lambda inner: inner["toolchain"].update(dotslash_binary_pin_verified=True),
            lambda inner: inner["dependency_inventory"]["target_final"].update(external_sha256="b" * 64),
            lambda inner: inner["metadata_coverage"].update(unselected_locked_external_record_count=0),
            lambda inner: inner.update(workspace_package_count=True),
        ]
        for index, mutate in enumerate(mutations):
            with self.subTest(index=index), self.assertRaises(route.RouteError):
                self.consume(self.mutate_receipt(mutate))

    def test_whole_bytes_duplicate_keys_sizes_digest_and_witness_unknowns_reject(self):
        self.assertEqual(self.produce(), 0)
        receipt, witness, patch = self.members()
        self.consume()
        for members in ((receipt + b"x", witness, patch), (receipt, witness, b"changed"),
                        (receipt, b"{}", patch), (receipt, witness + b"x", patch),
                        (b"x" * (route.preparation.MAX_RECEIPT_BYTES + 1), witness, patch),
                        (receipt, witness, b"x" * (route.preparation.MAX_PATCH_BYTES + 1))):
            with self.assertRaises(route.RouteError):
                self.consume(members)
        for change in (lambda outer: outer.update(extra=CANARY),
                       lambda outer: outer.update(helper_exit=None),
                       lambda outer: outer.update(helper_exit=False),
                       lambda outer: outer["receipt"].update(bytes=True),
                       lambda outer: outer["expectations"].update(registered_workflow_id=1)):
            outer = json.loads(witness)
            change(outer)
            with self.assertRaises(route.RouteError):
                self.consume((receipt, route.encode_witness(outer), patch))
        with self.assertRaises(route.RouteError):
            route.strict_json(b'{"a":1,"a":2}', 4096)
        with self.assertRaises(route.RouteError):
            route.strict_json(b'{"a":NaN}', 4096)

    def test_missing_final_witness_and_failed_final_persistence_never_accept(self):
        self.assertEqual(route.execute(self.args, preflight_only=True), 0)
        actual_persist = route.persist_witness
        def persist(args, witness):
            if witness["complete"]:
                raise route.RouteError("witness_persist_failed")
            return actual_persist(args, witness)
        with mock.patch.object(route, "persist_witness", side_effect=persist), mock.patch.object(
            route, "launch_helper", side_effect=self.fake_helper,
        ):
            self.assertEqual(route.execute(self.args), 1)
        witness = json.loads(route.witness_path(self.args).read_bytes())
        self.assertEqual(witness["failure_code"], "witness_persist_failed")
        with self.assertRaises(route.RouteError):
            self.consume()
        route.witness_path(self.args).unlink()
        self.assertEqual(route.execute(self.args), 1)
        self.assertEqual(len(self.launches), 1)

    def test_permanent_persistence_fault_and_interrupt_have_no_false_complete(self):
        with mock.patch.object(route, "persist_witness", side_effect=OSError(CANARY)), mock.patch.object(
            route, "launch_helper", side_effect=AssertionError("must not launch"),
        ):
            self.assertEqual(route.execute(self.args, preflight_only=True), 1)
        self.assertFalse(route.witness_path(self.args).exists())
        self.assertEqual(route.execute(self.args, preflight_only=True), 0)
        with mock.patch.object(route, "launch_helper", side_effect=KeyboardInterrupt(CANARY)):
            self.assertEqual(route.execute(self.args), 1)
        persisted = route.witness_path(self.args).read_bytes()
        self.assertNotIn(CANARY.encode(), persisted)
        outer = json.loads(persisted)
        self.assertEqual((outer["helper_exit"], outer["failure_code"]), (None, "execution_interrupted"))

    def test_preflight_real_git_rejects_dirty_wrong_base_tree_and_untrusted_delta(self):
        self.assertEqual(route.execute(self.args, preflight_only=True), 0)
        with mock.patch.object(route, "launch_helper", side_effect=AssertionError("must not launch")):
            (self.fixture.product / "unexpected.txt").write_text(CANARY)
            self.assertEqual(route.execute(self.args, preflight_only=True), 1)
        self.assertNotIn(CANARY.encode(), route.witness_path(self.args).read_bytes())
        (self.fixture.product / "unexpected.txt").unlink()
        for key, value in (("BASE_SHA", "8" * 40), ("BASE_TREE", "8" * 40),
                           ("TARGET_TREE", "8" * 40)):
            with mock.patch.object(route, key, value):
                self.assertEqual(route.execute(self.args, preflight_only=True), 1)
        (self.fixture.product / "justfile").write_text("untrusted\n")
        self.fixture.git(self.fixture.product, "add", "justfile")
        self.fixture.git(self.fixture.product, "commit", "--quiet", "-m", "changed input")
        target = self.fixture.git(self.fixture.product, "rev-parse", "HEAD").decode().strip()
        tree = self.fixture.git(self.fixture.product, "rev-parse", "HEAD^{tree}").decode().strip()
        args = argparse.Namespace(**dict(vars(self.args), target_sha=target))
        with mock.patch.object(route, "TARGET_SHA", target), mock.patch.object(route, "TARGET_TREE", tree):
            self.assertEqual(route.execute(args, preflight_only=True), 1)

    def rejected_delta(self, relative, mutation, expected_code):
        self.assertEqual(route.execute(self.args, preflight_only=True), 0)
        path = self.fixture.product / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        mutation(path)
        self.fixture.git(self.fixture.product, "add", "-A")
        self.fixture.git(self.fixture.product, "commit", "--quiet", "-m", "out of contract")
        target = self.fixture.git(self.fixture.product, "rev-parse", "HEAD").decode().strip()
        tree = self.fixture.git(self.fixture.product, "rev-parse", "HEAD^{tree}").decode().strip()
        args = argparse.Namespace(**dict(vars(self.args), target_sha=target))
        # Prove the isolated cause directly, without a dirty/identity masking failure.
        self.assertTrue(self.preparation.clean_checkout(self.fixture.product))
        with self.assertRaises(self.preparation.PreparationError) as error:
            self.preparation.check_candidate_delta(self.fixture.product, args.base_sha, target, route.BASE_TREE)
        self.assertEqual(error.exception.code, expected_code)
        with mock.patch.object(route, "TARGET_SHA", target), mock.patch.object(route, "TARGET_TREE", tree), \
                mock.patch.object(route, "launch_helper", side_effect=AssertionError("must not launch")) as launch:
            self.assert_preflight_diagnostic(expected_code, "candidate_delta", args)
        launch.assert_not_called()

    def test_preflight_diagnostic_valid_baseline_preserves_real_call_order_and_witness(self):
        result, diagnostic, calls = self.preflight()
        self.assertEqual((result, diagnostic), (0, ""))
        expected = []
        for root in (self.fixture.helper, self.fixture.base, self.fixture.product):
            expected.extend(("git", "-C", str(root), *suffix) for suffix in (
                ("rev-parse", "HEAD^{commit}"), ("rev-parse", "HEAD^{tree}"),
                ("status", "--porcelain=v1", "--untracked-files=all")))
        expected.extend(("git", "-C", str(self.fixture.product), *suffix) for suffix in (
            ("rev-parse", self.args.base_sha + "^{tree}"),
            ("diff", "--no-renames", "--raw", "-z", self.args.base_sha, self.args.target_sha)))
        self.assertEqual(calls, expected)
        witness = route.make_witness(self.args, route.platform_context(self.env, self.args.helper_sha))
        witness["identity"] = {"helper_sha": self.args.helper_sha, "helper_tree": self.fixture.helper_tree,
            "base_sha": self.args.base_sha, "base_tree": route.BASE_TREE,
            "target_sha": self.args.target_sha, "target_tree": route.TARGET_TREE}
        self.assertEqual(route.witness_path(self.args).read_bytes(), route.encode_witness(witness))

    def test_preflight_diagnostic_real_configuration_guards_are_closed_and_isolated(self):
        self.assertEqual(self.preflight()[:2], (0, ""))
        home = Path(self.env["RUNNER_TEMP"]) / "control-plane-home"
        home.mkdir()
        for path, code in ((home / ".bazelrc", "unexpected_user_configuration"),
                           (self.fixture.product / "user.bazelrc", "unexpected_product_bazel_configuration")):
            path.write_text(CANARY)
            try:
                self.assert_preflight_diagnostic(code, "user_configuration")
            finally:
                path.unlink()
        with mock.patch.dict(os.environ, {"BAZELISK_GITHUB_TOKEN": CANARY}):
            self.assert_preflight_diagnostic("unexpected_execution_environment", "user_configuration")
        bazelrc = self.fixture.product / ".bazelrc"
        original = bazelrc.read_bytes()
        cases = ((None, "product_bazelrc_unreadable"),
                 ("common --remote_cache=https://private.invalid\n", "active_remote_bazel_configuration"),
                 ("common --config=private\n", "active_bazel_config_import"),
                 ("import /private/internal/path\n", "unexpected_product_bazelrc_import"))
        for contents, code in cases:
            if contents is None:
                bazelrc.unlink()
            else:
                bazelrc.write_text(contents)
            try:
                self.assert_preflight_diagnostic(code, "user_configuration")
            finally:
                bazelrc.write_bytes(original)
        with mock.patch.object(self.preparation, "PHASES", (("isolated", "", ("bazel", "--config=private")),)):
            self.assert_preflight_diagnostic("unexpected_bazel_configuration_selection", "user_configuration")
        self.assertEqual(self.preflight()[:2], (0, ""))

    def test_preflight_diagnostic_real_trusted_inputs_and_all_checkout_guards(self):
        self.assertEqual(self.preflight()[:2], (0, ""))
        trusted = self.fixture.product / "justfile"
        original = trusted.read_bytes()
        trusted.write_text(CANARY)
        try:
            self.assert_preflight_diagnostic("trusted_execution_input_changed", "trusted_inputs")
        finally:
            trusted.write_bytes(original)
        for key, root in (("helper", self.fixture.helper), ("base", self.fixture.base), ("target", self.fixture.product)):
            dirty = root / "unexpected.txt"
            dirty.write_text(CANARY)
            try:
                self.assert_preflight_diagnostic("checkout_preflight_failed", key + "_cleanliness")
            finally:
                dirty.unlink()
        for key in ("BASE_TREE", "TARGET_TREE"):
            with mock.patch.object(route, key, "8" * 40):
                self.assert_preflight_diagnostic("checkout_preflight_failed", "base_identity_guard" if key == "BASE_TREE" else "target_identity_guard")
        args = argparse.Namespace(**dict(vars(self.args), helper_sha="8" * 40))
        with mock.patch.dict(os.environ, {"GITHUB_SHA": args.helper_sha, "GITHUB_WORKFLOW_SHA": args.helper_sha}):
            self.assert_preflight_diagnostic("checkout_preflight_failed", "helper_identity_guard", args)

    def test_preflight_diagnostic_git_and_inventory_faults_keep_actual_fixed_phase(self):
        self.assertEqual(self.preflight()[:2], (0, ""))
        for key, root in (("helper", self.fixture.helper), ("base", self.fixture.base), ("target", self.fixture.product)):
            for suffix, phase in ((("rev-parse", "HEAD^{commit}"), key + "_identity"),
                                  (("status", "--porcelain=v1", "--untracked-files=all"), key + "_cleanliness")):
                command = ("git", "-C", str(root), *suffix)
                def fault(argv):
                    return subprocess.CompletedProcess(argv, 9, CANARY.encode(), CANARY.encode()) if argv == command else None
                calls = self.assert_preflight_diagnostic("git_command_failed", phase, fault=fault)
                self.assertEqual(calls[-1], command)
                def interrupted(argv):
                    if argv == command:
                        raise OSError(CANARY)
                calls = self.assert_preflight_diagnostic("unexpected_exception", phase, fault=interrupted)
                self.assertEqual(calls[-1], command)
        for suffix, output, code in (
            (("rev-parse", self.args.base_sha + "^{tree}"), b"8" * 40 + b"\n", "base_tree_mismatch"),
            (("diff", "--no-renames", "--raw", "-z", self.args.base_sha, self.args.target_sha), b"invalid\0entry\0", "diff_inventory_invalid"),
        ):
            command = ("git", "-C", str(self.fixture.product), *suffix)
            def fault(argv):
                return subprocess.CompletedProcess(argv, 0, output, CANARY.encode()) if argv == command else None
            calls = self.assert_preflight_diagnostic(code, "candidate_delta", fault=fault)
            self.assertEqual(calls[-1], command)

    def test_preflight_diagnostic_candidate_count_guard_still_rejects_allowed_extra_delta(self):
        self.assertEqual(self.preflight()[:2], (0, ""))
        lock = self.fixture.product / "codex-rs/Cargo.lock"
        lock.write_bytes(lock.read_bytes() + b"\n# isolated allowed extra delta\n")
        self.fixture.git(self.fixture.product, "add", "--", "codex-rs/Cargo.lock")
        self.fixture.git(self.fixture.product, "commit", "--quiet", "-m", "extra allowed delta")
        target = self.fixture.git(self.fixture.product, "rev-parse", "HEAD").decode().strip()
        tree = self.fixture.git(self.fixture.product, "rev-parse", "HEAD^{tree}").decode().strip()
        args = argparse.Namespace(**dict(vars(self.args), target_sha=target))
        self.assertEqual(self.preparation.check_candidate_delta(self.fixture.product, args.base_sha, target, route.BASE_TREE), 26)
        with mock.patch.object(route, "TARGET_SHA", target), mock.patch.object(route, "TARGET_TREE", tree):
            self.assert_preflight_diagnostic("checkout_preflight_failed", "candidate_delta", args)

    def test_preflight_diagnostic_unknown_malformed_codes_and_generic_canaries_never_echo(self):
        self.assertEqual(self.preflight()[:2], (0, ""))
        for value in (CANARY, None, True, 1, [], {}, "tool_version_mismatch_rust"):
            error = self.preparation.PreparationError(value)
            with mock.patch.object(self.preparation, "check_user_configuration", side_effect=error):
                self.assert_preflight_diagnostic("checkout_preflight_failed", "user_configuration")
            error = route.RouteError("outputs_invalid")
            error.code = value
            with mock.patch.object(self.preparation, "check_user_configuration", side_effect=error):
                self.assert_preflight_diagnostic("unexpected_exception", "user_configuration")
        for error, code in ((OSError(CANARY), "unexpected_exception"),
                            (KeyboardInterrupt(CANARY), "execution_interrupted"),
                            (route.RouteError("outputs_invalid"), "outputs_invalid")):
            with mock.patch.object(self.preparation, "check_trusted_inputs", side_effect=error):
                self.assert_preflight_diagnostic(code, "trusted_inputs")
        for code in route.FAILURE_CODES:
            with mock.patch.object(self.preparation, "check_user_configuration", side_effect=route.RouteError(code)):
                self.assert_preflight_diagnostic(code, "user_configuration")

    def test_preflight_diagnostic_early_operations_and_persistence_keep_original_red_result(self):
        self.assertEqual(self.preflight()[:2], (0, ""))
        for name, phase in (("platform_context", "platform_context"), ("validate_request", "validate_request"),
                            ("make_witness", "make_witness")):
            with mock.patch.object(route, name, side_effect=OSError(CANARY)):
                self.assert_preflight_diagnostic("unexpected_exception", phase, witness=False)
        real_resolve, resolves = Path.resolve, []
        def resolve(path, *args, **kwargs):
            if path == self.fixture.product:
                resolves.append(path)
                # Two existing request validations resolve the product first;
                # the third call is checkout_preflight's explicit path resolution.
                if len(resolves) == 3:
                    raise OSError(CANARY)
            return real_resolve(path, *args, **kwargs)
        with mock.patch.object(Path, "resolve", resolve):
            self.assert_preflight_diagnostic("unexpected_exception", "checkout_paths")
        self.assertEqual(len(resolves), 3)
        real_persist = route.persist_witness
        for failed_call, phase in ((1, "witness_initial_persistence"), (2, "witness_identity_persistence")):
            calls = []
            def persist(args, witness):
                calls.append(None)
                if len(calls) == failed_call:
                    raise route.RouteError("witness_persist_failed")
                return real_persist(args, witness)
            with mock.patch.object(route, "persist_witness", side_effect=persist):
                self.assert_preflight_diagnostic("witness_persist_failed", phase)
            self.assertEqual(len(calls), failed_call + 1)

    def test_preflight_reuses_real_outside_path_rejection_before_helper(self):
        self.rejected_delta("foreign.txt", lambda path: path.write_text("changed\n"),
                            "candidate_delta_outside_allowlist")

    def test_preflight_reuses_real_catalog_rejection_before_helper(self):
        self.rejected_delta(".github/validation-named-tests.json", lambda path: path.write_text("{}\n"),
                            "candidate_delta_outside_allowlist")

    def test_preflight_reuses_real_mode_rejection_before_helper(self):
        self.rejected_delta("docs/carry-divergence-ledger.md", lambda path: path.chmod(0o755),
                            "candidate_delta_mode_change")

    def test_preflight_reuses_real_deletion_rejection_before_helper(self):
        relative = "codex-rs/Cargo.lock"
        self.assertIn(relative, self.preparation.ALLOWED_PATHS)
        self.assertEqual(self.fixture.git(self.fixture.product, "show", self.args.base_sha + ":" + relative),
                         (self.fixture.product / relative).read_bytes())
        def delete_modified_tracked_file(path):
            path.write_bytes(path.read_bytes() + b"\n# modified candidate fixture\n")
            self.fixture.git(self.fixture.product, "add", "--", relative)
            self.fixture.git(self.fixture.product, "commit", "--quiet", "-m", "modified deletion fixture")
            self.assertEqual(self.fixture.git(self.fixture.product, "diff", "--no-renames", "--name-status",
                                             self.args.base_sha, "HEAD", "--", relative),
                             ("M\t" + relative + "\n").encode())
            path.unlink()
        self.rejected_delta(relative, delete_modified_tracked_file, "candidate_delta_outside_allowlist")
        self.assertEqual(self.fixture.git(self.fixture.product, "diff", "--no-renames", "--name-status",
                                         self.args.base_sha, "HEAD", "--", relative),
                         ("D\t" + relative + "\n").encode())

    def test_preflight_reuses_real_rename_rejection_before_helper(self):
        self.rejected_delta("docs/carry-divergence-ledger.md", lambda path: path.rename(path.with_name("foreign.md")),
                            "candidate_delta_outside_allowlist")

    def test_preflight_reuses_real_symlink_mode_rejection_without_body_read(self):
        def symlink(path):
            path.unlink()
            path.symlink_to("/unavailable-private-fixture")
        self.rejected_delta("docs/carry-divergence-ledger.md", symlink, "candidate_delta_mode_change")

    def test_invalid_platform_identity_is_not_truncated_or_persisted(self):
        for key, value in (("GITHUB_REPOSITORY", CANARY), ("GITHUB_REF", "refs/heads/../unsafe"),
                           ("GITHUB_JOB", CANARY), ("GITHUB_RUN_ID", "0"),
                           ("GITHUB_RUN_ATTEMPT", 1), ("GITHUB_WORKFLOW_REF", "unsupported"),
                           ("GITHUB_WORKFLOW_SHA", "a" * 40)):
            with self.subTest(key=key):
                with self.assertRaises(route.RouteError):
                    route.platform_context(dict(self.env, **{key: value}), self.args.helper_sha)
                if isinstance(value, str):
                    with mock.patch.dict(os.environ, {key: value}):
                        self.assertEqual(route.execute(self.args, preflight_only=True), 1)
                    self.assertFalse(route.witness_path(self.args).exists())

    def test_before_tool_user_configuration_and_remote_env_are_rejected(self):
        self.assertEqual(route.execute(self.args, preflight_only=True), 0)
        with mock.patch.dict(os.environ, {"BAZELISK_GITHUB_TOKEN": CANARY}), mock.patch.object(
            route, "launch_helper", side_effect=AssertionError("must not launch"),
        ):
            self.assertEqual(route.execute(self.args, preflight_only=True), 1)
        self.assertNotIn(CANARY.encode(), route.witness_path(self.args).read_bytes())

    def test_witness_cap_and_symlink_output_rejection_remain_red(self):
        with self.assertRaises(route.RouteError):
            route.encode_witness({"extra": "x" * 4097})
        target = self.fixture.root / "synthetic-private.txt"
        target.write_text(CANARY)
        link = self.fixture.root / "output-link"
        link.symlink_to(target)
        with self.assertRaises(route.RouteError):
            route.read_output(link, 4096)

    def test_actual_cli_preflight_parser_preserves_nine_values_and_rejects_extra_switch(self):
        argv = list(route.helper_argv(self.args)[2:])
        self.assertEqual(route.main(["--preflight", *argv]), 0)
        self.assertEqual(route.main(["--preflight", *argv, "--command", "unsafe"]), 2)
        self.assertEqual(len(self.launches), 0)

    def test_launch_exact_subprocess_env_cwd_streams_and_no_credential_forwarding(self):
        context = route.platform_context(self.env, self.args.helper_sha)
        with mock.patch.dict(os.environ, {"BAZELISK_GITHUB_TOKEN": CANARY, "PRIVATE_DATA": CANARY}):
            env = route.helper_env(context)
        self.assertFalse({"BAZELISK_GITHUB_TOKEN", "PRIVATE_DATA"} & env.keys())
        def launched(argv, **kwargs):
            self.assertEqual(argv, route.helper_argv(self.args))
            self.assertEqual(kwargs, {"cwd": self.fixture.root, "env": env, "shell": False,
                "check": False, "stdout": subprocess.DEVNULL, "stderr": subprocess.DEVNULL})
            return subprocess.CompletedProcess(argv, 9)
        with mock.patch.object(route.subprocess, "run", side_effect=launched) as call:
            self.assertEqual(route.launch_helper(self.args, env), 9)
        self.assertEqual(call.call_count, 1)


class SelectionTests(unittest.TestCase):
    def test_closed_actual_profile_validator_positive_and_each_negative(self):
        env = {"MODE": "prepare-only", "PREPARATION_PROFILE": "control-plane", "CONSUMER_PROFILE": "full",
               "BASE_SHA": route.BASE_SHA, "TARGET_SHA": route.TARGET_SHA, "BASE_REF": "validation/comparison"}
        route.validate_profile(env)
        for key, value in (("MODE", "build"), ("MODE", "consume-existing"),
                           ("PREPARATION_PROFILE", "unknown"), ("CONSUMER_PROFILE", "focused"),
                           ("BASE_SHA", "8" * 40), ("TARGET_SHA", "a" * 40),
                           ("BASE_REF", "../unsafe"), ("PRODUCER_RUN_ID", "1"),
                           ("PRODUCER_WORKFLOW_HOST_SHA", "a" * 40), ("FIXTURE_SHA", "a" * 40),
                           ("SDK_SHA", "a" * 40)):
            with self.subTest(key=key), self.assertRaises(route.RouteError):
                route.validate_profile(dict(env, **{key: value}))

    def test_actual_workflow_job_predicates_are_disjoint_and_preserve_old_routes(self):
        source = (ROOT / "workflows/sedna-branch-build.yml").read_text()
        def selected(job, mode, profile):
            block = re.search(r"^  " + re.escape(job) + r":\n(.*?)(?=^  [A-Za-z0-9_-]+:|\Z)",
                              source, re.M | re.S).group(1)
            condition = re.search(r"^    if: \$\{\{ (.*?) \}\}$", block, re.M).group(1)
            expression = condition.replace("inputs.mode", "mode").replace("inputs.preparation_profile", "profile")
            return eval(expression.replace("&&", "and").replace("||", "or"), {"__builtins__": {}},
                        {"mode": mode, "profile": profile})
        for mode in ("build", "consume-existing", "prepare-only"):
            for profile in ("cargo-schema", "config-schema", "tui-snapshots", "control-plane"):
                with self.subTest(mode=mode, profile=profile):
                    self.assertEqual(selected("control_plane_preparation", mode, profile),
                                     mode == "prepare-only" and profile == "control-plane")
                    self.assertEqual(selected("prepare-only", mode, profile),
                                     mode == "prepare-only" and profile != "control-plane")

    def test_actual_reusable_graph_and_fixed_workflow_call_source_are_bounded(self):
        import check_standard_runner_graph as graph
        assignments = graph.check_workflow_graph(ROOT.parent, ROOT / "workflows/sedna-branch-build.yml")
        self.assertIn((Path(".github/workflows/validation-control-plane-prep.yml"), "prepare", "ubuntu-24.04"), assignments)
        reusable = (ROOT / "workflows/validation-control-plane-prep.yml").read_text()
        call = reusable.split("  workflow_call:\n", 1)[1].split("  workflow_dispatch:\n", 1)[0]
        self.assertEqual(re.findall(r"^      ([a-z_]+):$", call, re.M), ["target_sha", "base_sha", "base_ref"])
        self.assertLess(reusable.index("id: preflight"), reusable.index("Set up pinned Rust toolchain"))
        self.assertIn("id: prepare", reusable)
        self.assertEqual(reusable.count("retention-days: 3"), 2)
        self.assertIn(".route.json", reusable)
        self.assertNotIn("secrets: inherit", reusable)

    def test_executable_common_input_guards_keep_old_modes_and_reject_invalid_combinations(self):
        source = (ROOT / "workflows/sedna-branch-build.yml").read_text()
        block = source.split("      - name: Validate immutable inputs and workflow host\n", 1)[1]
        block = block.split("        run: |\n", 1)[1].split("          actual_h=", 1)[0]
        script = "\n".join(line[10:] for line in block.splitlines())
        env = {"PATH": os.environ["PATH"], "EXPECTED_H": "a" * 40, "TARGET_SHA": route.TARGET_SHA,
            "BASE_SHA": route.BASE_SHA, "BASE_REF": "validation/comparison", "MODE": "prepare-only",
            "PREPARATION_PROFILE": "control-plane", "CONSUMER_PROFILE": "full",
            "PRODUCER_RUN_ID": "", "PRODUCER_WORKFLOW_HOST_SHA": "", "FIXTURE_SHA": "", "SDK_SHA": ""}
        def outcome(values):
            return subprocess.run(("bash", "-c", script), env=values, shell=False, check=False,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode
        self.assertEqual(outcome(env), 0)
        for mode, profile in (("build", "cargo-schema"), ("prepare-only", "cargo-schema"),
                              ("prepare-only", "config-schema"), ("prepare-only", "tui-snapshots")):
            self.assertEqual(outcome(dict(env, MODE=mode, PREPARATION_PROFILE=profile)), 0)
        for key, value in (("MODE", "unknown"), ("MODE", "build"), ("PREPARATION_PROFILE", "unknown"),
                           ("CONSUMER_PROFILE", "focused"), ("PRODUCER_RUN_ID", "1"),
                           ("TARGET_SHA", "bad"), ("BASE_SHA", "bad"), ("BASE_REF", "unsafe value")):
            self.assertNotEqual(outcome(dict(env, **{key: value})), 0)
        trusted_ref = "refs/heads/reconstruct/first-binary-package-workflow-20261001"
        consume = dict(env, MODE="consume-existing", PREPARATION_PROFILE="cargo-schema", PRODUCER_RUN_ID="1",
            PRODUCER_WORKFLOW_HOST_SHA="b" * 40, FIXTURE_SHA="c" * 40, SDK_SHA="d" * 40,
            CONSUMER_PROFILE="focused", GITHUB_REF=trusted_ref,
            GITHUB_REF_NAME=trusted_ref.removeprefix("refs/heads/"), GITHUB_SHA="a" * 40,
            GITHUB_WORKFLOW_REF=route.REPOSITORY + "/" + route.CALLER + "@" + trusted_ref)
        self.assertEqual(outcome(consume), 0)

    def test_executable_incoming_computer_guard_preserves_exact_target_and_wrong_base_rejection(self):
        source = (ROOT / "workflows/sedna-branch-build.yml").read_text()
        block = source.split("      - name: Verify H, B, T, and standard preparation architecture\n", 1)[1]
        block = block.split("        run: |\n", 1)[1].split("          actual_h=", 1)[0]
        script = "\n".join(line[10:] for line in block.splitlines()) + "\nprintf '%s' \"${computer_use}\"\n"
        env = {"PATH": os.environ["PATH"], "EXPECTED_H": "a" * 40, "MODE": "prepare-only",
            "PREPARATION_PROFILE": "cargo-schema", "TARGET_SHA": "3cd6436609ad59eed7f174a8c3aa6e3a67c1dbec",
            "BASE_SHA": "8389b61d82cb6fb936e4e500b977f31682441ffe"}
        def run(values):
            return subprocess.run(("bash", "-c", script), env=values, shell=False, check=False,
                                  stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        selected = run(env)
        self.assertEqual((selected.returncode, selected.stdout), (0, b"true"))
        self.assertNotEqual(run(dict(env, BASE_SHA=route.BASE_SHA)).returncode, 0)
        ordinary = run(dict(env, TARGET_SHA=route.TARGET_SHA))
        self.assertEqual((ordinary.returncode, ordinary.stdout), (0, b"false"))

    def test_archive_extra_duplicate_path_traversal_and_oversized_members_reject(self):
        for entries in ([('receipt.json', b'{}'), ('extra', CANARY.encode())],
                        [('receipt.json', b'{}'), ('receipt.json', b'{}')],
                        [('../receipt.json', b'{}')], [('receipt.json', b'x' * 5)]):
            stream = io.BytesIO()
            with zipfile.ZipFile(stream, "w") as bundle:
                for name, data in entries:
                    bundle.writestr(name, data)
            with self.assertRaises(route.RouteError):
                route.archive_members(stream.getvalue(), {"receipt.json": 4})


if __name__ == "__main__":
    unittest.main()
