"""Hosted regression controls for the closed TUI snapshot preparation route."""

import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).with_name("prepare_tui_snapshots.py")
SPEC = importlib.util.spec_from_file_location("prepare_tui_snapshots", SCRIPT)
prepare_tui_snapshots = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(prepare_tui_snapshots)


class ProfileTests(unittest.TestCase):
    def test_only_tui_snapshot_prepare_only_profile_is_accepted(self):
        self.assertEqual(
            prepare_tui_snapshots.validate_profile("prepare-only", "tui-snapshots"),
            "tui-snapshots",
        )
        for mode in ("", "build", "consume-existing", "unknown"):
            with self.subTest(mode=mode), self.assertRaisesRegex(ValueError, "prepare-only"):
                prepare_tui_snapshots.validate_profile(mode, "tui-snapshots")
        for profile in (None, "", "cargo-schema", "config-schema", "unknown"):
            with self.subTest(profile=profile), self.assertRaisesRegex(ValueError, "profile"):
                prepare_tui_snapshots.validate_profile("prepare-only", profile)

    def test_workflow_has_closed_early_routing_and_preserves_schema_paths(self):
        script_dir = Path(__file__).parent
        workflow = (script_dir.parent / "workflows/sedna-branch-build.yml").read_text(
            encoding="utf-8"
        )
        schema_helper = script_dir / "prepare_config_schema.py"
        schema_tests = script_dir / "test_prepare_config_schema.py"
        self.assertEqual(
            hashlib.sha256(schema_helper.read_bytes()).hexdigest(),
            "e31cf88b5017222dac8c3c191a1992496289397063f8e40437de496fe5e6466c",
        )
        self.assertEqual(
            hashlib.sha256(schema_tests.read_bytes()).hexdigest(),
            "9184298cc614eefbac7c01c48343efeb269f092749df036e332a91e5e90d737c",
        )
        self.assertIn("preparation_profile:", workflow)
        self.assertIn("default: cargo-schema", workflow)
        self.assertIn("cargo-schema|config-schema|tui-snapshots", workflow)
        self.assertIn(
            'if [[ "${MODE}" != "prepare-only" && "${PREPARATION_PROFILE}" != "cargo-schema" ]]; then',
            workflow,
        )
        self.assertIn(
            "nondefault preparation profiles are restricted to prepare-only mode", workflow
        )
        self.assertIn("python3 .github/scripts/prepare_config_schema.py --validate-profile", workflow)
        self.assertIn("python3 .github/scripts/prepare_tui_snapshots.py --validate-profile", workflow)
        self.assertIn(
            "if: ${{ inputs.mode == 'prepare-only' && inputs.preparation_profile == 'config-schema' }}",
            workflow,
        )
        self.assertIn(
            "if: ${{ inputs.mode == 'prepare-only' && inputs.preparation_profile == 'tui-snapshots' }}",
            workflow,
        )
        self.assertIn(
            "if: ${{ inputs.preparation_profile == 'config-schema' }}",
            workflow,
        )
        self.assertIn(
            "if: ${{ inputs.preparation_profile == 'tui-snapshots' }}",
            workflow,
        )
        self.assertIn(
            "if: ${{ always() && inputs.preparation_profile == 'tui-snapshots' }}",
            workflow,
        )
        self.assertIn(
            "name: sedna-tui-snapshots-prep-${{ inputs.target_sha }}-${{ github.run_id }}-${{ github.run_attempt }}",
            workflow,
        )
        for legacy_step in (
            "- name: Install the repository-pinned schema generator runtime",
            "- name: Snapshot the input lockfile",
            "- name: Refresh only workspace package records",
            "- name: Generate the canonical app-server schema and SDK fixtures",
            "- name: Validate the lock delta and generated-path allowlist",
            "- name: Require the generated lockfile to be accepted as-is",
            "- name: Upload exact lock, schema, and identity diff",
        ):
            self.assertIn(
                legacy_step + "\n        if: ${{ inputs.preparation_profile == 'cargo-schema' }}",
                workflow,
            )

    def test_workflow_binds_exact_caller_environment_and_command(self):
        workflow = (Path(__file__).parents[1] / "workflows/sedna-branch-build.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("python3 .github/scripts/prepare_tui_snapshots.py --validate-profile", workflow)
        self.assertIn("python3 .github/scripts/test_prepare_tui_snapshots.py -v", workflow)
        self.assertIn("python3 .github/scripts/prepare_tui_snapshots.py\n", workflow)
        self.assertIn(
            "CARGO_TARGET_DIR: ${{ runner.temp }}/w16072-tui-snapshot-target", workflow
        )
        self.assertIn("INSTA_UPDATE: new", workflow)
        self.assertIn("taiki-e/install-action@4cef1412cce204788f482e778a0b9187f9626a29", workflow)
        self.assertIn("tool: just@1.51.0", workflow)
        self.assertIn("taiki-e/install-action@44c6d64aa62cd779e873306675c7a58e86d6d532", workflow)
        self.assertIn("version: 0.9.103", workflow)
        self.assertEqual(
            prepare_tui_snapshots.COMMAND,
            ["just", "test", "-p", "codex-tui", "--locked"],
        )
        self.assertEqual(prepare_tui_snapshots.TARGET_DIRECTORY_NAME, "w16072-tui-snapshot-target")

    def test_red_test_result_is_recorded_as_failure(self):
        self.assertEqual(
            prepare_tui_snapshots.test_result_metadata(0),
            {"test_exit_code": 0, "test_status": "passed"},
        )
        self.assertEqual(
            prepare_tui_snapshots.test_result_metadata(17),
            {"test_exit_code": 17, "test_status": "failed"},
        )
        self.assertIn("return result.returncode", SCRIPT.read_text(encoding="utf-8"))
        self.assertIn("if: ${{ always() && inputs.preparation_profile == 'tui-snapshots' }}", (
            Path(__file__).parents[1] / "workflows/sedna-branch-build.yml"
        ).read_text(encoding="utf-8"))


class OutputFixture(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.source_root = self.root / prepare_tui_snapshots.TUI_SOURCE_ROOT
        self.source_root.mkdir(parents=True)
        for index, relative in enumerate(prepare_tui_snapshots.SNAPSHOT_PATHS):
            path = self.root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(f"fixture snapshot {index}\n".encode())
        for relative in prepare_tui_snapshots.LOCK_PATHS:
            path = self.root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"lock fixture\n")
        (self.root / ".gitignore").write_text("*.snap.new\n", encoding="utf-8")
        self.git("init", "--quiet")
        self.git("add", "--", *prepare_tui_snapshots.SNAPSHOT_PATHS)
        self.git("add", "--", *prepare_tui_snapshots.LOCK_PATHS, ".gitignore")
        self.git(
            "-c",
            "user.name=Hosted Test",
            "-c",
            "user.email=hosted@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "snapshot fixture",
        )
        self.baseline = prepare_tui_snapshots._capture_baseline(self.root)
        self.locks = prepare_tui_snapshots._capture_locks(self.root)
        self.environment = {
            "GITHUB_WORKSPACE": str(self.root),
            "RUNNER_TEMP": str(self.root.parent / f"{self.root.name}-runner-temp"),
        }

    def tearDown(self):
        runner_temp = Path(self.environment["RUNNER_TEMP"])
        if runner_temp.exists():
            for child in runner_temp.iterdir():
                if child.is_dir() and not child.is_symlink():
                    for nested in sorted(child.rglob("*"), reverse=True):
                        if nested.is_dir() and not nested.is_symlink():
                            nested.rmdir()
                        else:
                            nested.unlink()
                    child.rmdir()
                else:
                    child.unlink()
            runner_temp.rmdir()

    def git(self, *args):
        return subprocess.check_output(
            ["git", "-C", str(self.root), *args], stderr=subprocess.DEVNULL
        )

    def validate(self):
        return prepare_tui_snapshots.validate_outputs(
            self.root, self.baseline, self.locks, self.environment
        )

    def add_pending(self, relative=None, contents=b"pending snapshot\n"):
        path = relative or f"{prepare_tui_snapshots.SNAPSHOT_PATHS[0]}.new"
        destination = self.root / path
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(contents)
        return path


class GeneratedSnapshotTests(OutputFixture):
    def test_success_with_no_outputs_is_a_valid_empty_subset(self):
        self.assertEqual(self.validate(), {})

    def test_actual_allowlisted_pending_output_is_captured_as_subset(self):
        relative = self.add_pending()
        self.assertEqual(self.validate(), {relative: b"pending snapshot\n"})

    def test_unlisted_pending_output_is_rejected_even_when_ignored(self):
        self.add_pending("codex-rs/tui/src/chatwidget/unlisted.snap.new")
        with self.assertRaisesRegex(ValueError, "outside the exact allowlist"):
            self.validate()

    def test_pending_snapshot_symlink_or_special_file_is_rejected(self):
        relative = self.add_pending()
        pending = self.root / relative
        pending.unlink()
        pending.symlink_to(self.root / prepare_tui_snapshots.LOCK_PATHS[0])
        with self.assertRaisesRegex(ValueError, "regular file"):
            self.validate()
        pending.unlink()
        os.mkfifo(pending)
        with self.assertRaisesRegex(ValueError, "regular file"):
            self.validate()

    def test_pending_snapshot_hardlink_is_rejected(self):
        relative = self.add_pending()
        os.link(self.root / relative, self.root / "codex-rs/tui/src/pending-copy")
        with self.assertRaisesRegex(ValueError, "hard links"):
            self.validate()

    def test_ignored_preexisting_pending_output_is_rejected_before_generation(self):
        self.add_pending()
        with self.assertRaisesRegex(ValueError, "pre-existing"):
            prepare_tui_snapshots.validate_no_preexisting_pending(self.root)

    def test_accepted_snapshot_bytes_and_mode_are_never_changed(self):
        path = self.root / prepare_tui_snapshots.SNAPSHOT_PATHS[0]
        path.write_bytes(b"rewritten accepted snapshot\n")
        with self.assertRaisesRegex(ValueError, "accepted .snap baseline"):
            self.validate()

    def test_snapshot_symlink_is_rejected(self):
        path = self.root / prepare_tui_snapshots.SNAPSHOT_PATHS[0]
        path.unlink()
        path.symlink_to(self.root / prepare_tui_snapshots.LOCK_PATHS[0])
        with self.assertRaisesRegex(ValueError, "symlink"):
            self.validate()

    def test_baseline_hardlink_is_rejected(self):
        path = self.root / prepare_tui_snapshots.SNAPSHOT_PATHS[0]
        os.link(path, self.root / "codex-rs/tui/src/hardlink-copy")
        with self.assertRaisesRegex(ValueError, "single-link"):
            prepare_tui_snapshots._capture_baseline(self.root)

    def test_missing_baseline_is_rejected(self):
        (self.root / prepare_tui_snapshots.SNAPSHOT_PATHS[0]).unlink()
        with self.assertRaisesRegex(ValueError, "missing|untracked"):
            prepare_tui_snapshots._capture_baseline(self.root)

    def test_symlink_baseline_parent_is_rejected(self):
        original = self.root / "codex-rs/tui/src/app"
        moved = self.root / "codex-rs/tui/src/app.saved"
        original.rename(moved)
        original.symlink_to(moved, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "symlink"):
            prepare_tui_snapshots._capture_baseline(self.root)

    def test_extra_nonignored_untracked_workspace_file_is_rejected(self):
        (self.root / "codex-rs/tui/src/other.txt").write_text("unexpected\n", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "unexpected change"):
            self.validate()

    def test_cargo_lock_is_preserved(self):
        (self.root / prepare_tui_snapshots.LOCK_PATHS[0]).write_bytes(b"mutated lock\n")
        with self.assertRaisesRegex(ValueError, "lock changed"):
            self.validate()

    def test_module_bazel_lock_is_preserved(self):
        (self.root / prepare_tui_snapshots.LOCK_PATHS[1]).write_bytes(b"mutated lock\n")
        with self.assertRaisesRegex(ValueError, "lock changed"):
            self.validate()

    def test_pending_snapshot_must_be_nonempty_and_within_file_limit(self):
        self.add_pending(contents=b"")
        with self.assertRaisesRegex(ValueError, "empty"):
            self.validate()

    def test_oversized_pending_snapshot_is_rejected(self):
        with mock.patch.object(prepare_tui_snapshots, "MAX_FILE_BYTES", 4):
            self.add_pending(contents=b"12345")
            with self.assertRaisesRegex(ValueError, "per-file"):
                self.validate()

    def test_output_count_limit_is_enforced(self):
        with mock.patch.object(prepare_tui_snapshots, "MAX_OUTPUTS", 0):
            self.add_pending()
            with self.assertRaisesRegex(ValueError, "count"):
                self.validate()

    def test_output_aggregate_size_limit_is_enforced(self):
        first, second = prepare_tui_snapshots.SNAPSHOT_PATHS[:2]
        with mock.patch.object(prepare_tui_snapshots, "MAX_OUTPUT_BYTES", 6):
            self.add_pending(f"{first}.new", b"1234")
            self.add_pending(f"{second}.new", b"5678")
            with self.assertRaisesRegex(ValueError, "aggregate"):
                self.validate()

    def test_metadata_and_total_artifact_size_limits_are_enforced(self):
        with tempfile.TemporaryDirectory() as temporary:
            artifact_root = Path(temporary) / "artifact"
            with mock.patch.object(prepare_tui_snapshots, "MAX_METADATA_BYTES", 1):
                with self.assertRaisesRegex(ValueError, "metadata"):
                    prepare_tui_snapshots._write_artifact(
                        artifact_root, {}, self.baseline, self.locks, {}
                    )
                self.assertFalse(artifact_root.exists())

    def test_artifact_contains_only_bounded_manifests_identity_and_pending_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            artifact_root = Path(temporary) / "artifact"
            relative = f"{prepare_tui_snapshots.SNAPSHOT_PATHS[0]}.new"
            contents = b"pending, not accepted\n"
            prepare_tui_snapshots._write_artifact(
                artifact_root,
                {"workflow_host_sha": "a" * 40},
                self.baseline,
                self.locks,
                {relative: contents},
            )
            expected_files = {
                "candidate-paths.json",
                "generated-paths.json",
                "identity.json",
                f"snapshots/{relative}",
            }
            actual_files = {
                path.relative_to(artifact_root).as_posix()
                for path in artifact_root.rglob("*")
                if path.is_file()
            }
            self.assertEqual(actual_files, expected_files)
            candidates = json.loads((artifact_root / "candidate-paths.json").read_text())
            generated = json.loads((artifact_root / "generated-paths.json").read_text())
            identity = json.loads((artifact_root / "identity.json").read_text())
            self.assertEqual(candidates["paths"], list(prepare_tui_snapshots.SNAPSHOT_PATHS))
            self.assertEqual(generated, {"count": 1, "paths": [relative]})
            self.assertEqual(
                identity["generated_snapshot_sha256"][relative],
                hashlib.sha256(contents).hexdigest(),
            )
            self.assertEqual(
                (artifact_root / "snapshots" / relative).read_bytes(), contents
            )
        with tempfile.TemporaryDirectory() as temporary:
            artifact_root = Path(temporary) / "artifact"
            with mock.patch.object(prepare_tui_snapshots, "MAX_ARTIFACT_BYTES", 1):
                with self.assertRaisesRegex(ValueError, "total byte limit"):
                    prepare_tui_snapshots._write_artifact(
                        artifact_root, {}, self.baseline, self.locks, {}
                    )
                self.assertFalse(artifact_root.exists())

    def test_allowed_output_rejects_workspace_and_runner_paths_or_token_like_text(self):
        relative = self.add_pending(
            contents=f"path {self.environment['GITHUB_WORKSPACE']}\n".encode()
        )
        with self.assertRaisesRegex(ValueError, "runner path"):
            self.validate()
        (self.root / relative).unlink()
        self.add_pending(contents=b"ghp_abcdefghijklmnopqrstuvwxyz123456\n")
        with self.assertRaisesRegex(ValueError, "token-like"):
            self.validate()

    def test_wrong_manifest_length_or_duplicate_is_rejected(self):
        with mock.patch.object(
            prepare_tui_snapshots, "SNAPSHOT_PATHS", prepare_tui_snapshots.SNAPSHOT_PATHS[:-1]
        ):
            with self.assertRaisesRegex(ValueError, "count or uniqueness"):
                prepare_tui_snapshots._capture_baseline(self.root)
        with mock.patch.object(
            prepare_tui_snapshots,
            "SNAPSHOT_PATHS",
            prepare_tui_snapshots.SNAPSHOT_PATHS[:-1] + (prepare_tui_snapshots.SNAPSHOT_PATHS[0],),
        ):
            with self.assertRaisesRegex(ValueError, "count or uniqueness"):
                prepare_tui_snapshots._capture_baseline(self.root)


class IdentityFixture(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        temporary_root = Path(temporary.name)
        self.workspace = temporary_root / "workspace"
        self.workspace.mkdir()
        self.workflow_root = self.workspace / ".workflow-src"
        self.product_root = self.workspace / "product"
        self._git_init(self.workflow_root)
        self._git_init(self.product_root)
        (self.workflow_root / "host.txt").write_text("host\n", encoding="utf-8")
        self._git(self.workflow_root, "add", "host.txt")
        self._commit(self.workflow_root, "host")
        self.host_sha = self._git(self.workflow_root, "rev-parse", "HEAD").decode().strip()
        for index, relative in enumerate(prepare_tui_snapshots.SNAPSHOT_PATHS):
            path = self.product_root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(f"accepted fixture snapshot {index}\n".encode())
        for relative in prepare_tui_snapshots.LOCK_PATHS:
            path = self.product_root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("lock\n", encoding="utf-8")
        self._git(
            self.product_root,
            "add",
            "--",
            *prepare_tui_snapshots.SNAPSHOT_PATHS,
            *prepare_tui_snapshots.LOCK_PATHS,
        )
        self._commit(self.product_root, "product")
        self.base_sha = self._git(self.product_root, "rev-parse", "HEAD").decode().strip()
        self.target_sha = self.base_sha
        self._git(self.product_root, "fetch", "--no-tags", ".", self.base_sha)
        runner_temp = temporary_root / "runner-temp"
        runner_temp.mkdir()
        self.environment = {
            "EXPECTED_H": self.host_sha,
            "GITHUB_SHA": self.host_sha,
            "TARGET_SHA": self.target_sha,
            "BASE_SHA": self.base_sha,
            "GITHUB_REPOSITORY": "sednalabs/codex",
            "GITHUB_RUN_ID": "123",
            "GITHUB_RUN_ATTEMPT": "1",
            "GITHUB_WORKFLOW": "sedna-branch-build",
            "GITHUB_SERVER_URL": "https://github.com",
            "GITHUB_WORKSPACE": str(self.workspace),
            "RUNNER_TEMP": str(runner_temp),
            "RUNNER_OS": "Linux",
            "RUNNER_ARCH": "X64",
            "MODE": "prepare-only",
            "PREPARATION_PROFILE": "tui-snapshots",
            "INSTA_UPDATE": "new",
            "CARGO_TARGET_DIR": str(runner_temp / prepare_tui_snapshots.TARGET_DIRECTORY_NAME),
        }

    @staticmethod
    def _git_init(root):
        root.mkdir(parents=True)
        subprocess.check_call(["git", "init", "--quiet", str(root)])

    @staticmethod
    def _git(root, *args):
        return subprocess.check_output(["git", "-C", str(root), *args], stderr=subprocess.DEVNULL)

    @classmethod
    def _commit(cls, root, message):
        cls._git(root, "-c", "user.name=Hosted Test", "-c", "user.email=hosted@example.invalid", "commit", "--quiet", "-m", message)


class InputIdentityTests(IdentityFixture):
    def test_exact_h_b_t_fetched_b_runner_and_fixed_paths_pass(self):
        identity = prepare_tui_snapshots.validate_input_identity(self.environment, self.workspace)
        self.assertEqual(identity["workflow_host_sha"], self.host_sha)
        self.assertEqual(identity["product_sha"], self.target_sha)
        self.assertEqual(identity["comparison_base_sha"], self.base_sha)
        target, artifact, staging = prepare_tui_snapshots._fixed_runner_paths(
            self.environment, self.workspace
        )
        self.assertEqual(target, Path(self.environment["CARGO_TARGET_DIR"]))
        self.assertEqual(
            artifact,
            Path(self.environment["RUNNER_TEMP"]) / "w16072-tui-snapshot-artifact",
        )
        self.assertEqual(
            staging,
            Path(self.environment["RUNNER_TEMP"]) / "w16072-tui-snapshot-artifact.staging",
        )

    def test_malformed_or_mismatched_h_b_t_and_run_identity_fail(self):
        for key, bad_value in (
            ("EXPECTED_H", ""),
            ("GITHUB_SHA", "0" * 40),
            ("TARGET_SHA", "x" * 40),
            ("BASE_SHA", "0" * 40),
            ("GITHUB_RUN_ID", "0"),
            ("GITHUB_RUN_ATTEMPT", ""),
            ("GITHUB_REPOSITORY", "other/repo"),
            ("INSTA_UPDATE", "always"),
        ):
            bad = dict(self.environment, **{key: bad_value})
            with self.subTest(key=key), self.assertRaises(ValueError):
                prepare_tui_snapshots.validate_input_identity(bad, self.workspace)

    def test_wrong_or_in_workspace_target_path_is_rejected(self):
        bad = dict(self.environment, CARGO_TARGET_DIR=str(self.product_root / "target"))
        with self.assertRaisesRegex(ValueError, "fixed external target"):
            prepare_tui_snapshots._fixed_runner_paths(bad, self.workspace)

    def test_runner_temp_ancestor_of_checkout_is_rejected(self):
        bad = dict(
            self.environment,
            RUNNER_TEMP=str(self.workspace),
            CARGO_TARGET_DIR=str(self.workspace / prepare_tui_snapshots.TARGET_DIRECTORY_NAME),
        )
        with self.assertRaisesRegex(ValueError, "outside both checkouts"):
            prepare_tui_snapshots._fixed_runner_paths(bad, self.workspace)

    def test_symlink_runner_temp_is_rejected(self):
        alias = self.workspace / "runner-link"
        alias.symlink_to(Path(self.environment["RUNNER_TEMP"]), target_is_directory=True)
        bad = dict(
            self.environment,
            RUNNER_TEMP=str(alias),
            CARGO_TARGET_DIR=str(alias / prepare_tui_snapshots.TARGET_DIRECTORY_NAME),
        )
        with self.assertRaisesRegex(ValueError, "unavailable"):
            prepare_tui_snapshots._fixed_runner_paths(bad, self.workspace)

    def test_existing_target_or_artifact_directory_is_rejected(self):
        for suffix in (
            prepare_tui_snapshots.TARGET_DIRECTORY_NAME,
            prepare_tui_snapshots.ARTIFACT_DIRECTORY_NAME,
            prepare_tui_snapshots.ARTIFACT_STAGING_DIRECTORY_NAME,
        ):
            path = Path(self.environment["RUNNER_TEMP"]) / suffix
            path.mkdir()
            with self.subTest(path=suffix), self.assertRaisesRegex(ValueError, "already exists"):
                prepare_tui_snapshots._fixed_runner_paths(self.environment, self.workspace)
            path.rmdir()


class PrepareExecutionTests(IdentityFixture):
    def _prepare_with_result(self, exit_code):
        baseline = prepare_tui_snapshots._capture_baseline(self.product_root)
        locks = prepare_tui_snapshots._capture_locks(self.product_root)
        relative = f"{prepare_tui_snapshots.SNAPSHOT_PATHS[0]}.new"
        contents = b"review-pending generated snapshot\n"
        pending_path = self.product_root / relative
        generator_kwargs = {
            "cwd": self.product_root,
            "env": self.environment,
            "check": False,
            "stdout": subprocess.DEVNULL,
            "stderr": subprocess.DEVNULL,
        }
        base_commit = self.base_sha
        workflow_root = self.workflow_root
        product_root = self.product_root
        allowed_git_commands = {
            ("git", "-C", str(workflow_root), "rev-parse", "HEAD"),
            ("git", "-C", str(workflow_root), "rev-parse", "HEAD^{tree}"),
            (
                "git", "-C", str(workflow_root), "status", "--porcelain=v1", "-z",
                "--untracked-files=all",
            ),
            ("git", "-C", str(product_root), "rev-parse", "HEAD"),
            ("git", "-C", str(product_root), "rev-parse", "FETCH_HEAD^{commit}"),
            (
                "git", "-C", str(product_root), "cat-file", "-e",
                f"{base_commit}^{{commit}}",
            ),
            ("git", "-C", str(product_root), "rev-parse", "HEAD^{tree}"),
            (
                "git", "-C", str(product_root), "rev-parse",
                f"{base_commit}^{{tree}}",
            ),
            (
                "git", "-C", str(product_root), "status", "--porcelain=v1", "-z",
                "--untracked-files=all",
            ),
            ("git", "-C", str(product_root), "ls-files", "-z"),
            (
                "git", "-C", str(product_root), "ls-files", "--others",
                "--exclude-standard", "-z",
            ),
        }
        allowed_git_commands.update(
            (
                "git", "-C", str(product_root), "ls-files", "--error-unmatch", "--",
                path,
            )
            for path in (
                *prepare_tui_snapshots.SNAPSHOT_PATHS,
                *prepare_tui_snapshots.LOCK_PATHS,
            )
        )
        original_run = prepare_tui_snapshots.subprocess.run
        generator_calls = []
        git_calls = []

        def run_command(command, *args, **kwargs):
            argv = tuple(command)
            if argv == tuple(prepare_tui_snapshots.COMMAND):
                self.assertFalse(args)
                self.assertEqual(kwargs, generator_kwargs)
                generator_calls.append((argv, kwargs.copy()))
                pending_path.write_bytes(contents)
                return mock.Mock(returncode=exit_code)
            if argv in allowed_git_commands:
                self.assertFalse(args)
                git_calls.append(argv)
                return original_run(command, *args, **kwargs)
            self.fail(f"unexpected subprocess command: {command!r}")

        with mock.patch.object(
            prepare_tui_snapshots.subprocess, "run", side_effect=run_command
        ) as run:
            result = prepare_tui_snapshots.prepare(self.environment)

        self.assertEqual(len(generator_calls), 1)
        self.assertEqual(generator_calls[0][0], tuple(prepare_tui_snapshots.COMMAND))
        self.assertEqual(generator_calls[0][1], generator_kwargs)
        self.assertTrue(git_calls)
        self.assertTrue(set(git_calls).issubset(allowed_git_commands))
        self.assertEqual(run.call_count, len(generator_calls) + len(git_calls))
        self.assertEqual(result, exit_code)

        runner_temp = Path(self.environment["RUNNER_TEMP"])
        artifact_root = runner_temp / prepare_tui_snapshots.ARTIFACT_DIRECTORY_NAME
        identity = json.loads(
            (artifact_root / "identity.json").read_text(encoding="utf-8")
        )
        candidates = json.loads(
            (artifact_root / "candidate-paths.json").read_text(encoding="utf-8")
        )
        generated = json.loads(
            (artifact_root / "generated-paths.json").read_text(encoding="utf-8")
        )
        self.assertEqual(identity["workflow_host_sha"], self.host_sha)
        self.assertEqual(
            identity["workflow_host_tree"],
            self._git(self.workflow_root, "rev-parse", "HEAD^{tree}").decode().strip(),
        )
        self.assertEqual(identity["product_sha"], self.target_sha)
        self.assertEqual(
            identity["product_tree"],
            self._git(self.product_root, "rev-parse", "HEAD^{tree}").decode().strip(),
        )
        self.assertEqual(identity["comparison_base_sha"], self.base_sha)
        self.assertEqual(
            identity["comparison_base_tree"],
            self._git(
                self.product_root, "rev-parse", f"{self.base_sha}^{{tree}}"
            ).decode().strip(),
        )
        self.assertEqual(identity["workflow_run_id"], "123")
        self.assertEqual(identity["workflow_run_attempt"], "1")
        self.assertEqual(identity["command"], prepare_tui_snapshots.COMMAND)
        self.assertEqual(identity["command_environment"]["INSTA_UPDATE"], "new")
        self.assertEqual(identity["test_exit_code"], exit_code)
        self.assertEqual(
            identity["test_status"], "passed" if exit_code == 0 else "failed"
        )
        self.assertEqual(
            candidates,
            {
                "count": len(prepare_tui_snapshots.SNAPSHOT_PATHS),
                "paths": list(prepare_tui_snapshots.SNAPSHOT_PATHS),
            },
        )
        self.assertEqual(generated, {"count": 1, "paths": [relative]})
        self.assertEqual(
            identity["baseline_snapshot_sha256"],
            {path: state["sha256"] for path, state in sorted(baseline.items())},
        )
        self.assertEqual(identity["lock_files"], locks)
        self.assertEqual(
            identity["generated_snapshot_sha256"][relative],
            hashlib.sha256(contents).hexdigest(),
        )
        self.assertEqual(
            (artifact_root / "snapshots" / relative).read_bytes(), contents
        )
        self.assertFalse(
            (runner_temp / prepare_tui_snapshots.ARTIFACT_STAGING_DIRECTORY_NAME).exists()
        )
        self.assertEqual(
            prepare_tui_snapshots.validate_outputs(
                self.product_root, baseline, locks, self.environment
            ),
            {relative: contents},
        )
        return result

    def test_prepare_invokes_exact_command_once_and_writes_success_artifact(self):
        self.assertEqual(self._prepare_with_result(0), 0)

    def test_red_suite_exit_writes_artifact_and_remains_a_failure(self):
        self.assertEqual(self._prepare_with_result(17), 17)


if __name__ == "__main__":
    unittest.main()
