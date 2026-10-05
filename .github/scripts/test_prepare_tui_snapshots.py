"""Hosted regression controls for the closed TUI snapshot preparation route."""

import hashlib
import io
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
            "e9e711be0335927979b9a08b2d92cb7c55a0a4a2e4aaf80fc696cb630d4c2d07",
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
    def _prepare_with_result(self, exit_code, all_outputs=False):
        baseline = prepare_tui_snapshots._capture_baseline(self.product_root)
        locks = prepare_tui_snapshots._capture_locks(self.product_root)
        relative = f"{prepare_tui_snapshots.SNAPSHOT_PATHS[0]}.new"
        contents = b"review-pending generated snapshot\n"
        pending = [path + ".new" for path in prepare_tui_snapshots.SNAPSHOT_PATHS] if all_outputs else [relative]
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
                for path in pending:
                    (self.product_root / path).write_bytes(contents)
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
        self.assertEqual(generated, {"count": len(pending), "paths": sorted(pending)})
        self.assertEqual(
            identity["baseline_snapshot_sha256"],
            {path: state["sha256"] for path, state in sorted(baseline.items())},
        )
        self.assertEqual(identity["lock_files"], locks)
        self.assertEqual(identity["generated_snapshot_sha256"],
                         {path: hashlib.sha256(contents).hexdigest() for path in pending})
        for path in pending:
            self.assertEqual((artifact_root / "snapshots" / path).read_bytes(), contents)
        self.assertFalse((artifact_root / "diagnostic.json").exists())
        self.assertFalse(
            (runner_temp / prepare_tui_snapshots.ARTIFACT_STAGING_DIRECTORY_NAME).exists()
        )
        self.assertEqual(
            prepare_tui_snapshots.validate_outputs(
                self.product_root, baseline, locks, self.environment
            ),
            {path: contents for path in pending},
        )
        return result

    def test_prepare_invokes_exact_command_once_and_writes_success_artifact(self):
        self.assertEqual(self._prepare_with_result(0), 0)

    def test_red_suite_exit_writes_artifact_and_remains_a_failure(self):
        self.assertEqual(self._prepare_with_result(17), 17)

    def test_all_69_valid_outputs_with_red_generator_preserve_accepted_artifact(self):
        self.assertEqual(self._prepare_with_result(17, all_outputs=True), 17)


class RejectedGenerationDiagnosticTests(IdentityFixture):
    def _additional_snapshot(self, relative="codex-rs/tui/src/chatwidget/snapshots/codex_tui__diagnostic_extra.snap"):
        path = self.product_root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b"tracked public fixture\n")
        self._git(self.product_root, "add", "--", relative)
        self._commit(self.product_root, "additional tracked fixture")
        self.target_sha = self._git(self.product_root, "rev-parse", "HEAD").decode().strip()
        self.environment["TARGET_SHA"] = self.target_sha
        return relative + ".new"

    def _pending(self, relative, contents=b"PRIVATE_BODY /home/runner/private ghp_abcdefghijklmnopqrstuvwxyz123456 https://private.invalid\n"):
        destination = self.product_root / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(contents)

    def _invoke(self, action, exit_code=101, generator_count=1, post_generator_git=None):
        original_run = subprocess.run
        calls, generators = [], []
        roots = {str(self.workflow_root), str(self.product_root)}
        suffixes = {
            ("rev-parse", "HEAD"), ("rev-parse", "HEAD^{tree}"),
            ("rev-parse", "FETCH_HEAD^{commit}"), ("rev-parse", f"{self.base_sha}^{{tree}}"),
            ("cat-file", "-e", f"{self.base_sha}^{{commit}}"),
            ("status", "--porcelain=v1", "-z", "--untracked-files=all"),
            ("ls-files", "-z"), ("ls-files", "--others", "--exclude-standard", "-z"),
            *(("ls-files", "--error-unmatch", "--", relative) for relative in (*prepare_tui_snapshots.SNAPSHOT_PATHS, *prepare_tui_snapshots.LOCK_PATHS)),
        }

        def run(command, *args, **kwargs):
            argv = tuple(command)
            calls.append(argv)
            if argv == tuple(prepare_tui_snapshots.COMMAND):
                generators.append(argv)
                self.assertFalse(args)
                self.assertEqual(kwargs, {"cwd": self.product_root, "env": self.environment, "check": False,
                                          "stdout": subprocess.DEVNULL, "stderr": subprocess.DEVNULL})
                action()
                return subprocess.CompletedProcess(command, exit_code)
            self.assertEqual(argv[:2], ("git", "-C"))
            self.assertIn(argv[2], roots)
            self.assertIn(argv[3:], suffixes)
            self.assertFalse(args)
            if post_generator_git is not None and generators and argv[2] == str(self.product_root):
                self.assertEqual({key: value for key, value in kwargs.items() if key != "timeout"},
                                 {"stdout": subprocess.PIPE, "stderr": subprocess.DEVNULL, "check": True})
                self.assertIsNone(kwargs.get("timeout"))
                replacement = post_generator_git(argv[3:])
                if replacement is not None:
                    return subprocess.CompletedProcess(command, 0, stdout=replacement)
            return original_run(command, **kwargs)

        with mock.patch.object(subprocess, "run", side_effect=run) as intercepted, \
                mock.patch("sys.stderr", new_callable=io.StringIO) as stderr:
            result = prepare_tui_snapshots.prepare(self.environment)
        self.assertEqual(result, 1)
        self.assertEqual(generators, [tuple(prepare_tui_snapshots.COMMAND)] * generator_count)
        self.assertEqual(intercepted.call_count, len(calls))
        self.invoked_commands = calls
        self.assertTrue(any(command[3:] == ("rev-parse", "FETCH_HEAD^{commit}") for command in calls if command[0] == "git"))
        artifact = Path(self.environment["RUNNER_TEMP"]) / prepare_tui_snapshots.ARTIFACT_DIRECTORY_NAME
        data = (artifact / "diagnostic.json").read_bytes() if artifact.exists() else None
        if artifact.exists():
            self.assertEqual({path.name for path in artifact.iterdir()}, {"diagnostic.json"})
        return data, stderr.getvalue()

    @staticmethod
    def _omissions(**changed):
        reasons = ("credential_shaped", "path_limit", "unsafe_syntax", "attribution_unavailable",
                   "not_prelaunch_tracked", "prelaunch_ineligible", "path_validation_failed",
                   "not_regular", "hardlinked", "observation_failed", "metadata_budget")
        return {reason: {"count": changed.get(reason, (0, 0))[0],
                         "prelaunch_tracked_count": changed.get(reason, (0, 0))[1]} for reason in reasons}

    def _expected(self, pending, code="output_count_exceeded", exit_code=101):
        def states(paths):
            return {relative: {"sha256": hashlib.sha256((self.product_root / relative).read_bytes()).hexdigest(),
                               "mode": (self.product_root / relative).stat().st_mode & 0o777} for relative in paths}
        verified = {"status": "verified", "actual_matches": True, "failure_code": ""}
        identity = {
            "workflow_host_sha": self.host_sha,
            "workflow_host_tree": self._git(self.workflow_root, "rev-parse", "HEAD^{tree}").decode().strip(),
            "product_sha": self.target_sha,
            "product_tree": self._git(self.product_root, "rev-parse", "HEAD^{tree}").decode().strip(),
            "comparison_base_sha": self.base_sha,
            "comparison_base_tree": self._git(self.product_root, "rev-parse", f"{self.base_sha}^{{tree}}").decode().strip(),
            "workflow_run_id": "123", "workflow_run_attempt": "1", "repository": "sednalabs/codex",
            "workflow": "sedna-branch-build", "runner_label": "ubuntu-24.04", "architecture": "x86_64",
        }
        paths = [{"path": relative, "origin": "historical69" if relative.removesuffix(".new") in prepare_tui_snapshots.SNAPSHOT_PATHS else "additional_tracked_snapshot",
                  "classification": "historical_allowed" if relative.removesuffix(".new") in prepare_tui_snapshots.SNAPSHOT_PATHS else "pending_source_owner"} for relative in sorted(pending)]
        evidence = {"status": "complete", "observed_count": 0, "observed_entry_count": 0,
                    "emitted_count": 0, "omitted_count": 0, "paths": [], "complete": True,
                    "queries": {query: {"status": "complete", "failure_code": "",
                        "observed_count": len(pending), "observed_entry_count": len(pending)}
                        for query in ("status", "nonignored_untracked")},
                    "pending_inventory_complete": True, "unclassified_pending_count": 0,
                    "attribution_available": True, "omission_reasons": self._omissions()}
        return {"schema_version": "sedna-tui-snapshot-diagnostic-v1", "status": "failure", "artifact_kind": "diagnostic-only",
                "generated_output_acceptance": False, "phase": "outputs-validation", "failure_code": code,
                "generator_exit_code": exit_code, "generation_attempted": True, "identity": identity,
                "historical_candidate_count": 69, "accepted_output_limit": 69,
                "inventory": {"status": "complete", "observed_count": len(paths), "observed_entry_count": len(paths), "paths": paths,
                    "emitted_count": len(paths), "omitted_count": 0, "unobserved_count": 0, "complete": True, "traversal_complete": True, "attribution_available": True,
                    "omission_reasons": self._omissions(), "omission_reasons_complete": True},
                "conservation": {"baselines": {**verified, "expected": states(prepare_tui_snapshots.SNAPSHOT_PATHS)},
                    "locks": {**verified, "expected": states(prepare_tui_snapshots.LOCK_PATHS)},
                    "other_files": {**verified, "evidence": evidence}, "workflow_host": dict(verified)}, "metadata_status": "complete"}

    def test_actual_70_outputs_persist_exact_safe_bytes_without_reading_bodies(self):
        extra = self._additional_snapshot()
        pending = [*(path + ".new" for path in prepare_tui_snapshots.SNAPSHOT_PATHS), extra]
        expected = self._expected(pending, exit_code=17)
        original_read = Path.read_bytes

        def read_without_pending(path):
            self.assertFalse(str(path).endswith(".snap.new"))
            return original_read(path)

        with mock.patch.object(Path, "read_bytes", read_without_pending):
            data, stderr = self._invoke(lambda: [self._pending(path) for path in pending], exit_code=17)
        self.assertEqual(data, (json.dumps(expected, indent=2, sort_keys=True) + "\n").encode())
        self.assertEqual(stderr, "TUI snapshot preparation failed: output_count_exceeded; artifact_state=diagnostic-only\n")
        for private in (b"PRIVATE_BODY", b"/home/runner", b"ghp_", b"https://", hashlib.sha256(b"PRIVATE_BODY").hexdigest().encode()):
            self.assertNotIn(private, data)
        self.assertNotIn(hashlib.sha256(b"PRIVATE_BODY /home/runner/private ghp_abcdefghijklmnopqrstuvwxyz123456 https://private.invalid\n").hexdigest().encode(), data)
        self.assertEqual(prepare_tui_snapshots.MAX_OUTPUTS, 69)

    def test_additional_tracked_output_is_not_autoaccepted_below_count_cap(self):
        extra = self._additional_snapshot()
        expected = self._expected([extra], code="output_outside_allowlist")
        data, _ = self._invoke(lambda: self._pending(extra))
        self.assertEqual(data, (json.dumps(expected, indent=2, sort_keys=True) + "\n").encode())

    def test_actual_80_outputs_and_complete_status_union_preserve_private_omissions(self):
        historical = [path + ".new" for path in prepare_tui_snapshots.SNAPSHOT_PATHS[:25]]
        additional = [f"codex-rs/tui/src/chatwidget/snapshots/codex_tui__inventory_{index}.snap.new" for index in range(54)]
        hidden = "codex-rs/tui/src/chatwidget/snapshots/codex_tui__github_pat_" + "x" * 20 + ".snap.new"
        sources = ["codex-rs/tui/src/diagnostic_fixture.rs", "codex-rs/tui/src/deleted_fixture.rs"]
        tracked = [path.removesuffix(".new") for path in [*additional, hidden]] + sources
        for relative in tracked:
            (self.product_root / relative).write_bytes(b"public fixture source\n")
        self._git(self.product_root, "add", "--", *tracked)
        self._commit(self.product_root, "inventory and status fixture")
        self.target_sha = self._git(self.product_root, "rev-parse", "HEAD").decode().strip()
        self.environment["TARGET_SHA"] = self.target_sha
        expected = self._expected([*historical, *additional], exit_code=100)
        expected["inventory"].update(status="partial", observed_count=80, observed_entry_count=80,
            omitted_count=1, complete=False, omission_reasons=self._omissions(credential_shaped=(1, 1)))
        paths = [{"path": sources[1], "git_status": " D", "observed_via": ["status"]},
                 {"path": sources[0], "git_status": " M", "observed_via": ["status"]}]
        evidence = expected["conservation"]["other_files"]["evidence"]
        evidence.update(status="partial", observed_count=3, observed_entry_count=3, emitted_count=2,
            omitted_count=1, paths=paths, complete=False, omission_reasons=self._omissions(unsafe_syntax=(1, 0)))
        for query, count in (("status", 83), ("nonignored_untracked", 81)):
            evidence["queries"][query].update(observed_count=count, observed_entry_count=count)
        expected["conservation"]["other_files"].update(status="failed", actual_matches=False, failure_code="conservation_mismatch")
        expected["metadata_status"] = "incomplete"
        def action():
            for relative in [*historical, *additional, hidden]:
                self._pending(relative)
            (self.product_root / sources[0]).write_bytes(b"PRIVATE_STATUS_BODY\n")
            (self.product_root / sources[1]).unlink()
            (self.product_root / "PRIVATE_STATUS credential-token").write_bytes(b"PRIVATE_UNTRACKED_BODY\n")
        original_read = Path.read_bytes
        def read(path):
            self.assertFalse(str(path).endswith(".snap.new"))
            self.assertNotIn(path, [self.product_root / relative for relative in sources])
            return original_read(path)
        with mock.patch.object(Path, "read_bytes", read):
            data, stderr = self._invoke(action, exit_code=100)
        generator_index = self.invoked_commands.index(tuple(prepare_tui_snapshots.COMMAND))
        # One baseline tracking query and one retained attribution catalogue query.
        self.assertEqual(sum(command == ("git", "-C", str(self.product_root), "ls-files", "-z")
                             for command in self.invoked_commands[:generator_index]), 2)
        after = [command[3:] for command in self.invoked_commands[generator_index + 1:]
                 if command[:3] == ("git", "-C", str(self.product_root)) and command[3:] in (
                     ("status", "--porcelain=v1", "-z", "--untracked-files=all"),
                     ("ls-files", "--others", "--exclude-standard", "-z"))]
        self.assertEqual(after, [("status", "--porcelain=v1", "-z", "--untracked-files=all"),
                                 ("ls-files", "--others", "--exclude-standard", "-z")])
        self.assertEqual(data, (json.dumps(expected, indent=2, sort_keys=True) + "\n").encode())
        self.assertEqual(stderr, "TUI snapshot preparation failed: output_count_exceeded; artifact_state=diagnostic-only\n")
        for value in (hidden.encode(), b"github_pat_", b"PRIVATE_STATUS", b"credential-token",
                      b"PRIVATE_BODY", b"/home/runner", b"https://", hashlib.sha256(b"PRIVATE_STATUS_BODY\n").hexdigest().encode()):
            self.assertNotIn(value, data)
        self.assertEqual(sum(item["count"] for item in expected["inventory"]["omission_reasons"].values()), 1)

    def test_actual_80_outputs_and_inline_item_join_only_closed_source_locators(self):
        candidate = "codex-rs/tui/src/app/snapshots/codex_tui__app__tests__app_server_thread_replacement_clears_previous_transcript_before_replay-2.snap"
        source = "codex-rs/tui/src/analytics/activity_chart_tests.rs"
        inline = "codex-rs/tui/src/analytics/.activity_chart_tests.rs.pending-snap"
        historical = [path + ".new" for path in prepare_tui_snapshots.SNAPSHOT_PATHS[:25]]
        additional = [f"codex-rs/tui/src/chatwidget/snapshots/codex_tui__closed_inventory_{index}.snap.new" for index in range(54)]
        for relative in [*(path.removesuffix(".new") for path in additional), candidate, source]:
            self._pending(relative, contents=b"public source fixture\n")
        self._git(self.product_root, "add", "--", *(path.removesuffix(".new") for path in additional), candidate, source)
        self._commit(self.product_root, "closed source locator fixture")
        self.target_sha = self._git(self.product_root, "rev-parse", "HEAD").decode().strip()
        self.environment["TARGET_SHA"] = self.target_sha
        pending = [*historical, *additional, candidate + ".new"]
        expected = self._expected([*historical, *additional], exit_code=100)
        expected["inventory"]["paths"].append({"source_locator": candidate, "kind": "tracked_snapshot_candidate", "classification": "pending_source_owner"})
        expected["inventory"]["paths"].sort(key=lambda item: item.get("path", item.get("source_locator", "")))
        expected["inventory"].update(observed_count=80, observed_entry_count=80, emitted_count=80)
        other = expected["conservation"]["other_files"]
        other.update(status="failed", actual_matches=False, failure_code="conservation_mismatch")
        other["evidence"].update(observed_count=1, observed_entry_count=1, emitted_count=1,
            paths=[{"source_locator": source, "kind": "insta_inline_pending", "git_status": "??",
                    "observed_via": ["nonignored_untracked", "status"]}])
        for query in other["evidence"]["queries"].values():
            query.update(observed_count=81, observed_entry_count=81)
        body = b"PRIVATE_CLOSED_BODY ghp_abcdefghijklmnopqrstuvwxyz123456 /home/runner/private https://private.invalid\n"
        original_read = Path.read_bytes
        def read(path):
            self.assertFalse(str(path).endswith(".snap.new"))
            self.assertNotIn(path, [self.product_root / relative for relative in (candidate, source, inline)])
            return original_read(path)
        def action():
            for relative in [*pending, inline]:
                self._pending(relative, contents=body)
        with mock.patch.object(Path, "read_bytes", read):
            data, stderr = self._invoke(action, exit_code=100)
        self.assertEqual(data, (json.dumps(expected, indent=2, sort_keys=True) + "\n").encode())
        self.assertEqual(stderr, "TUI snapshot preparation failed: output_count_exceeded; artifact_state=diagnostic-only\n")
        generator_index = self.invoked_commands.index(tuple(prepare_tui_snapshots.COMMAND))
        self.assertEqual(sum(command == ("git", "-C", str(self.product_root), "ls-files", "-z")
                             for command in self.invoked_commands[:generator_index]), 2)
        observed_queries = [command[3:] for command in self.invoked_commands[generator_index + 1:]
            if command[:3] == ("git", "-C", str(self.product_root)) and command[3:] in (
                ("status", "--porcelain=v1", "-z", "--untracked-files=all"),
                ("ls-files", "--others", "--exclude-standard", "-z"))]
        self.assertEqual(observed_queries, [("status", "--porcelain=v1", "-z", "--untracked-files=all"),
                                           ("ls-files", "--others", "--exclude-standard", "-z")])
        for private in (candidate.encode() + b".new", inline.encode(), body, b"PRIVATE_CLOSED_BODY", b"ghp_",
                        b"/home/runner", b"https://", hashlib.sha256(body).hexdigest().encode()):
            self.assertNotIn(private, data)
        self.assertFalse(expected["generated_output_acceptance"])
        self.assertEqual((expected["historical_candidate_count"], expected["accepted_output_limit"]), (69, 69))

    def test_exact_windows_degraded_candidate_is_diagnostic_not_allowed_output(self):
        source = "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__approvals_selection_popup@windows_degraded.snap"
        pending = self._additional_snapshot(source)
        expected = self._expected([], code="output_outside_allowlist")
        expected["inventory"].update(observed_count=1, observed_entry_count=1, emitted_count=1,
            paths=[{"source_locator": source, "kind": "tracked_snapshot_candidate", "classification": "pending_source_owner"}])
        for query in expected["conservation"]["other_files"]["evidence"]["queries"].values():
            query.update(observed_count=1, observed_entry_count=1)
        data, stderr = self._invoke(lambda: self._pending(pending))
        self.assertEqual(data, (json.dumps(expected, indent=2, sort_keys=True) + "\n").encode())
        self.assertEqual(stderr, "TUI snapshot preparation failed: output_outside_allowlist; artifact_state=diagnostic-only\n")
        self.assertNotIn(pending.encode(), data)
        self.assertFalse(json.loads(data)["generated_output_acceptance"])

    def test_closed_candidate_missing_prelaunch_source_stays_omitted(self):
        source = "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__approvals_selection_popup@windows_degraded.snap"
        def action():
            self._pending(source, contents=b"source appeared after prelaunch\n")
            self._pending(source + ".new")
        data, _ = self._invoke(action)
        inventory = json.loads(data)["inventory"]
        self.assertEqual((inventory["observed_count"], inventory["emitted_count"], inventory["omitted_count"], inventory["complete"]), (1, 0, 1, False))
        self.assertEqual(inventory["omission_reasons"], self._omissions(not_prelaunch_tracked=(1, 0)))
        self.assertNotIn(source.encode(), data)

    def test_inline_locator_missing_prelaunch_source_does_not_classify_untracked_item(self):
        source = "codex-rs/tui/src/analytics/activity_chart_tests.rs"
        self._pending(source, contents=b"regular but untracked source fixture\n")
        (self.product_root / ".git/info/exclude").write_text(source + "\n", encoding="utf-8")
        inline = "codex-rs/tui/src/analytics/.activity_chart_tests.rs.pending-snap"
        pending = prepare_tui_snapshots.SNAPSHOT_PATHS[0] + ".new"
        def action():
            self._pending(pending, contents=b"valid pending fixture\n")
            self._pending(inline)
        data, _ = self._invoke(action)
        other = json.loads(data)["conservation"]["other_files"]
        self.assertEqual((other["status"], other["actual_matches"], other["evidence"]["omitted_count"]), ("failed", False, 1))
        self.assertEqual(other["evidence"]["omission_reasons"], self._omissions(not_prelaunch_tracked=(1, 0)))
        self.assertFalse(other["evidence"]["complete"])
        self.assertNotIn(inline.encode(), data)
        self.assertNotIn(b"source_locator", data)

    def test_near_locator_and_token_canary_never_join_known_source(self):
        source = "codex-rs/tui/src/analytics/activity_chart_tests.rs"
        self._additional_snapshot(source)
        inline = "codex-rs/tui/src/analytics/.activity_chart_tests.rs.pending-snap.PRIVATE_CANARY"
        token_path = "codex-rs/tui/src/chatwidget/snapshots/codex_tui__chatwidget__tests__approvals_selection_popup@windows_degraded_github_pat_" + "x" * 20 + ".snap.new"
        pending = prepare_tui_snapshots.SNAPSHOT_PATHS[0] + ".new"
        def action():
            self._pending(pending, contents=b"valid pending fixture\n")
            for path in (inline, token_path):
                self._pending(path)
        data, _ = self._invoke(action)
        public = json.loads(data)
        self.assertEqual(public["inventory"]["omission_reasons"], self._omissions(credential_shaped=(1, 0)))
        other = public["conservation"]["other_files"]
        self.assertEqual((other["status"], other["actual_matches"], other["evidence"]["paths"], other["evidence"]["omitted_count"]), ("failed", False, [], 1))
        self.assertFalse(public["inventory"]["complete"])
        for value in (inline.encode(), token_path.encode(), b"PRIVATE_CANARY", b"github_pat_", b"source_locator"):
            self.assertNotIn(value, data)

    def test_closed_candidate_requires_prelaunch_single_link_source(self):
        source = "codex-rs/tui/src/app/snapshots/codex_tui__app__tests__app_server_thread_replacement_clears_previous_transcript_before_replay-2.snap"
        pending = self._additional_snapshot(source)
        os.link(self.product_root / source, self.product_root / ".git/private-source-link")
        data, _ = self._invoke(lambda: self._pending(pending))
        inventory = json.loads(data)["inventory"]
        self.assertEqual(inventory["omission_reasons"], self._omissions(prelaunch_ineligible=(1, 1)))
        self.assertEqual((inventory["emitted_count"], inventory["omitted_count"], inventory["complete"]), (0, 1, False))
        self.assertNotIn(source.encode(), data)

    def test_inline_locator_requires_prelaunch_single_link_source(self):
        source = "codex-rs/tui/src/analytics/activity_chart_tests.rs"
        self._additional_snapshot(source)
        os.link(self.product_root / source, self.product_root / ".git/private-source-link")
        inline = "codex-rs/tui/src/analytics/.activity_chart_tests.rs.pending-snap"
        pending = prepare_tui_snapshots.SNAPSHOT_PATHS[0] + ".new"
        def action():
            self._pending(pending, contents=b"valid pending fixture\n")
            self._pending(inline)
        data, _ = self._invoke(action)
        other = json.loads(data)["conservation"]["other_files"]
        self.assertEqual(other["evidence"]["omission_reasons"], self._omissions(prelaunch_ineligible=(1, 1)))
        self.assertEqual((other["status"], other["actual_matches"], other["evidence"]["emitted_count"]), ("failed", False, 0))
        self.assertNotIn(source.encode(), data)

    def test_inline_locator_requires_regular_not_symlink_prelaunch_source(self):
        source = "codex-rs/tui/src/analytics/activity_chart_tests.rs"
        self._additional_snapshot(source)
        (self.product_root / source).unlink()
        (self.product_root / source).symlink_to(self.product_root / prepare_tui_snapshots.LOCK_PATHS[0])
        self._git(self.product_root, "add", "--", source)
        self._commit(self.product_root, "ineligible source fixture")
        self.target_sha = self._git(self.product_root, "rev-parse", "HEAD").decode().strip()
        self.environment["TARGET_SHA"] = self.target_sha
        inline = "codex-rs/tui/src/analytics/.activity_chart_tests.rs.pending-snap"
        pending = prepare_tui_snapshots.SNAPSHOT_PATHS[0] + ".new"
        def action():
            self._pending(pending, contents=b"valid pending fixture\n")
            self._pending(inline)
        data, _ = self._invoke(action)
        other = json.loads(data)["conservation"]["other_files"]
        self.assertEqual(other["evidence"]["omission_reasons"], self._omissions(prelaunch_ineligible=(1, 1)))
        self.assertEqual((other["status"], other["actual_matches"], other["evidence"]["emitted_count"]), ("failed", False, 0))
        self.assertNotIn(source.encode(), data)

    def test_closed_candidate_observed_output_must_be_single_link(self):
        source = "codex-rs/tui/src/app/snapshots/codex_tui__app__tests__app_server_thread_replacement_clears_previous_transcript_before_replay-2.snap"
        pending = self._additional_snapshot(source)
        def action():
            self._pending(pending)
            os.link(self.product_root / pending, self.product_root / ".git/private-output-link")
        data, _ = self._invoke(action)
        inventory = json.loads(data)["inventory"]
        self.assertEqual(inventory["omission_reasons"], self._omissions(hardlinked=(1, 1)))
        self.assertFalse(inventory["complete"])
        self.assertNotIn(source.encode(), data)

    def test_inline_observed_symlink_is_not_a_source_locator_join(self):
        source = "codex-rs/tui/src/analytics/activity_chart_tests.rs"
        self._additional_snapshot(source)
        inline = "codex-rs/tui/src/analytics/.activity_chart_tests.rs.pending-snap"
        pending = prepare_tui_snapshots.SNAPSHOT_PATHS[0] + ".new"
        def action():
            self._pending(pending, contents=b"valid pending fixture\n")
            (self.product_root / inline).symlink_to(self.product_root / prepare_tui_snapshots.LOCK_PATHS[0])
        data, _ = self._invoke(action)
        other = json.loads(data)["conservation"]["other_files"]
        self.assertEqual(other["evidence"]["omission_reasons"], self._omissions(path_validation_failed=(1, 1)))
        self.assertEqual((other["status"], other["actual_matches"], other["evidence"]["emitted_count"]), ("failed", False, 0))
        self.assertNotIn(inline.encode(), data)
        self.assertNotIn(source.encode(), data)

    def test_metadata_overflow_drops_closed_locators_with_exact_omission_truth(self):
        diagnostic = self._expected([])
        paths = [{"path": f"codex-rs/tui/src/snapshots/codex_tui__{'x' * 180}{index}.snap.new", "origin": "additional_tracked_snapshot", "classification": "pending_source_owner"} for index in range(2000)]
        paths.extend({"source_locator": source, "kind": "tracked_snapshot_candidate", "classification": "pending_source_owner"}
                     for source in prepare_tui_snapshots.DIAGNOSTIC_SNAPSHOT_CANDIDATES)
        diagnostic["inventory"].update(paths=paths, observed_count=2002, observed_entry_count=2002, emitted_count=2002)
        other = diagnostic["conservation"]["other_files"]
        other.update(status="failed", actual_matches=False, failure_code="conservation_mismatch")
        other["evidence"].update(paths=[{"source_locator": prepare_tui_snapshots.DIAGNOSTIC_INLINE_SOURCE,
            "kind": "insta_inline_pending", "git_status": "??", "observed_via": ["nonignored_untracked", "status"]}],
            observed_count=1, observed_entry_count=1, emitted_count=1)
        runner = Path(self.environment["RUNNER_TEMP"])
        prepare_tui_snapshots._write_diagnostic_artifact(runner / "locator-overflow-staging", runner / "locator-overflow-artifact", diagnostic)
        data = (runner / "locator-overflow-artifact/diagnostic.json").read_bytes()
        public = json.loads(data)
        self.assertLessEqual(len(data), prepare_tui_snapshots.MAX_METADATA_BYTES)
        self.assertEqual((public["metadata_status"], public["generated_output_acceptance"]), ("incomplete", False))
        self.assertEqual((public["inventory"]["paths"], public["inventory"]["omitted_count"]), ([], 2002))
        self.assertEqual(public["inventory"]["omission_reasons"], self._omissions(metadata_budget=(2002, 2002)))
        self.assertEqual(public["conservation"]["other_files"]["evidence"]["omission_reasons"], self._omissions(metadata_budget=(1, 1)))
        self.assertEqual((public["conservation"]["other_files"]["status"], public["conservation"]["other_files"]["actual_matches"]), ("failed", False))
        self.assertNotIn(b"source_locator", data)

    def test_long_tracked_path_is_counted_before_syntax_or_attribution(self):
        relative = "codex-rs/tui/src/" + ("x" * 64 + "/") * 16 + "codex_tui__long.snap"
        pending = self._additional_snapshot(relative)
        data, _ = self._invoke(lambda: self._pending(pending))
        public = json.loads(data)
        self.assertEqual(public["inventory"]["omission_reasons"], self._omissions(path_limit=(1, 1)))
        self.assertEqual((public["inventory"]["observed_count"], public["inventory"]["omitted_count"]), (1, 1))
        self.assertNotIn(pending.encode(), data)

    def test_prelaunch_rejection_keeps_missing_catalogue_and_known_omission_count(self):
        (self.product_root / ".git/info/exclude").write_text("*.snap.new\n", encoding="utf-8")
        pending = prepare_tui_snapshots.SNAPSHOT_PATHS[0] + ".new"
        self._pending(pending)
        data, _ = self._invoke(lambda: self.fail("preexisting output must prevent generation"), generator_count=0)
        public = json.loads(data)
        self.assertEqual(public["inventory"]["omission_reasons"],
            {reason: {"count": int(reason == "attribution_unavailable"), "prelaunch_tracked_count": None}
             for reason in self._omissions()})
        self.assertEqual((public["inventory"]["observed_count"], public["inventory"]["complete"], public["generator_exit_code"]), (1, False, None))
        self.assertNotIn(pending.encode(), data)

    def test_tracked_but_prelaunch_ineligible_source_is_not_named(self):
        pending = self._additional_snapshot()
        os.link(self.product_root / pending.removesuffix(".new"), self.product_root / ".git/private-source-link")
        data, _ = self._invoke(lambda: self._pending(pending))
        public = json.loads(data)
        self.assertEqual(public["inventory"]["omission_reasons"], self._omissions(prelaunch_ineligible=(1, 1)))
        self.assertFalse(public["inventory"]["complete"])
        self.assertNotIn(pending.encode(), data)

    def test_special_pending_file_is_counted_without_opening_it(self):
        pending = prepare_tui_snapshots.SNAPSHOT_PATHS[0] + ".new"
        data, _ = self._invoke(lambda: os.mkfifo(self.product_root / pending))
        public = json.loads(data)
        self.assertEqual(public["inventory"]["omission_reasons"], self._omissions(not_regular=(1, 1)))
        self.assertEqual((public["inventory"]["emitted_count"], public["inventory"]["omitted_count"]), (0, 1))
        self.assertNotIn(pending.encode(), data)

    def test_pending_observation_error_is_coded_and_counted_without_raw_exception(self):
        pending = prepare_tui_snapshots.SNAPSHOT_PATHS[0] + ".new"
        original_path = prepare_tui_snapshots._relative_path
        observed = []
        def relative_path(root, relative):
            self.assertEqual(root, self.product_root)
            if relative == pending:
                observed.append(relative)
                raise OSError("PRIVATE_STAT https://private.invalid")
            self.assertIn(relative, (*prepare_tui_snapshots.SNAPSHOT_PATHS, *prepare_tui_snapshots.LOCK_PATHS))
            return original_path(root, relative)
        with mock.patch.object(prepare_tui_snapshots, "_relative_path", side_effect=relative_path):
            data, _ = self._invoke(lambda: self._pending(pending))
        self.assertEqual(observed, [pending, pending])
        self.assertEqual(json.loads(data)["inventory"]["omission_reasons"], self._omissions(observation_failed=(1, 1)))
        self.assertNotIn(b"PRIVATE_STAT", data)
        self.assertNotIn(b"https://", data)

    def test_credential_precedence_suppresses_tracked_unsafe_and_untracked_names(self):
        tracked = self._additional_snapshot("codex-rs/tui/src/chatwidget/snapshots/codex_tui__github_pat_" + "x" * 20 + " PRIVATE_NAME.snap")
        untracked = "codex-rs/tui/src/chatwidget/codex_tui__sk-" + "y" * 20 + ".snap.new"
        data, _ = self._invoke(lambda: [self._pending(path) for path in (tracked, untracked)])
        public = json.loads(data)
        self.assertEqual(public["inventory"]["omission_reasons"], self._omissions(credential_shaped=(2, 1)))
        self.assertEqual((public["inventory"]["observed_count"], public["inventory"]["emitted_count"], public["inventory"]["omitted_count"]), (2, 0, 2))
        for value in (tracked.encode(), untracked.encode(), b"PRIVATE_NAME", b"github_pat_", b"sk-"):
            self.assertNotIn(value, data)

    def test_known_status_mismatch_survives_second_query_failure(self):
        pending = self._additional_snapshot()
        failed_calls = []
        def fault(arguments):
            if arguments == ("ls-files", "--others", "--exclude-standard", "-z"):
                failed_calls.append(arguments)
                raise OSError("PRIVATE_QUERY /private https://private.invalid")
        def action():
            self._pending(pending)
            (self.product_root / "new_status_file.rs").write_bytes(b"PRIVATE_STATUS_BODY\n")
        data, _ = self._invoke(action, post_generator_git=fault)
        self.assertEqual(failed_calls, [("ls-files", "--others", "--exclude-standard", "-z")])
        public = json.loads(data)
        other = public["conservation"]["other_files"]
        self.assertEqual((other["status"], other["actual_matches"], other["evidence"]["observed_count"], other["evidence"]["observed_entry_count"]), ("failed", False, None, 1))
        self.assertEqual(other["evidence"]["queries"]["nonignored_untracked"]["status"], "unknown")
        self.assertEqual(other["evidence"]["omission_reasons"], self._omissions(not_prelaunch_tracked=(1, 0)))
        self.assertEqual(public["metadata_status"], "incomplete")
        for value in (b"PRIVATE_QUERY", b"new_status_file.rs", b"PRIVATE_STATUS_BODY", b"https://"):
            self.assertNotIn(value, data)

    def test_list_only_record_has_null_status_and_duplicate_rows_count_once(self):
        pending = self._additional_snapshot()
        source = pending.removesuffix(".new")
        selected = []
        def observation(arguments):
            if arguments == ("ls-files", "--others", "--exclude-standard", "-z"):
                selected.append(arguments)
                return (pending + "\0" + source + "\0" + source + "\0").encode()
        data, _ = self._invoke(lambda: self._pending(pending), post_generator_git=observation)
        self.assertEqual(selected, [("ls-files", "--others", "--exclude-standard", "-z")])
        evidence = json.loads(data)["conservation"]["other_files"]["evidence"]
        self.assertEqual(evidence["paths"], [{"path": source, "git_status": None, "observed_via": ["nonignored_untracked"]}])
        self.assertEqual((evidence["observed_count"], evidence["emitted_count"], evidence["omitted_count"], evidence["complete"]), (1, 1, 0, True))
        self.assertEqual(evidence["queries"]["nonignored_untracked"]["observed_count"], 3)
        expected = self._expected([pending], code="output_outside_allowlist")
        expected["conservation"]["other_files"].update(status="failed", actual_matches=False, failure_code="conservation_mismatch")
        expected["conservation"]["other_files"]["evidence"].update(observed_count=1, observed_entry_count=1,
            emitted_count=1, paths=[{"path": source, "git_status": None, "observed_via": ["nonignored_untracked"]}])
        expected["conservation"]["other_files"]["evidence"]["queries"]["nonignored_untracked"].update(observed_count=3, observed_entry_count=3)
        self.assertEqual(data, (json.dumps(expected, indent=2, sort_keys=True) + "\n").encode())

    def test_malformed_status_state_remains_unknown_and_second_query_runs(self):
        pending = self._additional_snapshot()
        selected = []
        def observation(arguments):
            if arguments == ("status", "--porcelain=v1", "-z", "--untracked-files=all"):
                selected.append(arguments)
                return b"ZZ PRIVATE_STATUS_NAME\0"
        data, _ = self._invoke(lambda: self._pending(pending), post_generator_git=observation)
        self.assertEqual(len(selected), 1)
        evidence = json.loads(data)["conservation"]["other_files"]["evidence"]
        self.assertEqual(evidence["queries"]["status"]["status"], "unknown")
        self.assertEqual(evidence["queries"]["nonignored_untracked"]["status"], "complete")
        self.assertEqual((evidence["observed_count"], evidence["paths"], evidence["complete"]), (None, [], False))
        self.assertEqual(json.loads(data)["conservation"]["other_files"]["status"], "unknown")
        self.assertEqual(json.loads(data)["metadata_status"], "incomplete")
        self.assertNotIn(b"PRIVATE_STATUS_NAME", data)

    def test_rename_and_undecodable_query_material_never_becomes_public_evidence(self):
        pending = self._additional_snapshot()
        selected = []
        def observation(arguments):
            if arguments == ("status", "--porcelain=v1", "-z", "--untracked-files=all"):
                selected.append("status")
                return b"R  PRIVATE_RENAME\0PRIVATE_OLD\0"
            if arguments == ("ls-files", "--others", "--exclude-standard", "-z"):
                selected.append("nonignored_untracked")
                return b"PRIVATE_UNDECODABLE\xff\0"
        data, _ = self._invoke(lambda: self._pending(pending), post_generator_git=observation)
        self.assertEqual(selected, ["status", "nonignored_untracked"])
        other = json.loads(data)["conservation"]["other_files"]
        self.assertEqual((other["status"], other["actual_matches"], other["evidence"]["observed_count"]), ("unknown", None, None))
        self.assertFalse(other["evidence"]["complete"])
        for value in (b"PRIVATE_RENAME", b"PRIVATE_OLD", b"PRIVATE_UNDECODABLE"):
            self.assertNotIn(value, data)

    def test_conflicting_duplicate_status_does_not_claim_complete_last_state(self):
        pending = self._additional_snapshot()
        source = pending.removesuffix(".new")
        selected = []
        def observation(arguments):
            if arguments == ("status", "--porcelain=v1", "-z", "--untracked-files=all"):
                selected.append(arguments)
                return (" M " + source + "\0 D " + source + "\0").encode()
        data, _ = self._invoke(lambda: self._pending(pending), post_generator_git=observation)
        self.assertEqual(len(selected), 1)
        other = json.loads(data)["conservation"]["other_files"]
        self.assertEqual((other["status"], other["actual_matches"], other["evidence"]["observed_count"]), ("failed", False, None))
        self.assertEqual(other["evidence"]["paths"], [{"path": source, "git_status": " M", "observed_via": ["status"]}])
        self.assertFalse(other["evidence"]["complete"])

    def test_unsafe_unattributed_names_are_omitted_with_actual_count(self):
        pending = [prepare_tui_snapshots.SNAPSHOT_PATHS[0] + ".new",
                   "codex-rs/tui/src/chatwidget/PRIVATE_FILENAME credential-token.snap.new",
                   "codex-rs/tui/src/chatwidget/codex_tui__unattributed.snap.new"]
        data, _ = self._invoke(lambda: [self._pending(path) for path in pending])
        inventory = json.loads(data)["inventory"]
        self.assertEqual((inventory["observed_count"], inventory["emitted_count"], inventory["omitted_count"], inventory["complete"]), (3, 1, 2, False))
        self.assertNotIn(b"PRIVATE_FILENAME", data)
        self.assertNotIn(b"credential-token", data)
        self.assertNotIn(b"unattributed", data)
        self.assertEqual(inventory["omission_reasons"], self._omissions(unsafe_syntax=(1, 0), not_prelaunch_tracked=(1, 0)))
        self.assertEqual(json.loads(data)["metadata_status"], "incomplete")

    def test_token_like_tracked_name_is_omitted_not_trusted_for_publication(self):
        pending = self._additional_snapshot("codex-rs/tui/src/chatwidget/snapshots/codex_tui__ghp_abcdefghijklmnopqrstuvwxyz123456.snap")
        data, _ = self._invoke(lambda: self._pending(pending))
        inventory = json.loads(data)["inventory"]
        self.assertEqual((inventory["observed_count"], inventory["emitted_count"], inventory["omitted_count"], inventory["complete"]), (1, 0, 1, False))
        self.assertNotIn(b"ghp_", data)
        self.assertEqual(inventory["omission_reasons"], self._omissions(credential_shaped=(1, 1)))

    def test_fine_grained_token_tracked_name_persists_exact_omitted_incomplete_bytes(self):
        token_shape = "github_pat_" + "x" * 20
        pending = self._additional_snapshot(f"codex-rs/tui/src/chatwidget/snapshots/codex_tui__{token_shape}.snap")
        expected = self._expected([], code="output_outside_allowlist")
        expected["inventory"].update(status="partial", observed_count=1, observed_entry_count=1,
                                     omitted_count=1, complete=False,
                                     omission_reasons=self._omissions(credential_shaped=(1, 1)))
        for query in expected["conservation"]["other_files"]["evidence"]["queries"].values():
            query.update(observed_count=1, observed_entry_count=1)
        expected["metadata_status"] = "incomplete"
        body = b"PRIVATE_FINE_GRAINED_BODY https://private.invalid /home/runner/private\n"
        data, stderr = self._invoke(lambda: self._pending(pending, contents=body))
        self.assertEqual(data, (json.dumps(expected, indent=2, sort_keys=True) + "\n").encode())
        self.assertEqual(stderr, "TUI snapshot preparation failed: output_outside_allowlist; artifact_state=diagnostic-only\n")
        for omitted in (token_shape.encode(), pending.encode(), body, hashlib.sha256(body).hexdigest().encode()):
            self.assertNotIn(omitted, data)

    def test_symlink_pending_and_hidden_symlink_directory_are_partial_unknown(self):
        def action():
            destination = self.product_root / (prepare_tui_snapshots.SNAPSHOT_PATHS[0] + ".new")
            destination.symlink_to(self.product_root / prepare_tui_snapshots.LOCK_PATHS[0])
            (self.product_root / prepare_tui_snapshots.TUI_SOURCE_ROOT / "hidden-link").symlink_to(self.workflow_root, target_is_directory=True)
        data, _ = self._invoke(action)
        inventory = json.loads(data)["inventory"]
        self.assertIsNone(inventory["observed_count"])
        self.assertFalse(inventory["complete"])
        self.assertEqual(inventory["omitted_count"], 1)
        self.assertEqual(inventory["omission_reasons"], self._omissions(path_validation_failed=(1, 1)))

    def test_conservation_checks_all_continue_after_first_baseline_rejection(self):
        pending = prepare_tui_snapshots.SNAPSHOT_PATHS[0] + ".new"
        def action():
            self._pending(pending)
            (self.product_root / prepare_tui_snapshots.SNAPSHOT_PATHS[0]).write_bytes(b"PRIVATE_BASELINE\n")
            (self.product_root / prepare_tui_snapshots.LOCK_PATHS[0]).write_bytes(b"PRIVATE_LOCK\n")
            (self.product_root / "other-file").write_bytes(b"PRIVATE_OTHER\n")
            (self.workflow_root / "host.txt").write_bytes(b"PRIVATE_HOST\n")
        data, _ = self._invoke(action)
        public = json.loads(data)
        self.assertEqual(public["failure_code"], "baseline_changed")
        self.assertEqual({key: value["status"] for key, value in public["conservation"].items()},
                         {"baselines": "failed", "locks": "failed", "other_files": "failed", "workflow_host": "failed"})
        for private in (b"PRIVATE_BASELINE", b"PRIVATE_LOCK", b"PRIVATE_OTHER", b"PRIVATE_HOST"):
            self.assertNotIn(private, data)
            self.assertNotIn(hashlib.sha256(private + b"\n").hexdigest().encode(), data)

    def test_generator_launch_exception_keeps_null_actual_exit_and_no_raw_error(self):
        def action():
            raise OSError("PRIVATE_EXCEPTION /private https://private.invalid credential-token")
        data, stderr = self._invoke(action)
        public = json.loads(data)
        self.assertEqual((public["phase"], public["failure_code"], public["generator_exit_code"]), ("generator", "generator_launch_failed", None))
        self.assertTrue(public["generation_attempted"])
        self.assertNotIn(b"PRIVATE_EXCEPTION", data)
        self.assertNotIn("credential-token", stderr)

    def test_malformed_generator_return_never_claims_an_actual_exit(self):
        data, _ = self._invoke(lambda: None, exit_code="PRIVATE_INVALID_EXIT")
        public = json.loads(data)
        self.assertEqual((public["phase"], public["failure_code"], public["generator_exit_code"]),
                         ("generator", "generator_result_invalid", None))
        self.assertNotIn(b"PRIVATE_INVALID_EXIT", data)

    def test_hardlinked_pending_name_is_omitted_and_not_accepted(self):
        pending = prepare_tui_snapshots.SNAPSHOT_PATHS[0] + ".new"
        def action():
            self._pending(pending)
            os.link(self.product_root / pending, self.product_root / "private-hardlink")
        data, _ = self._invoke(action)
        public = json.loads(data)
        self.assertEqual((public["inventory"]["observed_count"], public["inventory"]["emitted_count"],
                          public["inventory"]["omitted_count"], public["inventory"]["complete"]), (1, 0, 1, False))
        self.assertEqual(public["conservation"]["other_files"]["status"], "failed")
        self.assertEqual(public["inventory"]["omission_reasons"], self._omissions(hardlinked=(1, 1)))
        self.assertNotIn(b"private-hardlink", data)

    def test_prelaunch_capture_failure_records_not_run_without_generator(self):
        with mock.patch.object(prepare_tui_snapshots, "_capture_baseline", side_effect=OSError("PRIVATE_CAPTURE")) as capture:
            data, _ = self._invoke(lambda: self.fail("generator must not run"), generator_count=0)
        capture.assert_called_once()
        public = json.loads(data)
        self.assertEqual((public["phase"], public["generator_exit_code"], public["generation_attempted"]),
                         ("prelaunch", None, False))
        self.assertEqual(public["conservation"]["baselines"]["status"], "not-run")
        self.assertEqual(public["conservation"]["locks"]["status"], "not-run")
        self.assertEqual(public["conservation"]["workflow_host"]["status"], "verified")
        self.assertNotIn(b"PRIVATE_CAPTURE", data)

    def test_one_conservation_observation_failure_is_unknown_not_verified(self):
        original_capture = prepare_tui_snapshots._capture_baseline
        captures = []
        def capture(root):
            captures.append(root)
            if len(captures) == 1:
                return original_capture(root)
            raise OSError("PRIVATE_OBSERVATION")
        with mock.patch.object(prepare_tui_snapshots, "_capture_baseline", side_effect=capture):
            data, _ = self._invoke(lambda: None)
        self.assertEqual(captures, [self.product_root] * 3)
        public = json.loads(data)
        self.assertEqual(public["conservation"]["baselines"]["status"], "unknown")
        self.assertEqual(public["conservation"]["locks"]["status"], "verified")
        self.assertEqual(public["conservation"]["other_files"]["status"], "verified")
        self.assertEqual(public["conservation"]["workflow_host"]["status"], "verified")
        self.assertEqual(public["metadata_status"], "incomplete")
        self.assertNotIn(b"PRIVATE_OBSERVATION", data)

    def test_collector_failure_does_not_suppress_independent_checks(self):
        extra = self._additional_snapshot()
        with mock.patch.object(prepare_tui_snapshots, "_diagnostic_inventory", side_effect=OSError("PRIVATE_COLLECTOR")) as collector:
            data, _ = self._invoke(lambda: self._pending(extra))
        collector.assert_called_once()
        public = json.loads(data)
        self.assertIsNone(public["inventory"]["observed_count"])
        self.assertFalse(public["inventory"]["complete"])
        self.assertEqual(public["conservation"]["baselines"]["status"], "verified")
        self.assertEqual(public["conservation"]["locks"]["status"], "verified")
        self.assertEqual(public["conservation"]["workflow_host"]["status"], "verified")
        self.assertNotIn(b"PRIVATE_COLLECTOR", data)
        other = public["conservation"]["other_files"]
        self.assertEqual((other["status"], other["actual_matches"], other["evidence"]["unclassified_pending_count"]),
                         ("unknown", None, 1))

    def test_diagnostic_persistence_failure_is_unknown_with_no_retry(self):
        extra = self._additional_snapshot()
        original_write = Path.write_bytes
        writes = []
        def write(path, data):
            if path.name == "diagnostic.json":
                writes.append(path)
                raise OSError("PRIVATE_PERSISTENCE")
            return original_write(path, data)
        with mock.patch.object(Path, "write_bytes", write):
            data, stderr = self._invoke(lambda: self._pending(extra))
        self.assertIsNone(data)
        self.assertEqual(len(writes), 1)
        self.assertEqual(stderr, "TUI snapshot preparation failed: diagnostic_persistence_failed; artifact_state=unknown\n")

    def test_unpersistable_metadata_cap_has_coded_unknown_artifact_state(self):
        extra = self._additional_snapshot()
        with mock.patch.object(prepare_tui_snapshots, "MAX_METADATA_BYTES", 1):
            data, stderr = self._invoke(lambda: self._pending(extra))
        self.assertIsNone(data)
        self.assertEqual(stderr, "TUI snapshot preparation failed: diagnostic_metadata_overflow; artifact_state=unknown\n")
        self.assertFalse((Path(self.environment["RUNNER_TEMP"]) / prepare_tui_snapshots.ARTIFACT_STAGING_DIRECTORY_NAME).exists())

    def test_metadata_overflow_persists_only_incomplete_bounded_metadata(self):
        diagnostic = self._expected([])
        paths = [{"path": f"codex-rs/tui/src/snapshots/codex_tui__{'x' * 180}{index}.snap.new", "origin": "additional_tracked_snapshot", "classification": "pending_source_owner"} for index in range(2000)]
        diagnostic["inventory"].update(paths=paths, observed_count=2000, observed_entry_count=2000, emitted_count=2000)
        other_paths = [{"path": f"codex-rs/tui/src/{'x' * 180}{index}.rs", "git_status": " M", "observed_via": ["status"]} for index in range(2000)]
        diagnostic["conservation"]["other_files"].update(status="failed", actual_matches=False, failure_code="conservation_mismatch")
        diagnostic["conservation"]["other_files"]["evidence"].update(paths=other_paths, observed_count=2000,
            observed_entry_count=2000, emitted_count=2000)
        runner = Path(self.environment["RUNNER_TEMP"])
        staging, artifact = runner / "overflow-staging", runner / "overflow-artifact"
        prepare_tui_snapshots._write_diagnostic_artifact(staging, artifact, diagnostic)
        data = (artifact / "diagnostic.json").read_bytes()
        public = json.loads(data)
        self.assertLessEqual(len(data), prepare_tui_snapshots.MAX_METADATA_BYTES)
        self.assertEqual((public["status"], public["metadata_status"], public["inventory"]["complete"]), ("failure", "incomplete", False))
        self.assertEqual((public["inventory"]["paths"], public["inventory"]["omitted_count"]), ([], 2000))
        self.assertEqual(public["inventory"]["omission_reasons"], self._omissions(metadata_budget=(2000, 2000)))
        other = public["conservation"]["other_files"]
        self.assertEqual((other["status"], other["actual_matches"], other["evidence"]["paths"], other["evidence"]["omitted_count"]),
                         ("failed", False, [], 2000))
        self.assertEqual(other["evidence"]["omission_reasons"], self._omissions(metadata_budget=(2000, 2000)))
        self.assertFalse(other["evidence"]["complete"])
        self.assertGreater(public["original_metadata_bytes"], prepare_tui_snapshots.MAX_METADATA_BYTES)
        with mock.patch.object(prepare_tui_snapshots, "MAX_METADATA_BYTES", 1):
            with self.assertRaises(prepare_tui_snapshots.SnapshotPreparationError):
                prepare_tui_snapshots._write_diagnostic_artifact(runner / "tiny-staging", runner / "tiny-artifact", diagnostic)
            self.assertFalse((runner / "tiny-staging").exists())

    def test_overflow_retains_known_seen_count_without_inventing_complete_total(self):
        diagnostic = self._expected([])
        paths = [{"path": f"codex-rs/tui/src/snapshots/codex_tui__{'x' * 180}{index}.snap.new", "origin": "additional_tracked_snapshot", "classification": "pending_source_owner"} for index in range(2000)]
        diagnostic["inventory"].update(paths=paths, observed_count=None, observed_entry_count=2000,
            emitted_count=2000, traversal_complete=False, unobserved_count=None, complete=False)
        runner = Path(self.environment["RUNNER_TEMP"])
        prepare_tui_snapshots._write_diagnostic_artifact(runner / "partial-staging", runner / "partial-artifact", diagnostic)
        data = (runner / "partial-artifact/diagnostic.json").read_bytes()
        inventory = json.loads(data)["inventory"]
        self.assertLessEqual(len(data), prepare_tui_snapshots.MAX_METADATA_BYTES)
        self.assertEqual((inventory["observed_count"], inventory["observed_entry_count"], inventory["omitted_count"], inventory["complete"]),
                         (None, 2000, 2000, False))
        self.assertEqual(inventory["omission_reasons"], self._omissions(metadata_budget=(2000, 2000)))

    def test_invalid_identity_never_launches_or_persists_and_main_error_is_coded(self):
        with mock.patch.dict(os.environ, {**self.environment, "TARGET_SHA": "PRIVATE_IDENTITY"}, clear=True), \
                mock.patch("sys.argv", ["runner"]), mock.patch("sys.stderr", new_callable=io.StringIO) as stderr, \
                mock.patch.object(subprocess, "run") as run:
            self.assertEqual(prepare_tui_snapshots.main(), 1)
        run.assert_not_called()
        self.assertEqual(stderr.getvalue(), "TUI snapshot preparation failed: input_identity_invalid; artifact_state=unknown\n")
        self.assertFalse((Path(self.environment["RUNNER_TEMP"]) / prepare_tui_snapshots.ARTIFACT_DIRECTORY_NAME).exists())


if __name__ == "__main__":
    unittest.main()
