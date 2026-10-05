#!/usr/bin/env python3
"""Closed caller/exit/whole-artifact join for control-plane preparation."""

import hashlib
import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import zipfile

import prepare_control_plane as preparation


BASE_SHA = "2b34a5d5da9d7d171de722aebfa19b38fa2e3861"
BASE_TREE = "47fe6716626cdfd7c565c3752ee0192350d05ff0"
TARGET_SHA = "50c7d663e407d9bbf791efeee6ff7b925253f9fa"
TARGET_TREE = "f6a30df428b1d8a2ae3a491f9604cb6d279302d1"
REPOSITORY = "sednalabs/codex"
CALLER = ".github/workflows/sedna-branch-build.yml"
CALLEE = ".github/workflows/validation-control-plane-prep.yml"
WORKFLOW_ID = 250252262
MAX_WITNESS_BYTES = 4096
SHA256_RE = re.compile(r"[0-9a-f]{64}\Z")
POSITIVE_RE = re.compile(r"[1-9][0-9]{0,19}\Z")
JOB_RE = re.compile(r"[A-Za-z0-9_-]{1,128}\Z")
FAILURE_CODES = frozenset({
    "invalid_profile", "invalid_identity", "checkout_preflight_failed",
    "helper_failed", "execution_interrupted", "unexpected_exception",
    "outputs_invalid", "witness_persist_failed",
})
DECLARATIONS = {
    "caller_job": "control_plane_preparation", "callee_path": CALLEE,
    "callee_job": "prepare", "preparation_step": "prepare",
    "preparation_step_name": "Prepare exact candidate",
}
EXPECTATIONS = {"registered_caller_path": CALLER, "registered_workflow_id": WORKFLOW_ID}


class RouteError(Exception):
    def __init__(self, code):
        self.code = code if code in FAILURE_CODES else "unexpected_exception"
        super().__init__(self.code)


def require(condition, code="outputs_invalid"):
    if not condition:
        raise RouteError(code)


def exact_keys(value, keys):
    require(isinstance(value, dict) and set(value) == set(keys))


def count(value, maximum=None):
    require(type(value) is int and value >= 0)
    require(maximum is None or value <= maximum)
    return value


def digest(data):
    return hashlib.sha256(data).hexdigest()


def validate_profile(env):
    require(env.get("MODE") == "prepare-only", "invalid_profile")
    require(env.get("PREPARATION_PROFILE") == "control-plane", "invalid_profile")
    require(env.get("CONSUMER_PROFILE") == "full", "invalid_profile")
    require(all(not env.get(key) for key in (
        "PRODUCER_RUN_ID", "PRODUCER_WORKFLOW_HOST_SHA", "FIXTURE_SHA", "SDK_SHA",
    )), "invalid_profile")
    require(env.get("BASE_SHA") == BASE_SHA and env.get("TARGET_SHA") == TARGET_SHA,
            "invalid_identity")
    require(isinstance(env.get("BASE_REF"), str)
            and preparation.valid_ref(env["BASE_REF"]), "invalid_identity")


def platform_context(env, helper_sha):
    require(env.get("GITHUB_REPOSITORY") == REPOSITORY, "invalid_identity")
    require(isinstance(helper_sha, str) and preparation.SHA_RE.fullmatch(helper_sha) is not None, "invalid_identity")
    require(env.get("GITHUB_SHA") == helper_sha, "invalid_identity")
    require(env.get("GITHUB_WORKFLOW_SHA") == helper_sha, "invalid_identity")
    ref = env.get("GITHUB_REF", "")
    require(isinstance(ref, str) and ref.startswith("refs/heads/") and preparation.valid_ref(ref), "invalid_identity")
    workflow_ref = env.get("GITHUB_WORKFLOW_REF", "")
    caller = next((path for path in (CALLER, CALLEE)
                   if workflow_ref == f"{REPOSITORY}/{path}@{ref}"), None)
    require(isinstance(workflow_ref, str) and caller is not None
            and len(workflow_ref.encode()) <= 512, "invalid_identity")
    run_id, attempt = env.get("GITHUB_RUN_ID", ""), env.get("GITHUB_RUN_ATTEMPT", "")
    require(isinstance(run_id, str) and isinstance(attempt, str)
            and POSITIVE_RE.fullmatch(run_id) is not None
            and POSITIVE_RE.fullmatch(attempt) is not None, "invalid_identity")
    job = env.get("GITHUB_JOB", "")
    require(isinstance(job, str) and JOB_RE.fullmatch(job) is not None, "invalid_identity")
    return {
        "repository": REPOSITORY, "workflow_sha": helper_sha, "workflow_ref": workflow_ref,
        "ref": ref, "caller_path": caller, "run_id": run_id, "run_attempt": attempt,
        "runtime_job_key": job,
    }


def validate_request(args):
    require(args.base_sha == BASE_SHA and args.target_sha == TARGET_SHA, "invalid_identity")
    require(preparation.SHA_RE.fullmatch(args.helper_sha) is not None, "invalid_identity")
    require(preparation.valid_ref(args.base_ref), "invalid_identity")
    workspace = Path(os.environ["GITHUB_WORKSPACE"]).resolve()
    roots = tuple(Path(getattr(args, key)).resolve() for key in ("helper", "base", "product"))
    require(roots == (workspace / ".workflow-src", workspace / "base", workspace / "product"),
            "invalid_identity")
    require(all(root.is_dir() and not Path(getattr(args, key)).is_symlink()
                for root, key in zip(roots, ("helper", "base", "product"))), "invalid_identity")
    temporary = Path(os.environ["RUNNER_TEMP"]).resolve()
    stem = f"control-plane-preparation-{os.environ['GITHUB_RUN_ID']}-{os.environ['GITHUB_RUN_ATTEMPT']}"
    require(Path(args.receipt).absolute() == temporary / (stem + ".json")
            and Path(args.patch).absolute() == temporary / (stem + ".patch"), "invalid_identity")
    require(not any(temporary == root or temporary.is_relative_to(root) for root in roots),
            "invalid_identity")


def checkout_preflight(args):
    validate_request(args)
    product, base, helper = (Path(getattr(args, key)).resolve()
                             for key in ("product", "base", "helper"))
    preparation.check_user_configuration(product)
    preparation.check_trusted_inputs(product, base)
    actual = {}
    for key, root, expected_sha, expected_tree in (
        ("helper", helper, args.helper_sha, None),
        ("base", base, BASE_SHA, BASE_TREE),
        ("target", product, TARGET_SHA, TARGET_TREE),
    ):
        sha, tree = preparation.identity(root)
        require(sha == expected_sha and preparation.SHA_RE.fullmatch(tree) is not None,
                "checkout_preflight_failed")
        require(expected_tree is None or tree == expected_tree, "checkout_preflight_failed")
        require(preparation.clean_checkout(root), "checkout_preflight_failed")
        actual[key + "_sha"], actual[key + "_tree"] = sha, tree
    require(preparation.check_candidate_delta(product, args.base_sha, args.target_sha, BASE_TREE)
            == len(preparation.SOURCE_PATHS), "checkout_preflight_failed")
    return actual


def witness_path(args):
    return Path(args.receipt).with_suffix(".route.json")


def make_witness(args, context):
    inner = preparation.make_receipt(args)
    return {
        "schema": "control-plane-preparation-route-v1", "status": "incomplete",
        "failure_code": None, "platform": context, "declarations": dict(DECLARATIONS),
        "expectations": dict(EXPECTATIONS), "identity": None,
        "request_fingerprint": inner["request_fingerprint"],
        "catalog_fingerprint": inner["catalog_fingerprint"], "helper_exit": None,
        "receipt": None, "patch": None, "complete": False,
    }


def encode_witness(witness):
    data = (json.dumps(witness, sort_keys=True, separators=(",", ":")) + "\n").encode()
    require(len(data) <= MAX_WITNESS_BYTES, "witness_persist_failed")
    return data


def persist_witness(args, witness):
    try:
        preparation.atomic_write(witness_path(args), encode_witness(witness))
    except BaseException:
        raise RouteError("witness_persist_failed") from None


def strict_json(data, maximum):
    require(isinstance(data, bytes) and len(data) <= maximum)
    def pairs(items):
        result = {}
        for key, value in items:
            require(key not in result)
            result[key] = value
        return result
    try:
        return json.loads(data, object_pairs_hook=pairs,
                          parse_constant=lambda value: require(False))
    except (ValueError, UnicodeError):
        raise RouteError("outputs_invalid") from None


def passed(value):
    exact_keys(value, ("status", "exit_code"))
    require(value["status"] == "passed" and type(value["exit_code"]) is int
            and value["exit_code"] == 0)


def validate_inner(data, patch, args, context, actual):
    require(isinstance(patch, bytes) and len(patch) <= preparation.MAX_PATCH_BYTES)
    receipt = strict_json(data, preparation.MAX_RECEIPT_BYTES)
    template = preparation.make_receipt(args)
    exact_keys(receipt, template)
    require(receipt["schema"] == template["schema"] and receipt["status"] == "complete"
            and receipt["failure_code"] is None and receipt["omissions"] == [])
    expected_identity = dict(template["identity"])
    expected_identity.update(actual)
    expected_identity.update(repository=context["repository"], workflow_commit=context["workflow_sha"],
                             workflow_ref=context["ref"], run_id=context["run_id"],
                             run_attempt=context["run_attempt"])
    require(receipt["identity"] == expected_identity)
    require(receipt["request_fingerprint"] == template["request_fingerprint"]
            and receipt["catalog_fingerprint"] == template["catalog_fingerprint"]
            and receipt["fixed_commands"] == template["fixed_commands"]
            and receipt["preflight_current_phase"] is None)
    passed(receipt["source_style"])
    passed(receipt["self_tests"])
    exact_keys(receipt["phases"], template["phases"])
    for phase in receipt["phases"].values():
        passed(phase)
    inventory = receipt["inventory"]
    exact_keys(inventory, ("candidate_path_count", "changed_path_count", "omitted_path_count"))
    require(count(inventory["candidate_path_count"], len(preparation.ALLOWED_PATHS))
            == len(preparation.SOURCE_PATHS))
    changed = count(inventory["changed_path_count"], len(preparation.ALLOWED_PATHS))
    require(type(inventory["omitted_path_count"]) is int and inventory["omitted_path_count"] == 0)
    require((changed == 0) == (len(patch) == 0))
    exact_keys(receipt["patch"], ("status", "bytes", "sha256"))
    require(count(receipt["patch"]["bytes"], preparation.MAX_PATCH_BYTES) == len(patch)
            and receipt["patch"]["status"] == "emitted" and receipt["patch"]["sha256"] == digest(patch))
    exact_keys(receipt["dependency_inventory"], ("base", "target_initial", "target_after_update", "target_final"))
    inventories = list(receipt["dependency_inventory"].values())
    for inventory in inventories:
        exact_keys(inventory, ("workspace_package_count", "workspace_sha256", "external_record_count", "external_sha256"))
        count(inventory["workspace_package_count"])
        count(inventory["external_record_count"])
        require(all(isinstance(inventory[key], str) and SHA256_RE.fullmatch(inventory[key])
                    for key in ("workspace_sha256", "external_sha256")))
    require(all(inventory == inventories[0] for inventory in inventories))
    coverage = receipt["metadata_coverage"]
    exact_keys(coverage, ("coverage_scope", "workspace_package_count", "external_package_count",
                         "locked_external_record_count", "selected_external_identity_sha256",
                         "unselected_locked_external_record_count"))
    require(coverage["coverage_scope"] == "selected_resolved_external_packages_only")
    for key in ("workspace_package_count", "external_package_count", "locked_external_record_count",
                "unselected_locked_external_record_count"):
        count(coverage[key])
    require(isinstance(coverage["selected_external_identity_sha256"], str)
            and SHA256_RE.fullmatch(coverage["selected_external_identity_sha256"]))
    require(count(receipt["workspace_package_count"]) == coverage["workspace_package_count"]
            == inventories[0]["workspace_package_count"])
    require(count(receipt["external_package_record_count"]) == coverage["external_package_count"])
    require(coverage["locked_external_record_count"] == inventories[0]["external_record_count"]
            == coverage["external_package_count"] + coverage["unselected_locked_external_record_count"])
    tools = receipt["toolchain"]
    exact_keys(tools, template["toolchain"])
    for key, value in template["toolchain"].items():
        if key not in {"status", "observed", "commands"}:
            require(type(tools[key]) is type(value) and tools[key] == value)
    require(tools["status"] == "expected_versions_and_installer_provenance_observed")
    names = ("rust", "rustfmt", "clippy", "just", "uv", "bazelisk", "bazel", "dotslash")
    exact_keys(tools["commands"], names)
    exact_keys(tools["observed"], (*names, "bazel_from_version_pair"))
    for name in names:
        passed(tools["commands"][name])
        value = tools["observed"][name]
        require(isinstance(value, str) and len(value) <= 128)
        if name == "rustfmt":
            require(value in {"1.9.0", "1.9.0-stable"})
        elif name == "dotslash":
            require(preparation.TOOL_VERSION_PATTERNS[name].fullmatch("DotSlash " + value))
        else:
            require(value == template["toolchain"][name])
    require(tools["observed"]["bazel_from_version_pair"] == "9.0.0")
    return receipt


def read_output(path, maximum):
    require(not path.is_symlink() and path.is_file())
    with path.open("rb") as stream:
        data = stream.read(maximum + 1)
    require(len(data) <= maximum)
    return data


def helper_argv(args):
    return ("python3", str(Path(args.helper) / ".github/scripts/prepare_control_plane.py"),
            "--product", args.product, "--base", args.base, "--helper", args.helper,
            "--target-sha", args.target_sha, "--base-sha", args.base_sha,
            "--helper-sha", args.helper_sha, "--base-ref", args.base_ref,
            "--patch", args.patch, "--receipt", args.receipt)


def helper_env(context):
    env = preparation.minimal_env()
    env.update({"GITHUB_REPOSITORY": context["repository"], "GITHUB_SHA": context["workflow_sha"],
                "GITHUB_REF": context["ref"], "GITHUB_RUN_ID": context["run_id"],
                "GITHUB_RUN_ATTEMPT": context["run_attempt"], "RUNNER_TEMP": os.environ["RUNNER_TEMP"]})
    return env


def launch_helper(args, env):
    return subprocess.run(helper_argv(args), cwd=Path(os.environ["GITHUB_WORKSPACE"]),
                          env=env, shell=False, check=False,
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode


def execute(args, preflight_only=False):
    witness = None
    try:
        context = platform_context(os.environ, args.helper_sha)
        validate_request(args)
        witness = make_witness(args, context)
        if preflight_only:
            persist_witness(args, witness)
            witness["identity"] = checkout_preflight(args)
            persist_witness(args, witness)
            return 0
        previous = strict_json(read_output(witness_path(args), MAX_WITNESS_BYTES), MAX_WITNESS_BYTES)
        expected_checkpoint = dict(witness, identity=previous.get("identity"))
        require(previous == expected_checkpoint and previous["identity"] is not None,
                "checkout_preflight_failed")
        persist_witness(args, witness)
        actual = {}
        for key, root, sha, tree in (("helper", args.helper, args.helper_sha, None),
                                    ("base", args.base, BASE_SHA, BASE_TREE),
                                    ("target", args.product, TARGET_SHA, TARGET_TREE)):
            observed_sha, observed_tree = preparation.identity(Path(root))
            require(observed_sha == sha and (tree is None or observed_tree == tree), "checkout_preflight_failed")
            require(preparation.SHA_RE.fullmatch(observed_tree) and preparation.clean_checkout(Path(root)),
                    "checkout_preflight_failed")
            actual[key + "_sha"], actual[key + "_tree"] = observed_sha, observed_tree
        require(previous["identity"] == actual, "checkout_preflight_failed")
        witness["identity"] = actual
        persist_witness(args, witness)
        exit_code = launch_helper(args, helper_env(context))
        require(type(exit_code) is int and -255 <= exit_code <= 255, "helper_failed")
        witness["helper_exit"] = exit_code
        if exit_code != 0:
            raise RouteError("helper_failed")
        receipt = read_output(Path(args.receipt), preparation.MAX_RECEIPT_BYTES)
        patch = read_output(Path(args.patch), preparation.MAX_PATCH_BYTES)
        validate_inner(receipt, patch, args, context, actual)
        witness.update(status="complete", complete=True,
                       receipt={"bytes": len(receipt), "sha256": digest(receipt)},
                       patch={"bytes": len(patch), "sha256": digest(patch)})
        persist_witness(args, witness)
        return 0
    except BaseException as exc:
        if witness is not None:
            code = exc.code if isinstance(exc, RouteError) else (
                "checkout_preflight_failed" if isinstance(exc, preparation.PreparationError) else
                "execution_interrupted" if isinstance(exc, KeyboardInterrupt) else "unexpected_exception")
            witness.update(status="incomplete", complete=False, failure_code=code,
                           receipt=None, patch=None)
            try:
                persist_witness(args, witness)
            except BaseException:
                pass
        return 1


def validate_bundle(receipt_bytes, witness_bytes, patch_bytes, args, platform):
    """Consume complete members only after explicit owner-supplied platform joins."""
    require(all(isinstance(data, bytes) for data in (receipt_bytes, witness_bytes, patch_bytes)))
    witness = strict_json(witness_bytes, MAX_WITNESS_BYTES)
    expected = make_witness(args, platform_context(platform, args.helper_sha))
    exact_keys(witness, expected)
    require(witness["platform"] == expected["platform"] and witness["platform"]["caller_path"] == CALLER)
    require(witness["declarations"] == DECLARATIONS and witness["expectations"] == EXPECTATIONS)
    require(type(witness["expectations"]["registered_workflow_id"]) is int)
    require(witness["status"] == "complete" and witness["complete"] is True
            and witness["failure_code"] is None and type(witness["helper_exit"]) is int
            and witness["helper_exit"] == 0)
    require(platform.get("workflow_id") == WORKFLOW_ID and type(platform.get("workflow_id")) is int
            and platform.get("workflow_path") == CALLER)
    require(platform.get("inputs") == {"mode": "prepare-only", "preparation_profile": "control-plane",
            "consumer_profile": "full", "target_sha": args.target_sha, "base_sha": args.base_sha,
            "base_ref": args.base_ref, "producer_run_id": "", "producer_workflow_host_sha": "",
            "fixture_sha": "", "sdk_sha": ""})
    require(args.base_sha == BASE_SHA and args.target_sha == TARGET_SHA
            and preparation.valid_ref(args.base_ref)
            and platform.get("comparison_ref_sha") == BASE_SHA
            and platform.get("comparison_ref") == args.base_ref)
    for key in ("run_conclusion", "job_conclusion", "step_conclusion"):
        require(platform.get(key) == "success")
    require(platform.get("step_name") == DECLARATIONS["preparation_step_name"])
    for key in ("job_id", "step_number", "receipt_artifact_id", "patch_artifact_id"):
        require(type(platform.get(key)) is int and platform[key] > 0)
    require(platform["receipt_artifact_id"] != platform["patch_artifact_id"])
    suffix = f"{witness['platform']['run_id']}-{witness['platform']['run_attempt']}"
    require(Path(args.receipt).name == "control-plane-preparation-" + suffix + ".json"
            and Path(args.patch).name == "control-plane-preparation-" + suffix + ".patch")
    require(platform.get("receipt_artifact_name") == "control-plane-preparation-receipt-" + suffix
            and platform.get("patch_artifact_name") == "control-plane-preparation-patch-" + suffix)
    identity = witness["identity"]
    exact_keys(identity, ("helper_sha", "helper_tree", "base_sha", "base_tree", "target_sha", "target_tree"))
    require(identity == {"helper_sha": args.helper_sha, "helper_tree": platform.get("helper_tree"),
                         "base_sha": BASE_SHA, "base_tree": BASE_TREE,
                         "target_sha": TARGET_SHA, "target_tree": TARGET_TREE})
    require(isinstance(identity["helper_tree"], str) and preparation.SHA_RE.fullmatch(identity["helper_tree"]))
    require(witness["request_fingerprint"] == expected["request_fingerprint"]
            and witness["catalog_fingerprint"] == expected["catalog_fingerprint"])
    for key, data, maximum in (("receipt", receipt_bytes, preparation.MAX_RECEIPT_BYTES),
                               ("patch", patch_bytes, preparation.MAX_PATCH_BYTES)):
        exact_keys(witness[key], ("bytes", "sha256"))
        require(count(witness[key]["bytes"], maximum) == len(data)
                and witness[key]["sha256"] == digest(data))
    validate_inner(receipt_bytes, patch_bytes, args, witness["platform"], identity)
    return {"status": "accepted", "patch_base_sha": TARGET_SHA,
            "prepared_successor": "not_materialized", "helper_exit": 0,
            "receipt_sha256": digest(receipt_bytes), "patch_sha256": digest(patch_bytes)}


def archive_members(archive, expected):
    require(isinstance(archive, bytes) and len(archive) <= preparation.MAX_PATCH_BYTES + 2 * preparation.MAX_RECEIPT_BYTES)
    try:
        with zipfile.ZipFile(io.BytesIO(archive)) as bundle:
            infos = bundle.infolist()
            require(len(infos) == len(expected) and {item.filename for item in infos} == set(expected))
            result = {}
            for item in infos:
                require(not item.is_dir() and item.file_size <= expected[item.filename]
                        and not item.flag_bits & 1
                        and (item.external_attr >> 16) & 0o170000 in {0, 0o100000})
                with bundle.open(item) as stream:
                    value = stream.read(expected[item.filename] + 1)
                require(len(value) == item.file_size and len(value) <= expected[item.filename])
                result[item.filename] = value
            return result
    except RouteError:
        raise
    except Exception:
        raise RouteError("outputs_invalid") from None


def consume_artifacts(receipt_archive, patch_archive, args, platform):
    for key, archive in (("receipt", receipt_archive), ("patch", patch_archive)):
        require(isinstance(archive, bytes))
        supplied = platform.get(key + "_artifact_digest")
        require(supplied is None or supplied == "sha256:" + digest(archive))
    receipt_name = Path(args.receipt).name
    patch_name = Path(args.patch).name
    outer_name = witness_path(args).name
    members = archive_members(receipt_archive, {receipt_name: preparation.MAX_RECEIPT_BYTES,
                                               outer_name: MAX_WITNESS_BYTES})
    patches = archive_members(patch_archive, {patch_name: preparation.MAX_PATCH_BYTES})
    result = validate_bundle(members[receipt_name], members[outer_name], patches[patch_name], args, platform)
    return dict(result, receipt_archive_sha256=digest(receipt_archive), patch_archive_sha256=digest(patch_archive),
                platform_digest_coverage={key: "verified" if platform.get(key + "_artifact_digest") is not None
                                          else "not_exposed" for key in ("receipt", "patch")})


def main(argv=None):
    try:
        parser = preparation.parser()
        parser.add_argument("--preflight", action="store_true")
        if argv is None:
            argv = sys.argv[1:]
        if argv == ["--validate-profile"]:
            validate_profile(os.environ)
            return 0
        return execute(parser.parse_args(argv), preflight_only="--preflight" in argv)
    except BaseException:
        return 2


if __name__ == "__main__":
    sys.exit(main())
