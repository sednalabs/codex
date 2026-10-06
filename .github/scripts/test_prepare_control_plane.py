"""Focused controls for the bounded hosted preparation helper."""

from contextlib import contextmanager
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).with_name("prepare_control_plane.py")
SPEC = importlib.util.spec_from_file_location("prepare_control_plane", SCRIPT)
prepare_control_plane = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(prepare_control_plane)


@contextmanager
def isolated_runner_temp(path):
    with mock.patch.dict(
        prepare_control_plane.os.environ,
        {"RUNNER_TEMP": str(path)},
        clear=False,
    ):
        yield


class FixedContractTests(unittest.TestCase):
    def test_command_order_and_arguments_are_closed(self):
        self.assertEqual(
            [name for name, _, _ in prepare_control_plane.PHASES],
            [
                "workspace_update",
                "fix",
                "format",
                "bazel_lock_update",
                "metadata",
                "format_check",
                "clippy",
                "bazel_lock_check",
            ],
        )
        self.assertEqual(
            prepare_control_plane.PHASES[0][2],
            ("cargo", "update", "--workspace"),
        )
        self.assertEqual(
            prepare_control_plane.PHASES[1][2],
            (
                "just",
                "fix",
                "--locked",
                "-p",
                "codex-diagnostics",
                "-p",
                "codex-cli",
                "-p",
                "codex-state",
            ),
        )
        for _, _, argv in prepare_control_plane.PHASES:
            self.assertTrue(
                all(isinstance(arg, str) and "\n" not in arg for arg in argv)
            )
            self.assertFalse(any("REQUEST" in arg or "INPUT" in arg for arg in argv))
        with tempfile.TemporaryDirectory() as tempdir:
            product = Path(tempdir) / "product"
            self.assertEqual(
                prepare_control_plane.phase_cwd(product, "codex-rs"),
                product / "codex-rs",
            )
            self.assertEqual(
                prepare_control_plane.phase_cwd(product, "."),
                product,
            )

    def test_source_allowlist_matches_frozen_candidate_inventory(self):
        self.assertEqual(len(prepare_control_plane.SOURCE_PATHS), 25)
        self.assertEqual(
            prepare_control_plane.SOURCE_PATHS,
            frozenset(
                {
                    "codex-rs/cli/Cargo.toml",
                    "codex-rs/cli/src/control_plane.rs",
                    "codex-rs/cli/src/control_plane/envelopes.rs",
                    "codex-rs/cli/src/control_plane/summary.rs",
                    "codex-rs/cli/src/control_plane/usage.rs",
                    "codex-rs/cli/src/control_plane/usage_tests.rs",
                    "codex-rs/cli/src/control_plane_tests.rs",
                    "codex-rs/cli/src/main.rs",
                    "codex-rs/diagnostics/src/control_plane.rs",
                    "codex-rs/diagnostics/src/control_plane/lifecycle_timelines.rs",
                    "codex-rs/diagnostics/src/control_plane/recorder.rs",
                    "codex-rs/diagnostics/src/control_plane/summary.rs",
                    "codex-rs/diagnostics/src/control_plane/types.rs",
                    "codex-rs/diagnostics/src/control_plane/usage.rs",
                    "codex-rs/diagnostics/src/control_plane/usage_provider_reference.rs",
                    "codex-rs/diagnostics/src/control_plane/usage_tests.rs",
                    "codex-rs/diagnostics/src/control_plane_tests.rs",
                    "codex-rs/diagnostics/src/lib.rs",
                    "codex-rs/state/src/control_plane_usage.rs",
                    "codex-rs/state/src/control_plane_usage_reader.rs",
                    "codex-rs/state/src/control_plane_usage_tests.rs",
                    "codex-rs/state/src/lib.rs",
                    "docs/carry-divergence-ledger.md",
                    "docs/divergences/index.yaml",
                    "docs/downstream-regression-matrix.md",
                }
            ),
        )

    def test_checkout_identity_and_ref_grammars_are_closed(self):
        self.assertIsNotNone(prepare_control_plane.SHA_RE.fullmatch("a" * 40))
        self.assertIsNone(prepare_control_plane.SHA_RE.fullmatch("a" * 39))
        self.assertIsNotNone(
            prepare_control_plane.REF_RE.fullmatch("feature/example-1")
        )
        self.assertIsNone(prepare_control_plane.REF_RE.fullmatch("../credential"))
        self.assertFalse(prepare_control_plane.valid_ref("feature/../credential"))

    def test_rust_toolchain_and_component_versions_are_distinct_and_anchored(self):
        examples = {
            "rust": "rustc 1.95.0 (59807616e1 2025-04-14)",
            "rustfmt": "rustfmt 1.9.0-stable (59807616e1 2025-04-14)",
            "clippy": "clippy 0.1.95 (59807616e1 2025-04-14)",
        }
        for name, value in examples.items():
            self.assertIsNotNone(
                prepare_control_plane.TOOL_VERSION_PATTERNS[name].fullmatch(value)
            )
        self.assertIsNone(
            prepare_control_plane.TOOL_VERSION_PATTERNS["rustfmt"].fullmatch(
                "rustfmt 1.95.0"
            )
        )
        self.assertIsNone(
            prepare_control_plane.TOOL_VERSION_PATTERNS["clippy"].fullmatch(
                "clippy 0.1.95.0"
            )
        )
        supported = {
            "uv": b"uv 0.11.3 (x86_64-unknown-linux-gnu)\r\n",
            "rust": b"rustc 1.95.0\n",
            "rustfmt": b"rustfmt 1.9.0\n",
            "clippy": b"clippy 0.1.95\n",
            "just": b"just 1.51.0\n",
            "dotslash": b"DotSlash 0.5.8\n",
        }
        for name, raw in supported.items():
            self.assertIsNotNone(prepare_control_plane.parse_version_output(raw, name))
        self.assertIsNotNone(
            prepare_control_plane.parse_version_output(
                b"uv 0.11.3 (abc1234 2026-01-02 x86_64-unknown-linux-gnu)\n",
                "uv",
            )
        )
        bad = {
            "uv prefix collision": ("uv", b"uv 0.11.30 (x86_64-unknown-linux-gnu)\n"),
            "uv development suffix": (
                "uv",
                b"uv 0.11.3+1 (x86_64-unknown-linux-gnu)\n",
            ),
            "uv split metadata": (
                "uv",
                b"uv 0.11.3 (abc1234 2026-01-02) (x86_64-unknown-linux-gnu)\n",
            ),
            "uv arbitrary target": ("uv", b"uv 0.11.3 (not-a-target body-canary)\n"),
            "uv well-formed not-a-target": ("uv", b"uv 0.11.3 (not-a-target)\n"),
            "rust beta": ("rust", b"rustc 1.95.0-beta\n"),
            "rust nightly": ("rust", b"rustc 1.95.0-nightly\n"),
            "extra line": ("just", b"just 1.51.0\nextra\n"),
            "leading whitespace": ("just", b" just 1.51.0\n"),
            "invalid utf8": ("just", b"just \xff\n"),
            "oversize": ("just", b"x" * 257),
        }
        for label, (name, raw) in bad.items():
            with self.subTest(label=label):
                self.assertIsNone(prepare_control_plane.parse_version_output(raw, name))
        for raw in (
            b"Bazelisk v1.28.1\nbazel 9.0.0\n",
            b"Bazelisk v1.28.1\r\nbazel 9.0.0\r\n",
        ):
            self.assertIsNotNone(
                prepare_control_plane.parse_version_output(raw, "bazel_version_pair")
            )
        for raw in (
            b"Bazelisk v1.28.1+development\nbazel 9.0.0\n",
            b"Bazelisk v1.28.1\nbazel 9.0.0\nextra\n",
            b"Bazelisk v1.28.1\n",
            b"Bazelisk v1.28.1\nbazel 9.0.1\n",
        ):
            self.assertIsNone(
                prepare_control_plane.parse_version_output(raw, "bazel_version_pair")
            )
        args = type(
            "Args",
            (),
            {
                "base_sha": "b" * 40,
                "target_sha": "a" * 40,
                "helper_sha": "c" * 40,
                "base_ref": "main",
            },
        )()
        receipt = prepare_control_plane.make_receipt(args)
        self.assertEqual(receipt["toolchain"]["rust"], "1.95.0")
        self.assertEqual(receipt["toolchain"]["rustfmt"], "1.9.0")
        self.assertEqual(receipt["toolchain"]["clippy"], "0.1.95")

    def test_toolchain_rejects_old_wrong_rustfmt_identity_with_coded_receipt(self):
        outputs = iter(
            (
                b"rustc 1.95.0 (59807616e1 2025-04-14)\n",
                b"rustfmt 1.95.0\n",
            )
        )
        receipt = {
            "toolchain": {"status": "not_run", "failure_code": None, "observed": {}}
        }
        with tempfile.TemporaryDirectory() as tempdir:
            path = Path(tempdir) / "receipt.json"
            with mock.patch.object(
                prepare_control_plane,
                "run",
                side_effect=lambda argv, cwd, env: subprocess.CompletedProcess(
                    argv, 0, next(outputs), b""
                ),
            ):
                with self.assertRaisesRegex(
                    prepare_control_plane.PreparationError,
                    "tool_version_mismatch_rustfmt",
                ):
                    prepare_control_plane.check_toolchain(
                        {}, Path(tempdir), receipt, path
                    )
            recorded = json.loads(path.read_text(encoding="utf-8"))
            self.assertEqual(recorded["toolchain"]["observed"], {"rust": "1.95.0"})
            self.assertEqual(
                recorded["toolchain"]["failure_code"], "tool_version_mismatch_rustfmt"
            )

    def test_workflow_installs_the_exact_individual_dotslash_action(self):
        workflow = (
            Path(__file__).parents[1] / "workflows/validation-control-plane-prep.yml"
        )
        contents = workflow.read_text(encoding="utf-8")
        self.assertIn(
            "facebook/install-dotslash@1e4e7b3e07eaca387acb98f1d4720e0bee8dbb6a",
            contents,
        )
        self.assertEqual(contents.count("facebook/install-dotslash@"), 1)
        self.assertIn('("dotslash", "--version")', SCRIPT.read_text(encoding="utf-8"))

    def test_fix_phase_requests_cargo_json_without_changing_its_recipe(self):
        fix = next(phase for phase in prepare_control_plane.PHASES if phase[0] == "fix")
        self.assertEqual(
            fix,
            (
                "fix",
                ".",
                (
                    "just",
                    "fix",
                    "--locked",
                    "-p",
                    "codex-diagnostics",
                    "-p",
                    "codex-cli",
                    "-p",
                    "codex-state",
                    "--message-format=json",
                ),
            ),
        )

    def test_legacy_diagnostic_job_never_prints_cargo_log_tails(self):
        workflow = (
            Path(__file__).parents[1] / "workflows/validation-control-plane-prep.yml"
        )
        contents = workflow.read_text(encoding="utf-8")
        self.assertNotIn('tail -n 80 "${update_log}"', contents)
        self.assertNotIn('tail -n 120 "${fix_log}"', contents)
        self.assertNotIn('tail -n 30 "${fix_log}"', contents)
        self.assertIn("raw output withheld", contents)

    def test_metadata_covers_locked_subset_without_checksum_fiction(self):
        before = {
            "packages": [
                {"name": "workspace", "version": "1", "source": None},
                {"name": "dep", "version": "1.0", "source": "registry+index"},
            ]
        }
        lock = (
            frozenset({("workspace", "1")}),
            prepare_control_plane.Counter(
                {
                    ("dep", "1.0", "registry+index", "locked-checksum-a"): 1,
                    ("optional-dep", "2.0", "registry+index", "locked-checksum-b"): 1,
                }
            ),
        )
        prepare_control_plane.compare_metadata_to_lock(before, lock)
        receipt = {}
        prepare_control_plane.record_metadata_coverage(receipt, before, lock)
        self.assertEqual(receipt["metadata_coverage"]["external_package_count"], 1)
        self.assertEqual(
            receipt["metadata_coverage"]["locked_external_record_count"], 2
        )
        self.assertEqual(
            receipt["metadata_coverage"]["unselected_locked_external_record_count"], 1
        )
        with self.assertRaisesRegex(
            prepare_control_plane.PreparationError,
            "external_metadata_not_in_lock",
        ):
            prepare_control_plane.compare_metadata_to_lock(
                {
                    "packages": before["packages"]
                    + [{"name": "unlocked", "version": "9", "source": "registry+index"}]
                },
                lock,
            )

    def test_lock_inventory_compares_workspace_and_external_records(self):
        package_data = (
            "version = 4\n"
            '[[package]]\nname = "workspace"\nversion = "1.0"\n\n'
            '[[package]]\nname = "dep"\nversion = "2.0"\n'
            'source = "registry+index"\nchecksum = "abc"\n'
        )
        with tempfile.TemporaryDirectory() as tempdir:
            lock = Path(tempdir) / "Cargo.lock"
            lock.write_text(package_data, encoding="utf-8")
            before = prepare_control_plane.lock_inventory(lock)
            self.assertEqual(before[0], frozenset({("workspace", "1.0")}))
            self.assertEqual(sum(before[1].values()), 1)
            prepare_control_plane.compare_lock_inventories(before, before, "changed")
            lock.write_text(
                package_data.replace('checksum = "abc"', 'checksum = "def"'),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(
                prepare_control_plane.PreparationError,
                "input_external_package_records_changed",
            ):
                prepare_control_plane.compare_lock_inventories(
                    before,
                    prepare_control_plane.lock_inventory(lock),
                    "input_external_package_records_changed",
                )

    def test_metadata_requires_workspace_match_and_real_lock_identities(self):
        metadata = {
            "packages": [
                {"name": "workspace", "version": "1.0", "source": None},
                {
                    "name": "dep",
                    "version": "2.0",
                    "source": "registry+index",
                },
            ]
        }
        lock = (
            frozenset({("workspace", "1.0")}),
            prepare_control_plane.Counter({("dep", "2.0", "registry+index", "abc"): 1}),
        )
        prepare_control_plane.compare_metadata_to_lock(metadata, lock)
        metadata["packages"].append(
            {"name": "another", "version": "2.0", "source": "registry+index"}
        )
        with self.assertRaisesRegex(
            prepare_control_plane.PreparationError,
            "external_metadata_not_in_lock",
        ):
            prepare_control_plane.compare_metadata_to_lock(metadata, lock)

    def test_lock_fingerprints_conserve_checksum_and_workspace_changes(self):
        before = (
            frozenset({("workspace", "1.0")}),
            prepare_control_plane.Counter(
                {("dep", "2.0", "registry+index", "checksum-a"): 1}
            ),
        )
        changed_checksum = (
            before[0],
            prepare_control_plane.Counter(
                {("dep", "2.0", "registry+index", "checksum-b"): 1}
            ),
        )
        with self.assertRaisesRegex(
            prepare_control_plane.PreparationError,
            "input_external_package_records_changed",
        ):
            prepare_control_plane.compare_lock_inventories(
                before, changed_checksum, "input_external_package_records_changed"
            )
        receipt = {"dependency_inventory": {}}
        prepare_control_plane.record_lock_fingerprint(receipt, "before", before)
        prepare_control_plane.record_lock_fingerprint(
            receipt, "after", changed_checksum
        )
        self.assertNotEqual(
            receipt["dependency_inventory"]["before"]["external_sha256"],
            receipt["dependency_inventory"]["after"]["external_sha256"],
        )


class PatchInventoryTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.path = "codex-rs/Cargo.lock"
        (self.root / self.path).parent.mkdir(parents=True)
        (self.root / self.path).write_text("version = 1\n", encoding="utf-8")
        self.git("init", "--quiet")
        self.git("add", self.path)
        self.git(
            "-c",
            "user.name=Hosted Test",
            "-c",
            "user.email=hosted@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        )

    def git(self, *args):
        return subprocess.check_output(
            ["git", "-C", str(self.root), *args], stderr=subprocess.DEVNULL
        )

    def test_noop_delta_is_a_complete_empty_patch(self):
        with tempfile.TemporaryDirectory() as tempdir:
            index = Path(tempdir) / "index"
            self.assertEqual(prepare_control_plane.changed_paths(self.root, index), [])
            self.assertEqual(prepare_control_plane.patch_bytes(self.root, index), b"")

    def test_unexpected_paths_are_visible_to_closed_inventory(self):
        unexpected = self.root / "codex-rs/unexpected.rs"
        unexpected.write_text("// not admitted\n", encoding="utf-8")
        with tempfile.TemporaryDirectory() as tempdir:
            with self.assertRaisesRegex(
                prepare_control_plane.PreparationError,
                "candidate_delta_outside_allowlist",
            ):
                prepare_control_plane.changed_paths(self.root, Path(tempdir) / "index")
        self.assertNotIn("codex-rs/unexpected.rs", prepare_control_plane.ALLOWED_PATHS)

    def test_generated_deletions_and_mode_changes_reject(self):
        with tempfile.TemporaryDirectory() as tempdir:
            index = Path(tempdir) / "delete-index"
            self.git("rm", "--quiet", self.path)
            with self.assertRaisesRegex(
                prepare_control_plane.PreparationError,
                "candidate_delta_outside_allowlist",
            ):
                prepare_control_plane.changed_paths(self.root, index)

    def test_generated_mode_changes_reject(self):
        (self.root / self.path).chmod(0o755)
        with tempfile.TemporaryDirectory() as tempdir:
            with self.assertRaisesRegex(
                prepare_control_plane.PreparationError,
                "candidate_delta_mode_change",
            ):
                prepare_control_plane.changed_paths(
                    self.root, Path(tempdir) / "mode-index"
                )

    def test_candidate_delta_accepts_admitted_change_and_rejects_foreign_path(self):
        base_sha = self.git("rev-parse", "HEAD").decode().strip()
        base_tree = self.git("rev-parse", "HEAD^{tree}").decode().strip()
        (self.root / self.path).write_text("version = 2\n", encoding="utf-8")
        self.git("add", self.path)
        self.git(
            "-c",
            "user.name=Hosted Test",
            "-c",
            "user.email=hosted@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "candidate",
        )
        target_sha = self.git("rev-parse", "HEAD").decode().strip()
        self.assertEqual(
            prepare_control_plane.check_candidate_delta(
                self.root, base_sha, target_sha, base_tree
            ),
            1,
        )
        (self.root / "unexpected.txt").write_text("foreign\n", encoding="utf-8")
        self.git("add", "unexpected.txt")
        self.git(
            "-c",
            "user.name=Hosted Test",
            "-c",
            "user.email=hosted@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "foreign",
        )
        foreign_sha = self.git("rev-parse", "HEAD").decode().strip()
        with self.assertRaises(prepare_control_plane.PreparationError):
            prepare_control_plane.check_candidate_delta(
                self.root, base_sha, foreign_sha, base_tree
            )

    def test_patch_overflow_is_incomplete(self):
        with self.assertRaisesRegex(
            prepare_control_plane.PreparationError, "patch_overflow"
        ):
            prepare_control_plane.validate_patch_size(
                b"x" * (prepare_control_plane.MAX_PATCH_BYTES + 1)
            )


class PrepareFlowTests(unittest.TestCase):
    def git(self, root, *args):
        return subprocess.check_output(
            ["git", "-C", str(root), *args], stderr=subprocess.DEVNULL
        )

    def repository(self, path, files):
        path.mkdir()
        for relative, content in files.items():
            target = path / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(content, encoding="utf-8")
        self.git(path, "init", "--quiet")
        self.git(path, "add", "-A")
        self.git(
            path,
            "-c",
            "user.name=Hosted Test",
            "-c",
            "user.email=hosted@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        )
        return self.git(path, "rev-parse", "HEAD").decode().strip()

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.product = self.root / "product"
        self.base = self.root / "base"
        self.helper = self.root / "helper"
        self.base_sha = self.repository(
            self.base,
            {
                "codex-rs/Cargo.lock": (
                    "version = 3\n\n"
                    '[[package]]\nname = "workspace"\nversion = "1.0"\n\n'
                    '[[package]]\nname = "dep"\nversion = "2.0"\n'
                    'source = "registry+index"\nchecksum = "checksum-a"\n\n'
                    '[[package]]\nname = "inactive-dep"\nversion = "3.0"\n'
                    'source = "registry+index"\nchecksum = "checksum-b"\n'
                ),
                "codex-rs/state/src/control_plane_usage_reader.rs": (
                    "pub fn fixture() {}\n"
                ),
                ".bazelrc": "",
                ".bazelversion": "9.0.0\n",
                "justfile": "",
                "scripts/format.py": "",
            },
        )
        self.git(self.root, "clone", "--quiet", str(self.base), str(self.product))
        self.git(self.product, "config", "user.name", "Hosted Test")
        self.git(self.product, "config", "user.email", "hosted@example.invalid")
        (self.product / "docs").mkdir()
        (self.product / "docs/carry-divergence-ledger.md").write_text(
            "candidate\n", encoding="utf-8"
        )
        self.git(self.product, "add", "docs/carry-divergence-ledger.md")
        self.git(self.product, "commit", "--quiet", "-m", "candidate")
        self.target_sha = self.git(self.product, "rev-parse", "HEAD").decode().strip()
        self.helper_sha = self.repository(self.helper, {"helper.py": "trusted\n"})
        self.helper_tree = (
            self.git(self.helper, "rev-parse", "HEAD^{tree}").decode().strip()
        )

    def args(self):
        return type(
            "Args",
            (),
            {
                "product": str(self.product),
                "base": str(self.base),
                "helper": str(self.helper),
                "helper_sha": self.helper_sha,
                "target_sha": self.target_sha,
                "base_sha": self.base_sha,
                "base_ref": "main",
                "receipt": str(self.root / "receipt.json"),
                "patch": str(self.root / "candidate.patch"),
            },
        )()

    def mocked_run(
        self,
        fail_phase=None,
        calls=None,
        generate_path=None,
        generate_unexpected=False,
        interrupt_phase=None,
        interrupt_command=None,
        change_generated_lock=False,
    ):
        actual_run = prepare_control_plane.run
        versions = {
            ("rustc", "--version"): b"rustc 1.95.0 (59807616e1 2025-04-14)\n",
            ("rustfmt", "--version"): b"rustfmt 1.9.0-stable (59807616e1 2025-04-14)\n",
            (
                "cargo",
                "clippy",
                "--version",
            ): b"clippy 0.1.95 (59807616e1 2025-04-14)\n",
            ("just", "--version"): b"just 1.51.0\n",
            ("uv", "--version"): b"uv 0.11.3 (x86_64-unknown-linux-gnu)\n",
            ("bazel", "version", "--gnu_format"): b"Bazelisk v1.28.1\nbazel 9.0.0\n",
            ("bazel", "--version"): b"bazel 9.0.0\n",
            ("dotslash", "--version"): b"DotSlash 0.5.8\n",
        }

        def fake(argv, cwd, env):
            if calls is not None:
                calls.append((argv, cwd))
            if argv[0] == "git":
                return actual_run(argv, cwd, env)
            if argv == interrupt_command:
                raise OSError("credential-token-canary preflight detail")
            if (
                argv == prepare_control_plane.SOURCE_STYLE_ARGV
                or argv == prepare_control_plane.SELF_TEST_ARGV
            ):
                return subprocess.CompletedProcess(argv, 0, b"", b"")
            if argv in versions:
                return subprocess.CompletedProcess(argv, 0, versions[argv], b"")
            phase = next(
                (
                    name
                    for name, _, phase_argv in prepare_control_plane.PHASES
                    if argv == phase_argv
                ),
                None,
            )
            if phase == interrupt_phase:
                raise OSError("credential-token-canary invocation detail")
            if phase == "metadata":
                return subprocess.CompletedProcess(
                    argv,
                    0,
                    (
                        b'{"packages":[{"name":"workspace","version":"1.0","source":null},'
                        b'{"name":"dep","version":"2.0","source":"registry+index"}]}'
                    ),
                    b"",
                )
            if phase == "workspace_update" and change_generated_lock:
                lock = self.product / "codex-rs/Cargo.lock"
                lock.write_text(
                    lock.read_text(encoding="utf-8").replace(
                        'checksum = "checksum-a"',
                        'checksum = "upgraded-checksum"',
                    ),
                    encoding="utf-8",
                )
            if phase == fail_phase:
                if phase == "fix":
                    source_dir = self.product / "codex-rs/state/src"
                    source_file = source_dir / "control_plane_usage_reader.rs"
                    diagnostic = {
                        "reason": "compiler-message",
                        "message": {
                            "level": "error",
                            "code": {"code": "E0277"},
                            "message": "private output",
                            "rendered": "credential-token-canary",
                            "spans": [
                                {
                                    "file_name": str(source_file),
                                    "line_start": 41,
                                    "column_start": 3,
                                    "is_primary": True,
                                }
                            ],
                        },
                    }
                    return subprocess.CompletedProcess(
                        argv,
                        9,
                        json.dumps(diagnostic).encode("utf-8"),
                        b"credential-token-canary",
                    )
                return subprocess.CompletedProcess(
                    argv, 9, b"private output", b"credential-token-canary"
                )
            if phase:
                if phase == "format" and generate_path:
                    output = self.product / generate_path
                    output.parent.mkdir(parents=True, exist_ok=True)
                    output.write_text("generated\n", encoding="utf-8")
                if phase == "format" and generate_unexpected:
                    (self.product / "unexpected-generated.txt").write_text(
                        "credential-token-canary\n", encoding="utf-8"
                    )
                return subprocess.CompletedProcess(argv, 0, b"", b"")
            raise AssertionError("unexpected fixed command")

        return fake

    def expected_complete_receipt(self, args, patch, changed_path_count):
        base_packages = [["workspace", "1.0"]]
        external_records = [
            ["dep", "2.0", "registry+index", "checksum-a", 1],
            ["inactive-dep", "3.0", "registry+index", "checksum-b", 1],
        ]
        workspace_digest = hashlib.sha256(
            json.dumps(base_packages, separators=(",", ":")).encode()
        ).hexdigest()
        external_digest = hashlib.sha256(
            json.dumps(external_records, separators=(",", ":")).encode()
        ).hexdigest()
        lock_fingerprint = {
            "workspace_package_count": 1,
            "workspace_sha256": workspace_digest,
            "external_record_count": 2,
            "external_sha256": external_digest,
        }
        known_patch = b""
        if changed_path_count:
            content = b"generated\n"
            blob_hash = hashlib.sha1(b"blob 10\0" + content).hexdigest()
            known_patch = (
                b"diff --git a/docs/downstream-regression-matrix.md b/docs/downstream-regression-matrix.md\n"
                b"new file mode 100644\n"
                + f"index 0000000000000000000000000000000000000000..{blob_hash}\n".encode()
                + b"--- /dev/null\n"
                b"+++ b/docs/downstream-regression-matrix.md\n"
                b"@@ -0,0 +1 @@\n"
                b"+generated\n"
            )
        self.assertEqual(patch, known_patch)
        expected = {
            "schema": "control-plane-preparation-v1",
            "status": "complete",
            "failure_code": None,
            "identity": {
                "repository": prepare_control_plane.os.environ.get(
                    "GITHUB_REPOSITORY", "unavailable"
                ),
                "workflow_path": ".github/workflows/validation-control-plane-prep.yml",
                "workflow_commit": prepare_control_plane.os.environ.get(
                    "GITHUB_SHA", "unavailable"
                ),
                "helper_sha": args.helper_sha,
                "base_sha": args.base_sha,
                "base_tree": self.git(self.base, "rev-parse", "HEAD^{tree}")
                .decode()
                .strip(),
                "target_sha": args.target_sha,
                "target_tree": self.git(self.product, "rev-parse", "HEAD^{tree}")
                .decode()
                .strip(),
                "workflow_ref": prepare_control_plane.os.environ.get(
                    "GITHUB_REF", "unavailable"
                ),
                "comparison_ref": args.base_ref,
                "helper_tree": self.helper_tree,
                "run_id": prepare_control_plane.os.environ.get(
                    "GITHUB_RUN_ID", "unavailable"
                ),
                "run_attempt": prepare_control_plane.os.environ.get(
                    "GITHUB_RUN_ATTEMPT", "unavailable"
                ),
            },
            "request_fingerprint": hashlib.sha256(
                json.dumps(
                    [args.base_sha, args.target_sha, args.base_ref],
                    separators=(",", ":"),
                ).encode()
            ).hexdigest(),
            "catalog_fingerprint": hashlib.sha256(
                "\n".join(sorted(prepare_control_plane.ALLOWED_PATHS)).encode()
            ).hexdigest(),
            "toolchain": {
                "status": "expected_versions_and_installer_provenance_observed",
                "failure_code": None,
                "observed": {
                    "rust": "1.95.0",
                    "rustfmt": "1.9.0-stable",
                    "clippy": "0.1.95",
                    "just": "1.51.0",
                    "uv": "0.11.3",
                    "bazelisk": "1.28.1",
                    "bazel": "9.0.0",
                    "bazel_from_version_pair": "9.0.0",
                    "dotslash": "0.5.8",
                },
                "commands": {
                    name: {"status": "passed", "exit_code": 0}
                    for name in (
                        "rust",
                        "rustfmt",
                        "clippy",
                        "just",
                        "uv",
                        "bazelisk",
                        "bazel",
                        "dotslash",
                    )
                },
                "current_phase": None,
                "rust": "1.95.0",
                "rustfmt": "1.9.0",
                "clippy": "0.1.95",
                "just": "1.51.0",
                "uv": "0.11.3",
                "bazelisk": "1.28.1",
                "bazel": "9.0.0",
                "dotslash_expected": "unknown_dynamic_latest",
                "dotslash_binary_selection": "dynamic_latest",
                "dotslash_installer_action_sha": "1e4e7b3e07eaca387acb98f1d4720e0bee8dbb6a",
                "dotslash_binary_pin_verified": False,
            },
            "fixed_commands": [
                {
                    "phase": "source_style",
                    "cwd": "workflow",
                    "argv": list(prepare_control_plane.SOURCE_STYLE_ARGV),
                },
                {
                    "phase": "self_tests",
                    "cwd": "workflow",
                    "argv": list(prepare_control_plane.SELF_TEST_ARGV),
                },
                *[
                    {"phase": name, "cwd": cwd, "argv": list(argv)}
                    for name, cwd, argv in prepare_control_plane.PHASES
                ],
            ],
            "self_tests": {"status": "passed", "exit_code": 0},
            "source_style": {"status": "passed", "exit_code": 0},
            "preflight_current_phase": None,
            "phases": {
                name: {"status": "passed", "exit_code": 0}
                for name, _, _ in prepare_control_plane.PHASES
            },
            "inventory": {
                "candidate_path_count": 1,
                "changed_path_count": changed_path_count,
                "omitted_path_count": 0,
            },
            "workspace_package_count": 1,
            "external_package_record_count": 1,
            "dependency_inventory": {
                name: lock_fingerprint
                for name in (
                    "base",
                    "target_initial",
                    "target_after_update",
                    "target_final",
                )
            },
            "metadata_coverage": {
                "coverage_scope": "selected_resolved_external_packages_only",
                "workspace_package_count": 1,
                "external_package_count": 1,
                "locked_external_record_count": 2,
                "selected_external_identity_sha256": hashlib.sha256(
                    b'[["dep","2.0","registry+index",1]]'
                ).hexdigest(),
                "unselected_locked_external_record_count": 1,
            },
            "patch": {
                "status": "emitted",
                "bytes": len(known_patch),
                "sha256": hashlib.sha256(known_patch).hexdigest(),
            },
            "omissions": [],
        }
        return expected

    def expected_before_preflight_failure(self, args, code):
        expected = self.expected_complete_receipt(args, b"", 0)
        expected.update(status="incomplete", failure_code=code)
        expected["identity"]["helper_tree"] = None
        expected["inventory"] = {
            "changed_path_count": 0,
            "omitted_path_count": 0,
        }
        expected["source_style"] = {"status": "not_run", "exit_code": None}
        expected["self_tests"] = {"status": "not_run", "exit_code": None}
        expected["preflight_current_phase"] = None
        expected["toolchain"].update(
            status="not_run",
            failure_code=None,
            observed={},
            commands={},
            current_phase=None,
        )
        expected["phases"] = {
            name: {"status": "not_run", "exit_code": None}
            for name, _, _ in prepare_control_plane.PHASES
        }
        expected["dependency_inventory"] = {
            name: None
            for name in (
                "base",
                "target_initial",
                "target_after_update",
                "target_final",
            )
        }
        expected["metadata_coverage"] = None
        expected["workspace_package_count"] = None
        expected["external_package_record_count"] = None
        expected["patch"] = {"status": "not_emitted", "bytes": 0, "sha256": None}
        return expected

    def expected_preflight_interruption(self, args, field, tool_name=None):
        expected = self.expected_before_preflight_failure(args, "unexpected_exception")
        expected["identity"]["helper_tree"] = self.helper_tree
        expected["inventory"]["candidate_path_count"] = 1
        if field == "source_style":
            expected["source_style"] = {"status": "unknown", "exit_code": None}
            expected["preflight_current_phase"] = "source_style"
        elif field == "self_tests":
            expected["source_style"] = {"status": "passed", "exit_code": 0}
            expected["self_tests"] = {"status": "unknown", "exit_code": None}
            expected["preflight_current_phase"] = "self_tests"
        else:
            expected["source_style"] = {"status": "passed", "exit_code": 0}
            expected["self_tests"] = {"status": "passed", "exit_code": 0}
            complete = self.expected_complete_receipt(args, b"", 0)
            expected["dependency_inventory"]["base"] = complete["dependency_inventory"][
                "base"
            ]
            expected["dependency_inventory"]["target_initial"] = complete[
                "dependency_inventory"
            ]["target_initial"]
            expected["toolchain"]["status"] = "unknown"
            expected["toolchain"]["current_phase"] = tool_name
            expected["toolchain"]["commands"] = {
                tool_name: {"status": "unknown", "exit_code": None}
            }
        return expected

    def expected_phase_interruption(self, args, phase_name):
        expected = self.expected_complete_receipt(args, b"", 0)
        expected.update(status="incomplete", failure_code="unexpected_exception")
        expected["phases"] = {}
        for name, _, _ in prepare_control_plane.PHASES:
            status = "passed" if name == "workspace_update" else "not_run"
            exit_code = 0 if status == "passed" else None
            if name == phase_name:
                status, exit_code = "unknown", None
            expected["phases"][name] = {"status": status, "exit_code": exit_code}
            if name == phase_name:
                break
        expected["phases"].update(
            {
                name: {"status": "not_run", "exit_code": None}
                for name, _, _ in prepare_control_plane.PHASES
                if name not in expected["phases"]
            }
        )
        expected["dependency_inventory"]["target_final"] = None
        expected["workspace_package_count"] = None
        expected["external_package_record_count"] = None
        expected["metadata_coverage"] = None
        expected["patch"] = {"status": "not_emitted", "bytes": 0, "sha256": None}
        return expected

    @staticmethod
    def serialize_expected(receipt):
        return (
            json.dumps(receipt, sort_keys=True, separators=(",", ":")) + "\n"
        ).encode()

    def run_success(self, generate_path=None):
        calls = []
        with mock.patch.dict(
            prepare_control_plane.os.environ,
            {"RUNNER_TEMP": str(self.root)},
            clear=False,
        ):
            with mock.patch.object(
                prepare_control_plane,
                "run",
                side_effect=self.mocked_run(calls=calls, generate_path=generate_path),
            ):
                args = self.args()
                result = prepare_control_plane.prepare(args)
        self.assertEqual(result, 0)
        receipt = json.loads((self.root / "receipt.json").read_text(encoding="utf-8"))
        patch = (self.root / "candidate.patch").read_bytes()
        expected = self.expected_complete_receipt(
            args, patch, 1 if generate_path else 0
        )
        expected_bytes = (
            json.dumps(expected, sort_keys=True, separators=(",", ":")) + "\n"
        ).encode()
        self.assertEqual((self.root / "receipt.json").read_bytes(), expected_bytes)
        phase_calls = [
            (argv, cwd)
            for argv, cwd in calls
            if argv in {phase[2] for phase in prepare_control_plane.PHASES}
        ]
        self.assertEqual(
            [argv for argv, _ in phase_calls],
            [phase[2] for phase in prepare_control_plane.PHASES],
        )
        self.assertEqual(
            [cwd for _, cwd in phase_calls],
            [
                prepare_control_plane.phase_cwd(self.product, cwd)
                for _, cwd, _ in prepare_control_plane.PHASES
            ],
        )
        names = [
            "metadata"
            if argv[0] == "cargo" and argv[1] == "metadata"
            else "workspace_update"
            if argv[0:2] == ("cargo", "update")
            else None
            for argv, _ in calls
        ]
        self.assertGreater(names.index("metadata"), names.index("workspace_update"))
        return receipt, patch

    def change_candidate_lock(self, replacement):
        lock_path = self.product / "codex-rs/Cargo.lock"
        lock_path.write_text(replacement, encoding="utf-8")
        self.git(self.product, "add", "codex-rs/Cargo.lock")
        self.git(self.product, "commit", "--quiet", "-m", "candidate lock input")
        self.target_sha = self.git(self.product, "rev-parse", "HEAD").decode().strip()

    def test_prepare_flow_emits_exact_complete_empty_patch_receipt(self):
        receipt, patch = self.run_success()
        self.assertEqual(receipt["inventory"]["changed_path_count"], 0)
        self.assertEqual(patch, b"")

    def test_prepare_flow_emits_exact_complete_nonempty_patch_receipt(self):
        receipt, patch = self.run_success("docs/downstream-regression-matrix.md")
        self.assertEqual(receipt["inventory"]["changed_path_count"], 1)
        self.assertIn(b"+generated\n", patch)

    def test_prepare_rejects_incoming_lock_checksum_change_from_base(self):
        lock_path = self.product / "codex-rs/Cargo.lock"
        changed = lock_path.read_text(encoding="utf-8").replace(
            'checksum = "checksum-a"', 'checksum = "different-checksum"'
        )
        self.change_candidate_lock(changed)
        calls = []
        with mock.patch.dict(
            prepare_control_plane.os.environ,
            {"RUNNER_TEMP": str(self.root)},
            clear=False,
        ):
            with mock.patch.object(
                prepare_control_plane,
                "run",
                side_effect=self.mocked_run(calls=calls),
            ):
                result = prepare_control_plane.prepare(self.args())
        self.assertEqual(result, 1)
        receipt = json.loads((self.root / "receipt.json").read_text(encoding="utf-8"))
        expected = self.expected_before_preflight_failure(
            self.args(), "input_external_package_records_changed"
        )
        expected["inventory"]["candidate_path_count"] = 2
        expected["identity"]["helper_tree"] = self.helper_tree
        expected["source_style"] = {"status": "passed", "exit_code": 0}
        expected["self_tests"] = {"status": "passed", "exit_code": 0}
        expected["dependency_inventory"]["base"] = {
            "workspace_package_count": 1,
            "workspace_sha256": hashlib.sha256(b'[["workspace","1.0"]]').hexdigest(),
            "external_record_count": 2,
            "external_sha256": hashlib.sha256(
                b'[["dep","2.0","registry+index","checksum-a",1],["inactive-dep","3.0","registry+index","checksum-b",1]]'
            ).hexdigest(),
        }
        expected["dependency_inventory"]["target_initial"] = {
            "workspace_package_count": 1,
            "workspace_sha256": hashlib.sha256(b'[["workspace","1.0"]]').hexdigest(),
            "external_record_count": 2,
            "external_sha256": hashlib.sha256(
                b'[["dep","2.0","registry+index","different-checksum",1],["inactive-dep","3.0","registry+index","checksum-b",1]]'
            ).hexdigest(),
        }
        self.assertEqual(
            (self.root / "receipt.json").read_bytes(), self.serialize_expected(expected)
        )
        self.assertNotEqual(
            receipt["dependency_inventory"]["base"]["external_sha256"],
            receipt["dependency_inventory"]["target_initial"]["external_sha256"],
        )
        self.assertEqual(
            receipt["dependency_inventory"]["base"]["external_record_count"], 2
        )
        self.assertTrue(
            all(
                argv not in {phase[2] for phase in prepare_control_plane.PHASES}
                for argv, _ in calls
            )
        )
        self.assertFalse((self.root / "candidate.patch").exists())

    def test_prepare_rejects_generated_lock_change_and_records_both_snapshots(self):
        with mock.patch.dict(
            prepare_control_plane.os.environ,
            {"RUNNER_TEMP": str(self.root)},
            clear=False,
        ):
            with mock.patch.object(
                prepare_control_plane,
                "run",
                side_effect=self.mocked_run(change_generated_lock=True),
            ):
                result = prepare_control_plane.prepare(self.args())
        receipt = json.loads((self.root / "receipt.json").read_text(encoding="utf-8"))
        self.assertEqual(result, 1)
        expected = self.expected_complete_receipt(self.args(), b"", 0)
        expected.update(
            status="incomplete",
            failure_code="generated_external_package_records_changed",
        )
        expected["phases"] = {
            name: {
                "status": "passed" if name == "workspace_update" else "not_run",
                "exit_code": 0 if name == "workspace_update" else None,
            }
            for name, _, _ in prepare_control_plane.PHASES
        }
        changed_records = [
            ["dep", "2.0", "registry+index", "upgraded-checksum", 1],
            ["inactive-dep", "3.0", "registry+index", "checksum-b", 1],
        ]
        expected["dependency_inventory"]["target_after_update"] = {
            "workspace_package_count": 1,
            "workspace_sha256": hashlib.sha256(b'[["workspace","1.0"]]').hexdigest(),
            "external_record_count": 2,
            "external_sha256": hashlib.sha256(
                json.dumps(changed_records, separators=(",", ":")).encode()
            ).hexdigest(),
        }
        expected["dependency_inventory"]["target_final"] = None
        expected["workspace_package_count"] = None
        expected["external_package_record_count"] = None
        expected["metadata_coverage"] = None
        expected["patch"] = {"status": "not_emitted", "bytes": 0, "sha256": None}
        self.assertEqual(
            (self.root / "receipt.json").read_bytes(), self.serialize_expected(expected)
        )
        self.assertFalse((self.root / "candidate.patch").exists())

    def test_prepare_rejects_unexpected_generated_path_without_partial_patch(self):
        with mock.patch.dict(
            prepare_control_plane.os.environ,
            {"RUNNER_TEMP": str(self.root)},
            clear=False,
        ):
            with mock.patch.object(
                prepare_control_plane,
                "run",
                side_effect=self.mocked_run(generate_unexpected=True),
            ):
                result = prepare_control_plane.prepare(self.args())
        self.assertEqual(result, 1)
        receipt_bytes = (self.root / "receipt.json").read_bytes()
        receipt = json.loads(receipt_bytes)
        expected = self.expected_complete_receipt(self.args(), b"", 0)
        expected.update(
            status="incomplete", failure_code="candidate_delta_outside_allowlist"
        )
        expected["patch"] = {"status": "not_emitted", "bytes": 0, "sha256": None}
        self.assertEqual(receipt_bytes, self.serialize_expected(expected))
        self.assertNotIn(b"credential-token-canary", receipt_bytes)
        self.assertFalse((self.root / "candidate.patch").exists())

    def test_prepare_rejects_wrong_identity_before_fixed_phases(self):
        args = self.args()
        args.target_sha = "f" * 40
        calls = []
        with mock.patch.dict(
            prepare_control_plane.os.environ,
            {"RUNNER_TEMP": str(self.root)},
            clear=False,
        ):
            with mock.patch.object(
                prepare_control_plane,
                "run",
                side_effect=self.mocked_run(calls=calls),
            ):
                result = prepare_control_plane.prepare(args)
        receipt = json.loads((self.root / "receipt.json").read_text(encoding="utf-8"))
        self.assertEqual(result, 1)
        expected = self.expected_before_preflight_failure(
            args, "checkout_identity_mismatch"
        )
        expected["identity"]["target_tree"] = (
            self.git(self.product, "rev-parse", "HEAD^{tree}").decode().strip()
        )
        self.assertEqual(
            (self.root / "receipt.json").read_bytes(), self.serialize_expected(expected)
        )
        self.assertTrue(
            all(
                argv not in {phase[2] for phase in prepare_control_plane.PHASES}
                for argv, _ in calls
            )
        )

    def test_prepare_rejects_wrong_helper_sha_before_fixed_phases(self):
        args = self.args()
        args.helper_sha = "e" * 40
        calls = []
        with mock.patch.dict(
            prepare_control_plane.os.environ,
            {"RUNNER_TEMP": str(self.root)},
            clear=False,
        ):
            with mock.patch.object(
                prepare_control_plane,
                "run",
                side_effect=self.mocked_run(calls=calls),
            ):
                result = prepare_control_plane.prepare(args)
        receipt = json.loads((self.root / "receipt.json").read_text(encoding="utf-8"))
        self.assertEqual(result, 1)
        self.assertEqual(receipt["failure_code"], "helper_checkout_missing")
        self.assertTrue(
            all(
                argv not in {phase[2] for phase in prepare_control_plane.PHASES}
                for argv, _ in calls
            )
        )

    def test_prepare_rejects_dirty_candidate_checkout(self):
        (self.product / "dirty-canary.txt").write_text(
            "not trusted\n", encoding="utf-8"
        )
        with mock.patch.dict(
            prepare_control_plane.os.environ,
            {"RUNNER_TEMP": str(self.root)},
            clear=False,
        ):
            with mock.patch.object(
                prepare_control_plane,
                "run",
                side_effect=self.mocked_run(),
            ):
                result = prepare_control_plane.prepare(self.args())
        receipt = json.loads((self.root / "receipt.json").read_text(encoding="utf-8"))
        self.assertEqual(result, 1)
        self.assertEqual(receipt["failure_code"], "checkout_not_clean")
        self.assertEqual(receipt["status"], "incomplete")

    def test_interrupted_phase_is_checkpointed_unknown_with_coded_error(self):
        with mock.patch.dict(
            prepare_control_plane.os.environ,
            {"RUNNER_TEMP": str(self.root)},
            clear=False,
        ):
            with mock.patch.object(
                prepare_control_plane,
                "run",
                side_effect=self.mocked_run(interrupt_phase="fix"),
            ):
                result = prepare_control_plane.prepare(self.args())
        receipt_bytes = (self.root / "receipt.json").read_bytes()
        receipt = json.loads(receipt_bytes)
        self.assertEqual(result, 1)
        expected = self.expected_phase_interruption(self.args(), "fix")
        self.assertEqual(receipt_bytes, self.serialize_expected(expected))
        self.assertNotIn(b"credential-token-canary", receipt_bytes)

    def test_preflight_interruption_checkpoints_source_selftest_and_tool_version(self):
        scenarios = (
            (prepare_control_plane.SOURCE_STYLE_ARGV, "source_style", None),
            (prepare_control_plane.SELF_TEST_ARGV, "self_tests", None),
            (("rustc", "--version"), "toolchain", "rust"),
        )
        for argv, field, tool_name in scenarios:
            with self.subTest(field=field):
                args = self.args()
                receipt_path = self.root / "receipt.json"
                patch_path = self.root / "candidate.patch"
                receipt_path.unlink(missing_ok=True)
                patch_path.unlink(missing_ok=True)
                with mock.patch.dict(
                    prepare_control_plane.os.environ,
                    {"RUNNER_TEMP": str(self.root)},
                    clear=False,
                ):
                    with mock.patch.object(
                        prepare_control_plane,
                        "run",
                        side_effect=self.mocked_run(interrupt_command=argv),
                    ):
                        result = prepare_control_plane.prepare(args)
                serialized = receipt_path.read_bytes()
                self.assertEqual(result, 1)
                expected = self.expected_preflight_interruption(args, field, tool_name)
                self.assertEqual(serialized, self.serialize_expected(expected))
                self.assertNotIn(b"credential-token-canary", serialized)
                self.assertFalse(patch_path.exists())

    def test_initial_receipt_write_error_is_contained_without_raw_path(self):
        atomic_write = prepare_control_plane.atomic_write
        receipt_path = self.root / "receipt.json"
        failed = False

        def fail_once(path, data):
            nonlocal failed
            if path == receipt_path and not failed:
                failed = True
                raise OSError("/private/credential-token-canary")
            return atomic_write(path, data)

        with mock.patch.dict(
            prepare_control_plane.os.environ,
            {"RUNNER_TEMP": str(self.root)},
            clear=False,
        ):
            with mock.patch.object(
                prepare_control_plane, "atomic_write", side_effect=fail_once
            ):
                result = prepare_control_plane.prepare(self.args())
        receipt_bytes = receipt_path.read_bytes()
        expected = self.expected_before_preflight_failure(
            self.args(), "receipt_persist_failed"
        )
        expected["identity"]["base_tree"] = None
        expected["identity"]["target_tree"] = None
        self.assertEqual(result, 1)
        self.assertEqual(receipt_bytes, self.serialize_expected(expected))
        self.assertNotIn(b"credential-token-canary", receipt_bytes)
        self.assertFalse((self.root / "candidate.patch").exists())

    def test_uncertain_final_receipt_write_cannot_leave_durable_complete_claim(self):
        atomic_write = prepare_control_plane.atomic_write
        receipt_path = self.root / "receipt.json"
        patch_path = self.root / "candidate.patch"
        failed_complete = False
        unlink = Path.unlink

        def write_then_report_failure(path, data):
            nonlocal failed_complete
            if (
                path == receipt_path
                and json.loads(data)["status"] == "complete"
                and not failed_complete
            ):
                failed_complete = True
                atomic_write(path, data)
                raise OSError("credential-token-canary final receipt write")
            return atomic_write(path, data)

        def fail_patch_cleanup(path, *args, **kwargs):
            if path == patch_path:
                raise OSError("/private/credential-token-canary cleanup")
            return unlink(path, *args, **kwargs)

        with mock.patch.dict(
            prepare_control_plane.os.environ,
            {"RUNNER_TEMP": str(self.root)},
            clear=False,
        ):
            with mock.patch.object(
                prepare_control_plane, "run", side_effect=self.mocked_run()
            ):
                with mock.patch.object(
                    prepare_control_plane,
                    "atomic_write",
                    side_effect=write_then_report_failure,
                ):
                    with mock.patch.object(Path, "unlink", fail_patch_cleanup):
                        result = prepare_control_plane.prepare(self.args())
        receipt_bytes = receipt_path.read_bytes()
        expected = self.expected_complete_receipt(self.args(), b"", 0)
        expected.update(status="incomplete", failure_code="receipt_persist_failed")
        expected["patch"] = {"status": "not_emitted", "bytes": 0, "sha256": None}
        self.assertEqual(result, 1)
        self.assertEqual(receipt_bytes, self.serialize_expected(expected))
        self.assertNotIn(b"credential-token-canary", receipt_bytes)
        # A failed unlink can leave private scratch output, but the failing step
        # cannot publish it because the workflow uploads patches only on success.
        self.assertTrue(patch_path.exists())

    def test_permanent_final_persistence_loss_leaves_external_outcome_unreconciled(
        self,
    ):
        atomic_write = prepare_control_plane.atomic_write
        receipt_path = self.root / "receipt.json"
        failed_complete = False

        def lose_final_receipt(path, data):
            nonlocal failed_complete
            if path == receipt_path:
                if json.loads(data)["status"] == "complete" and not failed_complete:
                    failed_complete = True
                    atomic_write(path, data)
                if failed_complete:
                    raise OSError("credential-token-canary persistent receipt failure")
            return atomic_write(path, data)

        with mock.patch.dict(
            prepare_control_plane.os.environ,
            {"RUNNER_TEMP": str(self.root)},
            clear=False,
        ):
            with mock.patch.object(
                prepare_control_plane, "run", side_effect=self.mocked_run()
            ):
                with mock.patch.object(
                    prepare_control_plane,
                    "atomic_write",
                    side_effect=lose_final_receipt,
                ):
                    result = prepare_control_plane.prepare(self.args())
        durable_bytes = receipt_path.read_bytes()
        expected = self.expected_complete_receipt(self.args(), b"", 0)
        self.assertEqual(result, 1)
        self.assertEqual(durable_bytes, self.serialize_expected(expected))
        self.assertTrue((self.root / "candidate.patch").exists())

    def test_prepare_flow_failure_keeps_checkpoint_and_never_emits_partial_patch(self):
        with mock.patch.dict(
            prepare_control_plane.os.environ,
            {"RUNNER_TEMP": str(self.root)},
            clear=False,
        ):
            with mock.patch.object(
                prepare_control_plane,
                "run",
                side_effect=self.mocked_run(fail_phase="fix"),
            ):
                result = prepare_control_plane.prepare(self.args())
        self.assertEqual(result, 1)
        receipt = json.loads((self.root / "receipt.json").read_text(encoding="utf-8"))
        self.assertEqual(receipt["status"], "incomplete")
        self.assertEqual(receipt["failure_code"], "command_failed_fix")
        self.assertEqual(
            receipt["phases"]["fix"],
            {
                "status": "failed",
                "exit_code": 9,
                "compiler_failure_projection": {
                    "projection_status": "errors_found",
                    "error_count": 1,
                    "class_counts": {"trait_bound": 1},
                    "errors": [
                        {
                            "class": "trait_bound",
                            "file": "codex-rs/state/src/control_plane_usage_reader.rs",
                            "line": 41,
                            "column": 3,
                        }
                    ],
                    "omitted_count": 0,
                    "emitted_unlocated_count": 0,
                    "unparsed_line_count": 0,
                },
            },
        )
        self.assertEqual(receipt["phases"]["format"]["status"], "not_run")
        self.assertFalse((self.root / "candidate.patch").exists())
        serialized = (self.root / "receipt.json").read_bytes()
        self.assertNotIn(b"credential-token-canary", serialized)
        self.assertNotIn(b"private output", serialized)

    def test_cargo_json_failure_projection_keeps_safe_classes_and_verified_coordinates(
        self,
    ):
        source_path = self.product / "codex-rs/state/src/control_plane_usage_reader.rs"
        records = (
            {
                "reason": "compiler-message",
                "message": {
                    "level": "error",
                    "code": {"code": "E0277", "explanation": "PRIVATE_CODE_TEXT"},
                    "message": "PRIVATE_MESSAGE_TEXT",
                    "rendered": "PRIVATE_RENDERED_TEXT",
                    "spans": [
                        {
                            "file_name": str(source_path),
                            "line_start": 23,
                            "column_start": 7,
                            "is_primary": True,
                        }
                    ],
                },
            },
            {
                "reason": "compiler-message",
                "message": {
                    "level": "error",
                    "code": {"code": "E0282"},
                    "message": "PRIVATE_INFERENCE_TEXT",
                    "rendered": "PRIVATE_INFERENCE_RENDERED",
                    "spans": [
                        {
                            "file_name": "state/src/control_plane_usage_reader.rs",
                            "line_start": 29,
                            "column_start": 11,
                            "is_primary": True,
                        }
                    ],
                },
            },
            {
                "reason": "compiler-message",
                "message": {
                    "level": "error",
                    "code": {"code": "E0277"},
                    "message": "PRIVATE_PATH_TEXT",
                    "spans": [
                        {
                            "file_name": "state/src/not_a_tracked_source.rs",
                            "line_start": 31,
                            "column_start": 4,
                            "is_primary": True,
                        }
                    ],
                },
            },
        )
        stdout = b"\n".join(json.dumps(item).encode("utf-8") for item in records)
        projection = prepare_control_plane.cargo_json_failure_projection(
            stdout, b"", self.product
        )
        self.assertEqual(
            projection,
            {
                "projection_status": "errors_found",
                "error_count": 3,
                "class_counts": {"trait_bound": 2, "type_inference": 1},
                "errors": [
                    {
                        "class": "trait_bound",
                        "file": "codex-rs/state/src/control_plane_usage_reader.rs",
                        "line": 23,
                        "column": 7,
                    },
                    {
                        "class": "type_inference",
                        "file": "codex-rs/state/src/control_plane_usage_reader.rs",
                        "line": 29,
                        "column": 11,
                    },
                    {
                        "class": "trait_bound",
                        "file": "unavailable",
                        "line": None,
                        "column": None,
                    },
                ],
                "omitted_count": 0,
                "emitted_unlocated_count": 1,
                "unparsed_line_count": 0,
            },
        )
        serialized = json.dumps(projection, sort_keys=True).encode("utf-8")
        for private_value in (
            b"PRIVATE_CODE_TEXT",
            b"PRIVATE_MESSAGE_TEXT",
            b"PRIVATE_RENDERED_TEXT",
            b"PRIVATE_INFERENCE_TEXT",
            b"PRIVATE_INFERENCE_RENDERED",
            b"PRIVATE_PATH_TEXT",
            b"E0277",
            b"E0282",
            b"not_a_tracked_source.rs",
        ):
            self.assertNotIn(private_value, serialized)


class ExecutionBoundaryTests(unittest.TestCase):
    def test_unexpected_user_configuration_and_remote_environment_reject(self):
        with tempfile.TemporaryDirectory() as tempdir:
            home = Path(tempdir) / "control-plane-home"
            home.mkdir()
            with mock.patch.dict(
                prepare_control_plane.os.environ,
                {"RUNNER_TEMP": tempdir},
                clear=False,
            ):
                with self.assertRaisesRegex(
                    prepare_control_plane.PreparationError,
                    "unexpected_execution_environment",
                ):
                    with mock.patch.dict(
                        prepare_control_plane.os.environ,
                        {"BAZELRC": "/untrusted/config"},
                        clear=False,
                    ):
                        prepare_control_plane.check_user_configuration(Path(tempdir))
            (home / ".bazelrc").write_text("# untrusted\n", encoding="utf-8")
            with mock.patch.dict(
                prepare_control_plane.os.environ,
                {"RUNNER_TEMP": tempdir},
                clear=False,
            ):
                with self.assertRaisesRegex(
                    prepare_control_plane.PreparationError,
                    "unexpected_user_configuration",
                ):
                    prepare_control_plane.check_user_configuration(Path(tempdir))

    def test_trusted_execution_inputs_must_match_base(self):
        with tempfile.TemporaryDirectory() as tempdir:
            base = Path(tempdir) / "base"
            product = Path(tempdir) / "product"
            (base / "codex-rs/.cargo").mkdir(parents=True)
            (product / "codex-rs/.cargo").mkdir(parents=True)
            (base / ".bazelrc").write_text("safe\n", encoding="utf-8")
            (product / ".bazelrc").write_text("safe\n", encoding="utf-8")
            prepare_control_plane.check_trusted_inputs(product, base)
            (product / ".bazelrc").write_text("changed\n", encoding="utf-8")
            with self.assertRaisesRegex(
                prepare_control_plane.PreparationError,
                "trusted_execution_input_changed",
            ):
                prepare_control_plane.check_trusted_inputs(product, base)

    def test_inactive_named_remote_config_is_allowed_but_active_flags_reject(self):
        with tempfile.TemporaryDirectory() as tempdir:
            product = Path(tempdir)
            bazelrc = product / ".bazelrc"
            bazelrc.write_text(
                "common:buildbuddy-rbe --remote_executor=grpcs://inactive.invalid\n"
                "try-import %workspace%/user.bazelrc\n",
                encoding="utf-8",
            )
            prepare_control_plane.check_product_bazelrc(product)
            bazelrc.write_text(
                "common --remote_executor=grpcs://active.invalid\n", encoding="utf-8"
            )
            with self.assertRaisesRegex(
                prepare_control_plane.PreparationError,
                "active_remote_bazel_configuration",
            ):
                prepare_control_plane.check_product_bazelrc(product)

    def test_product_imported_user_bazelrc_rejects_without_reading_contents(self):
        with tempfile.TemporaryDirectory() as tempdir:
            product = Path(tempdir)
            (product / ".bazelrc").write_text(
                "try-import %workspace%/user.bazelrc\n", encoding="utf-8"
            )
            (product / "user.bazelrc").write_text(
                "credential-token-canary\n", encoding="utf-8"
            )
            with self.assertRaisesRegex(
                prepare_control_plane.PreparationError,
                "unexpected_product_bazel_configuration",
            ):
                prepare_control_plane.check_user_configuration(product)


class PublicArtifactTests(unittest.TestCase):
    def test_serialized_receipt_contains_only_allowlisted_provenance(self):
        args = type(
            "Args",
            (),
            {
                "base_sha": "b" * 40,
                "target_sha": "a" * 40,
                "helper_sha": "c" * 40,
                "base_ref": "main",
            },
        )()
        receipt = prepare_control_plane.make_receipt(args)
        serialized = prepare_control_plane.safe_receipt(receipt)
        for canary in (
            "private-message-body-canary",
            "credential-token-canary",
            "/home/private/workspace-canary",
            "https://private.example/canary",
        ):
            self.assertNotIn(canary.encode(), serialized)
        decoded = json.loads(serialized)
        self.assertEqual(decoded["status"], "incomplete")
        self.assertEqual(decoded["patch"]["status"], "not_emitted")

    def test_untrusted_invalid_input_is_not_copied_into_receipt(self):
        args = type(
            "Args",
            (),
            {
                "base_sha": "credential-token-canary",
                "target_sha": "private-message-body-canary",
                "helper_sha": "invalid",
                "base_ref": "../private-message-body-canary",
            },
        )()
        serialized = prepare_control_plane.safe_receipt(
            prepare_control_plane.make_receipt(args)
        )
        self.assertNotIn(b"credential-token-canary", serialized)
        self.assertNotIn(b"private-message-body-canary", serialized)

    def test_receipt_limit_rejects_overflow(self):
        with self.assertRaises(prepare_control_plane.PreparationError):
            prepare_control_plane.safe_receipt(
                {"x": "y" * prepare_control_plane.MAX_RECEIPT_BYTES}
            )

    def test_self_test_failure_receipt_projects_only_known_contract_location(self):
        with tempfile.TemporaryDirectory() as tempdir:
            helper = Path(tempdir)
            test_file = helper / prepare_control_plane.SELF_TEST_SOURCE
            test_file.parent.mkdir(parents=True)
            test_file.write_text(
                "import unittest\n"
                "class ProjectionTests(unittest.TestCase):\n"
                "    def test_expected_contract(self):\n"
                "        self.assertEqual(1, 2)\n",
                encoding="utf-8",
            )
            stderr = (
                b"FAIL: test_expected_contract "
                b"(test_prepare_control_plane.ProjectionTests.test_expected_contract)\n"
                b"Traceback (most recent call last):\n"
                b'  File "/home/runner/private/.github/scripts/'
                b'test_prepare_control_plane.py", line 4, in test_expected_contract\n'
                b"AssertionError: PRIVATE_ASSERTION_CANARY\n"
                b"FAIL: test_untrusted_canary "
                b"(test_prepare_control_plane.Private.test_untrusted_canary)\n"
            )
            receipt = {"self_tests": {"status": "not_run", "exit_code": None}}
            receipt_path = helper / "receipt.json"
            completed = subprocess.CompletedProcess(
                args=("python3",), returncode=1, stdout=b"", stderr=stderr
            )
            with mock.patch.object(
                prepare_control_plane, "run", return_value=completed
            ):
                prepare_control_plane.run_preflight(
                    "self_tests",
                    prepare_control_plane.SELF_TEST_ARGV,
                    helper,
                    {},
                    receipt,
                    receipt_path,
                )

            expected = [
                {
                    "test": "test_expected_contract",
                    "status": "assertion_failure",
                    "file": prepare_control_plane.SELF_TEST_SOURCE,
                    "line": 4,
                }
            ]
            self.assertEqual(receipt["self_test_failures"], expected)
            serialized = receipt_path.read_bytes()
            self.assertNotIn(b"PRIVATE_ASSERTION_CANARY", serialized)
            self.assertNotIn(b"/home/runner/private", serialized)
            self.assertNotIn(b"test_untrusted_canary", serialized)


if __name__ == "__main__":
    unittest.main()
