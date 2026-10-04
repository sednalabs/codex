"""Hosted regression controls for the closed config-schema preparation route."""

import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("prepare_config_schema.py")
SPEC = importlib.util.spec_from_file_location("prepare_config_schema", SCRIPT)
prepare_config_schema = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(prepare_config_schema)


class ProfileTests(unittest.TestCase):
    def test_omitted_and_explicit_cargo_schema_keep_legacy_default(self):
        self.assertEqual(prepare_config_schema.resolve_profile(None), "cargo-schema")
        self.assertEqual(
            prepare_config_schema.validate_profile(
                "prepare-only", "cargo-schema"
            ),
            "cargo-schema",
        )
        self.assertEqual(prepare_config_schema.validate_profile("build", None), "cargo-schema")
        self.assertEqual(
            prepare_config_schema.validate_profile("consume-existing", "cargo-schema"),
            "cargo-schema",
        )

    def test_config_schema_is_prepare_only(self):
        self.assertEqual(
            prepare_config_schema.validate_profile("prepare-only", "config-schema"),
            "config-schema",
        )
        for mode in ("build", "consume-existing"):
            with self.subTest(mode=mode), self.assertRaisesRegex(
                ValueError, "restricted"
            ):
                prepare_config_schema.validate_profile(mode, "config-schema")

    def test_unknown_empty_and_unknown_mode_are_rejected(self):
        for profile in ("", "unknown"):
            with self.subTest(profile=profile), self.assertRaisesRegex(
                ValueError, "profile"
            ):
                prepare_config_schema.validate_profile("prepare-only", profile)
        with self.assertRaisesRegex(ValueError, "workflow mode"):
            prepare_config_schema.validate_profile("unknown", "cargo-schema")

    def test_workflow_defaults_to_and_guards_the_closed_profiles(self):
        workflow = (Path(__file__).parents[1] / "workflows/sedna-branch-build.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("preparation_profile:", workflow)
        self.assertIn("default: cargo-schema", workflow)
        self.assertIn("cargo-schema|config-schema", workflow)
        self.assertIn(
            "config-schema preparation profile is restricted to prepare-only mode",
            workflow,
        )
        self.assertIn(
            "run: python3 .github/scripts/prepare_config_schema.py --validate-profile",
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
                legacy_step
                + "\n        if: ${{ inputs.preparation_profile == 'cargo-schema' }}",
                workflow,
            )
        self.assertIn("if: ${{ inputs.preparation_profile == 'config-schema' }}", workflow)
        self.assertEqual(
            prepare_config_schema.GENERATOR_ARGV,
            [
                "cargo",
                "run",
                "--locked",
                "-p",
                "codex-config-schema",
                "--bin",
                "codex-write-config-schema",
            ],
        )


class OutputFixture(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.schema = self.root / prepare_config_schema.SCHEMA_PATH
        self.lock = self.root / prepare_config_schema.LOCK_PATH
        self.schema.parent.mkdir(parents=True)
        self.lock.parent.mkdir(parents=True)
        self.schema_before = b'{"title":"before"}\n'
        self.lock_before = b"version = 4\n"
        self.schema.write_bytes(self.schema_before)
        self.lock.write_bytes(self.lock_before)
        self.git("init", "--quiet")
        self.git("add", prepare_config_schema.SCHEMA_PATH, prepare_config_schema.LOCK_PATH)
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

    def generated_schema(self, contents=b'{"title":"after"}\n'):
        self.schema.write_bytes(contents)

    def validate(self):
        return prepare_config_schema.validate_generated_output(
            self.root, self.schema_before, self.lock_before
        )


class GeneratedOutputTests(OutputFixture):
    def test_success_is_one_schema_patch_and_never_changes_lock(self):
        self.generated_schema()
        patch, changed_paths = self.validate()
        self.assertTrue(patch.startswith(b"diff --git a/codex-rs/core/config.schema.json"))
        self.assertEqual(changed_paths, [prepare_config_schema.SCHEMA_PATH])
        self.assertEqual(self.lock.read_bytes(), self.lock_before)

    def test_missing_schema_change_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "exact schema path"):
            self.validate()

    def test_extra_tracked_path_is_rejected(self):
        self.generated_schema()
        (self.root / "codex-rs/extra.json").write_text("{}\n", encoding="utf-8")
        self.git("add", "codex-rs/extra.json")
        with self.assertRaisesRegex(ValueError, "exact schema path"):
            self.validate()

    def test_cargo_lock_mutation_is_rejected(self):
        self.generated_schema()
        self.lock.write_bytes(b"version = 5\n")
        with self.assertRaisesRegex(ValueError, "changed Cargo.lock"):
            self.validate()

    def test_rename_is_rejected(self):
        self.schema.rename(self.root / "codex-rs/core/config.schema.moved.json")
        with self.assertRaisesRegex(ValueError, "exact schema path"):
            self.validate()

    def test_deletion_is_rejected(self):
        self.schema.unlink()
        with self.assertRaisesRegex(ValueError, "exact schema path"):
            self.validate()

    def test_untracked_file_is_rejected(self):
        self.generated_schema()
        (self.root / "codex-rs/extra.json").write_text("{}\n", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "exact schema path"):
            self.validate()

    def test_symlink_schema_is_rejected(self):
        self.schema.unlink()
        self.schema.symlink_to(self.lock)
        with self.assertRaisesRegex(ValueError, "nonsymlink"):
            self.validate()

    def test_empty_and_invalid_json_are_rejected(self):
        for contents in (b"", b"not json\n"):
            with self.subTest(contents=contents):
                self.schema.write_bytes(self.schema_before)
                self.generated_schema(contents)
                with self.assertRaisesRegex(ValueError, "empty|valid JSON"):
                    self.validate()


class IdentityFixture(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.workspace = Path(self.temporary.name)
        self.workflow_root = self.workspace / ".workflow-src"
        self.product_root = self.workspace / "product"
        self.git_init(self.workflow_root)
        self.git_init(self.product_root)
        (self.product_root / "base.txt").write_text("base\n", encoding="utf-8")
        self.git(self.product_root, "add", "base.txt")
        self.git(
            self.product_root,
            "-c",
            "user.name=Hosted Test",
            "-c",
            "user.email=hosted@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "base",
        )
        self.base_sha = self.git(self.product_root, "rev-parse", "HEAD").decode().strip()
        (self.product_root / "target.txt").write_text("target\n", encoding="utf-8")
        self.git(self.product_root, "add", "target.txt")
        self.git(
            self.product_root,
            "-c",
            "user.name=Hosted Test",
            "-c",
            "user.email=hosted@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "target",
        )
        self.target_sha = self.git(self.product_root, "rev-parse", "HEAD").decode().strip()
        self.git(self.product_root, "fetch", "--no-tags", ".", self.base_sha)
        (self.workflow_root / "host.txt").write_text("host\n", encoding="utf-8")
        self.git(self.workflow_root, "add", "host.txt")
        self.git(
            self.workflow_root,
            "-c",
            "user.name=Hosted Test",
            "-c",
            "user.email=hosted@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "host",
        )
        self.host_sha = self.git(self.workflow_root, "rev-parse", "HEAD").decode().strip()
        self.environment = {
            "EXPECTED_H": self.host_sha,
            "TARGET_SHA": self.target_sha,
            "BASE_SHA": self.base_sha,
            "GITHUB_REPOSITORY": "sednalabs/codex",
            "GITHUB_RUN_ID": "123",
            "GITHUB_RUN_ATTEMPT": "1",
            "GITHUB_WORKFLOW": "sedna-branch-build",
            "GITHUB_SERVER_URL": "https://github.com",
        }

    @staticmethod
    def git_init(root):
        root.mkdir(parents=True)
        subprocess.check_call(["git", "init", "--quiet", str(root)])

    @staticmethod
    def git(root, *args):
        return subprocess.check_output(["git", "-C", str(root), *args], stderr=subprocess.DEVNULL)


class InputIdentityTests(IdentityFixture):
    def test_exact_h_b_t_identity_passes(self):
        identity = prepare_config_schema.validate_input_identity(
            self.environment, self.workspace
        )
        self.assertEqual(identity["workflow_host_sha"], self.host_sha)
        self.assertEqual(identity["product_sha"], self.target_sha)
        self.assertEqual(identity["comparison_base_sha"], self.base_sha)
        self.assertTrue(identity["workflow_host_tree"])
        self.assertTrue(identity["product_tree"])

    def test_mismatched_h_b_or_t_is_rejected(self):
        for key in ("EXPECTED_H", "BASE_SHA", "TARGET_SHA"):
            bad_environment = dict(self.environment, **{key: "0" * 40})
            with self.subTest(key=key), self.assertRaisesRegex(
                ValueError, "does not match"
            ):
                prepare_config_schema.validate_input_identity(
                    bad_environment, self.workspace
                )


if __name__ == "__main__":
    unittest.main()
